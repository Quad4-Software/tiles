//! Shared test helpers: fixture containers and a mini HTTP range server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;
use versatiles_container::{MockReader, TileSourceMetadata, TilesRuntime, Traversal};
use versatiles_core::{TileCompression, TileFormat, TilePyramid};

/// Silence progress bars in tests.
pub fn test_runtime() -> TilesRuntime {
    TilesRuntime::builder().silent_progress(true).build()
}

/// Write a small container (`z0..=3`, gzipped mock MVT tiles) to `dir`.
pub async fn write_fixture(dir: &Path, ext: &str) -> PathBuf {
    let pyramid = TilePyramid::new_full_up_to(3);
    let metadata =
        TileSourceMetadata::new(TileFormat::MVT, TileCompression::Gzip, Traversal::ANY, None);
    let reader = MockReader::new_mock(pyramid, metadata).expect("mock reader");
    let path = dir.join(format!("fixture.{ext}"));
    test_runtime()
        .write_to_path(Arc::new(reader), &path)
        .await
        .expect("write fixture");
    path
}

/// Serve `bytes` at `GET <base>/fixture.<ext>` with RFC 7233 Range support,
/// on an ephemeral localhost port. Returns the base URL.
pub async fn mini_http(bytes: Vec<u8>, ext: &'static str) -> String {
    let bytes = Arc::new(bytes);
    let route = format!("/fixture.{ext}");
    let app = Router::new()
        .route(&route, get(range_handler))
        .with_state(bytes);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

async fn range_handler(State(bytes): State<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
    let total = bytes.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="));

    let Some(range) = range else {
        return (
            [(header::ACCEPT_RANGES, "bytes")],
            bytes.as_slice().to_vec(),
        )
            .into_response();
    };

    let (start_s, end_s) = range.split_once('-').unwrap_or((range, ""));
    let start: usize = start_s.parse().unwrap_or(0);
    let end: usize = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse().unwrap_or(total - 1)
    };
    if start >= total || end >= total || start > end {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    }
    let body = bytes[start..=end].to_vec();
    (
        StatusCode::PARTIAL_CONTENT,
        [
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}"),
            ),
        ],
        body,
    )
        .into_response()
}
