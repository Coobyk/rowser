//! True GPU rasterization: rasterize a display list with wgpu instead of
//! the CPU pixel loops.
//!
//! Native paths: solid/rounded fills, border edges, and text glyphs share
//! a single instanced pipeline (rects are SDF quads, glyphs sample an R8
//! atlas — glyphs still rasterize on CPU, the GPU does compositing), so
//! one painter-ordered instance stream covers both and draws only split
//! at image-pipe boundaries. Images/rasters draw as textured quads, and
//! rectangular clips ride inside each instance (fragment discard) instead
//! of scissor state, so clip changes cost zero draw calls.
//!
//! Anything the pipelines can't express — transform groups, effect groups
//! (filter/backdrop/blend/opacity), box shadows, decorated borders,
//! rounded clips, tiled backgrounds — is rasterized by the existing CPU
//! painter into a bounded tile, uploaded, and drawn as a textured quad in
//! painter order. Output is premultiplied BGRA, identical layout to
//! [`crate::paint`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rowser_image::Image;
use rowser_layout::xform::Mat;
use rowser_layout::{BorderEdge, BorderStyle, Color, DisplayItem, offset_item};
use rowser_text::FontCache;

use crate::{Canvas, Fonts, paint_list};

const ATLAS_SIZE: u32 = 2048;

// ---------------------------------------------------------------------------
// Shaders

/// One pipeline for rects AND glyphs: `misc.x` picks the coverage source
/// per instance (0 = rounded-rect SDF, 1 = atlas glyph quad). Sharing a
/// pipeline — and therefore a single painter-ordered instance stream —
/// means rect↔text transitions no longer split draws; on a text-heavy
/// page that alone removes most of the per-frame draw calls.
const SH_MAIN: &str = r#"
struct View { dims: vec4f };
@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var gsamp: sampler;

struct UIn {
    @location(0) geom: vec4f,  // rect: x,y,w,h   | glyph: dst x,y,w,h
    @location(1) aux: vec4f,   // rect: x radii   | glyph: src u,v,w,h (atlas px)
    @location(2) aux2: vec4f,  // rect: y radii   | glyph: unused
    @location(3) color: vec4f, // straight rgba 0..1
    @location(4) clip: vec4f,  // folded clip stack, target px: x0,y0,x1,y1
    @location(5) misc: vec4f,  // x: kind — 0 rect (SDF), 1 glyph (atlas)
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) tpos: vec2f,
    @location(1) geom: vec4f,
    @location(2) aux: vec4f,
    @location(3) aux2: vec4f,
    @location(4) color: vec4f,
    @location(5) uv: vec2f,
    @location(6) clip: vec4f,
    @location(7) misc: vec4f,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32, u: UIn) -> VOut {
    var corners = array<vec2f, 4>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0), vec2f(1.0, 1.0));
    let c = corners[vi];
    let p = u.geom.xy + c * u.geom.zw;
    let ndc = vec2f(p.x / view.dims.x * 2.0 - 1.0, 1.0 - p.y / view.dims.y * 2.0);
    var o: VOut;
    o.pos = vec4f(ndc, 0.0, 1.0);
    o.tpos = p;
    o.geom = u.geom;
    o.aux = u.aux;
    o.aux2 = u.aux2;
    o.color = u.color;
    // for rects `aux` is radii — this samples a meaningless (but in-bounds
    // after clamping) texel whose value `misc` discards in the fragment
    o.uv = (u.aux.xy + 0.5 + c * (u.aux.zw - 1.0)) / vec2f(textureDimensions(atlas));
    o.clip = u.clip;
    o.misc = u.misc;
    return o;
}

// signed distance to a per-corner rounded rect (elliptical approx)
fn sd_rounded(p: vec2f, b: vec4f, rx: vec4f, ry: vec4f) -> f32 {
    let c = b.xy + b.zw * 0.5;
    let q = p - c;
    let rcx = select(select(rx.x, rx.y, q.x > 0.0), select(rx.w, rx.z, q.x > 0.0), q.y > 0.0);
    let rcy = select(select(ry.x, ry.y, q.x > 0.0), select(ry.w, ry.z, q.x > 0.0), q.y > 0.0);
    let r = min(vec2f(rcx, rcy), b.zw * 0.5);
    let d = abs(q) - (b.zw * 0.5 - r);
    let outside = length(max(d, vec2f(0.0)));
    let inside = min(max(d.x, d.y), 0.0);
    return outside + inside - min(r.x, r.y);
}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    // Per-instance clip — fragment centres are integer+0.5, so comparing
    // against the integer clip rect reproduces the CPU painter's hard
    // [x0,x1) clip exactly. This replaces scissor state: clip changes no
    // longer split draws.
    if v.tpos.x < v.clip.x || v.tpos.y < v.clip.y
        || v.tpos.x >= v.clip.z || v.tpos.y >= v.clip.w {
        discard;
    }
    let d = sd_rounded(v.tpos, v.geom, v.aux, v.aux2);
    let sdf_cov = clamp(0.5 - d, 0.0, 1.0);
    // explicit LOD so the sample is legal regardless of `misc` uniformity
    let tex_cov = textureSampleLevel(atlas, gsamp, v.uv, 0.0).r;
    let cov = select(sdf_cov, tex_cov, v.misc.x > 0.5);
    let a = v.color.a * cov;
    return vec4f(v.color.rgb * a, a);
}
"#;

