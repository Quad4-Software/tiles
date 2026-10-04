//! Vector tile generation from .osm.pbf files (e.g. Geofabrik extracts).
//!
//! Pipeline: parse nodes, then relations, then ways. Build classified
//! features, project to web mercator tile space, simplify per zoom, clip
//! per tile, encode as MVT, and write a .pmtiles or .versatiles container
//! through the normal container writer.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use geo::{
    BooleanOps, Contains, Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon, SimplifyVw,
};
use osmpbfreader::{OsmId, OsmObj, OsmPbfReader, Tags};
use tracing::info;
use versatiles_container::{
    SourceType, Tile, TileSource, TileSourceMetadata, TilesRuntime, Traversal,
};
use versatiles_core::{
    json::{JsonObject, JsonValue},
    types::*,
};
use versatiles_geometry::{
    geo::{GeoFeature, GeoProperties, GeoValue},
    vector_tile::{VectorTile, VectorTileLayer},
};

const EXTENT: i64 = 4096;
const BUFFER: i64 = 64;
const MAX_LAT: f64 = 85.051_128_78;

/// Per-tile bucket: (layer name, clipped geometry in tile units, props).
type TileEntries = Vec<(&'static str, Geometry<f64>, Vec<(String, String)>)>;

#[derive(Debug, Clone)]
pub struct GenerateArgs {
    pub input: PathBuf,
    pub output: PathBuf,
    pub minzoom: u8,
    pub maxzoom: u8,
}

struct Feature {
    layer: &'static str,
    minzoom: u8,
    geom: Geometry<f64>,
    props: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn classify(tags: &Tags, is_area: bool) -> Option<(&'static str, u8)> {
    let natural = tags.get("natural").map(|v| v.as_str());
    if matches!(natural, Some("water" | "bay" | "strait" | "spring"))
        || tags
            .get("landuse")
            .is_some_and(|v| v.as_str() == "reservoir")
    {
        return Some(("water", 4));
    }
    if tags.contains_key("waterway") {
        return Some(("water", 8));
    }
    if let Some(hw) = tags.get("highway") {
        let z = match hw.as_str() {
            "motorway" | "motorway_link" | "trunk" | "trunk_link" => 4,
            "primary" | "primary_link" => 7,
            "secondary" | "secondary_link" => 9,
            "tertiary" | "tertiary_link" => 10,
            "path" | "footway" | "cycleway" | "track" | "bridleway" | "steps" => 13,
            _ => 12,
        };
        return Some(("roads", z));
    }
    if tags.contains_key("railway") {
        return Some(("transit", 7));
    }
    if tags.contains_key("building") {
        return Some(("buildings", 13));
    }
    if let Some(pl) = tags.get("place") {
        let z = match pl.as_str() {
            "country" | "state" => 0,
            "city" | "town" => 4,
            "village" => 9,
            _ => 11,
        };
        return Some(("places", z));
    }
    if tags.contains_key("landuse") {
        return Some(("landuse", 8));
    }
    if matches!(
        natural,
        Some("wood" | "scrub" | "grassland" | "wetland" | "beach" | "sand" | "heath" | "coastline")
    ) {
        return Some(("natural", 8));
    }
    if !is_area
        && (tags.contains_key("amenity")
            || tags.contains_key("shop")
            || tags.contains_key("tourism"))
    {
        return Some(("pois", 14));
    }
    None
}

/// A closed way is an area unless tagged otherwise.
fn is_area_way(way: &osmpbfreader::Way) -> bool {
    if !way.is_closed() || way.nodes.len() < 4 {
        return false;
    }
    if way.tags.contains("area", "no") {
        return false;
    }
    if way.tags.contains("area", "yes") {
        return true;
    }
    // Ways tagged as lines stay lines even when closed.
    !(way.tags.contains_key("highway")
        || way.tags.contains_key("barrier")
        || way.tags.contains_key("waterway"))
}

fn props_for(tags: &Tags, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|k| tags.get(*k).map(|v| ((*k).to_string(), v.to_string())))
        .collect()
}

// ---------------------------------------------------------------------------
// Multipolygon ring assembly
// ---------------------------------------------------------------------------

/// Join way node-id chains into closed rings by matching endpoints.
fn assemble_rings(mut chains: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
    let mut rings = vec![];
    while let Some(mut ring) = chains.pop() {
        let mut progress = true;
        while progress && ring.first() != ring.last() {
            progress = false;
            let head = *ring.first().unwrap();
            let tail = *ring.last().unwrap();
            let mut i = 0;
            while i < chains.len() {
                let c = &chains[i];
                let (cf, cl) = (*c.first().unwrap(), *c.last().unwrap());
                let merged = if tail == cf {
                    ring.extend_from_slice(&c[1..]);
                    true
                } else if tail == cl {
                    ring.extend(c[..c.len() - 1].iter().rev().copied());
                    true
                } else if head == cl {
                    let mut new = c[..c.len() - 1].to_vec();
                    new.extend_from_slice(&ring);
                    ring = new;
                    true
                } else if head == cf {
                    let mut new: Vec<i64> = c[1..].iter().rev().copied().collect();
                    new.extend_from_slice(&ring);
                    ring = new;
                    true
                } else {
                    false
                };
                if merged {
                    chains.remove(i);
                    progress = true;
                    break;
                }
                i += 1;
            }
        }
        if ring.first() == ring.last() && ring.len() >= 4 {
            rings.push(ring);
        }
    }
    rings
}

// ---------------------------------------------------------------------------
// Projection, simplification, clipping
// ---------------------------------------------------------------------------

fn mercator(lon: f64, lat: f64, scale: f64) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    (
        (lon + 180.0) / 360.0 * scale,
        (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * scale,
    )
}

fn project_geom(g: &Geometry<f64>, scale: f64) -> Geometry<f64> {
    match g {
        Geometry::Point(p) => {
            let (x, y) = mercator(p.x(), p.y(), scale);
            Geometry::Point(Point::new(x, y))
        }
        Geometry::MultiPoint(mp) => Geometry::MultiPoint(MultiPoint(
            mp.0.iter()
                .map(|p| {
                    let (x, y) = mercator(p.x(), p.y(), scale);
                    Point::new(x, y)
                })
                .collect(),
        )),
        Geometry::LineString(ls) => Geometry::LineString(LineString(
            ls.0.iter()
                .map(|c| {
                    let (x, y) = mercator(c.x, c.y, scale);
                    Coord { x, y }
                })
                .collect(),
        )),
        Geometry::MultiLineString(mls) => Geometry::MultiLineString(MultiLineString(
            mls.0
                .iter()
                .map(|ls| {
                    LineString(
                        ls.0.iter()
                            .map(|c| {
                                let (x, y) = mercator(c.x, c.y, scale);
                                Coord { x, y }
                            })
                            .collect(),
                    )
                })
                .collect(),
        )),
        Geometry::Polygon(p) => Geometry::Polygon(Polygon::new(
            LineString(
                p.exterior()
                    .0
                    .iter()
                    .map(|c| {
                        let (x, y) = mercator(c.x, c.y, scale);
                        Coord { x, y }
                    })
                    .collect(),
            ),
            p.interiors()
                .iter()
                .map(|r| {
                    LineString(
                        r.0.iter()
                            .map(|c| {
                                let (x, y) = mercator(c.x, c.y, scale);
                                Coord { x, y }
                            })
                            .collect(),
                    )
                })
                .collect(),
        )),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(MultiPolygon(
            mp.0.iter()
                .map(
                    |p| match project_geom(&Geometry::Polygon(p.clone()), scale) {
                        Geometry::Polygon(p) => p,
                        _ => unreachable!(),
                    },
                )
                .collect(),
        )),
        _ => g.clone(),
    }
}

fn geom_bbox(g: &Geometry<f64>) -> Option<(f64, f64, f64, f64)> {
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let mut any = false;
    let mut visit = |c: &Coord<f64>| {
        any = true;
        x0 = x0.min(c.x);
        y0 = y0.min(c.y);
        x1 = x1.max(c.x);
        y1 = y1.max(c.y);
    };
    match g {
        Geometry::Point(p) => visit(&p.0),
        Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| visit(&p.0)),
        Geometry::LineString(ls) => ls.0.iter().for_each(&mut visit),
        Geometry::MultiLineString(mls) => {
            mls.0.iter().flat_map(|l| l.0.iter()).for_each(&mut visit)
        }
        Geometry::Polygon(p) => {
            p.exterior().0.iter().for_each(&mut visit);
            p.interiors()
                .iter()
                .flat_map(|r| r.0.iter())
                .for_each(&mut visit);
        }
        Geometry::MultiPolygon(mp) => {
            mp.0.iter()
                .flat_map(|p| {
                    p.exterior()
                        .0
                        .iter()
                        .chain(p.interiors().iter().flat_map(|r| r.0.iter()))
                })
                .for_each(&mut visit);
        }
        _ => return None,
    }
    any.then_some((x0, y0, x1, y1))
}

