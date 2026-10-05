//! `tiles fetch`: download a remote container, or extract a subset of it.
//!
//! Without filters this is a raw byte copy (fast: one stream, no decoding).
//! With `--bbox`/`--minzoom`/`--maxzoom`, when the output extension asks for a
//! different container format, or when the input is a tile directory, it goes
//! through the converter pipeline and rewrites the container.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tracing::info;
use versatiles_container::{TilesConvertReader, TilesConverterParameters, TilesRuntime};
use versatiles_core::{ByteRange, GeoBBox, TilePyramid};

use crate::s3;
use crate::source;

const COPY_CHUNK: u64 = 16 * 1024 * 1024;
const CONTAINER_EXTS: &[&str] = &["versatiles", "pmtiles", "mbtiles", "tar"];

#[derive(Debug, Default)]
pub struct FetchOptions {
    pub http: crate::http::HttpOpts,
    pub geo_bbox: Option<GeoBBox>,
    pub level_min: Option<u8>,
    pub level_max: Option<u8>,
    /// Force a raw byte copy even when filters were passed (they are ignored).
    pub raw: bool,
}

/// `minlon,minlat,maxlon,maxlat` in degrees.
pub fn parse_geo_bbox(s: &str) -> Result<GeoBBox> {
    let parts: Vec<f64> = s
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .with_context(|| format!("invalid number '{p}' in bbox"))
        })
        .collect::<Result<_>>()?;
    let [x0, y0, x1, y1] = parts
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 4 comma-separated numbers, got {}", parts.len()))?;
    GeoBBox::new(x0, y0, x1, y1).context("invalid bounding box")
}

fn ext_of(path_or_uri: &str) -> Option<String> {
    let clean = path_or_uri.split(['?', '#']).next().unwrap_or(path_or_uri);
    Path::new(clean)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

pub async fn fetch(
    src: &str,
    dst: &Path,
    opts: &FetchOptions,
    runtime: &TilesRuntime,
) -> Result<()> {
    let filtered = opts.geo_bbox.is_some() || opts.level_min.is_some() || opts.level_max.is_some();
    let src_is_dir = !source::is_remote(src) && Path::new(src).is_dir();
    let dst_ext = dst
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let format_change = match (ext_of(src), dst_ext.as_deref()) {
        (Some(s), Some(d)) => {
            CONTAINER_EXTS.contains(&s.as_str()) && CONTAINER_EXTS.contains(&d) && s != d
        }
        _ => false,
    };

    if !filtered && !format_change && !src_is_dir {
        raw_copy(src, dst, opts).await
    } else if opts.raw {
        bail!("--raw cannot be combined with filters, directories, or format conversion")
    } else {
        convert_copy(src, dst, opts, runtime).await
    }
}

/// One pass over the object, no decoding: byte-identical copy.
async fn raw_copy(src: &str, dst: &Path, opts: &FetchOptions) -> Result<()> {
    if s3::is_s3_uri(src) {
        return s3_copy(src, dst).await;
    }
    if src.starts_with("http://") || src.starts_with("https://") {
        return http_copy(src, dst, &opts.http).await;
    }
    tokio::fs::copy(src, dst)
        .await
        .with_context(|| format!("failed to copy '{src}' to '{}'", dst.display()))?;
    info!("copied {src} -> {}", dst.display());
    Ok(())
}

async fn s3_copy(src: &str, dst: &Path) -> Result<()> {
    let uri = s3::parse_s3_uri(src)?;
    let store = s3::s3_store(&uri.bucket)?;
    let total = s3::s3_object_size(&store, &uri.key).await?;
    let reader = s3::s3_data_reader(src)?;

    let mut out = tokio::fs::File::create(dst).await?;
    let mut offset = 0u64;
    while offset < total {
        let len = COPY_CHUNK.min(total - offset);
        let blob = reader
            .read_range(&ByteRange::new(offset, len))
            .await
            .with_context(|| format!("S3 read failed at offset {offset}"))?;
        out.write_all(blob.as_slice()).await?;
        offset += len;
    }
    out.flush().await?;
    info!("downloaded {total} bytes {src} -> {}", dst.display());
    Ok(())
}

async fn http_copy(src: &str, dst: &Path, opts: &crate::http::HttpOpts) -> Result<()> {
    let client = opts.client(None)?;
    let resp = client
        .get(src)
        .send()
        .await
        .with_context(|| format!("GET {src} failed"))?
        .error_for_status()
        .with_context(|| format!("GET {src} returned an error"))?;
    let total = resp.content_length();

    let mut out = tokio::fs::File::create(dst).await?;
    let mut received = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        out.write_all(&chunk).await?;
        received += chunk.len() as u64;
    }
    out.flush().await?;
    if let Some(t) = total
        && t != received
    {
        bail!("incomplete download: got {received} of {t} bytes");
    }
    info!("downloaded {received} bytes {src} -> {}", dst.display());
    Ok(())
}

/// Filtered/convert path: open the source as tiles, restrict the pyramid,
/// and write a new container (format chosen by the output extension).
async fn convert_copy(
    src: &str,
    dst: &Path,
    opts: &FetchOptions,
    runtime: &TilesRuntime,
) -> Result<()> {
    let reader =
        if (src.starts_with("http://") || src.starts_with("https://")) && !opts.http.is_empty() {
            let ext = src
                .split(['?', '#'])
                .next()
                .unwrap_or(src)
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string();
            let r = crate::http::AuthedHttpReader::new(src.to_string(), &opts.http)?;
            source::open_reader(Box::new(r), &ext, src, runtime).await?
        } else {
            source::open(src, runtime).await?
        };

    let mut pyramid = TilePyramid::new_full();
    if let Some(b) = &opts.geo_bbox {
        pyramid.intersect_geo_bbox(b)?;
    }
    if let Some(z) = opts.level_min {
        pyramid.set_level_min(z);
    }
    if let Some(z) = opts.level_max {
        pyramid.set_level_max(z);
    }

    let params = TilesConverterParameters {
        tile_pyramid: Some(pyramid),
        geo_bbox: opts.geo_bbox,
        ..Default::default()
    };
    let reader = TilesConvertReader::new_from_reader(reader, params).await?;
    runtime
        .write_to_path(Arc::new(reader), dst)
        .await
        .with_context(|| format!("failed writing '{}'", dst.display()))?;
    info!("extracted {src} -> {}", dst.display());
    Ok(())
}