const SH_IMG: &str = r#"
struct View { dims: vec4f };
@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct IIn {
    @location(0) dst: vec4f,   // x, y, w, h
    @location(1) uv: vec4f,    // u0, v0, uw, vh in 0..1
    @location(2) m0: vec4f,    // affine row 1 + dst offset (transformed tiles)
    @location(3) m1: vec4f,    // affine row 2 + v offset base
    @location(4) clip: vec4f,  // folded clip stack, target px: x0,y0,x1,y1
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) tpos: vec2f,
    @location(2) clip: vec4f,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32, g: IIn) -> VOut {
    var corners = array<vec2f, 4>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0), vec2f(1.0, 1.0));
    let c = corners[vi];
    let lp = g.dst.xy + c * g.dst.zw;          // tile-space pixel
    let p = vec2f(g.m0.x * lp.x + g.m0.y * lp.y + g.m0.z,
                  g.m1.x * lp.x + g.m1.y * lp.y + g.m1.z);
    let ndc = vec2f(p.x / view.dims.x * 2.0 - 1.0, 1.0 - p.y / view.dims.y * 2.0);
    var o: VOut;
    o.pos = vec4f(ndc, 0.0, 1.0);
    o.uv = g.uv.xy + c * g.uv.zw;
    o.tpos = p;
    o.clip = g.clip;
    return o;
}
@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    if v.tpos.x < v.clip.x || v.tpos.y < v.clip.y
        || v.tpos.x >= v.clip.z || v.tpos.y >= v.clip.w {
        discard;
    }
    return textureSample(tex, samp, v.uv);     // pixels are premultiplied
}
"#;

// ---------------------------------------------------------------------------
// Instance records (bytemuck-free: plain arrays, cast to bytes)

type F4 = [f32; 4];

/// Unified rect/glyph instance — `misc[0]` selects coverage (0 = SDF
/// rounded rect, 1 = atlas glyph). Both primitives share one
/// painter-ordered instance stream so the walk only splits draws at
/// image-pipe boundaries.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UI {
    geom: F4,   // rect: x,y,w,h      | glyph: dst x,y,w,h
    aux: F4,    // rect: x radii      | glyph: src u,v,w,h in atlas px
    aux2: F4,   // rect: y radii      | glyph: unused
    color: F4,
    clip: F4,   // folded clip stack: x0,y0,x1,y1 (target px)
    misc: F4,   // x: kind — 0 rect, 1 glyph
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ImgI {
    dst: F4,
    uv: F4,
    m0: F4,
    m1: F4,
    clip: F4,
}

/// Match the CPU rasterizer's `x as i32` coordinate snap.
fn snap(v: f32) -> f32 {
    (v as i32) as f32
}
/// Match the CPU rasterizer's `v as u32` size snap.
fn snatu(v: f32) -> f32 {
    (v as u32) as f32
}
/// fx::shade — lighten/darken a BGRA color by factor f (-1..1).
fn gpu_shade(c: u32, f: f32) -> u32 {
    let m = |v: u8| -> u8 {
        if f >= 0.0 {
            (v as f32 + (255.0 - v as f32) * f) as u8
        } else {
            (v as f32 * (1.0 + f)) as u8
        }
    };
    u32::from_le_bytes([m((c & 0xff) as u8), m(((c >> 8) & 0xff) as u8), m(((c >> 16) & 0xff) as u8), 255])
}

fn as_bytes<T: Copy>(s: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

fn srgb(c: Color) -> F4 {
    // Color is a BGRA u32 (LE bytes [b,g,r,a]) — premultiplied. Shader-space
    // colors are straight vec4(r,g,b,a): the Bgra8Unorm target swizzles on
    // write, and blending expects premultiplied sources so multiply rgb by a.
    let a = (c >> 24) as f32 / 255.0;
    [
        ((c >> 16) & 0xff) as f32 / 255.0,
        ((c >> 8) & 0xff) as f32 / 255.0,
        (c & 0xff) as f32 / 255.0,
        a,
    ]
}

// ---------------------------------------------------------------------------
// GPU context (lazy global — device init is expensive)

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    main_pipe: wgpu::RenderPipeline,
    img_pipe: wgpu::RenderPipeline,
    atlas: wgpu::Texture,
    atlas_bg: wgpu::BindGroup,
    atlas_map: HashMap<(usize, u16, u32), (u32, u32)>,
    atlas_pen: (u32, u32, u32), // x, y, row height
    img_bgl: wgpu::BindGroupLayout,
    samp_lin: wgpu::Sampler,
    samp_near: wgpu::Sampler,
    // keyed by src pixel ptr; the u64 is a per-texture id used to merge
    // consecutive same-texture draws
    img_tex: HashMap<usize, (u64, wgpu::Texture, wgpu::BindGroup)>,
    next_img_id: u64,
    // Per-frame resources cached across calls — device-side allocation is
    // the expensive part, not reuse. Recreated only on size change/growth.
    target: Option<(u32, u32, wgpu::Texture)>,
    vbuf: Option<wgpu::Buffer>,
    vcap: u64,
    rbuf: Option<(u32, u32, wgpu::Buffer)>, // keyed by (w, h)
}

fn gpu() -> Result<&'static Mutex<Gpu>, String> {
    static G: OnceLock<Result<Mutex<Gpu>, String>> = OnceLock::new();
    G.get_or_init(|| Gpu::new().map(Mutex::new))
        .as_ref()
        .map_err(|e| e.clone())
}

fn inst_layout(step: u64, attrs: &'static [wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: step,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: attrs,
    }
}