/// Clip a geometry to a tile rect expanded by the buffer, in tile units.
fn clip_geom(g: &Geometry<f64>, rect: &Polygon<f64>) -> Option<Geometry<f64>> {
    match g {
        Geometry::Point(p) => rect.contains(p).then(|| g.clone()),
        Geometry::MultiPoint(mp) => {
            let pts: Vec<Point<f64>> = mp.0.iter().filter(|p| rect.contains(*p)).copied().collect();
            (!pts.is_empty()).then_some(Geometry::MultiPoint(MultiPoint(pts)))
        }
        Geometry::LineString(ls) => {
            let clipped = rect.clip(&MultiLineString(vec![ls.clone()]), false);
            (!clipped.0.is_empty()).then_some(Geometry::MultiLineString(clipped))
        }
        Geometry::MultiLineString(mls) => {
            let clipped = rect.clip(mls, false);
            (!clipped.0.is_empty()).then_some(Geometry::MultiLineString(clipped))
        }
        Geometry::Polygon(p) => {
            let clipped = p.intersection(rect);
            (!clipped.0.is_empty()).then_some(Geometry::MultiPolygon(clipped))
        }
        Geometry::MultiPolygon(mp) => {
            let clipped = mp.intersection(rect);
            (!clipped.0.is_empty()).then_some(Geometry::MultiPolygon(clipped))
        }
        _ => None,
    }
}

