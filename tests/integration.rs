//! End-to-end tests: serve local / S3 / HTTP-backed containers and fetch.

mod common;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use object_store::ObjectStoreExt;
use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use tempfile::TempDir;
use tower::ServiceExt;
use versatiles_container::{TilesReader, VersaTilesReader};
use versatiles_core::io::DataReaderTrait;
use versatiles_core::{ByteRange, TileCompression, TileCoord};

use tiles::fetch::{self, FetchOptions};
use tiles::s3::{self, ObjectStoreDataReader};
use tiles::server;
use tiles::source::{self, NamedSource};

fn named(name: &str, spec: &str) -> NamedSource {
    NamedSource {
        name: name.into(),
        spec: spec.into(),
        style: None,
        headers: vec![],
    }
}

async fn get(
    app: axum::Router,
    uri: &str,
    accept_encoding: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .uri(uri)
        .header(header::HOST, "tiles.test")
        .header(header::ACCEPT_ENCODING, accept_encoding)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

// ---------- unit-level checks ----------

#[test]
fn named_source_parses() {
    let s: NamedSource = "osm=/data/osm.versatiles".parse().unwrap();
    assert_eq!(s.name, "osm");
    assert_eq!(s.spec, "/data/osm.versatiles");
    assert!(NamedSource::from_str("noequals").is_err());
    assert!(NamedSource::from_str("bad/name=x.pmtiles").is_err());
}

#[test]
fn s3_uri_parses() {
    let u = s3::parse_s3_uri("s3://my-bucket/a/b/osm.pmtiles").unwrap();
    assert_eq!(u.bucket, "my-bucket");
    assert_eq!(u.key, "a/b/osm.pmtiles");
    assert!(s3::parse_s3_uri("s3://nokey").is_err());
    assert!(s3::parse_s3_uri("s3://b/").is_err());
}

#[test]
fn bbox_parses() {
    assert!(fetch::parse_geo_bbox("4.0,51.0,5.0,52.0").is_ok());
    assert!(fetch::parse_geo_bbox("1,2,3").is_err());
    assert!(fetch::parse_geo_bbox("a,b,c,d").is_err());
}

// ---------- serving ----------

#[tokio::test]
async fn serves_local_versatiles() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;

    let runtime = common::test_runtime();
    let state = server::build_state(
        &[named("osm", fixture.to_str().unwrap())],
        &runtime,
        3600,
        None,
        None,
    )
    .await
    .unwrap();
    let app = server::router(state);

    // health + index
    let (s, _, _) = get(app.clone(), "/health", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, body) = get(app.clone(), "/", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("osm"));

    // tilejson advertises our URL template
    let (s, _, body) = get(app.clone(), "/osm/tilejson.json", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    let tj: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tile_url = tj["tiles"][0].as_str().unwrap().to_string();
    assert!(tile_url.starts_with("http://tiles.test/osm/"));

    // tile request (gzipped passthrough)
    let ext = tile_url.rsplit('.').next().unwrap().to_string();
    let (s, headers, body) = get(app.clone(), &format!("/osm/0/0/0.{ext}"), "gzip").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(!body.is_empty());

    // missing tile, wrong extension, unknown source
    let (s, _, _) = get(app.clone(), "/osm/4/0/0.mvt", "gzip").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = get(app.clone(), "/osm/0/0/0.png", "gzip").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = get(app, "/nope/0/0/0.mvt", "gzip").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serves_local_pmtiles() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "pmtiles").await;

    let runtime = common::test_runtime();
    let state = server::build_state(
        &[named("pm", fixture.to_str().unwrap())],
        &runtime,
        3600,
        None,
        None,
    )
    .await
    .unwrap();
    let app = server::router(state);

    let (s, _, body) = get(app.clone(), "/pm/0/0/0.mvt", "br, gzip").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.is_empty());
}

