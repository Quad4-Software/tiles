//! Incremental update of a local `.pmtiles` from a newer remote `.pmtiles`.
//!
//! PMTiles stores a directory of (tile_id, offset, length, run_length)
//! entries addressed by byte ranges. Updating means: fetch the remote
//! header and directory via small range requests, diff against the local
//! file's directory, download only the blobs that are new or whose stored
//! length changed, then write a fresh container that copies unchanged tile
//! bytes straight out of the local file. For a daily planet build this
//! turns a ~100 GB download into a small delta.
//!
//! PMTiles has no per-tile hash, so "unchanged" means same stored length;
//! a rebuilt tile with an identical byte length is a false negative. Rare,
//! but real - use `--full` when correctness matters more than bandwidth.
//!
//! Directories use leaf pages (run_length == 0 pointers) on large files;
//! those are resolved recursively. Fetched blobs are spooled to a temp file
//! so memory stays flat regardless of how much changed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::info;
use versatiles_container::{
    SharedTileSource, SourceType, Tile, TileSource, TileSourceMetadata, TilesRuntime, Traversal,
};
use versatiles_core::compression::{decompress_brotli, decompress_gzip, decompress_zstd};
use versatiles_core::io::{DataReader, DataReaderFile, DataReaderTrait};
use versatiles_core::json::JsonValue;
use versatiles_core::types::*;
use versatiles_core::utils::HilbertIndex;
use versatiles_core::{Blob, ByteRange, GeoBBox, TileCoord, TileJSON};

// ---------------------------------------------------------------------------
// PMTiles v3 structures

#[derive(Debug, Clone)]
struct PmtHeader {
    root_offset: u64,
    root_bytes: u64,
    meta_offset: u64,
    meta_bytes: u64,
    leaf_offset: u64,
    data_offset: u64,
    data_bytes: u64,
    internal_compression: u8,
    tile_compression: u8,
    tile_type: u8,
    min_zoom: u8,
    max_zoom: u8,
    min_lon_e7: i32,
    min_lat_e7: i32,
    max_lon_e7: i32,
    max_lat_e7: i32,
    center_zoom: u8,
    center_lon_e7: i32,
    center_lat_e7: i32,
}

impl PmtHeader {
    fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 127 || &b[0..7] != b"PMTiles" || b[7] != 3 {
            bail!("not a PMTiles v3 file");
        }
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Ok(Self {
            root_offset: u64_at(8),
            root_bytes: u64_at(16),
            meta_offset: u64_at(24),
            meta_bytes: u64_at(32),
            leaf_offset: u64_at(40),
            data_offset: u64_at(56),
            data_bytes: u64_at(64),
            internal_compression: b[97],
            tile_compression: b[98],
            tile_type: b[99],
            min_zoom: b[100],
            max_zoom: b[101],
            min_lon_e7: i32_at(102),
            min_lat_e7: i32_at(106),
            max_lon_e7: i32_at(110),
            max_lat_e7: i32_at(114),
            center_zoom: b[118],
            center_lon_e7: i32_at(119),
            center_lat_e7: i32_at(123),
        })
    }
}

/// One directory entry: tile_ids [tile_id, tile_id+run_length) share the
/// blob at (offset, length). run_length == 0 means leaf-directory pointer.
#[derive(Debug, Clone, Copy)]
struct DirEnt {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

struct Varint<'a> {
    b: &'a [u8],
    p: usize,
}

impl Varint<'_> {
    fn next(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        let mut shift = 0;
        loop {
            let Some(&x) = self.b.get(self.p) else {
                bail!("truncated varint in pmtiles directory");
            };
            self.p += 1;
            v |= u64::from(x & 0x7f) << shift;
            if x & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift >= 64 {
                bail!("varint overflow in pmtiles directory");
            }
        }
    }
}