fn tile_rect(tx: i64, ty: i64) -> Polygon<f64> {
    let x0 = (tx * EXTENT - BUFFER) as f64;
    let y0 = (ty * EXTENT - BUFFER) as f64;
    let x1 = ((tx + 1) * EXTENT + BUFFER) as f64;
    let y1 = ((ty + 1) * EXTENT + BUFFER) as f64;
    Polygon::new(
        LineString(vec![
            Coord { x: x0, y: y0 },
            Coord { x: x1, y: y0 },
            Coord { x: x1, y: y1 },
            Coord { x: x0, y: y1 },
            Coord { x: x0, y: y0 },
        ]),
        vec![],
    )
}

// ---------------------------------------------------------------------------
// In-memory tile source feeding the container writer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GeneratedSource {
    tiles: HashMap<TileCoord, Tile>,
    pyramid: TilePyramid,
    metadata: TileSourceMetadata,
    tilejson: TileJSON,
    source_type: Arc<SourceType>,
}

#[async_trait]
impl TileSource for GeneratedSource {
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
        Ok(self.tiles.get(coord).cloned())
    }

    async fn tile_stream(&self, bbox: TileBBox) -> Result<TileStream<'static, Tile>> {
        let items: Vec<(TileCoord, Tile)> = bbox
            .iter_coords()
            .filter_map(|c| self.tiles.get(&c).map(|t| (c, t.clone())))
            .collect();
        Ok(TileStream::from_vec(items))
    }

    async fn tile_coord_stream(&self, bbox: TileBBox) -> Result<TileStream<'static, ()>> {
        let items: Vec<(TileCoord, ())> = bbox
            .iter_coords()
            .filter(|c| self.tiles.contains_key(c))
            .map(|c| (c, ()))
            .collect();
        Ok(TileStream::from_vec(items))
    }
}

// ---------------------------------------------------------------------------
// Main pipeline
// ---------------------------------------------------------------------------

