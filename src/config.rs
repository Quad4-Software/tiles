//! YAML config file support.
//!
//! ```yaml
//! server:
//!   host: 0.0.0.0
//!   port: 8080
//!   public_url: https://tiles.example.com   # optional override
//!   cache_max_age: 86400
//! sources:
//!   - name: osm
//!     src: /data/osm.versatiles
//!   - name: backup
//!     src: s3://my-bucket/osm.pmtiles
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::source::NamedSource;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub sources: Vec<ConfigSource>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "default_cache_max_age")]
    pub cache_max_age: u32,
    /// Optional API key protecting every route except /health.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Proxy tile cache directory (enabled when proxy sources exist).
    #[serde(default)]
    pub cache_dir: Option<std::path::PathBuf>,
    /// Fallback TTL seconds for proxied tiles.
    #[serde(default)]
    pub cache_ttl: Option<u64>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            public_url: None,
            cache_max_age: default_cache_max_age(),
            api_key: None,
            cache_dir: None,
            cache_ttl: None,
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8080
}
fn default_cache_max_age() -> u32 {
    86400
}

#[derive(Debug, Deserialize)]
pub struct ConfigSource {
    pub name: String,
    pub src: String,
    /// Optional path to a custom style.json served at /{name}/style.json.
    #[serde(default)]
    pub style: Option<std::path::PathBuf>,
    /// Extra headers sent to the upstream (proxy sources only), as
    /// "Name: value" strings.
    #[serde(default)]
    pub headers: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{}'", path.display()))?;
        serde_saphyr::from_str(&text)
            .with_context(|| format!("cannot parse config file '{}'", path.display()))
    }

    pub fn named_sources(&self) -> Vec<NamedSource> {
        self.sources
            .iter()
            .map(|s| NamedSource {
                name: s.name.clone(),
                spec: s.src.clone(),
                style: s.style.clone(),
                headers: s.headers.clone(),
            })
            .collect()
    }
}
