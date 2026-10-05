//! tiles: self-hosted tile server and fetcher for PMTiles and VersaTiles containers.
//!
//! Sources can be local files, HTTP(S) URLs, or s3:// URIs. Everything is served
//! from a single binary: tiles, TileJSON, a generated style, and a vendored
//! MapLibre viewer, so a browser never talks to a third party.

pub mod config;
pub mod fetch;
pub mod generate;
pub mod http;
pub mod mirror;
pub mod s3;
pub mod server;
pub mod source;
pub mod update;
pub mod viewer;