pub async fn run_generate(args: GenerateArgs, runtime: &TilesRuntime) -> Result<()> {
    let input = &args.input;
    let output = &args.output;
    bail_unless_container(output)?;
    if args.minzoom > args.maxzoom || args.maxzoom > 15 {
        bail!(
            "invalid zoom range {}..={} (max supported: 15)",
            args.minzoom,
            args.maxzoom
        );
    }
    info!(input = %input.display(), "pass 1/3: reading nodes");
    let nodes = read_nodes(input)?;
    info!(count = nodes.len(), "nodes indexed");
    info!("pass 2/3: reading relations");
    let rels = read_relations(input)?;
    let member_ways: HashSet<i64> = rels
        .iter()
        .flat_map(|r| r.members.iter().map(|(id, _)| *id))
        .collect();
    info!(
        count = rels.len(),
        members = member_ways.len(),
        "multipolygon relations"
    );
    info!("pass 3/3: reading ways");

    let mut features: Vec<Feature> = vec![];
    let mut way_geoms: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];

    let file =
        BufReader::new(File::open(input).with_context(|| format!("open {}", input.display()))?);
    let mut pbf = OsmPbfReader::new(file);
    for obj in pbf.par_iter().filter_map(std::result::Result::ok) {
        let OsmObj::Way(way) = obj else { continue };
        let wid = way.id.0;
        if member_ways.contains(&wid) {
            way_geoms.insert(wid, way.nodes.iter().map(|n| n.0).collect());
        }
        let Some((layer, mz)) = classify(&way.tags, is_area_way(&way)) else {
            continue;
        };
        let coords: Vec<Coord<f64>> = way
            .nodes
            .iter()
            .filter_map(|n| nodes.get(&n.0))
            .map(|&(lon, lat)| Coord { x: lon, y: lat })
            .collect();
        if coords.len() < 2 {
            continue;
        }
        for c in &coords {
            grow(&mut bounds, c.x, c.y);
        }
        let geom = if is_area_way(&way) {
            let mut ring = coords;
            if ring.first() != ring.last() {
                ring.push(ring[0]);
            }
            Geometry::Polygon(Polygon::new(LineString(ring), vec![]))
        } else {
            Geometry::LineString(LineString(coords))
        };
        features.push(Feature {
            layer,
            minzoom: mz,
            geom,
            props: props_for(
                &way.tags,
                &[
                    "name", "highway", "railway", "building", "natural", "landuse", "waterway",
                ],
            ),
        });
    }

    // Nodes as point features (places, pois).
    let file = BufReader::new(File::open(input)?);
    let mut pbf = OsmPbfReader::new(file);
    for obj in pbf.par_iter().filter_map(std::result::Result::ok) {
        let OsmObj::Node(n) = obj else { continue };
        let Some((layer, mz)) = classify(&n.tags, false) else {
            continue;
        };
        if layer != "places" && layer != "pois" {
            continue;
        }
        let (lon, lat) = (n.lon(), n.lat());
        grow(&mut bounds, lon, lat);
        features.push(Feature {
            layer,
            minzoom: mz,
            geom: Geometry::Point(Point::new(lon, lat)),
            props: props_for(&n.tags, &["name", "place", "amenity", "shop", "tourism"]),
        });
    }

    // Multipolygon relations.
    for rel in &rels {
        let Some((layer, mz)) = classify(&rel.tags, true) else {
            continue;
        };
        let mut outers: Vec<Vec<i64>> = vec![];
        let mut inners: Vec<Vec<i64>> = vec![];
        for (wid, role) in &rel.members {
            if let Some(ids) = way_geoms.get(wid) {
                if role == "inner" {
                    inners.push(ids.clone());
                } else {
                    outers.push(ids.clone());
                }
            }
        }
        let outer_rings = assemble_rings(outers);
        let inner_rings = assemble_rings(inners);
        if outer_rings.is_empty() {
            continue;
        }
        let mut inner_polys: Vec<LineString<f64>> = vec![];
        for ring in &inner_rings {
            inner_polys.push(ring_to_linestring(ring, &nodes));
        }
        let mut polys = vec![];
        for ring in &outer_rings {
            let ext = ring_to_linestring(ring, &nodes);
            let int: Vec<LineString<f64>> = inner_polys
                .iter()
                .filter(|inner| inner.0.first().is_some_and(|c| point_in_ring(*c, &ext)))
                .cloned()
                .collect();
            for c in ext.0.iter() {
                grow(&mut bounds, c.x, c.y);
            }
            polys.push(Polygon::new(ext, int));
        }
        features.push(Feature {
            layer,
            minzoom: mz,
            geom: Geometry::MultiPolygon(MultiPolygon(polys)),
            props: props_for(
                &rel.tags,
                &["name", "natural", "landuse", "waterway", "building"],
            ),
        });
    }

    info!(features = features.len(), "features extracted");
    if bounds[0] > bounds[2] {
        bail!("no usable features found in input");
    }

    let tiles = build_tiles(&features, args.minzoom, args.maxzoom)?;
    info!(tiles = tiles.len(), "tiles built");

    let geo_bbox = GeoBBox::new_normalized(bounds[0], bounds[1], bounds[2], bounds[3]);
    let pyramid = TilePyramid::from_geo_bbox(args.minzoom, args.maxzoom, &geo_bbox)?;
    let mut tilejson = TileJSON::default();
    tilejson.update_from_pyramid(&pyramid);
    let layer_names: Vec<&str> = {
        let mut v: Vec<&str> = features.iter().map(|f| f.layer).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    tilejson.set_vector_layers(&layers_json(&layer_names))?;
    let _ = tilejson.set_string(
        "name",
        &format!(
            "{} (osm.pbf)",
            input.file_stem().unwrap_or_default().to_string_lossy()
        ),
    );
    let _ = tilejson.set_string("format", "pbf");

    let source = GeneratedSource {
        tiles,
        metadata: TileSourceMetadata::new(
            TileFormat::MVT,
            TileCompression::Brotli,
            Traversal::ANY,
            Some(pyramid.clone()),
        ),
        tilejson,
        source_type: SourceType::new_container("osm.pbf", &input.display().to_string()),
        pyramid,
    };

    runtime.write_to_path(Arc::new(source), output).await?;
    info!(output = %output.display(), "done");
    Ok(())
}