/// Decode a PMTiles directory blob (already decompressed).
fn decode_dir(b: &[u8]) -> Result<Vec<DirEnt>> {
    let mut r = Varint { b, p: 0 };
    let n = usize::try_from(r.next()?).context("dir entry count overflow")?;
    let mut entries = Vec::with_capacity(n.min(1 << 20));
    let mut last_id = 0u64;
    let mut ids = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        last_id += r.next()?;
        ids.push(last_id);
    }
    let mut runs = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        runs.push(r.next()?);
    }
    let mut lens = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        lens.push(r.next()?);
    }
    let mut last_off = 0u64;
    let mut last_len = 0u64;
    for i in 0..n {
        let v = r.next()?;
        // Spec: stored offset is (offset + 1); 0 means contiguous with the
        // previous entry, i.e. prev_offset + prev_length.
        let off = if v == 0 { last_off + last_len } else { v - 1 };
        entries.push(DirEnt {
            tile_id: ids[i],
            offset: off,
            length: u32::try_from(lens[i]).context("dir length overflow")?,
            run_length: u32::try_from(runs[i]).context("dir run overflow")?,
        });
        last_off = off;
        last_len = lens[i];
    }
    Ok(entries)
}

/// Binary search for the entry covering `tile_id` (sorted by tile_id).
fn find_entry(entries: &[DirEnt], tile_id: u64) -> Option<DirEnt> {
    let i = entries.partition_point(|e| e.tile_id <= tile_id);
    let i = i.checked_sub(1)?;
    let e = entries[i];
    (tile_id < e.tile_id + u64::from(e.run_length)).then_some(e)
}

fn map_compression(v: u8) -> Result<TileCompression> {
    Ok(match v {
        0 | 1 => TileCompression::Uncompressed,
        2 => TileCompression::Gzip,
        3 => TileCompression::Brotli,
        4 => TileCompression::Zstd,
        other => bail!("unknown pmtiles compression {other}"),
    })
}

fn map_tile_type(v: u8) -> TileFormat {
    match v {
        1 => TileFormat::MVT,
        2 => TileFormat::PNG,
        3 => TileFormat::JPG,
        4 => TileFormat::WEBP,
        5 => TileFormat::AVIF,
        _ => TileFormat::BIN,
    }
}

fn decompress(data: &[u8], comp: u8) -> Result<Vec<u8>> {
    let blob = Blob::from(data);
    Ok(match comp {
        0 | 1 => blob.as_slice().to_vec(),
        2 => decompress_gzip(&blob)?.as_slice().to_vec(),
        3 => decompress_brotli(&blob)?.as_slice().to_vec(),
        4 => decompress_zstd(&blob)?.as_slice().to_vec(),
        other => bail!("unsupported internal compression {other}"),
    })
}

/// Fetch + decode a pmtiles directory, following leaf pages.
async fn load_dir(reader: &dyn DataReaderTrait, h: &PmtHeader) -> Result<Vec<DirEnt>> {
    let root = reader
        .read_range(&ByteRange::new(h.root_offset, h.root_bytes))
        .await?;
    let raw = decompress(root.as_slice(), h.internal_compression)?;
    let entries = decode_dir(&raw)?;
    let mut data_entries = Vec::with_capacity(entries.len());
    for e in &entries {
        if e.run_length > 0 {
            data_entries.push(*e);
            continue;
        }
        let leaf = reader
            .read_range(&ByteRange::new(
                h.leaf_offset + e.offset,
                u64::from(e.length),
            ))
            .await?;
        let leaf_raw = decompress(leaf.as_slice(), h.internal_compression)?;
        data_entries.extend(decode_dir(&leaf_raw)?);
    }
    data_entries.sort_by_key(|e| e.tile_id);
    Ok(data_entries)
}

// ---------------------------------------------------------------------------
// Merged tile source: local bytes + spooled deltas

