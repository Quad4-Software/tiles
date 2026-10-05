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

/// A container source opened through versatiles, or an upstream tile proxy.
pub enum Backend {
    Container(SharedTileSource),
    Proxy(ProxySpec),
}

#[derive(Debug)]
pub struct ProxySpec {
    pub template: String,
    pub extensions: Vec<String>,
    pub raster: bool,
    /// Per-upstream client (carries auth headers / custom UA).
    pub client: reqwest::Client,
}

/// One opened tile source, ready to serve.
pub struct ServerSource {
    pub name: String,
    pub backend: Backend,
    pub mime: String,
    pub extensions: Vec<String>,
    /// Optional custom MapLibre style JSON file served at /{name}/style.json.
    pub style_path: Option<std::path::PathBuf>,
}

impl ServerSource {
    fn container(&self) -> Result<&SharedTileSource, StatusCode> {
        match &self.backend {
            Backend::Container(s) => Ok(s),
            Backend::Proxy(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub sources: Arc<HashMap<String, ServerSource>>,
    pub cache_max_age: u32,
    pub public_url: Option<String>,
    pub runtime: TilesRuntime,
    /// Optional API key. When set, every route except /health requires
    /// ?key=<key>, an "Authorization: Bearer" header, or an "X-Api-Key" header.
    pub api_key: Option<String>,
    /// Disk cache for proxy sources; None disables caching.
    pub cache: Option<Arc<crate::cache::ProxyCache>>,
}

/// Open every configured source up front, so bad specs fail fast at startup.
pub async fn build_state(
    defs: &[NamedSource],
    runtime: &TilesRuntime,
    cache_max_age: u32,
    public_url: Option<String>,
    api_key: Option<String>,
    cache_dir: Option<std::path::PathBuf>,
    cache_ttl_secs: u64,
) -> Result<AppState> {
    let mut sources = HashMap::new();
    for def in defs {
        if sources.contains_key(&def.name) {
            bail!("duplicate source name '{}'", def.name);
        }
        let (backend, mime, extensions) = if source::is_proxy_template(&def.spec) {
            if !def.spec.starts_with("http") {
                bail!("proxy source '{}' must be an http(s) url", def.spec);
            }
            let (mime, raster) = proxy_mime(&def.spec);
            let ext = proxy_extension(&def.spec);
            let mut opts = crate::http::HttpOpts {
                headers: def.headers.clone(),
                ..Default::default()
            };
            // an explicit User-Agent header becomes the client UA
            for h in &def.headers {
                if let Ok((k, v)) = crate::http::HttpOpts::parse_header(h)
                    && k.eq_ignore_ascii_case("user-agent")
                {
                    opts.user_agent = Some(v);
                    opts.headers.retain(|x| !x.eq_ignore_ascii_case(h));
                    break;
                }
            }
            info!("source '{}' -> {} (upstream proxy)", def.name, def.spec);
            (
                Backend::Proxy(ProxySpec {
                    template: def.spec.clone(),
                    extensions: vec![ext.clone()],
                    raster,
                    client: opts.client(Some(std::time::Duration::from_secs(30)))?,
                }),
                mime.to_string(),
                vec![ext],
            )
        } else {
            let src = if (def.spec.starts_with("http://") || def.spec.starts_with("https://"))
                && !def.headers.is_empty()
            {
                let ext = def
                    .spec
                    .split(['?', '#'])
                    .next()
                    .unwrap_or(&def.spec)
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let r = crate::http::AuthedHttpReader::new(
                    def.spec.clone(),
                    &crate::http::HttpOpts {
                        headers: def.headers.clone(),
                        ..Default::default()
                    },
                )?;
                source::open_reader(Box::new(r), &ext, &def.spec, runtime).await?
            } else {
                source::open(&def.spec, runtime).await?
            };
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
            (Backend::Container(src), mime, extensions)
        };
        sources.insert(
            def.name.clone(),
            ServerSource {
                name: def.name.clone(),
                backend,
                mime,
                extensions,
                style_path: def.style.clone(),
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
        api_key,
        cache: cache_dir.map(|d| {
            Arc::new(crate::cache::ProxyCache::new(
                d,
                std::time::Duration::from_secs(cache_ttl_secs),
            ))
        }),
    })
}

/// Auth middleware: when an api key is configured every route except
/// /health requires it via ?key=, "Authorization: Bearer", or "X-Api-Key".
async fn auth(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = &state.api_key else {
        return Ok(next.run(req).await);
    };
    // /health for liveness probes; OPTIONS for CORS preflights, which
    // never carry credentials by design.
    if req.uri().path() == "/health" || req.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(req).await);
    }
    let ok = req
        .uri()
        .query()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "key")
                .map(|(_, v)| v.to_string())
        })
        .is_some_and(|k| k == *expected)
        || req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|v| v == expected)
        || req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == expected);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Infer the upstream content type and whether tiles are raster from the
