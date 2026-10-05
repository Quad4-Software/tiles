//! Bulk downloads from public tile and OSM extract providers.
//!
//! - geofabrik:   https://download.geofabrik.de/index-v1.json
//! - versatiles:  https://download.versatiles.org/ index page + release feeds
//! - protomaps:   https://build-metadata.protomaps.dev/builds.json

use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;
use futures_util::StreamExt;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Provider {
    /// Geofabrik .osm.pbf extracts (554+ regional extracts).
    Geofabrik,
    /// download.versatiles.org .versatiles releases.
    Versatiles,
    /// Protomaps daily basemap .pmtiles builds.
    Protomaps,
}

pub struct RemoteFile {
    /// Suggested local relative path (may contain subdirs).
    pub name: String,
    pub url: String,
}

pub async fn catalog(provider: Provider, client: &reqwest::Client) -> Result<Vec<RemoteFile>> {
    match provider {
        Provider::Geofabrik => geofabrik_catalog(client).await,
        Provider::Versatiles => versatiles_catalog(client).await,
        Provider::Protomaps => protomaps_catalog(client).await,
    }
}

async fn geofabrik_catalog(client: &reqwest::Client) -> Result<Vec<RemoteFile>> {
    let body = client
        .get("https://download.geofabrik.de/index-v1.json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let mut out = vec![];
    for f in json["features"]
        .as_array()
        .context("index-v1.json: no features")?
    {
        if let Some(url) = f["properties"]["urls"]["pbf"].as_str() {
            let name = url
                .strip_prefix("https://download.geofabrik.de/")
                .unwrap_or(url)
                .to_string();
            out.push(RemoteFile {
                name,
                url: url.to_string(),
            });
        }
    }
    Ok(out)
}

async fn versatiles_catalog(client: &reqwest::Client) -> Result<Vec<RemoteFile>> {
    const BASE: &str = "https://download.versatiles.org";
    let mut urls = vec![];
    // The index page links every current file; release feeds list dated
    // planet builds.
    let mut pages = vec![String::new()];
    for feed in ["feed-osm.xml", "feed-elevation.xml", "feed-satellite.xml"] {
        pages.push(format!("/{feed}"));
    }
    for page in pages {
        let body = client
            .get(format!("{BASE}{page}"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        // Match href="/x.versatiles" and <link>https://.../x.versatiles</link>.
        for m in body.split('"').chain(body.split("</link>")) {
            let cand = m
                .trim_start_matches("href=")
                .trim_start_matches("<link>")
                .trim();
            let url = if let Some(rest) = cand.strip_prefix('/') {
                format!("{BASE}/{rest}")
            } else if cand.starts_with(BASE) {
                cand.to_string()
            } else {
                continue;
            };
            // Only root-level files resolve; deeper site paths are dead links.
            let is_root_file = url
                .strip_prefix(&format!("{BASE}/"))
                .is_some_and(|p| !p.contains('/'));
            if url.ends_with(".versatiles") && is_root_file {
                urls.push(url);
            }
        }
    }
    urls.sort();
    urls.dedup();
    // The site links some files at both the root and /home/download/... paths.
    // Sorting puts the root path first; keep one entry per file name.
    let mut seen = std::collections::HashSet::new();
    Ok(urls
        .into_iter()
        .map(|url| RemoteFile {
            name: url.rsplit('/').next().unwrap_or(&url).to_string(),
            url,
        })
        .filter(|r| seen.insert(r.name.clone()))
        .collect())
}

async fn protomaps_catalog(client: &reqwest::Client) -> Result<Vec<RemoteFile>> {
    let body = client
        .get("https://build-metadata.protomaps.dev/builds.json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let mut out = vec![];
    for b in json.as_array().context("builds.json: not a list")? {
        if let Some(key) = b["key"].as_str() {
            out.push(RemoteFile {
                name: key.to_string(),
                url: format!("https://build.protomaps.com/{key}"),
            });
        }
    }
    Ok(out)
}

/// Download every catalog entry matching `filter` into `dest_dir`.
/// `limit` caps the number of downloads; `dry_run` only prints the plan.
pub async fn mirror(
    provider: Provider,
    dest_dir: &Path,
    filter: Option<&str>,
    limit: Option<usize>,
    dry_run: bool,
    opts: &crate::http::HttpOpts,
) -> Result<()> {
    let client = opts.client(None)?;
    let mut files = catalog(provider, &client).await?;
    if let Some(f) = filter {
        files.retain(|r| r.name.contains(f));
    }
    if let Some(n) = limit {
        files.truncate(n);
    }
    info!(provider = ?provider, files = files.len(), dir = %dest_dir.display(), "catalog");
    if files.is_empty() {
        warn!("nothing matched");
        return Ok(());
    }
    if dry_run {
        for r in &files {
            println!("{}\t{}", r.name, r.url);
        }
        return Ok(());
    }
    std::fs::create_dir_all(dest_dir)?;
    for r in &files {
        let dst = dest_dir.join(&r.name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if skip_existing(&client, &r.url, &dst).await? {
            info!(file = %r.name, "already downloaded, skipping");
            continue;
        }
        download(&client, &r.url, &dst).await?;
    }
    Ok(())
}

/// Skip when the local file exists and its size matches Content-Length.
async fn skip_existing(client: &reqwest::Client, url: &str, dst: &Path) -> Result<bool> {
    let Ok(meta) = dst.metadata() else {
        return Ok(false);
    };
    let local_len = meta.len();
    let remote_len = client
        .head(url)
        .send()
        .await?
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    Ok(remote_len.is_some_and(|r| r == local_len))
}

async fn download(client: &reqwest::Client, url: &str, dst: &Path) -> Result<()> {
    let part = dst.with_extension("part");
    let mut stream = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .bytes_stream();
    let mut file = tokio::fs::File::create(&part).await?;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        total += chunk.len() as u64;
    }
    file.sync_all().await?;
    drop(file);
    std::fs::rename(&part, dst)?;
    info!(file = %dst.display(), bytes = total, "downloaded");
    Ok(())
}