/// Serves the remote file's tile set: unchanged tiles are read from the
/// local file, changed ones from the spool of fetched blobs.
#[derive(Debug)]
struct MergedSource {
    remote_entries: Arc<Vec<DirEnt>>,
    /// remote blob offset -> (spool offset, len)
    spool_index: Arc<HashMap<u64, (u64, u64)>>,
    spool: Arc<std::sync::Mutex<File>>,
    local_file: Arc<std::sync::Mutex<File>>,
    local_entries: Arc<Vec<DirEnt>>,
    local_data_offset: u64,
    compression: TileCompression,
    format: TileFormat,
    metadata: TileSourceMetadata,
    tilejson: TileJSON,
    source_type: Arc<SourceType>,
    pyramid: TilePyramid,
}

#[async_trait::async_trait]
impl TileSource for MergedSource {
    fn source_type(&self) -> Arc<SourceType> {
        Arc::clone(&self.source_type)
    }
    fn metadata(&self) -> &TileSourceMetadata {
        &self.metadata
    }
    fn tilejson(&self) -> &TileJSON {
        &self.tilejson
    }
    async fn tile_pyramid(&self) -> Result<Arc<TilePyramid>> {
        Ok(Arc::new(self.pyramid.clone()))
    }
    async fn tile(&self, coord: &TileCoord) -> Result<Option<Tile>> {
        self.read_tile(coord)
    }
    async fn tile_stream(&self, bbox: TileBBox) -> Result<TileStream<'static, Tile>> {
        let s = self.shared();
        let iter = CoordIter::new(&bbox).into_iter().flatten();
        Ok(TileStream::from_iter_coord(iter, move |c| {
            s.read_tile(&c).ok().flatten()
        }))
    }
    async fn tile_coord_stream(&self, bbox: TileBBox) -> Result<TileStream<'static, ()>> {
        let entries = Arc::clone(&self.remote_entries);
        let iter = CoordIter::new(&bbox).into_iter().flatten();
        Ok(TileStream::from_iter_coord(iter, move |c| {
            let id = c.get_hilbert_index().ok()?;
            find_entry(&entries, id).is_some().then_some(())
        }))
    }
}

/// Send iterator over a bbox's coords (TileBBox::iter_coords is !Send).
struct CoordIter {
    level: u8,
    x_min: u32,
    x_max: u32,
    y_max: u32,
    x: u32,
    y: u32,
}

impl CoordIter {
    fn new(bbox: &TileBBox) -> Option<Self> {
        Some(Self {
            level: bbox.level(),
            x_min: bbox.x_min().ok()?,
            x_max: bbox.x_max().ok()?,
            y_max: bbox.y_max().ok()?,
            x: bbox.x_min().ok()?,
            y: bbox.y_min().ok()?,
        })
    }
}

impl Iterator for CoordIter {
    type Item = TileCoord;
    fn next(&mut self) -> Option<Self::Item> {
        if self.x > self.x_max || self.y > self.y_max {
            return None;
        }
        let c = TileCoord::new(self.level, self.x, self.y).ok()?;
        self.x += 1;
        if self.x > self.x_max {
            self.x = self.x_min;
            self.y += 1;
        }
        Some(c)
    }
}

/// Read-only parts of MergedSource cloned into stream closures.
#[derive(Clone)]
struct MergedReader {
    remote_entries: Arc<Vec<DirEnt>>,
    spool_index: Arc<HashMap<u64, (u64, u64)>>,
    spool: Arc<std::sync::Mutex<File>>,
    local_file: Arc<std::sync::Mutex<File>>,
    local_entries: Arc<Vec<DirEnt>>,
    local_data_offset: u64,
    compression: TileCompression,
    format: TileFormat,
}

impl MergedReader {
    fn read_tile(&self, coord: &TileCoord) -> Result<Option<Tile>> {
        let id = coord.get_hilbert_index()?;
        let Some(e) = find_entry(&self.remote_entries, id) else {
            return Ok(None);
        };
        let blob: Vec<u8> = if let Some(&(sp, len)) = self.spool_index.get(&e.offset) {
            let mut f = self.spool.lock().unwrap();
            let mut buf = vec![0u8; usize::try_from(len)?];
            f.seek(SeekFrom::Start(sp))?;
            f.read_exact(&mut buf)?;
            buf
        } else {
            let Some(le) = find_entry(&self.local_entries, id) else {
                return Ok(None);
            };
            let mut f = self.local_file.lock().unwrap();
            let mut buf = vec![0u8; usize::try_from(le.length)?];
            f.seek(SeekFrom::Start(self.local_data_offset + le.offset))?;
            f.read_exact(&mut buf)?;
            buf
        };
        Ok(Some(Tile::from_blob(
            Blob::from(blob),
            self.compression,
            self.format,
        )))
    }
}

