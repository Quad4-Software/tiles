//! Shared outbound HTTP plumbing: auth, headers, user-agent, timeouts.
//!
//! Used by fetch, mirror, update, and upstream-proxy sources so every
//! outbound request can carry the same credentials and a real user agent.

use std::fmt::Debug;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use versatiles_core::io::DataReaderTrait;
use versatiles_core::{Blob, ByteRange};

/// Outbound request options shared by every command that talks to a server.
#[derive(Debug, Clone, Default)]
pub struct HttpOpts {
    /// Extra headers as "Name: value" or "Name=value" strings.
    pub headers: Vec<String>,
    /// Override the User-Agent string.
    pub user_agent: Option<String>,
    /// HTTP basic auth as "user:pass".
    pub basic_auth: Option<String>,
    /// Bearer token (sent as "Authorization: Bearer <token>").
    pub bearer: Option<String>,
    /// API key sent as a header: ("Header-Name", "key") e.g. ("X-Api-Key", "k").
    pub api_key: Option<(String, String)>,
}

impl HttpOpts {
    /// True when no auth or custom headers are configured.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
            && self.user_agent.is_none()
            && self.basic_auth.is_none()
            && self.bearer.is_none()
            && self.api_key.is_none()
    }

    /// Parse a raw "Name: value" or "Name=value" header pair.
    pub fn parse_header(s: &str) -> Result<(String, String)> {
        if let Some((k, v)) = s.split_once(':') {
            return Ok((k.trim().to_string(), v.trim().to_string()));
        }
        if let Some((k, v)) = s.split_once('=') {
            return Ok((k.trim().to_string(), v.trim().to_string()));
        }
        bail!("invalid header '{s}'; expected 'Name: value' or 'Name=value'")
    }

    /// Build a reqwest client with default headers, auth and timeouts applied.
    /// `per_request_timeout` caps total request time when set (proxy use);
    /// fetch/mirror clients get no total timeout so large downloads can run.
    pub fn client(&self, per_request_timeout: Option<Duration>) -> Result<reqwest::Client> {
        let mut hm = reqwest::header::HeaderMap::new();
        for h in &self.headers {
            let (k, v) = Self::parse_header(h)?;
            hm.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .with_context(|| format!("invalid header name '{k}'"))?,
                v.parse()
                    .with_context(|| format!("invalid header value for '{k}'"))?,
            );
        }
        if let Some(basic) = &self.basic_auth {
            let encoded = base64::engine::general_purpose::STANDARD.encode(basic.as_bytes());
            hm.insert(
                reqwest::header::AUTHORIZATION,
                format!("Basic {encoded}").parse().unwrap(),
            );
        }
        if let Some(token) = &self.bearer {
            hm.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}")
                    .parse()
                    .context("invalid bearer token")?,
            );
        }
        if let Some((k, v)) = &self.api_key {
            hm.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .with_context(|| format!("invalid api-key header '{k}'"))?,
                v.parse().context("invalid api-key value")?,
            );
        }
        let mut b = reqwest::Client::builder()
            .default_headers(hm)
            .user_agent(
                self.user_agent
                    .clone()
                    .unwrap_or_else(|| concat!("tiles/", env!("CARGO_PKG_VERSION")).to_string()),
            )
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120));
        if let Some(t) = per_request_timeout {
            b = b.timeout(t);
        }
        Ok(b.build()?)
    }
}

/// A ranged HTTP reader that carries auth headers, so remote pmtiles on
/// protected endpoints work with fetch/update/serve.
#[derive(Debug)]
pub struct AuthedHttpReader {
    client: reqwest::Client,
    url: String,
}

impl AuthedHttpReader {
    pub fn new(url: String, opts: &HttpOpts) -> Result<Self> {
        Ok(Self {
            client: opts.client(None)?,
            url,
        })
    }
}

#[async_trait]
impl DataReaderTrait for AuthedHttpReader {
    async fn read_range(&self, range: &ByteRange) -> Result<Blob> {
        let resp = self
            .client
            .get(&self.url)
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", range.offset, range.offset + range.length - 1),
            )
            .send()
            .await
            .with_context(|| format!("range GET {} failed", self.url))?;
        if !resp.status().is_success() && resp.status().as_u16() != 206 {
            bail!("range GET {} -> {}", self.url, resp.status());
        }
        Ok(Blob::from(resp.bytes().await?.to_vec()))
    }

    async fn read_all(&self) -> Result<Blob> {
        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", self.url))?
            .error_for_status()
            .context("remote returned an error")?;
        Ok(Blob::from(resp.bytes().await?.to_vec()))
    }

    fn name(&self) -> &str {
        &self.url
    }
}
