//! Built-in viewer: vendored MapLibre assets plus a generated style, so the
//! demo page works without touching any CDN or third-party service.

use std::collections::HashMap;

use crate::server::{Backend, ServerSource};

pub const MAPLIBRE_JS: &[u8] = include_bytes!("../assets/maplibre-gl.js");
pub const MAPLIBRE_CSS: &[u8] = include_bytes!("../assets/maplibre-gl.css");

const PAGE_CSS: &str = "html,body,#map{height:100%;margin:0;padding:0}";

pub fn index_html(sources: &HashMap<String, ServerSource>, base: &str, qs: &str) -> String {
    let mut rows: Vec<_> = sources.values().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let items: String = rows
        .iter()
        .map(|s| {
            let kind = match &s.backend {
                Backend::Proxy(_) => "proxy",
                _ if s.mime.starts_with("image/") => "raster",
                _ => "vector",
            };
            format!(
                "<div class=card><div class=head><span class=name>{n}</span>				 <span class=badge>{kind}</span><span class=badge>{e}</span></div>				 <div class=links><a href=\"/{n}/view{qs}\">view</a>				 <a href=\"/{n}/tilejson.json{qs}\">tilejson</a>				 <a href=\"/{n}/style.json{qs}\">style</a></div>				 <code class=url>{base}/{n}/{{z}}/{{x}}/{{y}}.{e}</code></div>",
                n = s.name,
                e = s.extensions[0],
            )
        })
        .collect();
    format!(
        r#"<!doctype html><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>tiles</title><style>
:root{{--bg:#f6f7f8;--fg:#1c1f22;--mut:#6b7280;--line:#e2e5e8;--card:#fff;--acc:#2f6feb}}
@media(prefers-color-scheme:dark){{:root{{--bg:#0f1214;--fg:#e4e7ea;--mut:#8b949c;--line:#262b30;--card:#171b1f;--acc:#6ea8fe}}}}
*{{box-sizing:border-box}}
body{{font:15px/1.5 system-ui,-apple-system,sans-serif;background:var(--bg);color:var(--fg);max-width:56rem;margin:0 auto;padding:3rem 1.25rem}}
h1{{font-size:1.5rem;font-weight:600;letter-spacing:-.01em;margin:0 0 .4rem}}
.sub{{color:var(--mut);margin:0 0 2rem}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:1rem 1.25rem;margin-bottom:.75rem}}
.head{{display:flex;align-items:baseline;gap:.5rem;margin-bottom:.4rem}}
.name{{font-weight:600}}
.badge{{font:11px/1 ui-monospace,monospace;color:var(--mut);border:1px solid var(--line);border-radius:99px;padding:.2rem .55rem}}
.links{{display:flex;gap:1rem;margin:.3rem 0 .5rem}}
a{{color:var(--acc);text-decoration:none}}
a:hover{{text-decoration:underline}}
.url{{display:block;font:12px/1.6 ui-monospace,monospace;color:var(--mut);background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:.35rem .6rem;overflow-x:auto;white-space:nowrap}}
</style>
<h1>tiles</h1><p class=sub>{count} source{sfx} serving on this instance.</p>
{items}"#,
        count = rows.len(),
        sfx = if rows.len() == 1 { "" } else { "s" },
    )
}

pub fn view_html(name: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>{name} | tiles</title>\
		 <link rel=stylesheet href=/assets/maplibre-gl.css><style>{PAGE_CSS}\
		 #err{{position:fixed;top:0;left:0;right:0;background:#c33;color:#fff;\
		 font:14px/1.4 monospace;padding:.5em 1em;z-index:9;white-space:pre-wrap}}\
		 #stat{{position:fixed;bottom:0;right:0;background:#000a;color:#0f0;\
		 font:12px/1.5 monospace;padding:.3em .8em;z-index:9;text-align:right}}\
		 </style>\
		 <div id=map></div><script src=/assets/maplibre-gl.js></script><script>\
		 function err(m){{var e=document.createElement('div');e.id='err';\
		 e.textContent='tiles viewer: '+m;document.body.appendChild(e)}}\
		 window.onerror=function(m,s,l,c){{err(m+' @'+s+':'+l+':'+c)}};\
		 function stat(){{\
		 Promise.all([\
		 fetch('/{name}/tilejson.json'+location.search).then(r=>r.status),\
		 fetch('/{name}/style.json'+location.search).then(r=>r.status),\
		 fetch('/{name}/0/0/0.pbf'+location.search).then(r=>r.status+' '+r.headers.get('content-length')+'B')\
		 ]).then(function(x){{var d=document.createElement('div');d.id='stat';\
		 d.textContent='tilejson '+x[0]+' | style '+x[1]+' | tile '+x[2];\
		 document.body.appendChild(d)}})}}\
		 stat();\
		 if(typeof maplibregl==='undefined'){{err('maplibre-gl.js failed to load')}}\
		 else{{try{{var map=new maplibregl.Map({{container:'map',\
		 style:'/{name}/style.json'+location.search,hash:true,\
		 attributionControl:{{compact:true}}}});\
		 map.addControl(new maplibregl.NavigationControl());\
		 map.addControl(new maplibregl.ScaleControl());\
		 map.on('error',function(e){{err('map: '+(e.error&&e.error.message||e.type))}})\
		 }}catch(e){{err('init: '+((e.error&&e.error.message)||e.message||e))}}}}\
		 </script>"
    )
}

