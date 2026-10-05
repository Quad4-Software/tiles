//! Disk cache for proxied upstream tiles.
//!
//! Layout: `<dir>/<source>/<z>/<x>/<y>.<ext>` plus a `<file>.meta` JSON
//! sidecar holding the upstream validator headers and expiry.
//!
//! Behavior: fresh entries serve straight from disk; expired entries are
//! revalidated with If-None-Match / If-Modified-Since (a 304 refreshes the
//! TTL without a download); upstream errors serve the stale tile rather
//! than failing. Concurrent misses on the same tile collapse into a single
//! upstream fetch via a per-key lock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default freshness when the upstream sends no cache headers: 7 days.
pub const DEFAULT_TTL_SECS: u64 = 604_800;

#[derive(Debug)]
pub struct ProxyCache {
    dir: PathBuf,
    default_ttl: Duration,
    /// Per-tile-key in-flight fetch locks for request coalescing.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileMeta {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    pub content_type: String,
    /// unix seconds after which the entry needs revalidation.
    pub expires_at: u64,
}

pub struct CachedTile {
    pub body: Vec<u8>,
    pub meta: TileMeta,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ProxyCache {
    pub fn new(dir: PathBuf, default_ttl: Duration) -> Self {
        Self {
            dir,
            default_ttl,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve a default cache dir: XDG cache, else system temp.
    pub fn default_dir() -> PathBuf {
        std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| Path::new(&h).join(".cache"))
            })
            .unwrap_or_else(std::env::temp_dir)
            .join("tiles")
    }

    fn paths(&self, key: &str) -> (PathBuf, PathBuf) {
        let tile = self.dir.join(key);
        let mut meta = tile.clone().into_os_string();
        meta.push(".meta");
        (tile, PathBuf::from(meta))
    }

    fn read(&self, key: &str) -> Option<(Vec<u8>, TileMeta)> {
        let (tile, meta) = self.paths(key);
        let body = std::fs::read(&tile).ok()?;
        let meta: TileMeta = serde_json::from_slice(&std::fs::read(&meta).ok()?).ok()?;
        Some((body, meta))
    }

    /// Entry present and still fresh?
    pub fn fresh(&self, key: &str) -> Option<CachedTile> {
        let (body, meta) = self.read(key)?;
        (meta.expires_at > now()).then_some(CachedTile { body, meta })
    }

    /// Entry present regardless of freshness (for revalidation/stale serving).
    pub fn stale(&self, key: &str) -> Option<CachedTile> {
        self.read(key).map(|(body, meta)| CachedTile { body, meta })
    }

    /// Store tile bytes + metadata.
    pub fn store(&self, key: &str, body: &[u8], meta: &TileMeta) -> Result<()> {
        let (tile, meta_path) = self.paths(key);
        if let Some(d) = tile.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&tile, body).context("write cache tile")?;
        std::fs::write(meta_path, serde_json::to_vec(meta)?).context("write cache meta")?;
        Ok(())
    }

    /// Refresh expiry on an existing entry (after a 304).
    pub fn touch(&self, key: &str, ttl: Duration) -> Result<()> {
        let (_, meta_path) = self.paths(key);
        let Some((_, mut meta)) = self.read(key) else {
            return Ok(());
        };
        meta.expires_at = now() + ttl.as_secs();
        std::fs::write(meta_path, serde_json::to_vec(&meta)?)?;
        Ok(())
    }

    /// The configured default TTL.
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// Get the lock Arc for a key (for cleanup after use).
    pub fn lock_arc(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().unwrap();
        Arc::clone(
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Drop idle lock entries to keep the map bounded.
    pub fn unlock_cleanup(&self, key: &str, m: &Arc<tokio::sync::Mutex<()>>) {
        let mut map = self.locks.lock().unwrap();
        if Arc::strong_count(m) <= 2 {
            map.remove(key);
        }
    }
}

/// TTL from upstream response headers, else `default`.
pub fn ttl_from(headers: &reqwest::header::HeaderMap, default: Duration) -> Duration {
    if let Some(cc) = headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
    {
        for part in cc.split(',') {
            let p = part.trim();
            if let Some(v) = p.strip_prefix("max-age=")
                && let Ok(secs) = v.trim_matches('"').parse::<u64>()
            {
                return Duration::from_secs(secs.max(1));
            }
            if p.eq_ignore_ascii_case("no-store") || p.eq_ignore_ascii_case("no-cache") {
                return Duration::ZERO;
            }
        }
    }
    if let Some(exp) = headers
        .get(reqwest::header::EXPIRES)
        .and_then(|v| v.to_str().ok())
        && let Ok(t) = httpdate::parse_http_date(exp)
    {
        let d = t.duration_since(SystemTime::now()).unwrap_or_default();
        if !d.is_zero() {
            return d;
        }
    }
    default
}

impl TileMeta {
    /// Build metadata for a fresh response.
    pub fn from_response(
        resp: &reqwest::Response,
        default_content_type: &str,
        ttl: Duration,
    ) -> Self {
        Self {
            etag: resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            last_modified: resp
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            content_type: resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(default_content_type)
                .to_string(),
            expires_at: now() + ttl.as_secs(),
        }
    }
}
