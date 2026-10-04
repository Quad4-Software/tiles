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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            public_url: None,
            cache_max_age: default_cache_max_age(),
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
            })
            .collect()
    }
}