/// Deterministic, pleasant-enough color from a layer name.
fn layer_color(id: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in id.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    format!("hsl({}, 65%, 45%)", h % 360)
}

/// Build a minimal MapLibre style for a source: one fill, line, and circle
/// layer per declared vector layer (so every geometry type renders), or a
/// single raster layer. No glyphs are used, so no font server is needed.
pub fn style_for(
    entry: &ServerSource,
    base: &str,
    tilejson: &serde_json::Value,
) -> serde_json::Value {
    let is_raster = entry.mime.starts_with("image/");

    let mut layers = vec![serde_json::json!({
        "id": "background",
        "type": "background",
        "paint": { "background-color": "#dfe9ef" }
    })];

    if is_raster {
        layers.push(serde_json::json!({
            "id": "raster", "type": "raster", "source": "tiles"
        }));
    } else if let Some(vl) = tilejson.get("vector_layers").and_then(|v| v.as_array()) {
        for layer in vl {
            let Some(id) = layer.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let color = layer_color(id);
            layers.push(serde_json::json!({
                "id": format!("{id}-fill"), "type": "fill",
                "source": "tiles", "source-layer": id,
                "filter": ["==", ["geometry-type"], "Polygon"],
                "paint": { "fill-color": color, "fill-opacity": 0.4 }
            }));
            layers.push(serde_json::json!({
                "id": format!("{id}-line"), "type": "line",
                "source": "tiles", "source-layer": id,
                "paint": { "line-color": color, "line-width": 1 }
            }));
            layers.push(serde_json::json!({
                "id": format!("{id}-point"), "type": "circle",
                "source": "tiles", "source-layer": id,
                "filter": ["==", ["geometry-type"], "Point"],
                "paint": { "circle-radius": 3, "circle-color": color }
            }));
        }
    } else {
        // No vector_layers advertised: try a generic pass over common layers.
        for id in [
            "water",
            "landuse",
            "roads",
            "buildings",
            "boundaries",
            "places",
        ] {
            layers.push(serde_json::json!({
                "id": id, "type": "line",
                "source": "tiles", "source-layer": id,
                "paint": { "line-color": layer_color(id), "line-width": 1 }
            }));
        }
    }

    let mut style = serde_json::json!({
        "version": 8,
        "name": "tiles",
        "sources": {
            "tiles": {
                "type": if is_raster { "raster" } else { "vector" },
                "url": format!("{base}/{}/tilejson.json", entry.name)
            }
        },
        "layers": layers
    });
    if let Some(c) = tilejson.get("center").and_then(|v| v.as_array())
        && c.len() >= 2
    {
        style["center"] = serde_json::json!([c[0], c[1]]);
        if let Some(z) = c.get(2) {
            style["zoom"] = z.clone();
        }
    } else if let Some(b) = tilejson.get("bounds").and_then(|v| v.as_array())
        && b.len() == 4
    {
        let (x0, y0, x1, y1) = (
            b[0].as_f64().unwrap_or(0.0),
            b[1].as_f64().unwrap_or(0.0),
            b[2].as_f64().unwrap_or(0.0),
            b[3].as_f64().unwrap_or(0.0),
        );
        style["center"] = serde_json::json!([(x0 + x1) / 2.0, (y0 + y1) / 2.0]);
        // crude fit: zoom where ~360/2^z spans the larger extent
        let span = (x1 - x0).max((y1 - y0) * 1.6).max(0.0001);
        let zoom = (360.0 / span).log2().floor().clamp(1.0, 16.0);
        style["zoom"] = serde_json::json!(zoom - 1.0);
    }
    style
}
