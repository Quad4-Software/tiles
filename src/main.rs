use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;
use versatiles_container::TilesRuntime;

use tiles::config::Config;
use tiles::fetch::{self, FetchOptions};
use tiles::generate::{self, GenerateArgs};
use tiles::mirror::{self, Provider};
use tiles::server;
use tiles::source::NamedSource;
use tiles::update;

#[derive(Parser)]
#[command(
    name = "tiles",
    version,
    about = "Fast self-hosted PMTiles/VersaTiles server and fetcher"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve tiles over HTTP.
    Serve {
        /// YAML config file (server + sources).
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Tile source as NAME=URI. Repeatable.
        /// URI may be a local path, http(s):// or s3://bucket/key.
        #[arg(short, long, value_name = "NAME=URI")]
        source: Vec<String>,
        /// Bind address.
        #[arg(long, value_name = "IP", env = "TILES_HOST")]
        host: Option<String>,
        /// Bind port.
        #[arg(short, long, value_name = "PORT", env = "TILES_PORT")]
        port: Option<u16>,
        /// Cache-Control max-age for tile responses, seconds.
        #[arg(long, value_name = "SECS")]
        cache_max_age: Option<u32>,
        /// Public base URL for generated tilejson/style URLs
        /// (otherwise derived from request headers).
        #[arg(long, value_name = "URL", env = "TILES_PUBLIC_URL")]
        public_url: Option<String>,
        /// Custom MapLibre style file for a source: NAME=PATH. Repeatable.
        /// In the style file, a source with no url or "url":"auto" is wired
        /// to this server's tilejson automatically.
        #[arg(long, value_name = "NAME=PATH")]
        style: Vec<String>,
        /// Extra header sent to an upstream (proxy sources): "NAME|Header: v".
        /// Repeatable. An Authorization/User-Agent header works the same.
        #[arg(long, value_name = "NAME|Header: v")]
        upstream_header: Vec<String>,
        /// Require an API key on every route except /health. Clients may pass
        /// it as ?key=, "Authorization: Bearer", or "X-Api-Key".
        #[arg(long, value_name = "KEY", env = "TILES_API_KEY")]
        api_key: Option<String>,
    },
    /// Download a remote container, or extract a subset.
    Fetch {
        /// Source: local path, http(s):// URL, or s3://bucket/key.
        src: String,
        /// Output path; container format comes from the extension.
        dst: PathBuf,
        /// Only keep tiles inside "minlon,minlat,maxlon,maxlat" (implies convert).
        #[arg(long, value_name = "BBOX")]
        bbox: Option<String>,
        #[arg(long, value_name = "Z")]
        minzoom: Option<u8>,
        #[arg(long, value_name = "Z")]
        maxzoom: Option<u8>,
        /// Byte-for-byte copy only; refuse any conversion.
        #[arg(long)]
        raw: bool,
        /// Extra request header for the remote: "Name: value". Repeatable.
        #[arg(long = "header", value_name = "Name: value", env = "TILES_HTTP_HEADER")]
        http_header: Vec<String>,
        /// User-Agent sent to the remote.
        #[arg(long, value_name = "UA", env = "TILES_USER_AGENT")]
        user_agent: Option<String>,
        /// HTTP basic auth as "user:pass".
        #[arg(long, value_name = "USER:PASS", env = "TILES_BASIC_AUTH")]
        basic_auth: Option<String>,
        /// Bearer token for the remote.
        #[arg(long, value_name = "TOKEN", env = "TILES_BEARER")]
        bearer: Option<String>,
        /// API key sent as a header: "Header-Name:value" e.g. "X-Api-Key:abc".
        #[arg(long, value_name = "NAME:VALUE", env = "TILES_API_KEY_HEADER")]
        api_key_header: Option<String>,
    },
    /// Print TileJSON metadata for a source.
    Info { src: String },
    /// Generate vector tiles from a .osm.pbf file (e.g. a Geofabrik extract).
    Generate {
        /// Input .osm.pbf file.
        input: PathBuf,
        /// Output container; format comes from the extension (.pmtiles/.versatiles).
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        #[arg(long, value_name = "Z", default_value_t = 0)]
        minzoom: u8,
        #[arg(long, value_name = "Z", default_value_t = 14)]
        maxzoom: u8,
        /// Directory for the on-disk extraction index; defaults to a temp dir.
        /// Node lookups are disk-backed, so RAM stays flat even on large
        /// extracts. Point this at fast disk with free space.
        #[arg(long, value_name = "DIR")]
        workdir: Option<PathBuf>,
    },
    /// Incrementally update a local .pmtiles from a newer remote build:
    /// diffs the remote directory via range requests and downloads only
    /// the tiles that changed. Writes <local>.new.pmtiles and swaps it in.
    Update {
        /// Remote .pmtiles: https:// URL or a local path
        remote: String,
        /// Local .pmtiles to update
        local: PathBuf,
        /// Write output here instead of replacing the local file
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Download every tile instead of diffing (guaranteed correctness)
        #[arg(long)]
        full: bool,
        /// Extra request header for the remote: "Name: value". Repeatable.
        #[arg(long = "header", value_name = "Name: value", env = "TILES_HTTP_HEADER")]
        http_header: Vec<String>,
        /// User-Agent sent to the remote.
        #[arg(long, value_name = "UA", env = "TILES_USER_AGENT")]
        user_agent: Option<String>,
        /// HTTP basic auth as "user:pass".
        #[arg(long, value_name = "USER:PASS", env = "TILES_BASIC_AUTH")]
        basic_auth: Option<String>,
        /// Bearer token for the remote.
        #[arg(long, value_name = "TOKEN", env = "TILES_BEARER")]
        bearer: Option<String>,
        /// API key sent as a header: "Header-Name:value" e.g. "X-Api-Key:abc".
        #[arg(long, value_name = "NAME:VALUE", env = "TILES_API_KEY_HEADER")]
        api_key_header: Option<String>,
    },
    /// Download every file a provider publishes (geofabrik / versatiles /
    /// protomaps) into a directory, skipping files already present.
    Mirror {
        #[arg(value_enum)]
        provider: Provider,
        /// Destination directory.
        dest: PathBuf,
        /// Only files whose path contains this substring.
        #[arg(long, value_name = "SUBSTR")]
        filter: Option<String>,
        /// Cap the number of downloads.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// List what would be downloaded, don't download.
        #[arg(long)]
        dry_run: bool,
        /// Extra request header for the remote: "Name: value". Repeatable.
        #[arg(long = "header", value_name = "Name: value", env = "TILES_HTTP_HEADER")]
        http_header: Vec<String>,
        /// User-Agent sent to the remote.
        #[arg(long, value_name = "UA", env = "TILES_USER_AGENT")]
        user_agent: Option<String>,
        /// HTTP basic auth as "user:pass".
        #[arg(long, value_name = "USER:PASS", env = "TILES_BASIC_AUTH")]
        basic_auth: Option<String>,
        /// Bearer token for the remote.
        #[arg(long, value_name = "TOKEN", env = "TILES_BEARER")]
        bearer: Option<String>,
        /// API key sent as a header: "Header-Name:value" e.g. "X-Api-Key:abc".
        #[arg(long, value_name = "NAME:VALUE", env = "TILES_API_KEY_HEADER")]
        api_key_header: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let _ = tracing_log::LogTracer::init();
    let _ = versatiles_core::io::set_product("tiles", env!("CARGO_PKG_VERSION"), None);

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            config,
            source,
            host,
            port,
            cache_max_age,
            public_url,
            style,
            upstream_header,
            api_key,
        } => {
            serve(ServeOpts {
                config,
                cli_sources: source,
                host,
                port,
                cache_max_age,
                public_url,
                cli_styles: style,
                cli_upstream_headers: upstream_header,
                api_key,
            })
            .await
        }
        Cmd::Fetch {
            src,
            dst,
            bbox,
            minzoom,
            maxzoom,
            raw,
            http_header,
            user_agent,
            basic_auth,
            bearer,
            api_key_header,
        } => {
            let opts = FetchOptions {
                http: http_opts(http_header, user_agent, basic_auth, bearer, api_key_header),
                geo_bbox: bbox.map(|s| fetch::parse_geo_bbox(&s)).transpose()?,
                level_min: minzoom,
                level_max: maxzoom,
                raw,
            };
            let runtime = TilesRuntime::default();
            fetch::fetch(&src, &dst, &opts, &runtime).await
        }
        Cmd::Generate {
            input,
            output,
            minzoom,
            maxzoom,
            workdir,
        } => {
            let runtime = TilesRuntime::default();
            generate::run_generate(
                GenerateArgs {
                    input,
                    output,
                    minzoom,
                    maxzoom,
                    workdir,
                },
                &runtime,
            )
            .await
        }
        Cmd::Update {
            remote,
            local,
            output,
            full,
            http_header,
            user_agent,
            basic_auth,
            bearer,
            api_key_header,
        } => {
            let runtime = TilesRuntime::default();
            update::run_update(
                update::UpdateArgs {
                    remote,
                    local,
                    output,
                    full,
                    http: http_opts(http_header, user_agent, basic_auth, bearer, api_key_header),
                },
                &runtime,
            )
            .await
        }
        Cmd::Mirror {
            provider,
            dest,
            filter,
            limit,
            dry_run,
            http_header,
            user_agent,
            basic_auth,
            bearer,
            api_key_header,
        } => {
            let opts = http_opts(http_header, user_agent, basic_auth, bearer, api_key_header);
            mirror::mirror(provider, &dest, filter.as_deref(), limit, dry_run, &opts).await
        }
        Cmd::Info { src } => {
            let runtime = TilesRuntime::builder().silent_progress(true).build();
            let reader = tiles::source::open(&src, &runtime).await?;
            println!("{}", reader.tilejson().stringify());
            Ok(())
        }
    }
}