/// A .versatiles file stored in an object store is range-read correctly,
/// proving the S3 backend path (InMemory implements the same trait calls).
#[tokio::test]
async fn serves_s3_via_object_store() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;
    let bytes = std::fs::read(&fixture).unwrap();

    let store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    store
        .put(&StorePath::from("osm.versatiles"), bytes.clone().into())
        .await
        .unwrap();

    // raw range read
    let reader = ObjectStoreDataReader::new(
        store.clone(),
        "osm.versatiles",
        "s3://t/osm.versatiles".into(),
    );
    let head = reader.read_range(&ByteRange::new(0, 16)).await.unwrap();
    assert_eq!(head.as_slice(), &bytes[..16]);
    let all = reader.read_all().await.unwrap();
    assert_eq!(all.len() as usize, bytes.len());

    // full container open through the DataReader
    let runtime = common::test_runtime();
    let src = VersaTilesReader::open_reader(Box::new(reader), runtime)
        .await
        .unwrap();
    let coord = TileCoord::new(1, 0, 0).unwrap();
    let tile = src.tile(&coord).await.unwrap().expect("tile exists");
    let blob = tile.into_blob(&TileCompression::Gzip).unwrap();
    assert!(!blob.as_slice().is_empty());
}

/// A remote .pmtiles served over HTTP range requests.
#[tokio::test]
async fn serves_remote_pmtiles_over_http() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "pmtiles").await;
    let base = common::mini_http(std::fs::read(&fixture).unwrap(), "pmtiles").await;

    let runtime = common::test_runtime();
    let url = format!("{base}/fixture.pmtiles");
    assert!(source::supports_remote_extension(&url));

    let state = server::build_state(&[named("remote", &url)], &runtime, 60, None, None)
        .await
        .unwrap();
    let app = server::router(state);

    let (s, _, body) = get(app.clone(), "/remote/0/0/0.mvt", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.is_empty());
    let (s, _, _) = get(app, "/remote/6/60/60.mvt", "gzip").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ---------- fetch ----------

#[tokio::test]
async fn fetch_raw_local_copy() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;
    let dst = tmp.path().join("copy.versatiles");
    let src_str = fixture.to_str().unwrap().to_string();
    fetch::fetch(
        &src_str,
        &dst,
        &FetchOptions::default(),
        &common::test_runtime(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(&fixture).unwrap(),
        std::fs::read(&dst).unwrap()
    );
}

#[tokio::test]
async fn fetch_raw_http_download() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;
    let base = common::mini_http(std::fs::read(&fixture).unwrap(), "versatiles").await;

    let dst = tmp.path().join("dl.versatiles");
    fetch::fetch(
        &format!("{base}/fixture.versatiles"),
        &dst,
        &FetchOptions::default(),
        &common::test_runtime(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(&fixture).unwrap(),
        std::fs::read(&dst).unwrap()
    );
}

