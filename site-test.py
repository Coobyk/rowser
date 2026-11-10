#!/usr/bin/env python3
"""Site-test harness for rowser-renderer (drives stdin/stdout IPC).

Run from the repo root:
    python3 site-test.py                 # all sites
    python3 site-test.py github crates   # subset by short name

Outputs:
    /tmp/site-tests/<name>.png         last painted frame (magick-converted)
    /tmp/site-tests/<name>.ppm         raw P6 PPM fallback
    /tmp/site-tests/<name>.stderr      renderer stderr (panics, [prof] lines)
    /tmp/site-tests/summary.txt        table of results

Renderer IPC (rowser-ipc): [u32 len][payload]; ToRenderer tags
0=Ping,1=Navigate,2=ResizeViewport,3=Scroll,4=Shutdown; ToBrowser tags
1=LoadStarted,2=Title,3=Frame(u32 w,h,u32 len,BGRA),4=LoadDone,5=Log.
"""

import os
import shutil
import struct
import subprocess
import sys
import time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "target", "debug", "rowser-renderer")
OUTDIR = "/tmp/site-tests"
W, H = 1280, 800
TIMEOUT = 240

SITES = {
    "github":    "https://github.com",
    "crates":    "https://crates.io",
    "rustlang":  "https://www.rust-lang.org",
    "mdn":       "https://developer.mozilla.org/en-US/docs/Web/HTML",
    "rustbook":  "https://doc.rust-lang.org/book/",
}