/// template's file extension.
fn proxy_mime(spec: &str) -> (&'static str, bool) {
    let ext = proxy_extension(spec);
    match ext.as_str() {
        "png" => ("image/png", true),
        "jpg" | "jpeg" => ("image/jpeg", true),
        "webp" => ("image/webp", true),
        "avif" => ("image/avif", true),
        "pbf" | "mvt" => ("application/vnd.mapbox-vector-tile", false),
        _ => ("application/octet-stream", false),
    }
}

fn proxy_extension(spec: &str) -> String {
    let path = spec.split(['?', '#']).next().unwrap_or(spec);
    path.rsplit(['/', '.'])
        .next()
        .filter(|e| !e.contains('{') && !e.is_empty())
        .unwrap_or("bin")
        .to_string()
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
        .with_state(state.clone());
    app = app.layer(CorsLayer::permissive());
    app = app.layer(axum::middleware::from_fn_with_state(state.clone(), auth));
    app
}

/// Extract the request's ?key= for injecting into generated URLs.
fn key_qs(uri: &axum::http::Uri, state: &AppState) -> String {
    if state.api_key.is_none() {
        return String::new();
    }
    uri.query()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "key")
                .map(|(_, v)| v.to_string())
        })
        .map(|k| format!("?key={k}"))
        .unwrap_or_default()
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