#[tokio::test]
async fn fetch_extract_maxzoom_to_pmtiles() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;
    let dst = tmp.path().join("extract.pmtiles");

    fetch::fetch(
        fixture.to_str().unwrap(),
        &dst,
        &FetchOptions {
            level_max: Some(1),
            ..Default::default()
        },
        &common::test_runtime(),
    )
    .await
    .unwrap();

    let runtime = common::test_runtime();
    let src = source::open(dst.to_str().unwrap(), &runtime).await.unwrap();
    // extracted container keeps z0/z1 tiles, drops z2+
    assert!(
        src.tile(&TileCoord::new(1, 1, 1).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        src.tile(&TileCoord::new(2, 0, 0).unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn fetch_raw_rejects_filters() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "versatiles").await;
    let dst = tmp.path().join("x.versatiles");
    let err = fetch::fetch(
        fixture.to_str().unwrap(),
        &dst,
        &FetchOptions {
            level_max: Some(1),
            raw: true,
            ..Default::default()
        },
        &common::test_runtime(),
    )
    .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn generate_osm_pbf_to_pmtiles() {
    let tmp = TempDir::new().unwrap();
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/monaco.osm.pbf");
    let output = tmp.path().join("monaco.pmtiles");
    tiles::generate::run_generate(
        tiles::generate::GenerateArgs {
            input,
            output: output.clone(),
            minzoom: 0,
            maxzoom: 14,
            workdir: Some(tmp.path().join("workdir")),
        },
        &common::test_runtime(),
    )
    .await
    .expect("generate");

    let runtime = common::test_runtime();
    let reader = tiles::source::open(output.to_str().unwrap(), &runtime)
        .await
        .unwrap();
    let tj = reader.tilejson().stringify();
    assert!(tj.contains("roads"));
    assert!(tj.contains("buildings"));
    assert!(tj.contains("water"));
    // Monaco z14 tile containing the city center must exist and be non-empty.
    let coord = TileCoord::new(14, 8529, 5973).unwrap();
    let tile = reader.tile(&coord).await.unwrap().expect("tile");
    assert!(
        tile.into_blob(&TileCompression::Uncompressed)
            .unwrap()
            .len()
            > 100
    );
}

/// Delta-update a local pmtiles from a newer file: generate two versions,
/// run update, and verify the result serves the new file's tile set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_pmtiles_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let input = std::path::Path::new("tests/testdata/monaco.osm.pbf").to_path_buf();
    if !input.exists() {
        return;
    }
    let old_path = tmp.path().join("old.pmtiles");
    let new_path = tmp.path().join("new.pmtiles");
    let runtime = common::test_runtime();

    tiles::generate::run_generate(
        tiles::generate::GenerateArgs {
            input: input.clone(),
            output: old_path.clone(),
            minzoom: 0,
            maxzoom: 13,
            workdir: Some(tmp.path().join("w1")),
        },
        &runtime,
    )
    .await
    .unwrap();
    tiles::generate::run_generate(
        tiles::generate::GenerateArgs {
            input,
            output: new_path.clone(),
            minzoom: 0,
            maxzoom: 14,
            workdir: Some(tmp.path().join("w2")),
        },
        &runtime,
    )
    .await
    .unwrap();

    let updated = tmp.path().join("updated.pmtiles");
    tiles::update::run_update(
        tiles::update::UpdateArgs {
            remote: new_path.to_str().unwrap().to_string(),
            http: tiles::http::HttpOpts::default(),
            local: old_path.clone(),
            output: Some(updated.clone()),
            full: false,
        },
        &runtime,
    )
    .await
    .unwrap();

    let reader = tiles::source::open(updated.to_str().unwrap(), &runtime)
        .await
        .unwrap();
    let tj: serde_json::Value = serde_json::from_str(&reader.tilejson().stringify()).unwrap();
    assert_eq!(tj["maxzoom"], 14);

    // A z14 tile present only in the new file must be served.
    let coord = TileCoord::new(14, 8529, 5973).unwrap();
    let tile = reader.tile(&coord).await.unwrap().expect("z14 tile");
    assert!(
        tile.into_blob(&TileCompression::Uncompressed)
            .unwrap()
            .len()
            > 100
    );
}

/// Proxy source specs (upstream tile templates) are detected from {z}/{x}/{y}.
#[test]
fn proxy_template_detection() {
    assert!(source::is_proxy_template(
        "https://a.tile.opentopomap.org/{z}/{x}/{y}.png"
    ));
    assert!(!source::is_proxy_template("https://x.com/map.pmtiles"));
    assert!(!source::is_proxy_template("/data/map.versatiles"));
}

/// API-key auth: everything except /health is protected, and the key works
/// via ?key=, Bearer, or X-Api-Key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_key_auth() {
    let tmp = TempDir::new().unwrap();
    let fixture = common::write_fixture(tmp.path(), "pmtiles").await;
    let runtime = common::test_runtime();
    let state = server::build_state(
        &[named("osm", fixture.to_str().unwrap())],
        &runtime,
        60,
        None,
        Some("secret".into()),
    )
    .await
    .unwrap();
    let app = server::router(state);

    // health open
    let (s, _, _) = get(app.clone(), "/health", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    // everything else closed
    for path in ["/", "/osm/tilejson.json", "/osm/0/0/0.pbf", "/osm/view"] {
        let (s, _, _) = get(app.clone(), path, "gzip").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}");
    }
    // query param works, and generated urls carry the key
    let (s, _, body) = get(app.clone(), "/osm/tilejson.json?key=secret", "gzip").await;
    assert_eq!(s, StatusCode::OK);
    let tj: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(tj["tiles"][0].as_str().unwrap().contains("key=secret"));
    // bearer + x-api-key
    let req = Request::builder()
        .uri("/osm/0/0/0.pbf")
        .header(header::HOST, "tiles.test")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let req = Request::builder()
        .uri("/osm/tilejson.json")
        .header(header::HOST, "tiles.test")
        .header("x-api-key", "secret")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