def frame(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def navigate_msgs(url: str) -> bytes:
    u = url.encode()
    return (
        frame(bytes([2]) + struct.pack("<II", W, H))          # ResizeViewport
        + frame(bytes([1]) + struct.pack("<I", len(u)) + u)   # Navigate
        + frame(bytes([4]))                                   # Shutdown
    )


def parse_stdout(out: bytes):
    """Return (title, frames[(w,h,bgra)], logs, n_started, n_done, trailing)."""
    pos, title, frames, logs = 0, None, [], []
    n_started = n_done = 0
    while pos + 4 <= len(out):
        (ln,) = struct.unpack_from("<I", out, pos)
        pos += 4
        p = out[pos:pos + ln]
        pos += ln
        if not p:
            continue
        tag = p[0]
        if tag == 1:
            n_started += 1
        elif tag == 2:
            (sl,) = struct.unpack_from("<I", p, 1)
            title = p[5:5 + sl].decode(errors="replace")
        elif tag == 3:
            w, h = struct.unpack_from("<II", p, 1)
            (bl,) = struct.unpack_from("<I", p, 9)
            frames.append((w, h, p[13:13 + bl]))
        elif tag == 4:
            n_done += 1
        elif tag == 5:
            (sl,) = struct.unpack_from("<I", p, 1)
            logs.append(p[5:5 + sl].decode(errors="replace"))
        elif tag == 6:
            (sl,) = struct.unpack_from("<I", p, 1)
            logs.append("NAVREQ:" + p[5:5 + sl].decode(errors="replace"))
    return title, frames, logs, n_started, n_done, len(out) - pos


def bgra_to_ppm(w: int, h: int, bgra: bytes, path: str):
    rgb = bytearray(w * h * 3)
    rgb[0::3] = bgra[2::4]
    rgb[1::3] = bgra[1::4]
    rgb[2::3] = bgra[0::4]
    with open(path, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (w, h))
        f.write(rgb)


def nonwhite_frac(bgra: bytes) -> float:
    """Fraction of pixels that aren't (near-)white — blank-page detector."""
    if not bgra:
        return 0.0
    n = len(bgra) // 4
    if n == 0:
        return 0.0
    nw = 0
    step = max(1, n // 20000) * 4  # sample ~20k px
    for i in range(0, len(bgra) - 3, step):
        b, g, r = bgra[i], bgra[i + 1], bgra[i + 2]
        if b < 240 or g < 240 or r < 240:
            nw += 1
    total = len(range(0, len(bgra) - 3, step))
    return nw / max(1, total)


def run_site(name: str, url: str) -> dict:
    res = {"name": name, "url": url, "time": None, "rc": None,
           "title": None, "verdict": "", "png": "", "notes": []}
    env = dict(os.environ, ROWSER_PROFILE="1")
    t0 = time.monotonic()
    try:
        p = subprocess.run([BIN], input=navigate_msgs(url),
                           capture_output=True, timeout=TIMEOUT, env=env)
        res["time"] = time.monotonic() - t0
        res["rc"] = p.returncode
        out, err = p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        res["time"] = TIMEOUT
        res["rc"] = "TIMEOUT"
        out, err = e.stdout or b"", e.stderr or b""
        res["notes"].append(f"killed after {TIMEOUT}s (hang)")

    err_s = err.decode(errors="replace")
    stderr_path = os.path.join(OUTDIR, f"{name}.stderr")
    with open(stderr_path, "w") as f:
        f.write(err_s)
    if "panic" in err_s or "RUST_BACKTRACE" in err_s:
        res["notes"].append("PANIC on stderr")
    if err_s.strip() and "[prof]" not in err_s.splitlines()[0][:200]:
        res["notes"].append("stderr: " + err_s.strip().splitlines()[-1][:200])

    title, frames, logs, n_started, n_done, trailing = parse_stdout(out)
    res["title"] = title
    for m in logs:
        res["notes"].append(f"log: {m[:200]}")
    if trailing:
        res["notes"].append(f"{trailing} trailing undecoded bytes")
    if n_done == 0:
        res["notes"].append("no LoadDone — load never completed")
    if title == url:
        res["notes"].append(
            "title==url → no <title> found (error page, JS shell, or "
            "undecoded body — suspect content-encoding br/zstd)")

    if frames:
        # First frame after resize (pre-doc) is a checkerboard; the LAST
        # frame is the loaded page.
        w, h, px = frames[-1]
        if len(px) != w * h * 4:
            res["notes"].append(f"bad frame size {len(px)} != {w*h*4}")
        else:
            ppm = os.path.join(OUTDIR, f"{name}.ppm")
            bgra_to_ppm(w, h, px, ppm)
            frac = nonwhite_frac(px)
            res["frac"] = frac
            if shutil.which("magick"):
                png = os.path.join(OUTDIR, f"{name}.png")
                subprocess.run(["magick", ppm, png], check=False)
                res["png"] = png
            else:
                res["png"] = ppm
            if frac < 0.02:
                res["notes"].append(f"frame ~blank ({frac:.1%} non-white)")
    else:
        res["notes"].append("NO FRAMES received")

    if res["rc"] == "TIMEOUT":
        res["verdict"] = "HANG"
    elif res["rc"] not in (0, None):
        res["verdict"] = "CRASH"
    elif not frames:
        res["verdict"] = "NO FRAME"
    elif res.get("frac", 1) < 0.02:
        res["verdict"] = "BLANK"
    else:
        res["verdict"] = "RENDERED"
    return res


def main():
    os.makedirs(OUTDIR, exist_ok=True)
    names = sys.argv[1:] or list(SITES)
    rows = []
    for name in names:
        url = SITES[name]
        print(f"[{name}] {url} ...", flush=True)
        r = run_site(name, url)
        t = f"{r['time']:.1f}s" if isinstance(r["time"], float) else str(r["time"])
        print(f"  rc={r['rc']} time={t} verdict={r['verdict']} "
              f"title={r['title']!r}")
        for n in r["notes"]:
            print(f"    - {n}")
        rows.append(r)

    lines = ["site | url | time | rc | verdict | title"]
    for r in rows:
        t = f"{r['time']:.1f}s" if isinstance(r["time"], float) else str(r["time"])
        lines.append(f"{r['name']} | {r['url']} | {t} | {r['rc']} | "
                     f"{r['verdict']} | {r['title']!r} | png={r['png']}")
    summary = "\n".join(lines)
    print("\n" + summary)
    with open(os.path.join(OUTDIR, "summary.txt"), "w") as f:
        f.write(summary + "\n")


if __name__ == "__main__":
    main()