static MAIN_ATTRS: [wgpu::VertexAttribute; 6] = [
    wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 16, shader_location: 1, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 32, shader_location: 2, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 48, shader_location: 3, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 64, shader_location: 4, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 80, shader_location: 5, format: wgpu::VertexFormat::Float32x4 },
];
static IMG_ATTRS: [wgpu::VertexAttribute; 5] = [
    wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 16, shader_location: 1, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 32, shader_location: 2, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 48, shader_location: 3, format: wgpu::VertexFormat::Float32x4 },
    wgpu::VertexAttribute { offset: 64, shader_location: 4, format: wgpu::VertexFormat::Float32x4 },
];

const BLEND_PREMULT: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

impl Gpu {
    fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rowser-paint"),
            ..Default::default()
        }))
        .map_err(|e| format!("device request failed: {e}"))?;

        let view_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("view"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let view_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view"),
            layout: &view_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            }],
        });
        let glyph_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let img_bgl = glyph_bgl.clone();
        let main_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("main"),
            bind_group_layouts: &[Some(&view_bgl), Some(&glyph_bgl)],
            immediate_size: 0,
        });
        let img_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("img"),
            bind_group_layouts: &[Some(&view_bgl), Some(&img_bgl)],
            immediate_size: 0,
        });

        let mk_pipe = |src: &str,
                       name: &str,
                       layout: &wgpu::PipelineLayout,
                       vbuf: wgpu::VertexBufferLayout<'_>|
         -> wgpu::RenderPipeline {
            let sh = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(name),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &sh,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(vbuf)],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &sh,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        blend: Some(BLEND_PREMULT),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };

        let main_pipe = mk_pipe(
            SH_MAIN,
            "main",
            &main_pl,
            inst_layout(std::mem::size_of::<UI>() as u64, &MAIN_ATTRS),
        );
        let img_pipe = mk_pipe(
            SH_IMG,
            "img",
            &img_pl,
            inst_layout(std::mem::size_of::<ImgI>() as u64, &IMG_ATTRS),
        );

        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas.create_view(&Default::default());

        let samp_lin = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let samp_near = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let atlas_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &glyph_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&samp_near),
                },
            ],
        });

        Ok(Gpu {
            device,
            queue,
            view_buf,
            view_bg,
            main_pipe,
            img_pipe,
            atlas,
            atlas_bg,
            atlas_map: HashMap::new(),
            atlas_pen: (0, 0, 0),
            img_bgl,
            samp_lin,
            samp_near,
            img_tex: HashMap::new(),
            next_img_id: 1,
            target: None,
            vbuf: None,
            vcap: 0,
            rbuf: None,
        })
    }

    fn atlas_slot(&mut self, key: (usize, u16, u32), w: u32, h: u32) -> Option<(u32, u32)> {
        if let Some(&uv) = self.atlas_map.get(&key) {
            return Some(uv);
        }
        let (w, h) = (w + 1, h + 1); // pad
        let (mut x, mut y, mut rh) = self.atlas_pen;
        if x + w > ATLAS_SIZE {
            x = 0;
            y += rh + 1;
            rh = 0;
        }
        if y + h > ATLAS_SIZE {
            return None;
        }
        rh = rh.max(h);
        self.atlas_pen = (x + w, y, rh);
        self.atlas_map.insert(key, (x, y));
        Some((x, y))
    }

    /// Cached image-texture bind group. Returns `(id, bind group)` — the
    /// id identifies the texture so consecutive draws on it can merge.
    fn img_bg(&mut self, key: usize, img: &Image, near: bool) -> (u64, wgpu::BindGroup) {
        if !self.img_tex.contains_key(&key) {
            let id = self.next_img_id;
            self.next_img_id += 1;
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("img"),
                size: wgpu::Extent3d {
                    width: img.width,
                    height: img.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &img.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(img.width * 4),
                    rows_per_image: Some(img.height),
                },
                wgpu::Extent3d {
                    width: img.width,
                    height: img.height,
                    depth_or_array_layers: 1,
                },
            );
            let view = tex.create_view(&Default::default());
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("img"),
                layout: &self.img_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(if near {
                            &self.samp_near
                        } else {
                            &self.samp_lin
                        }),
                    },
                ],
            });
            self.img_tex.insert(key, (id, tex, bg));
        }
        let e = &self.img_tex[&key];
        (e.0, e.2.clone())
    }

    /// Fresh texture + bind group for a CPU-rendered tile. Each tile gets
    /// a unique id (tiles are transient — never merged across draws).
    fn tile_bg(&mut self, w: u32, h: u32, px: &[u8]) -> (u64, wgpu::Texture, wgpu::BindGroup) {
        let id = self.next_img_id;
        self.next_img_id += 1;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tile"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = tex.create_view(&Default::default());
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tile"),
            layout: &self.img_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.samp_lin),
                },
            ],
        });
        (id, tex, bg)
    }
}

// ---------------------------------------------------------------------------

/// Rasterize `items` on the GPU. Returns `None` when no adapter/device is
/// available — callers should fall back to [`crate::paint_par`].
pub fn paint_gpu(
    items: &[DisplayItem],
    fonts: &mut FontCache,
    width: u32,
    height: u32,
    bg: Color,
    images: &HashMap<String, Image>,
) -> Option<Vec<u8>> {
    let g = gpu().ok()?;
    let mut g = g.lock().ok()?;
    Some(g.paint(items, fonts, width, height, bg, images))
}

