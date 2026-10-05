//! Source spec parsing and container opening.
//!
//! A source spec is any of:
//!   /path/to/tiles.{versatiles,pmtiles,mbtiles,tar}  or a tile directory
//!   http(s)://host/tiles.{versatiles,pmtiles}      (byte-range reads)
//!   s3://bucket/key.{versatiles,pmtiles}           (byte-range reads)

use anyhow::{Context, Result, bail};
use versatiles_container::{
    PMTilesReader, SharedTileSource, TilesReader, TilesRuntime, VersaTilesReader,
};

use crate::s3;

/// Container formats that can be opened through a generic `DataReader`
/// (i.e. over HTTP or S3 range requests). MBTiles and TAR need a real file.
const READER_EXTS: &[&str] = &["versatiles", "pmtiles"];

pub fn is_remote(spec: &str) -> bool {
    spec.starts_with("http://") || spec.starts_with("https://") || s3::is_s3_uri(spec)
}

fn extension_of(spec: &str) -> Result<String> {
    let path_part = spec.split(['?', '#']).next().unwrap_or(spec);
    path_part
        .rsplit(['/', '.'])
        .next()
        .filter(|e| !e.is_empty() && !e.contains('/'))
        .map(|e| e.to_ascii_lowercase())
        .with_context(|| format!("cannot determine container format of '{spec}' (no extension)"))
}

/// Open a container from a remote `DataReader` (http/s3/blob), dispatching on
/// the file extension the same way the container registry does.
pub async fn open_reader(
    reader: versatiles_core::io::DataReader,
    extension: &str,
    spec: &str,
    runtime: &TilesRuntime,
) -> Result<SharedTileSource> {
    match extension {
        "versatiles" => VersaTilesReader::open_reader(reader, runtime.clone()).await,
        "pmtiles" => PMTilesReader::open_reader(reader, runtime.clone()).await,
        ext => bail!(
            "'.{ext}' containers cannot be read over the network; \
			 fetch '{spec}' first and serve the local file"
        ),
    }
}

/// Open any supported source spec as a shared tile source.
pub async fn open(spec: &str, runtime: &TilesRuntime) -> Result<SharedTileSource> {
    if s3::is_s3_uri(spec) {
        let ext = extension_of(spec)?;
        let reader = s3::s3_data_reader(spec)?;
        return open_reader(reader, &ext, spec, runtime)
            .await
            .with_context(|| format!("failed to open '{spec}'"));
    }

    // Local paths and http(s):// URLs go through the versatiles registry,
    // which handles extension dispatch and range-request readers itself.
    runtime
        .reader_from_str(spec)
        .await
        .with_context(|| format!("failed to open '{spec}'"))
}

/// A `name=spec` pair as accepted by `--source` and the config file.
/// `style` optionally points to a custom MapLibre style JSON file.
#[derive(Debug, Clone)]
pub struct NamedSource {
    pub name: String,
    pub spec: String,
    pub style: Option<std::path::PathBuf>,
    /// Extra headers sent to upstream (proxy sources only).
    pub headers: Vec<String>,
}

impl std::str::FromStr for NamedSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (name, spec) = s
            .split_once('=')
            .with_context(|| format!("expected NAME=URI, got '{s}'"))?;
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '.', ' ', '?', '#']) {
            bail!("invalid source name '{name}'");
        }
        Ok(Self {
            name: name.to_string(),
            spec: spec.trim().to_string(),
            style: None,
            headers: vec![],
        })
    }
}

/// A source spec containing `{z}`/`{x}`/`{y}` placeholders is an upstream
/// tile proxy template, not a container.
pub fn is_proxy_template(spec: &str) -> bool {
    spec.contains("{z}") && spec.contains("{x}") && spec.contains("{y}")
}

/// Convenience used by tests: open a remote container only when the format
/// supports range reads.
pub fn supports_remote_extension(spec: &str) -> bool {
    extension_of(spec)
        .map(|e| READER_EXTS.contains(&e.as_str()))
        .unwrap_or(false)
}
