# tiles

Self-hosted map tile server and toolchain in a single Rust binary. Serves
PMTiles, VersaTiles, MBTiles, TAR archives and tile directories from disk,
HTTP(S) or S3, generates vector tiles from `.osm.pbf` extracts, mirrors
provider catalogs, delta-updates PMTiles files, and proxies upstream tile
endpoints with a disk cache.

Container I/O is [versatiles-rs](https://github.com/versatiles-org/versatiles-rs)
(`versatiles_container` 4.15). HTTP is axum. The viewer is a vendored
OpenLayers bundle (`ol` + `ol-mapbox-style`, built from `web/`): the browser
makes no third-party code requests.

## Install

```sh
# prebuilt x86_64 Linux binary (see Releases for the current tag)
curl -sL https://github.com/Quad4-Software/tiles/releases/download/v0.2.1/tiles-v0.2.1-x86_64-unknown-linux-gnu.tar.gz | tar xz

cargo build --release        # or build from source
```

## Quick start

```sh
tiles serve --source osm=data/firenze.pmtiles
# -> http://localhost:8080/ (source index, viewer at /osm/view)
```

Sources are `NAME=SPEC` pairs. A spec is a local path, an `http(s)://`
container URL (byte-range reads), an `s3://bucket/key` object, or an
upstream tile template containing `{z}/{x}/{y}` (proxied).

```sh
tiles serve --source osm=data/osm.pmtiles \
            --source topo=https://a.tile.opentopomap.org/{z}/{x}/{y}.png
```

An `http(s)://` spec without `{z}/{x}/{y}` placeholders and without a
container extension is fetched as a TileJSON document; the first `tiles[]`
entry becomes the upstream template. Providers that publish dated tile
snapshots (for example OpenFreeMap) can be followed this way:

```sh
tiles serve --source ofm=https://tiles.openfreemap.org/planet
```

A source under `tiles.openfreemap.org` with no custom style gets the
OpenFreeMap Bright style automatically, rewired to this server's tilejson
so tiles flow through the proxy cache.

A custom `style` (in config or `--style NAME=PATH_OR_URL`) may be a local
JSON file or an `http(s)://` URL fetched once at startup. In the style, a
source of the same type whose `url` is missing, empty, `"auto"`, or an
ancestor/descendant of the proxy prefix is rewritten to this server's
tilejson; unrelated sources (sprites, glyphs, relief overlays) are left
untouched.

## Commands

```sh
tiles serve   [--config FILE | -s NAME=SPEC...] [--port 8080] ...
tiles fetch   SRC DST [--bbox L,B,L,B] [--minzoom Z] [--maxzoom Z] [--raw]
tiles generate IN.osm.pbf -o OUT.pmtiles   # or .versatiles, .mbtiles,
                                         # postgres://host/db
tiles update  REMOTE.pmtiles LOCAL.pmtiles [--full]
tiles mirror  geofabrik|versatiles|protomaps DEST [--filter STR] [--limit N]
              [--dry-run]
tiles info    SRC
```

`fetch` byte-copies a container, or extracts a region/zoom subset when
`--bbox`/`--minzoom`/`--maxzoom` are given (container format follows the
output extension). `update` diffs the remote PMTiles directory against the
local file via range requests and downloads only changed blobs; same-length
entries are assumed unchanged, so use `--full` when a byte-exact refresh is
required. `generate` keeps its node/way index on disk (`--workdir`, default
system temp) so inputs do not need to fit in RAM.

## Configuration

Everything on `serve` can come from a YAML file instead of flags
(`config.example.yaml` is a full reference):

```yaml
server:
  host: 0.0.0.0
  port: 8080
  cache_max_age: 86400
  # public_url: https://tiles.example.com
  # api_key: s3cret
  # cache_dir: /var/cache/tiles

sources:
  - name: osm
    src: /data/osm.pmtiles
    # style: /data/osm-style.json       # served at /osm/style.json
  - name: topo                          # upstream proxy + disk cache
    src: https://a.tile.opentopomap.org/{z}/{x}/{y}.png
    headers:                            # sent to the upstream
      - "Authorization: Bearer TOKEN"
```

## HTTP

| Route | Purpose |
|---|---|
| `GET /health` | liveness (always open) |
| `GET /` | source index |
| `GET /{name}/tilejson.json` | TileJSON pointing at this server |
| `GET /{name}/{z}/{x}/{y}.{ext}` | tile |
| `GET /{name}/style.json` | generated or custom MapLibre style |
| `GET /{name}/view` | OpenLayers viewer |

Tiles are served with their stored compression when the client accepts it
(gzip/brotli) and recompressed otherwise. Responses carry an `ETag`;
`If-None-Match` returns 304. `Cache-Control: max-age` comes from
`--cache-max-age` (default 86400).

### Proxy cache

Proxy sources write tiles to `--cache-dir` (default `$XDG_CACHE_HOME/tiles`
or `~/.cache/tiles`) with a JSON sidecar holding the upstream validators.
Fresh entries serve from disk; expired entries are revalidated with
`If-None-Match`/`If-Modified-Since`; upstream failures serve the stale tile;
concurrent misses collapse into one upstream fetch. TTL comes from the
upstream `Cache-Control`/`Expires` header, falling back to `--cache-ttl`
(default 7 days). Responses report `x-tiles-cache: hit|miss|stale|revalidated`.

### Auth

Inbound: `--api-key` / `api_key` / `TILES_API_KEY` guards all routes except
`/health`. Clients authenticate with `?key=`, `Authorization: Bearer`, or
`X-Api-Key`; generated tilejson/style URLs embed `?key=` automatically.

Outbound: `fetch`, `mirror` and `update` accept `--header "K: v"` (repeatable),
`--user-agent`, `--basic-auth user:pass`, `--bearer TOK`, and
`--api-key-header "Name: v"`. For proxy sources, set `headers:` per source in
the config, or `--upstream-header "NAME|K: v"` on the CLI. An upstream
`User-Agent` header overrides the default `tiles/<version>` UA.

### Environment variables

| Variable | Equivalent |
|---|---|
| `TILES_HOST`, `TILES_PORT`, `TILES_PUBLIC_URL` | `--host`, `--port`, `--public-url` |
| `TILES_API_KEY` | `--api-key` |
| `TILES_CACHE_DIR`, `TILES_CACHE_TTL` | `--cache-dir`, `--cache-ttl` |
| `TILES_HTTP_HEADER` | `--header` (fetch/mirror/update) |
| `TILES_USER_AGENT`, `TILES_BASIC_AUTH`, `TILES_BEARER`, `TILES_API_KEY_HEADER` | outbound auth flags |
| `RUST_LOG` | log filter (default `info`) |

S3 uses the standard `AWS_REGION`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_ENDPOINT`,
`AWS_ALLOW_HTTP` variables via `object_store`.

## Viewer bundle

`assets/ol-viewer.js` and `assets/ol.css` are vendored build output. To
rebuild after changing `web/viewer.js` or bumping `ol`:

```sh
cd web && npm install && npm run build
```

## Container

```sh
podman build -t tiles -f Containerfile .
podman run --rm -p 8080:8080 -v ./data:/data:ro \
    tiles serve --source osm=/data/firenze.pmtiles
```

Multi-stage build: pinned Rust toolchain, distroless `cc-debian12:nonroot`
runtime (no shell, no package manager, uid 65532), about 51 MB. The default
command reads `/data/config.yaml`.

## OSM extract generation

`generate` is a three-pass `osmpbfreader` pipeline: nodes go into a redb
index, relation member ways are stored on disk, and ways resolve to
geometries that are projected, simplified per zoom, clipped to tiles with a
64-unit buffer, and encoded as MVT.

Layers: `water`, `landuse`, `natural`, `roads`, `transit`, `aeroway`,
`buildings`, `boundaries` (admin levels map to zooms), `places`, `pois`.
Multipolygon and `type=boundary` relations are assembled. Known gaps:
coastlines render as lines rather than ocean polygons, and administrative
coverage is partial. Max zoom is 15.

PostGIS output (`-o postgres://...`) writes one `tiles_<layer>` table per
layer (`geometry(Geometry,4326)`, `name`, `tags` jsonb) with a gist index;
tables are dropped and recreated on each run.

## Development

```sh
cargo test                                        # unit + integration
cargo clippy --all-targets -- -D warnings         # strict lint
```

CI runs fmt, strict clippy and tests on every push; tagging `v*` builds the
release binary and publishes it with a sha256 checksum.

## License

Quad4 Permissive License. See LICENSE.