impl MergedSource {
    fn shared(&self) -> MergedReader {
        MergedReader {
            remote_entries: Arc::clone(&self.remote_entries),
            spool_index: Arc::clone(&self.spool_index),
            spool: Arc::clone(&self.spool),
            local_file: Arc::clone(&self.local_file),
            local_entries: Arc::clone(&self.local_entries),
            local_data_offset: self.local_data_offset,
            compression: self.compression,
            format: self.format,
        }
    }

    fn read_tile(&self, coord: &TileCoord) -> Result<Option<Tile>> {
        let r = MergedReader {
            remote_entries: Arc::clone(&self.remote_entries),
            spool_index: Arc::clone(&self.spool_index),
            spool: Arc::clone(&self.spool),
            local_file: Arc::clone(&self.local_file),
            local_entries: Arc::clone(&self.local_entries),
            local_data_offset: self.local_data_offset,
            compression: self.compression,
            format: self.format,
        };
        r.read_tile(coord)
    }
}

// ---------------------------------------------------------------------------
// Entry point

pub struct UpdateArgs {
    /// Outbound auth/headers for the remote.
    pub http: crate::http::HttpOpts,
    /// Newer .pmtiles: http(s):// URL or a local path.
    pub remote: String,
    /// Existing local .pmtiles to diff against.
    pub local: PathBuf,
    /// Output path. Default: <local>.new.pmtiles, renamed over `local`.
    pub output: Option<PathBuf>,
    /// Download every tile instead of diffing (guaranteed correctness).
    pub full: bool,
}