fn grow(bounds: &mut [f64; 4], lon: f64, lat: f64) {
    bounds[0] = bounds[0].min(lon);
    bounds[1] = bounds[1].min(lat);
    bounds[2] = bounds[2].max(lon);
    bounds[3] = bounds[3].max(lat);
}

fn bail_unless_container(output: &Path) -> Result<()> {
    match output.extension().and_then(|e| e.to_str()) {
        Some("pmtiles" | "versatiles") => Ok(()),
        other => bail!("output must end in .pmtiles or .versatiles (got {other:?})"),
    }
}

fn read_nodes(input: &Path) -> Result<HashMap<i64, (f64, f64)>> {
    let file = BufReader::new(File::open(input)?);
    let mut pbf = OsmPbfReader::new(file);
    let mut nodes = HashMap::new();
    for obj in pbf.par_iter_nodes().filter_map(std::result::Result::ok) {
        nodes.insert(obj.id.0, (obj.lon(), obj.lat()));
    }
    Ok(nodes)
}

struct RelSpec {
    members: Vec<(i64, String)>,
    tags: Tags,
}

fn read_relations(input: &Path) -> Result<Vec<RelSpec>> {
    let file = BufReader::new(File::open(input)?);
    let mut pbf = OsmPbfReader::new(file);
    let mut rels = vec![];
    for rel in pbf.par_iter_relations().filter_map(std::result::Result::ok) {
        if !rel.tags.contains("type", "multipolygon") && !rel.tags.contains("type", "boundary") {
            continue;
        }
        let members = rel
            .refs
            .iter()
            .filter_map(|r| match r.member {
                OsmId::Way(id) => Some((id.0, r.role.to_string())),
                _ => None,
            })
            .collect();
        rels.push(RelSpec {
            members,
            tags: rel.tags,
        });
    }
    Ok(rels)
}

fn ring_to_linestring(ring: &[i64], nodes: &HashMap<i64, (f64, f64)>) -> LineString<f64> {
    LineString(
        ring.iter()
            .filter_map(|id| nodes.get(id))
            .map(|&(lon, lat)| Coord { x: lon, y: lat })
            .collect(),
    )
}

