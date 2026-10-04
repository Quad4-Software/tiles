//! Axum HTTP server: tiles, TileJSON, generated style, and a self-hosted viewer.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;
use versatiles_container::{SharedTileSource, TilesRuntime};
use versatiles_core::{TileCompression, TileCoord};

use crate::source::{self, NamedSource};
use crate::viewer;

/// One opened tile source, ready to serve.
pub struct ServerSource {
    pub name: String,
    pub source: SharedTileSource,
    pub mime: String,
    pub extensions: Vec<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub sources: Arc<HashMap<String, ServerSource>>,
    pub cache_max_age: u32,
    pub public_url: Option<String>,
    pub runtime: TilesRuntime,
}

/// Open every configured source up front, so bad specs fail fast at startup.
pub async fn build_state(
    defs: &[NamedSource],
    runtime: &TilesRuntime,
    cache_max_age: u32,
    public_url: Option<String>,
) -> Result<AppState> {
    let mut sources = HashMap::new();
    for def in defs {
        if sources.contains_key(&def.name) {
            bail!("duplicate source name '{}'", def.name);
        }
        let src = source::open(&def.spec, runtime).await?;
        let format = *src.metadata().tile_format();
        let mime = format.as_mime_str().to_string();
        // as_extension() includes the leading dot; strip it for route matching
        let mut extensions = vec![format.as_extension().trim_start_matches('.').to_string()];
        if mime.contains("mapbox-vector-tile") {
            for alias in ["pbf", "mvt"] {
                if !extensions.iter().any(|e| e == alias) {
                    extensions.push(alias.into());
                }
            }
        }
        info!("source '{}' -> {} ({})", def.name, def.spec, mime);
        sources.insert(
            def.name.clone(),
            ServerSource {
                name: def.name.clone(),
                source: src,
                mime,
                extensions,
            },
        );
    }
    if sources.is_empty() {
        bail!("no tile sources configured");
    }
    Ok(AppState {
        sources: Arc::new(sources),
        cache_max_age,
        public_url,
        runtime: runtime.clone(),
    })
}

pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/assets/maplibre-gl.js", get(maplibre_js))
        .route("/assets/maplibre-gl.css", get(maplibre_css))
        .route("/{name}/tilejson.json", get(tilejson))
        .route("/{name}/style.json", get(style))
        .route("/{name}/view", get(view))
        .route("/{name}/{z}/{x}/{file}", get(tile))
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    app = app.layer(CorsLayer::permissive());
    app
}

fn get_source<'a>(state: &'a AppState, name: &str) -> Result<&'a ServerSource, StatusCode> {
    state.sources.get(name).ok_or(StatusCode::NOT_FOUND)
}

fn base_url(headers: &HeaderMap, public_url: &Option<String>) -> String {
    if let Some(u) = public_url {
        return u.trim_end_matches('/').to_string();
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

fn tilejson_for(entry: &ServerSource, base: &str, ext: &str) -> serde_json::Value {
    let mut tj: serde_json::Value =
        serde_json::from_str(&entry.source.tilejson().stringify()).unwrap_or_default();
    tj["tilejson"] = serde_json::json!("3.0.0");
    tj["scheme"] = serde_json::json!("xyz");
    tj["tiles"] = serde_json::json!([format!("{base}/{}/{{z}}/{{x}}/{{y}}.{ext}", entry.name)]);
    tj
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    let base = base_url(&headers, &state.public_url);
    Html(viewer::index_html(&state.sources, &base))
}

async fn health() -> &'static str {
    "ok"
}

async fn maplibre_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        viewer::MAPLIBRE_JS,
    )
        .into_response()
}

async fn maplibre_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        viewer::MAPLIBRE_CSS,
    )
        .into_response()
}

async fn tilejson(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let entry = get_source(&state, &name)?;
    let base = base_url(&headers, &state.public_url);
    Ok(Json(tilejson_for(entry, &base, &entry.extensions[0])))
}

async fn style(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let entry = get_source(&state, &name)?;
    let base = base_url(&headers, &state.public_url);
    let tj = tilejson_for(entry, &base, &entry.extensions[0]);
    Ok(Json(viewer::style_for(entry, &base, &tj)))
}

async fn view(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let entry = get_source(&state, &name)?;
    Ok(Html(viewer::view_html(&entry.name)))
}

/// HTTP `Content-Encoding` token for a stored tile compression.
fn encoding_token(c: TileCompression) -> Option<&'static str> {
    match c {
        TileCompression::Uncompressed => None,
        TileCompression::Gzip => Some("gzip"),
        TileCompression::Brotli => Some("br"),
        TileCompression::Zstd => Some("zstd"),
    }
}

/// Choose the compression to send: passthrough when the client accepts the
/// stored encoding, otherwise recompress to gzip (or identity as last resort).
fn negotiate(stored: TileCompression, accept: Option<&str>) -> TileCompression {
    let accepted = |tok: &str| {
        accept
            .map(|a| {
                a.split(',').any(|p| {
                    p.trim()
                        .split(';')
                        .next()
                        .is_some_and(|t| t.eq_ignore_ascii_case(tok) || t == "*")
                })
            })
            .unwrap_or(false)
    };
    if let Some(tok) = encoding_token(stored) {
        if accepted(tok) {
            return stored;
        }
    } else {
        return stored;
    }
    if accepted("gzip") {
        TileCompression::Gzip
    } else {
        TileCompression::Uncompressed
    }
}

async fn tile(
    State(state): State<AppState>,
    Path((name, z, x, file)): Path<(String, u8, u32, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let entry = get_source(&state, &name)?;
    let Some((y, ext)) = file.rsplit_once('.') else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !entry.extensions.iter().any(|e| e == ext) {
        return Err(StatusCode::NOT_FOUND);
    }
    let y: u32 = y.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let coord = TileCoord::new(z, x, y).map_err(|_| StatusCode::BAD_REQUEST)?;
    let tile = entry
        .source
        .tile(&coord)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(tile) = tile else {
        return Err(StatusCode::NOT_FOUND);
    };

    let accept = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let target = negotiate(tile.compression(), accept);
    let blob = tile
        .into_blob(&target)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // ETag covers the exact response bytes, so each negotiated encoding gets
    // its own validator; Vary: Accept-Encoding keeps caches honest.
    let etag = etag_for(&blob);
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == "*" || t.trim() == etag)
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(
                header::CACHE_CONTROL,
                format!("public, max-age={}", state.cache_max_age),
            )
            .header(header::VARY, "Accept-Encoding")
            .body(axum::body::Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &*entry.mime)
        .header(header::ETAG, etag)
        .header(
            header::CACHE_CONTROL,
            format!("public, max-age={}", state.cache_max_age),
        )
        .header(header::VARY, "Accept-Encoding");
    if let Some(tok) = encoding_token(target) {
        builder = builder.header(header::CONTENT_ENCODING, tok);
    }
    builder
        .body(axum::body::Body::from(blob.into_vec()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Content hash used as a strong ETag: SipHash over the payload plus its
/// length. Stable within a process; revalidation reverts to a full fetch
/// after a restart, which is acceptable for a tile server.
fn etag_for(blob: &versatiles_core::Blob) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    blob.as_slice().hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}