pub async fn run_update(args: UpdateArgs, runtime: &TilesRuntime) -> Result<()> {
    if !args.remote.contains(".pmtiles") {
        bail!("remote must be a .pmtiles file or url");
    }
    if args.local.extension().and_then(|e| e.to_str()) != Some("pmtiles") {
        bail!("local file must be .pmtiles");
    }

    // ---- remote ----
    let remote: DataReader = if args.remote.starts_with("http") {
        Box::new(crate::http::AuthedHttpReader::new(
            args.remote.clone(),
            &args.http,
        )?)
    } else {
        let p = Path::new(&args.remote);
        let f = DataReaderFile::open(p).context("open remote pmtiles")?;
        f as DataReader
    };
    let head = remote.read_range(&ByteRange::new(0, 127)).await?;
    let rh = PmtHeader::parse(head.as_slice())?;
    info!(data_bytes = rh.data_bytes, "remote header parsed");
    let remote_entries = load_dir(remote.as_ref(), &rh).await?;
    info!(entries = remote_entries.len(), "remote directory parsed");

    // ---- local ----
    let local_reader = DataReaderFile::open(&args.local)
        .with_context(|| format!("cannot open '{}'", args.local.display()))?;
    let head = local_reader.read_range(&ByteRange::new(0, 127)).await?;
    let lh = PmtHeader::parse(head.as_slice())?;
    let local_entries = load_dir(local_reader.as_ref(), &lh).await?;
    info!(entries = local_entries.len(), "local directory parsed");
    let local_file = File::open(&args.local)?;

    // ---- diff: which remote blobs do we need? ----
    let mut to_fetch: Vec<(u64, u64)> = vec![]; // (remote blob offset, len)
    let mut seen_off: HashMap<u64, ()> = HashMap::new();
    for e in &remote_entries {
        for k in 0..u64::from(e.run_length) {
            let id = e.tile_id + k;
            let needs = args.full
                || match find_entry(&local_entries, id) {
                    Some(le) => le.length != e.length,
                    None => true,
                };
            if needs && seen_off.insert(e.offset, ()).is_none() {
                to_fetch.push((e.offset, u64::from(e.length)));
            }
        }
    }
    let fetch_bytes: u64 = to_fetch.iter().map(|(_, l)| l).sum();
    info!(
        changed_blobs = to_fetch.len(),
        bytes = fetch_bytes,
        pct = format!(
            "{:.2}%",
            100.0 * fetch_bytes as f64 / rh.data_bytes.max(1) as f64
        ),
        "diff complete"
    );

    // ---- fetch changed blobs into a temp spool ----
    let spool_path =
        std::env::temp_dir().join(format!("tiles-update-{}.spool", std::process::id()));
    let mut spool = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&spool_path)?;
    let mut spool_index: HashMap<u64, (u64, u64)> = HashMap::with_capacity(to_fetch.len());
    let mut done: u64 = 0;
    for (off, len) in &to_fetch {
        let data = remote
            .read_range(&ByteRange::new(rh.data_offset + off, *len))
            .await?;
        let pos = spool.stream_position()?;
        spool.write_all(data.as_slice())?;
        spool_index.insert(*off, (pos, *len));
        done += len;
    }
    spool.flush()?;
    info!(downloaded = done, "changed blobs fetched");

    // ---- build merged source and write the new file ----
    // Carry the remote's json metadata (name, attribution, vector_layers)
    // into the new file's tilejson.
    let mut tilejson = TileJSON::default();
    if rh.meta_bytes > 0
        && let Ok(meta) = remote
            .read_range(&ByteRange::new(rh.meta_offset, rh.meta_bytes))
            .await
        && let Ok(raw) = decompress(meta.as_slice(), rh.internal_compression)
    {
        tilejson = TileJSON::try_from(raw).unwrap_or_default();
    }
    tilejson
        .values
        .insert(
            "center",
            &JsonValue::from(vec![
                f64::from(rh.center_lon_e7) / 1e7,
                f64::from(rh.center_lat_e7) / 1e7,
                f64::from(rh.center_zoom),
            ]),
        )
        .ok();

    let geo_bbox = GeoBBox::new(
        f64::from(rh.min_lon_e7) / 1e7,
        f64::from(rh.min_lat_e7) / 1e7,
        f64::from(rh.max_lon_e7) / 1e7,
        f64::from(rh.max_lat_e7) / 1e7,
    )?;
    let pyramid = TilePyramid::from_geo_bbox(rh.min_zoom, rh.max_zoom, &geo_bbox)?;
    let _ = tilejson.set_string("format", "pbf");

    let comp = map_compression(rh.tile_compression)?;
    let fmt = map_tile_type(rh.tile_type);
    let source = MergedSource {
        remote_entries: Arc::new(remote_entries),
        spool_index: Arc::new(spool_index),
        spool: Arc::new(std::sync::Mutex::new(spool)),
        local_file: Arc::new(std::sync::Mutex::new(local_file)),
        local_entries: Arc::new(local_entries),
        local_data_offset: lh.data_offset,
        compression: comp,
        format: fmt,
        metadata: TileSourceMetadata::new(fmt, comp, Traversal::ANY, Some(pyramid.clone())),
        tilejson,
        source_type: SourceType::new_container("pmtiles", &args.remote),
        pyramid,
    };

    let out = args
        .output
        .clone()
        .unwrap_or_else(|| args.local.with_extension("new.pmtiles"));
    let shared: SharedTileSource = Arc::new(source);
    runtime.write_to_path(shared.clone(), &out).await?;
    drop(shared);
    spool_file_cleanup(&spool_path);

    if args.output.is_none() {
        std::fs::rename(&out, &args.local)
            .with_context(|| format!("rename {} over {}", out.display(), args.local.display()))?;
        info!(output = %args.local.display(), "updated in place");
    } else {
        info!(output = %out.display(), "done");
    }
    Ok(())
}

fn spool_file_cleanup(path: &Path) -> Option<()> {
    std::fs::remove_file(path).ok()
}