fn http_opts(
    http_header: Vec<String>,
    user_agent: Option<String>,
    basic_auth: Option<String>,
    bearer: Option<String>,
    api_key_header: Option<String>,
) -> tiles::http::HttpOpts {
    tiles::http::HttpOpts {
        headers: http_header,
        user_agent,
        basic_auth,
        bearer,
        api_key: api_key_header.and_then(|s| tiles::http::HttpOpts::parse_header(&s).ok()),
    }
}

struct ServeOpts {
    config: Option<PathBuf>,
    cli_sources: Vec<String>,
    host: Option<String>,
    port: Option<u16>,
    cache_max_age: Option<u32>,
    public_url: Option<String>,
    cli_styles: Vec<String>,
    cli_upstream_headers: Vec<String>,
    api_key: Option<String>,
}

async fn serve(o: ServeOpts) -> Result<()> {
    let ServeOpts {
        config,
        cli_sources,
        host,
        port,
        cache_max_age,
        public_url,
        cli_styles,
        cli_upstream_headers,
        api_key,
    } = o;
    let cfg = config
        .as_deref()
        .map(Config::load)
        .transpose()?
        .unwrap_or_default();

    let mut sources = cfg.named_sources();
    for s in &cli_sources {
        sources.push(s.parse::<NamedSource>()?);
    }
    for s in &cli_styles {
        let (name, path) = s
            .split_once('=')
            .with_context(|| format!("expected NAME=PATH, got '{s}'"))?;
        let Some(src) = sources.iter_mut().find(|x| x.name == name) else {
            bail!("--style '{s}': no source named '{name}'");
        };
        src.style = Some(PathBuf::from(path));
    }
    for h in &cli_upstream_headers {
        let (name, hv) = h
            .split_once('|')
            .with_context(|| format!("expected NAME|Header: value, got '{h}'"))?;
        let Some(src) = sources.iter_mut().find(|x| x.name == name) else {
            bail!("--upstream-header '{h}': no source named '{name}'");
        };
        src.headers.push(hv.to_string());
    }

    let host = host.unwrap_or(cfg.server.host);
    let port = port.unwrap_or(cfg.server.port);
    let cache_max_age = cache_max_age.unwrap_or(cfg.server.cache_max_age);
    let public_url = public_url.or(cfg.server.public_url);
    let api_key = api_key.or(cfg.server.api_key);
    if api_key.is_some() {
        info!("api-key auth enabled (all routes except /health)");
    }

    let runtime = TilesRuntime::builder().silent_progress(true).build();
    let state = server::build_state(&sources, &runtime, cache_max_age, public_url, api_key).await?;

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("invalid host/port")?;
    let listener = TcpListener::bind(addr).await?;
    info!(
        "listening on http://{addr} ({} source(s))",
        state.sources.len()
    );
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("shutting down");
}
