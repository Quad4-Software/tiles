//! Built-in viewer: vendored OpenLayers assets plus a generated style, so the
//! demo page works without touching any CDN or third-party service.

use std::collections::HashMap;

use crate::server::{Backend, ServerSource};

pub const OL_JS: &[u8] = include_bytes!("../assets/ol-viewer.js");
pub const OL_CSS: &[u8] = include_bytes!("../assets/ol.css");

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
                "<div class=card><div class=head><a class=name href=\"/{n}/view{qs}\">{n}</a>\
				 <span class=badge>{kind}</span><span class=badge>{e}</span></div>\
				 <pre class=eps>view      {base}/{n}/view{qs}\ntilejson  {base}/{n}/tilejson.json{qs}\nstyle     {base}/{n}/style.json{qs}\ntiles     {base}/{n}/{{z}}/{{x}}/{{y}}.{e}{qs}</pre></div>",
                n = s.name,
                e = s.extensions[0],
            )
        })
        .collect();
    format!(
        r#"<!doctype html><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>tiles</title><style>
:root{{--bg:#fafafa;--fg:#18181b;--mut:#71717a;--line:#e9e9ec;--card:#fff;--well:#f4f4f5}}
@media(prefers-color-scheme:dark){{:root{{--bg:#0a0a0b;--fg:#d9d9de;--mut:#a1a1aa;--line:#1f1f24;--card:#16161a;--well:#101013}}}}
*{{box-sizing:border-box}}
body{{font:15px/1.5 system-ui,-apple-system,sans-serif;background:var(--bg);color:var(--fg);max-width:56rem;margin:0 auto;padding:3rem 1.25rem}}
h1{{font-size:1.25rem;font-weight:600;letter-spacing:-.01em;margin:0 0 .4rem}}
.sub{{color:var(--mut);margin:0 0 2rem}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:.9rem 1.1rem;margin-bottom:.6rem}}
.head{{display:flex;align-items:baseline;gap:.5rem}}
.name{{font-weight:600}}
a{{color:inherit}}
a.name{{text-decoration:none}}
a.name:hover{{text-decoration:underline}}
.badge{{font:11px/1 ui-monospace,monospace;color:var(--mut);border:1px solid var(--line);border-radius:99px;padding:.2rem .55rem}}
.head .badge:first-of-type{{margin-left:auto}}
.eps{{margin:.6rem 0 0;font:12px/1.7 ui-monospace,monospace;color:var(--mut);background:var(--well);border:1px solid var(--line);border-radius:6px;padding:.5rem .7rem;overflow-x:auto}}
</style>
<h1>tiles</h1><p class=sub>{count} source{sfx} serving on this instance.</p>
{items}"#,
        count = rows.len(),
        sfx = if rows.len() == 1 { "" } else { "s" },
    )
}

pub fn view_html(name: &str, tile_ext: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8>\
		 <meta name=viewport content=\"width=device-width,initial-scale=1\">\
		 <title>{name} | tiles</title>\
		 <link rel=stylesheet href=/assets/ol.css><style>{PAGE_CSS}\
		 #err{{position:fixed;top:0;left:0;right:0;background:#c33;color:#fff;\
		 font:14px/1.4 monospace;padding:.5em 1em;z-index:9;white-space:pre-wrap}}\
		 #stat{{position:fixed;bottom:0;right:0;background:#000a;color:#ddd;\
		 font:12px/1.5 monospace;padding:.3em .8em;z-index:9;text-align:right}}\
		 </style>\
		 <div id=map></div><script src=/assets/ol-viewer.js></script><script>\
		 function err(m){{var e=document.createElement('div');e.id='err';\
		 e.textContent='tiles viewer: '+m;document.body.appendChild(e)}}\
		 window.onerror=function(m,s,l,c){{err(m+' @'+s+':'+l+':'+c)}};\
		 function stat(){{\
		 Promise.all([\
		 fetch('/{name}/tilejson.json'+location.search).then(r=>r.status),\
		 fetch('/{name}/style.json'+location.search).then(r=>r.status),\
		 fetch('/{name}/0/0/0.{tile_ext}'+location.search).then(r=>r.arrayBuffer().then(b=>r.status+' '+b.byteLength+'B'))\
		 ]).then(function(x){{var d=document.createElement('div');d.id='stat';\
		 d.textContent='tilejson '+x[0]+' | style '+x[1]+' | tile '+x[2];\
		 document.body.appendChild(d)}})}}\
		 stat();\
		 if(typeof tilesViewer==='undefined'){{err('ol-viewer.js failed to load')}}\
		 else{{try{{tilesViewer.init('/{name}/style.json'+location.search)\
		 .catch(function(e){{err('style: '+(e&&e.message||e))}})\
		 }}catch(e){{err('init: '+(e&&e.message||e))}}}}\
		 </script>"
    )
}

/// Muted palette for common OpenMapTiles-ish layer names. Anything else gets
/// a deterministic, desaturated hash color.
fn layer_color(id: &str) -> String {
    const KNOWN: &[(&[&str], &str)] = &[
        (&["water", "ocean"], "#aac6d8"),
        (&["landuse", "natural", "landcover", "park"], "#dde4dc"),
        (&["buildings", "building"], "#cfcdc8"),
        (
            &["roads", "transportation", "transit", "aeroway", "streets"],
            "#a09a90",
        ),
        (&["boundaries", "boundary"], "#7d7d88"),
        (&["places", "pois", "poi"], "#52525f"),
    ];
    for (names, color) in KNOWN {
        if names.contains(&id) {
            return color.to_string();
        }
    }
    let mut h: u32 = 0x811c9dc5;
    for b in id.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    format!("hsl({}, 28%, 42%)", h % 360)
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
        "paint": { "background-color": "#f4f4f5" }
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
            // No geometry-type filters: renderers ignore mismatched geometry
            // (fills only apply to polygons, circles only to points), and
            // filtering on "Polygon" would wrongly skip MultiPolygons.
            layers.push(serde_json::json!({
                "id": format!("{id}-fill"), "type": "fill",
                "source": "tiles", "source-layer": id,
                "paint": { "fill-color": color, "fill-opacity": 0.55 }
            }));
            layers.push(serde_json::json!({
                "id": format!("{id}-line"), "type": "line",
                "source": "tiles", "source-layer": id,
                "paint": { "line-color": color, "line-width": 1 }
            }));
            layers.push(serde_json::json!({
                "id": format!("{id}-point"), "type": "circle",
                "source": "tiles", "source-layer": id,
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
