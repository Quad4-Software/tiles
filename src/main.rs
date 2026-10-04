use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
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
        #[arg(long, value_name = "IP")]
        host: Option<String>,
        /// Bind port.
        #[arg(short, long, value_name = "PORT")]
        port: Option<u16>,
        /// Cache-Control max-age for tile responses, seconds.
        #[arg(long, value_name = "SECS")]
        cache_max_age: Option<u32>,
        /// Public base URL for generated tilejson/style URLs
        /// (otherwise derived from request headers).
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
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
        } => serve(config, source, host, port, cache_max_age, public_url).await,
        Cmd::Fetch {
            src,
            dst,
            bbox,
            minzoom,
            maxzoom,
            raw,
        } => {
            let opts = FetchOptions {
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
        } => {
            let runtime = TilesRuntime::default();
            generate::run_generate(
                GenerateArgs {
                    input,
                    output,
                    minzoom,
                    maxzoom,
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
        } => mirror::mirror(provider, &dest, filter.as_deref(), limit, dry_run).await,
        Cmd::Info { src } => {
            let runtime = TilesRuntime::builder().silent_progress(true).build();
            let reader = tiles::source::open(&src, &runtime).await?;
            println!("{}", reader.tilejson().stringify());
            Ok(())
        }
    }
}

async fn serve(
    config: Option<PathBuf>,
    cli_sources: Vec<String>,
    host: Option<String>,
    port: Option<u16>,
    cache_max_age: Option<u32>,
    public_url: Option<String>,
) -> Result<()> {
    let cfg = config
        .as_deref()
        .map(Config::load)
        .transpose()?
        .unwrap_or_default();

    let mut sources = cfg.named_sources();
    for s in &cli_sources {
        sources.push(s.parse::<NamedSource>()?);
    }

    let host = host.unwrap_or(cfg.server.host);
    let port = port.unwrap_or(cfg.server.port);
    let cache_max_age = cache_max_age.unwrap_or(cfg.server.cache_max_age);
    let public_url = public_url.or(cfg.server.public_url);

    let runtime = TilesRuntime::builder().silent_progress(true).build();
    let state = server::build_state(&sources, &runtime, cache_max_age, public_url).await?;

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