/// Ray casting point-in-polygon for inner ring assignment.
fn point_in_ring(p: Coord<f64>, ring: &LineString<f64>) -> bool {
    let mut inside = false;
    let pts = &ring.0;
    for i in 0..pts.len() {
        let j = if i == 0 { pts.len() - 1 } else { i - 1 };
        let (a, b) = (pts[i], pts[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

fn layers_json(names: &[&str]) -> JsonValue {
    JsonValue::Array(versatiles_core::json::JsonArray(
        names
            .iter()
            .map(|n| {
                let mut o = JsonObject::new();
                o.set("id", JsonValue::from(*n));
                JsonValue::Object(o)
            })
            .collect(),
    ))
}

/// Project, simplify, clip and bucket features into per-tile MVT blobs.
fn build_tiles(features: &[Feature], minzoom: u8, maxzoom: u8) -> Result<HashMap<TileCoord, Tile>> {
    let mut out: HashMap<TileCoord, TileEntries> = HashMap::new();
    for z in minzoom..=maxzoom {
        let n = 1i64 << z;
        let scale = (EXTENT * n) as f64;
        let eps = 16.0 * 2f64.powf(f64::from(14u8.saturating_sub(z)).max(0.0) / 2.0);
        for f in features {
            if f.minzoom > z {
                continue;
            }
            let g = project_geom(&f.geom, scale);
            let g = simplify(&g, eps);
            let Some((x0, y0, x1, y1)) = geom_bbox(&g) else {
                continue;
            };
            let (x0, y0, x1, y1) = (
                x0 - BUFFER as f64,
                y0 - BUFFER as f64,
                x1 + BUFFER as f64,
                y1 + BUFFER as f64,
            );
            let (tx0, ty0) = (
                (x0 / EXTENT as f64).floor() as i64,
                (y0 / EXTENT as f64).floor() as i64,
            );
            let (tx1, ty1) = (
                (x1 / EXTENT as f64).floor() as i64,
                (y1 / EXTENT as f64).floor() as i64,
            );
            for tx in tx0.max(0)..=tx1.min(n - 1) {
                for ty in ty0.max(0)..=ty1.min(n - 1) {
                    let rect = tile_rect(tx, ty);
                    if let Some(clipped) = clip_geom(&g, &rect) {
                        out.entry(TileCoord::new(z, tx as u32, ty as u32)?)
                            .or_default()
                            .push((f.layer, clipped, f.props.clone()));
                    }
                }
            }
        }
    }

    let mut tiles = HashMap::new();
    for (coord, entries) in out {
        let mut feats_by_layer: HashMap<&'static str, Vec<GeoFeature>> = HashMap::new();
        for (layer_name, geom, props) in entries {
            let mut f = GeoFeature::new(geom);
            let props: GeoProperties = props
                .into_iter()
                .map(|(k, v)| (k, GeoValue::String(v)))
                .collect();
            f.set_properties(props);
            feats_by_layer.entry(layer_name).or_default().push(f);
        }
        let mut layer_vec: Vec<VectorTileLayer> = feats_by_layer
            .into_iter()
            .filter_map(|(name, feats)| {
                VectorTileLayer::from_features(name.to_string(), feats, 4096, 2).ok()
            })
            .collect();
        layer_vec.sort_by(|a, b| a.name.cmp(&b.name));
        let vt = VectorTile::new(layer_vec);
        let tile = Tile::from_vector(vt, TileFormat::MVT)?;
        // Store brotli-compressed: ~15-25% smaller than gzip for MVT, and the
        // server passes it through untouched to br-capable clients.
        let blob = tile.into_blob(&TileCompression::Brotli)?;
        tiles.insert(
            coord,
            Tile::from_blob(blob, TileCompression::Brotli, TileFormat::MVT),
        );
    }
    Ok(tiles)
}

fn simplify(g: &Geometry<f64>, eps: f64) -> Geometry<f64> {
    match g {
        Geometry::LineString(ls) => Geometry::LineString(ls.simplify_vw(eps)),
        Geometry::MultiLineString(mls) => Geometry::MultiLineString(mls.simplify_vw(eps)),
        Geometry::Polygon(p) => Geometry::Polygon(p.simplify_vw(eps)),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(mp.simplify_vw(eps)),
        _ => g.clone(),
    }
}