/// GPU rasterization with CPU fallback. Off by default — set
/// `ROWSR_GPU=1` to enable. The renderer ships every frame to the
/// chrome process as CPU bytes over IPC; GPU+readback still carries an
/// unavoidable synchronous map+poll round-trip, so the tuned CPU
/// rasterizer keeps the default at typical item counts. The GPU path
/// exists for correctness work and for a future on-screen-surface path
/// where the frame never leaves the device. Output is premultiplied
/// BGRA either way.
pub fn paint_auto(
    items: &[DisplayItem],
    fonts: &mut FontCache,
    width: u32,
    height: u32,
    bg: Color,
    images: &HashMap<String, Image>,
) -> Vec<u8> {
    if std::env::var_os("ROWSR_GPU").is_some_and(|v| v != "0") {
        if let Some(px) = paint_gpu(items, fonts, width, height, bg, images) {
            return px;
        }
    }
    crate::paint_par(items, fonts, width, height, bg, images)
}

impl Gpu {
    fn paint(
        &mut self,
        items: &[DisplayItem],
        fonts: &mut FontCache,
        width: u32,
        height: u32,
        bg: Color,
        images: &HashMap<String, Image>,
    ) -> Vec<u8> {
        let (w, h) = (width.max(1), height.max(1));
        self.queue
            .write_buffer(&self.view_buf, 0, &as_bytes(&[w as f32, h as f32, 0.0, 0.0]));

        // Cached render target — recreated only when the size changes.
        if !matches!(&self.target, Some((tw, th, _)) if *tw == w && *th == h) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("target"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            self.target = Some((w, h, tex));
        }
        let tview = self.target.as_ref().unwrap().2.create_view(&Default::default());

        // Instance streams + recorded draws. Rects and glyphs share one
        // painter-ordered stream (`maini`) — the unified pipeline reads a
        // per-instance kind — and the folded clip stack travels inside
        // each instance instead of a scissor command, so clip changes and
        // rect↔text transitions no longer split draws. The only splits
        // left are at image-pipe boundaries: a ~17k-item page drops from
        // thousands of draws to typically a few dozen.
        let mut maini: Vec<UI> = Vec::with_capacity(items.len() + 64);
        let mut imgi: Vec<ImgI> = Vec::new();
        let mut live: Vec<wgpu::BindGroup> = Vec::new();
        let mut live_map: HashMap<u64, usize> = HashMap::new();
        let mut keep: Vec<wgpu::Texture> = Vec::new();
        #[derive(Clone, Copy)]
        enum Cmd {
            Main { start: u32, n: u32 },
            Img { id: u64, start: u32, n: u32 },
        }
        let mut cmds: Vec<Cmd> = Vec::with_capacity(64);
        let mut main_start = 0usize;

        let mut fonts = Fonts::Direct(fonts);
        let mut clip_stack: Vec<[i32; 4]> = Vec::new();
        let mut cur_clip = [0i32, 0, w as i32, h as i32];
        let mut glyphs_scratch: Vec<(u16, f32)> = Vec::new();

        let clipf = |c: [i32; 4]| -> F4 {
            [c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32]
        };

        macro_rules! flush_main {
            () => {
                if maini.len() > main_start {
                    cmds.push(Cmd::Main {
                        start: main_start as u32,
                        n: (maini.len() - main_start) as u32,
                    });
                    main_start = maini.len();
                }
            };
        }
        // Queue an image-pipe draw: flushes pending main instances, keeps
        // the bind group alive, and merges with the previous draw when it
        // used the same texture (instances are contiguous in `imgi`).
        macro_rules! push_img {
            ($id:expr, $bgroup:expr, $inst:expr) => {{
                flush_main!();
                let id: u64 = $id;
                let inst: Vec<ImgI> = $inst;
                if !inst.is_empty() {
                    let start = imgi.len() as u32;
                    let n = inst.len() as u32;
                    imgi.extend_from_slice(&inst);
                    if !live_map.contains_key(&id) {
                        live_map.insert(id, live.len());
                        live.push($bgroup);
                    }
                    // merge only when the immediately-preceding draw used
                    // the same texture — the new instances are contiguous
                    // in `imgi`, so just extend its count
                    let merged = match cmds.last_mut() {
                        Some(Cmd::Img { id: prev, n: pn, .. }) if *prev == id => {
                            *pn += n;
                            true
                        }
                        _ => false,
                    };
                    if !merged {
                        cmds.push(Cmd::Img { id, start, n });
                    }
                }
            }};
        }
        // Re-fold the clip stack — instances carry the clip rect, so this
        // is a pure CPU-side state update: no flush, no scissor command.
        macro_rules! fold_clip {
            () => {
                cur_clip = clip_stack
                    .iter()
                    .fold([0i32, 0, w as i32, h as i32], |acc, c| {
                        [
                            acc[0].max(c[0]),
                            acc[1].max(c[1]),
                            acc[2].min(c[2]),
                            acc[3].min(c[3]),
                        ]
                    });
            };
        }
        macro_rules! push_rect {
            ($geom:expr, $rx:expr, $ry:expr, $color:expr) => {
                maini.push(UI {
                    geom: $geom,
                    aux: $rx,
                    aux2: $ry,
                    color: srgb($color),
                    clip: clipf(cur_clip),
                    misc: [0.0; 4],
                })
            };
        }

        // helper: CPU-paint a span into a tile and queue it as an image draw
        macro_rules! cpu_tile {
            ($span:expr, $m:expr) => {{
                let span: &[DisplayItem] = $span;
                if let Some(bb) = crate::fx::items_bbox_pub(span, &mut fonts) {
                    let pad = 24.0f32;
                    let (bx, by) = (bb[0] - pad, bb[1] - pad);
                    let (tw, th) = (
                        (bb[2] - bb[0] + 2.0 * pad).ceil() as u32,
                        (bb[3] - bb[1] + 2.0 * pad).ceil() as u32,
                    );
                    if tw > 0 && th > 0 && tw as u64 * th as u64 <= 64 * 1024 * 1024 {
                        let mut tile = Canvas {
                            width: tw,
                            height: th,
                            pixels: vec![0; tw as usize * th as usize * 4],
                            clips: Vec::new(),
                            band: (0, th as i32),
                        };
                        let shifted: Vec<DisplayItem> = span
                            .iter()
                            .cloned()
                            .map(|it| offset_item(it, -bx, -by))
                            .collect();
                        paint_list(&mut tile, &shifted, &mut fonts, images);
                        let (tid, tex, bgt) = self.tile_bg(tw, th, &tile.pixels);
                        keep.push(tex);
                        let bm: Mat = $m.mul(&Mat::translate(bx, by));
                        push_img!(
                            tid,
                            bgt,
                            vec![ImgI {
                                dst: [0.0, 0.0, tw as f32, th as f32],
                                uv: [0.0, 0.0, 1.0, 1.0],
                                m0: [bm.0[0], bm.0[2], bm.0[4], 0.0],
                                m1: [bm.0[1], bm.0[3], bm.0[5], 0.0],
                                clip: clipf(cur_clip),
                            }]
                        );
                    }
                }
            }};
        }

        let twalk = std::time::Instant::now();
        let mut i = 0;
        while i < items.len() {
            let item = &items[i];
            match item {
                DisplayItem::FillRect { x, y, w: iw, h: ih, color } => {
                    // CPU snaps to i32/u32 — matching keeps coverage exact
                    push_rect!(
                        [snap(*x), snap(*y), snatu(*iw), snatu(*ih)],
                        [0.0; 4],
                        [0.0; 4],
                        *color
                    );
                }
                DisplayItem::RoundedRect { x, y, w: iw, h: ih, radius, color } => {
                    let r = *radius;
                    push_rect!(
                        [snap(*x), snap(*y), snatu(*iw), snatu(*ih)],
                        [r; 4],
                        [r; 4],
                        *color
                    );
                }
                DisplayItem::RoundedRect2 { x, y, w: iw, h: ih, radii, color } => {
                    push_rect!(
                        [snap(*x), snap(*y), snatu(*iw), snatu(*ih)],
                        [radii[0][0], radii[1][0], radii[2][0], radii[3][0]],
                        [radii[0][1], radii[1][1], radii[2][1], radii[3][1]],
                        *color
                    );
                }
                DisplayItem::BorderSide { x, y, w: iw, h: ih, side, style, color } => {
                    // Mirror fx::border_side — same snap + expansion math.
                    let horiz = matches!(side, BorderEdge::Top | BorderEdge::Bottom);
                    let thick = if horiz { *ih } else { *iw };
                    let len = if horiz { *iw } else { *ih };
                    if thick <= 0.0 || len <= 0.0 {
                        i += 1;
                        continue;
                    }
                    macro_rules! bside {
                        ($bx:expr, $by:expr, $bw:expr, $bh:expr, $c:expr) => {
                            push_rect!(
                                [snap($bx), snap($by), snatu($bw), snatu($bh)],
                                [0.0; 4],
                                [0.0; 4],
                                $c
                            )
                        };
                    }
                    match style {
                        BorderStyle::Solid => {
                            bside!(*x, *y, *iw, *ih, *color);
                        }
                        BorderStyle::Dashed => {
                            let dash = (3.0 * thick).max(1.0);
                            let gap = dash;
                            let mut p = 0.0f32;
                            while p < len {
                                let e = (p + dash).min(len);
                                if horiz {
                                    bside!(*x + p, *y, e - p, *ih, *color);
                                } else {
                                    bside!(*x, *y + p, *iw, e - p, *color);
                                }
                                p += dash + gap;
                            }
                        }
                        BorderStyle::Double => {
                            let t = (thick / 3.0).floor().max(1.0);
                            if thick < 3.0 {
                                bside!(*x, *y, *iw, *ih, *color);
                            } else if horiz {
                                bside!(*x, *y, *iw, t, *color);
                                bside!(*x, *y + thick - t, *iw, t, *color);
                            } else {
                                bside!(*x, *y, t, *ih, *color);
                                bside!(*x + thick - t, *y, t, *ih, *color);
                            }
                        }
                        BorderStyle::Groove | BorderStyle::Ridge
                        | BorderStyle::Inset | BorderStyle::Outset => {
                            let dark = gpu_shade(*color, -0.45);
                            let light = gpu_shade(*color, 0.55);
                            let sunken =
                                matches!(style, BorderStyle::Groove | BorderStyle::Inset);
                            let outer_dark =
                                matches!(side, BorderEdge::Top | BorderEdge::Left) == sunken;
                            let (outer, inner) =
                                if outer_dark { (dark, light) } else { (light, dark) };
                            let half = (thick / 2.0).max(0.0);
                            if half < 0.5 {
                                bside!(*x, *y, *iw, *ih, outer);
                            } else {
                                match side {
                                    BorderEdge::Top => {
                                        bside!(*x, *y, *iw, half, outer);
                                        bside!(*x, *y + half, *iw, *ih - half, inner);
                                    }
                                    BorderEdge::Bottom => {
                                        bside!(*x, *y + half, *iw, *ih - half, outer);
                                        bside!(*x, *y, *iw, half, inner);
                                    }
                                    BorderEdge::Left => {
                                        bside!(*x, *y, half, *ih, outer);
                                        bside!(*x + half, *y, *iw - half, *ih, inner);
                                    }
                                    BorderEdge::Right => {
                                        bside!(*x + half, *y, *iw - half, *ih, outer);
                                        bside!(*x, *y, half, *ih, inner);
                                    }
                                }
                            }
                        }
                        _ => {
                            // Dotted needs per-pixel discs — CPU tile.
                            cpu_tile!(&items[i..i + 1], &Mat::IDENTITY);
                        }
                    }
                }
                DisplayItem::Text {
                    x,
                    baseline,
                    text,
                    font,
                    size,
                    color,
                    deco,
                    ls,
                    ..
                } => {
                    glyphs_scratch.clear();
                    let mut pen = *x;
                    fonts.with_font(*font, |f| {
                        let upem = f.units_per_em() as f32;
                        let mut prev = 0u16;
                        for c in text.chars() {
                            let gid = f.glyph_id(c);
                            pen += f.kern(prev, gid) as f32 * size / upem;
                            glyphs_scratch.push((gid, pen));
                            pen += f.advance(gid, *size) + ls;
                            prev = gid;
                        }
                    });
                    let pxq = (*size * 4.0) as u32;
                    for &(gid, gp) in &glyphs_scratch {
                        let Some(r) = fonts.raster(*font, gid, *size) else {
                            continue;
                        };
                        // empty raster (space, .notdef) — nothing to draw
                        if r.alpha.is_empty() || r.width == 0 || r.height == 0 {
                            continue;
                        }
                        let key = (*font, gid, pxq);
                        let Some((au, av)) = (match self.atlas_map.get(&key) {
                            Some(&uv) => Some(uv),
                            None => {
                                let slot =
                                    self.atlas_slot(key, r.width.max(1), r.height.max(1));
                                if let Some((au, av)) = slot {
                                    self.queue.write_texture(
                                        wgpu::TexelCopyTextureInfo {
                                            texture: &self.atlas,
                                            mip_level: 0,
                                            origin: wgpu::Origin3d { x: au, y: av, z: 0 },
                                            aspect: wgpu::TextureAspect::All,
                                        },
                                        &r.alpha,
                                        wgpu::TexelCopyBufferLayout {
                                            offset: 0,
                                            bytes_per_row: Some(r.width.max(1) * 1),
                                            rows_per_image: Some(r.height.max(1)),
                                        },
                                        wgpu::Extent3d {
                                            width: r.width.max(1),
                                            height: r.height.max(1),
                                            depth_or_array_layers: 1,
                                        },
                                    );
                                }
                                slot
                            }
                        }) else {
                            continue;
                        };
                        maini.push(UI {
                            geom: [
                                (gp + r.bearing_x as f32).round(),
                                (*baseline - r.bearing_y as f32).round(),
                                r.width as f32,
                                r.height as f32,
                            ],
                            aux: [au as f32, av as f32, r.width as f32, r.height as f32],
                            aux2: [0.0; 4],
                            color: srgb(*color),
                            clip: clipf(cur_clip),
                            misc: [1.0, 0.0, 0.0, 0.0],
                        });
                    }
                    // text-decoration lines → rect instances
                    let end = pen;
                    let dcol = deco.color.unwrap_or(*color);
                    let thick = deco.thick.unwrap_or((size / 14.0).max(1.0));
                    let ys = [
                        deco.underline()
                            .then_some(*baseline + 0.1 * size + deco.offset.unwrap_or(0.0)),
                        (deco.lines & 2 != 0).then_some(*baseline - size * 0.85),
                        deco.strike().then_some(*baseline - 0.28 * size),
                    ];
                    for ly in ys.into_iter().flatten() {
                        // paint_deco_line integer math: x0/y/t all truncate
                        match deco.style {
                            rowser_layout::DecoStyle::Solid | rowser_layout::DecoStyle::Double => {
                                let x0 = snap(*x);
                                let w = snatu((end - *x).max(0.0));
                                let t = snatu(thick.max(1.0));
                                let yi = snap(ly);
                                // CPU: yi - (t as i32)/2 — integer division
                                let th = (t as i32 / 2) as f32;
                                push_rect!([x0, yi - th, w, t], [0.0; 4], [0.0; 4], dcol);
                                if matches!(deco.style, rowser_layout::DecoStyle::Double) {
                                    let g = (t as i32).max(1) as f32;
                                    push_rect!(
                                        [x0, yi - th - g, w, t],
                                        [0.0; 4],
                                        [0.0; 4],
                                        dcol
                                    );
                                    push_rect!(
                                        [x0, yi + th + g, w, t],
                                        [0.0; 4],
                                        [0.0; 4],
                                        dcol
                                    );
                                }
                            }
                            _ => {
                                cpu_tile!(&items[i..i + 1], &Mat::IDENTITY);
                                break;
                            }
                        }
                    }
                }
                DisplayItem::Image { x, y, w: iw, h: ih, src, near } => {
                    if let Some(img) = images.get(src) {
                        let key = img.pixels.as_ptr() as usize;
                        let (iid, bgt) = self.img_bg(key, img, *near);
                        push_img!(
                            iid,
                            bgt,
                            vec![ImgI {
                                dst: [*x, *y, *iw, *ih],
                                uv: [0.0, 0.0, 1.0, 1.0],
                                m0: [1.0, 0.0, 0.0, 0.0],
                                m1: [0.0, 1.0, 0.0, 0.0],
                                clip: clipf(cur_clip),
                            }]
                        );
                    } else {
                        push_rect!(
                            [*x, *y, iw.max(1.0), ih.max(1.0)],
                            [0.0; 4],
                            [0.0; 4],
                            0xffe8e8e8
                        );
                    }
                }
                DisplayItem::Raster { x, y, w: iw, h: ih, pixels } => {
                    let (rid, tex, bgt) =
                        self.tile_bg(*iw as u32, *ih as u32, pixels);
                    keep.push(tex);
                    push_img!(
                        rid,
                        bgt,
                        vec![ImgI {
                            dst: [*x, *y, *iw, *ih],
                            uv: [0.0, 0.0, 1.0, 1.0],
                            m0: [1.0, 0.0, 0.0, 0.0],
                            m1: [0.0, 1.0, 0.0, 0.0],
                            clip: clipf(cur_clip),
                        }]
                    );
                }
                DisplayItem::PushClip { x, y, w: iw, h: ih, radii } => {
                    if radii.iter().all(|r| r[0] <= 0.0 && r[1] <= 0.0) {
                        clip_stack.push([
                            *x as i32,
                            *y as i32,
                            (*x + *iw) as i32,
                            (*y + *ih) as i32,
                        ]);
                        fold_clip!();
                    } else {
                        // rounded clip: the whole group goes CPU
                        let mut depth = 1i32;
                        let mut j = i + 1;
                        while j < items.len() && depth > 0 {
                            match items[j] {
                                DisplayItem::PushClip { .. } => depth += 1,
                                DisplayItem::PopClip => depth -= 1,
                                _ => {}
                            }
                            j += 1;
                        }
                        let mut span: Vec<DisplayItem> = Vec::with_capacity(j - i);
                        span.push(item.clone());
                        span.extend_from_slice(&items[i + 1..j]);
                        cpu_tile!(&span, &Mat::IDENTITY);
                        i = j;
                        continue;
                    }
                }
                DisplayItem::PopClip => {
                    if !clip_stack.is_empty() {
                        clip_stack.pop();
                        fold_clip!();
                    }
                }
                DisplayItem::PushTransform { m } => {
                    let mut depth = 1i32;
                    let mut j = i + 1;
                    while j < items.len() && depth > 0 {
                        match items[j] {
                            DisplayItem::PushTransform { .. } => depth += 1,
                            DisplayItem::PopTransform => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    if m.inverse().is_some() {
                        cpu_tile!(&items[i + 1..j - 1], m);
                    }
                    i = j;
                    continue;
                }
                DisplayItem::PopTransform => {}
                DisplayItem::PushEffect(_) => {
                    // group through matching PopEffect — fx::process handles it
                    let mut depth = 1i32;
                    let mut j = i + 1;
                    while j < items.len() && depth > 0 {
                        match items[j] {
                            DisplayItem::PushEffect(_) => depth += 1,
                            DisplayItem::PopEffect => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    cpu_tile!(&items[i..j], &Mat::IDENTITY);
                    i = j;
                    continue;
                }
                DisplayItem::PopEffect => {}
                DisplayItem::Shadow { .. } | DisplayItem::BgImage { .. } => {
                    cpu_tile!(&items[i..i + 1], &Mat::IDENTITY);
                }
            }
            i += 1;
        }
        flush_main!();
        if std::env::var_os("ROWSR_GPU_PROF").is_some() {
            eprintln!(
                "[gpu] walk {:?} ({} cmds, {} main + {} img inst)",
                twalk.elapsed(),
                cmds.len(),
                maini.len(),
                imgi.len()
            );
        }

        // Upload both instance streams in one write — main first (96B
        // stride), images after (80B). The buffer is cached and only
        // regrows when a frame needs more room than ever before.
        let main_bytes = (maini.len() * std::mem::size_of::<UI>()) as u64;
        let need = main_bytes + (imgi.len() * std::mem::size_of::<ImgI>()) as u64;
        if self.vcap < need.max(1) {
            let cap = need.max(1 << 20).next_power_of_two();
            self.vbuf = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("inst"),
                size: cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.vcap = cap;
        }
        let vbuf = self.vbuf.as_ref().unwrap();
        if need > 0 {
            let mut arena = Vec::with_capacity(need as usize);
            arena.extend_from_slice(as_bytes(&maini));
            arena.extend_from_slice(as_bytes(&imgi));
            self.queue.write_buffer(vbuf, 0, &arena);
        }
        let _ = &keep;
        let b = srgb(bg);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("paint") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("paint"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &tview,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: (b[0] * b[3]) as f64,
                            g: (b[1] * b[3]) as f64,
                            b: (b[2] * b[3]) as f64,
                            a: b[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            for c in &cmds {
                match *c {
                    Cmd::Main { start, n } => {
                        pass.set_pipeline(&self.main_pipe);
                        pass.set_bind_group(0, &self.view_bg, &[]);
                        pass.set_bind_group(1, &self.atlas_bg, &[]);
                        let off = (start as u64) * 96;
                        pass.set_vertex_buffer(0, vbuf.slice(off..off + (n as u64) * 96));
                        pass.draw(0..4, 0..n);
                    }
                    Cmd::Img { id, start, n } => {
                        pass.set_pipeline(&self.img_pipe);
                        pass.set_bind_group(0, &self.view_bg, &[]);
                        pass.set_bind_group(1, &live[live_map[&id]], &[]);
                        let off = main_bytes + (start as u64) * 80;
                        pass.set_vertex_buffer(0, vbuf.slice(off..off + (n as u64) * 80));
                        pass.draw(0..4, 0..n);
                    }
                }
            }
        }
        // readback — buffer cached across calls, recreated on resize
        let stride = (w * 4 + 255) & !255;
        if !matches!(&self.rbuf, Some((rw, rh, _)) if *rw == w && *rh == h) {
            let rb = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: (stride * h) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.rbuf = Some((w, h, rb));
        }
        let rb = &self.rbuf.as_ref().unwrap().2;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target.as_ref().unwrap().2,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: rb,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let trb = std::time::Instant::now();
        let slice = rb.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        // fill the output in place — no zero-init-then-overwrite
        let n_out = (w * h * 4) as usize;
        let w4 = w as usize * 4;
        let mut out: Vec<u8> = Vec::with_capacity(n_out);
        if rx.recv().is_ok_and(|r| r.is_ok()) {
            if let Ok(view) = slice.get_mapped_range() {
                let srcp = view.as_ptr();
                let dstp = out.as_mut_ptr();
                for y in 0..h as usize {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            srcp.add(y * stride as usize),
                            dstp.add(y * w4),
                            w4,
                        );
                    }
                }
                unsafe { out.set_len(n_out) };
            }
        }
        rb.unmap();
        if out.len() != n_out {
            out.resize(n_out, 0);
        }
        if std::env::var_os("ROWSR_GPU_PROF").is_some() {
            eprintln!("[gpu] readback {:?}", trb.elapsed());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_layout::DisplayItem;

    /// GPU init smoke + a basic render — skipped silently when no adapter
    /// exists (CI/headless machines without Vulkan).
    #[test]
    fn gpu_paint_runs_when_adapter_exists() {
        let Ok(g) = gpu() else {
            eprintln!("no GPU adapter — skipping");
            return;
        };
        let _ = g;
    }

    #[test]
    fn gpu_paints_rect_and_text() {
        let mut fonts = FontCache::new();
        let font = match fonts.resolve("liberation sans", false, false)
            .or_else(|| fonts.resolve("sans", false, false)) {
            Some(f) => f,
            None => { eprintln!("no font — skipping"); return; }
        };
        let items = vec![
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 200.0,
                h: 120.0,
                color: 0xffff0000u32,
            },
            DisplayItem::RoundedRect {
                x: 10.0,
                y: 10.0,
                w: 60.0,
                h: 40.0,
                radius: 8.0,
                color: 0xffffffff,
            },
            DisplayItem::Text {
                x: 10.0,
                baseline: 90.0,
                text: "GPU".into(),
                font,
                size: 20.0,
                color: 0xff000000,
                deco: rowser_layout::TextDeco::NONE,
                ls: 0.0,
                sel: true,
                href: None,
            },
        ];
        let Some(px) = paint_gpu(&items, &mut fonts, 200, 120, 0xffffffff, &HashMap::new())
        else {
            eprintln!("no GPU adapter — skipping");
            return;
        };
        assert_eq!(px.len(), 200 * 120 * 4);
        // the red fill covers most of the canvas — count red pixels
        let red = px
            .chunks_exact(4)
            .filter(|p| p[2] > 128 && p[0] < 64 && p[1] < 64 && p[3] == 255)
            .count();
        let mut hist = std::collections::BTreeMap::<(u8,u8,u8,u8), usize>::new();
        for p in px.chunks_exact(4) { *hist.entry((p[0],p[1],p[2],p[3])).or_default() += 1; }
        assert!(red > 10000, "expected mostly red canvas, got {red} red px; top: {hist:?}");
    }

    /// Single-glyph fidelity: GPU vs CPU bounding box + coverage must
    /// agree — catches atlas slot/upload/UV bugs that smear glyphs.
    #[test]
    fn gpu_glyph_matches_cpu() {
        let mut fonts = FontCache::new();
        let font = match fonts.resolve("liberation sans", false, false)
            .or_else(|| fonts.resolve("sans", false, false)) {
            Some(f) => f,
            None => { eprintln!("no font — skipping"); return; }
        };
        let items = vec![DisplayItem::Text {
            x: 10.0,
            baseline: 40.0,
            text: "o".into(),
            font,
            size: 24.0,
            color: 0xff000000,
            deco: rowser_layout::TextDeco::NONE,
            ls: 0.0,
            sel: true,
            href: None,
        }];
        let Some(gpx) = paint_gpu(&items, &mut fonts, 100, 60, 0xffffffff, &HashMap::new())
        else {
            eprintln!("no GPU adapter — skipping");
            return;
        };
        let cpx = crate::paint(&items, &mut fonts, 100, 60, 0xffffffff, &HashMap::new());
        let ink = |px: &[u8]| {
            let mut n = 0usize;
            let mut min = (100usize, 60usize);
            let mut max = (0usize, 0usize);
            for (i, p) in px.chunks_exact(4).enumerate() {
                if p[0] < 128 {
                    n += 1;
                    let (x, y) = (i % 100, i / 100);
                    min = (min.0.min(x), min.1.min(y));
                    max = (max.0.max(x), max.1.max(y));
                }
            }
            (n, min, max)
        };
        let (gn, gmin, gmax) = ink(&gpx);
        let (cn, cmin, cmax) = ink(&cpx);
        eprintln!("gpu ink={gn} bbox={gmin:?}..{gmax:?}  cpu ink={cn} bbox={cmin:?}..{cmax:?}");
        // bbox should be nearly identical (±1px)
        assert!((gmin.0 as i32 - cmin.0 as i32).abs() <= 1
            && (gmin.1 as i32 - cmin.1 as i32).abs() <= 1
            && (gmax.0 as i32 - cmax.0 as i32).abs() <= 1
            && (gmax.1 as i32 - cmax.1 as i32).abs() <= 1,
            "glyph bbox diverges");
        // ink pixel counts within 20%
        assert!(gn * 4 >= cn * 3 && cn * 4 >= gn * 3,
            "ink coverage diverges: gpu={gn} cpu={cn}");
    }
}
