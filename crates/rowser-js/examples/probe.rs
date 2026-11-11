fn main() {
    let src = r#"
var B=Symbol.for('react.activity'),j=Symbol.for('react.fragment'),M=Symbol.for('react.strict_mode'),
N=Symbol.for('react.profiler'),P=Symbol.for('react.consumer'),F=Symbol.for('react.context'),
I=Symbol.for('react.forward_ref'),L=Symbol.for('react.suspense'),R=Symbol.for('react.suspense_list'),
ee=Symbol.for('react.memo'),z=Symbol.for('react.lazy'),te=Symbol.for('react.legacy_hidden'),re=Symbol.for('react.memo_cache_sentinel');
function ki(){return false}function qm(){return false}var _e={current:null};
function Oi(s){return{s:s}}function Ni(){return{}}function i(n){return'err'+n}
function Mi(e,t,n,r,a,o){var s=0;if(r=e,typeof r==`function`)ki(r)&&(s=1);else if(typeof r==`string`)s=qm(e,n,_e.current)?26:e===`html`||e===`head`||e===`body`?27:5;else a:switch(r){case B:return e=Oi(31,n,t,a),e.elementType=B,e.lanes=o,e;case j:return Ni(n.children,a,o,t);case M:s=8,a|=24;break;case N:return e=Oi(12,n,t,a|2),e.elementType=N,e.lanes=o,e;case L:return e=Oi(13,n,t,a),e.elementType=L,e.lanes=o,e;case R:return e=Oi(19,n,t,a),e.elementType=R,e.lanes=o,e;case te:case re:return e=a|32,e=Oi(30,n,t,e),e.elementType=re,e.lanes=o,e.stateNode={autoName:null,paired:null,clones:null,ref:null},e;default:if(typeof r==`object`&&r)switch(r.$$typeof){case F:s=10;break a;case P:s=9;break a;case I:s=11;break a;case ee:s=14;break a;case z:s=16,r=null;break a}s=29,n=Error(i(130,e===null?`null`:typeof e,``)),r=null}return t=Oi(s,n,t,a),t.elementType=e,t.type=r,t.lanes=o,t}
var ctx={$$typeof:F};
var out=Mi(ctx,0,null,0,0,0);
out.s
"#;
    let mut c = rowser_js::Context::new();
    match c.eval(src) {
        Ok(v) => println!("=> {}", c.interp.to_str(&v).unwrap_or_default()),
        Err(e) => println!("=> ERR {e}"),
    }
}