fn tilejson_for(entry: &ServerSource, base: &str, ext: &str, qs: &str) -> serde_json::Value {
    let tiles = format!("{base}/{}/{{z}}/{{x}}/{{y}}.{ext}{qs}", entry.name);
    match &entry.backend {
        Backend::Container(src) => {
            let mut tj: serde_json::Value =
                serde_json::from_str(&src.tilejson().stringify()).unwrap_or_default();
            tj["tilejson"] = serde_json::json!("3.0.0");
            tj["scheme"] = serde_json::json!("xyz");
            tj["tiles"] = serde_json::json!([tiles]);
            tj
        }
        Backend::Proxy(_) => serde_json::json!({
            "tilejson": "3.0.0",
            "scheme": "xyz",
            "tiles": [tiles],
            "minzoom": 0,
            "maxzoom": 22,
            "format": ext,
            "name": entry.name,
        }),
    }
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Html<String> {
    let base = base_url(&headers, &state.public_url);
    Html(viewer::index_html(
        &state.sources,
        &base,
        &key_qs(&uri, &state),
    ))
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
    uri: axum::http::Uri,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let entry = get_source(&state, &name)?;
    let base = base_url(&headers, &state.public_url);
    Ok(Json(tilejson_for(
        entry,
        &base,
        &entry.extensions[0],
        &key_qs(&uri, &state),
    )))
}

async fn style(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Result<Response, StatusCode> {
    let entry = get_source(&state, &name)?;
    let base = base_url(&headers, &state.public_url);
    let qs = key_qs(&uri, &state);
    if let Some(path) = &entry.style_path {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let mut style: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
        let tj_url = format!("{base}/{}/tilejson.json{qs}", entry.name);
        if let Some(sources) = style["sources"].as_object_mut() {
            for (_k, v) in sources.iter_mut() {
                let needs_url = v["url"]
                    .as_str()
                    .is_none_or(|u| u == "auto" || u.is_empty())
                    && v["tiles"].is_null();
                if needs_url {
                    v["url"] = serde_json::json!(tj_url);
                }
            }
        }
        return Ok(Json(style).into_response());
    }
    let tj = tilejson_for(entry, &base, &entry.extensions[0], &qs);
    let mut style = viewer::style_for(entry, &base, &tj);
    if !qs.is_empty()
        && let Some(u) = style["sources"]["tiles"]["url"].as_str()
    {
        let url = u.to_string();
        style["sources"]["tiles"]["url"] = serde_json::json!(format!("{url}{qs}"));
    }
    Ok(Json(style).into_response())
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
    if let Backend::Proxy(p) = &entry.backend {
        return proxy_tile(&state, p, entry, &coord, &headers).await;
    }
    let tile = entry
        .container()?
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

/// Forward a tile request to an upstream provider's tile endpoint.
/// With a cache configured: serve fresh disk hits, revalidate expired
/// entries conditionally, serve stale on upstream errors, and collapse
/// concurrent misses on the same tile into one upstream fetch.
async fn proxy_tile(
    state: &AppState,
    proxy: &ProxySpec,
    entry: &ServerSource,
    coord: &TileCoord,
    _headers: &HeaderMap,
) -> Result<Response, StatusCode> {
    let url = proxy
        .template
        .replace("{z}", &coord.level.to_string())
        .replace("{x}", &coord.x.to_string())
        .replace("{y}", &coord.y.to_string());
    let ext = proxy.extensions[0].as_str();
    let key = format!(
        "{}/{}/{}/{}.{}",
        entry.name, coord.level, coord.x, coord.y, ext
    );

    let Some(cache) = &state.cache else {
        return proxy_fetch(state, proxy, entry, &url, None).await;
    };

    // Fast path: fresh on disk.
    if let Some(hit) = cache.fresh(&key) {
        return proxy_response(&hit.body, &hit.meta, "hit", state);
    }

    let m = cache.lock_arc(&key);
    let _guard = m.lock().await;
    // Recheck: another request may have filled the cache while we waited.
    if let Some(hit) = cache.fresh(&key) {
        drop(_guard);
        cache.unlock_cleanup(&key, &m);
        return proxy_response(&hit.body, &hit.meta, "hit", state);
    }
    let stale = cache.stale(&key);
    let out = proxy_fetch_cached(state, proxy, entry, &url, &key, stale).await;
    drop(_guard);
    cache.unlock_cleanup(&key, &m);
    out
}

fn proxy_response(
    body: &[u8],
    meta: &crate::cache::TileMeta,
    hit: &str,
    _state: &AppState,
) -> Result<Response, StatusCode> {
    let remaining = meta.expires_at.saturating_sub(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, meta.content_type.clone())
        .header(header::CONTENT_LENGTH, body.len())
        .header(
            header::CACHE_CONTROL,
            format!("public, max-age={remaining}"),
        )
        .header("x-tiles-cache", hit)
        .body(axum::body::Body::from(body.to_vec()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Cache-aware upstream fetch: conditional request when we have a stale
/// entry, 304 refreshes TTL, upstream failure falls back to stale.
async fn proxy_fetch_cached(
    state: &AppState,
    proxy: &ProxySpec,
    entry: &ServerSource,
    url: &str,
    key: &str,
    stale: Option<crate::cache::CachedTile>,
) -> Result<Response, StatusCode> {
    let cache = state.cache.as_ref().unwrap();
    let mut req = proxy.client.get(url);
    if let Some(s) = &stale {
        if let Some(e) = &s.meta.etag {
            req = req.header(header::IF_NONE_MATCH, e);
        }
        if let Some(lm) = &s.meta.last_modified {
            req = req.header(header::IF_MODIFIED_SINCE, lm);
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(_) => {
            return match stale {
                Some(s) => proxy_response(&s.body, &s.meta, "stale", state),
                None => Err(StatusCode::BAD_GATEWAY),
            };
        }
    };
    if resp.status() == StatusCode::NOT_MODIFIED
        && let Some(s) = stale
    {
        let ttl = crate::cache::ttl_from(resp.headers(), cache.default_ttl());
        let _ = cache.touch(key, ttl);
        return proxy_response(&s.body, &s.meta, "revalidated", state);
    }
    if !resp.status().is_success() {
        return match &stale {
            Some(s) => proxy_response(&s.body, &s.meta, "stale", state),
            None => {
                Err(StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
            }
        };
    }
    let ttl = crate::cache::ttl_from(resp.headers(), cache.default_ttl());
    let meta = crate::cache::TileMeta::from_response(&resp, &entry.mime, ttl);
    let body = resp
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .to_vec();
    let _ = cache.store(key, &body, &meta);
    proxy_response(&body, &meta, "miss", state)
}

/// Direct passthrough when no cache is configured.
async fn proxy_fetch(
    state: &AppState,
    proxy: &ProxySpec,
    entry: &ServerSource,
    url: &str,
    _unused: Option<()>,
) -> Result<Response, StatusCode> {
    let resp = proxy
        .client
        .get(url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = resp.status();
    let mut builder =
        Response::builder().status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK));
    for h in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(v) = resp.headers().get(&h) {
            builder = builder.header(h, v.clone());
        }
    }
    builder = builder.header(
        header::CACHE_CONTROL,
        format!("public, max-age={}", state.cache_max_age),
    );
    if resp.headers().get(header::CONTENT_TYPE).is_none() {
        builder = builder.header(header::CONTENT_TYPE, entry.mime.clone());
    }
    let stream = resp.bytes_stream();
    builder
        .body(axum::body::Body::from_stream(stream))
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
