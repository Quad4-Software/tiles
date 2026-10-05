# tiles

Fast, self-hosted map tile server and fetcher in Rust. One static-ish binary
serves PMTiles and VersaTiles containers from local disk, HTTP(S) URLs, or S3,
with no external services and no third-party requests from the browser.

Built on [versatiles-rs](https://github.com/versatiles-org/versatiles-rs)
(`versatiles_container` 4.15) for container I/O and axum for HTTP.

## Features

- Serve `.versatiles`, `.pmtiles`, `.mbtiles`, `.tar`, and tile directories
  (`.mbtiles`/`.tar` local-only: they need a real file)
- Sources: local path, `http(s)://` (byte-range reads), `s3://bucket/key`
- `fetch` downloads remote containers or extracts a bbox/zoom subset
- `generate` builds vector tiles from `.osm.pbf` files (Geofabrik extracts)
- `mirror` bulk-downloads every file a provider publishes (Geofabrik,
  VersaTiles, Protomaps), skipping files already on disk
- Zero-copy serving: tiles keep their stored compression (gzip/brotli) when
  the client accepts it, recompressing on the fly only when needed; ETag +
  If-None-Match returns 304 for unchanged tiles
- TileJSON, a generated MapLibre style, and a built-in viewer with vendored
  MapLibre assets: the browser never touches a CDN
- YAML config or pure CLI flags

## Usage

```sh
# serve a local container
tiles serve --source osm=data/firenze.pmtiles

# multiple sources, config file
tiles serve --config config.yaml

# a container straight out of S3 (AWS_* env vars)
AWS_REGION=eu-central-1 tiles serve --source osm=s3://my-bucket/osm.pmtiles

# download a remote container
tiles fetch https://example.org/osm.pmtiles data/osm.pmtiles

# extract a region / zoom range (converts formats by output extension)
tiles fetch s3://my-bucket/planet.versatiles region.pmtiles \
    --bbox 11.22,43.74,11.29,43.79 --maxzoom 14

# generate vector tiles from a Geofabrik .osm.pbf extract
tiles fetch https://download.geofabrik.de/europe/monaco-latest.osm.pbf data/monaco.osm.pbf
tiles generate data/monaco.osm.pbf -o data/monaco.pmtiles --maxzoom 14
# other output targets: .mbtiles, or straight into PostGIS
tiles generate data/monaco.osm.pbf -o data/monaco.mbtiles
tiles generate data/monaco.osm.pbf -o postgres://user@localhost/gis

# incrementally update a local .pmtiles to a newer remote build
# (diffs directories, downloads only changed tiles)
tiles update https://build.protomaps.com/20261005.pmtiles data/planet.pmtiles
tiles update /mnt/new/planet.pmtiles data/planet.pmtiles --full   # force all

# proxy an upstream tile endpoint through this server
tiles serve --source topo=https://a.tile.opentopomap.org/{z}/{x}/{y}.png

# custom MapLibre style for a source (or set style: in config.yaml)
tiles serve --source osm=data/osm.pmtiles --style osm=mystyle.json

# inspect metadata
tiles info data/firenze.pmtiles

# bulk downloads (see what a provider offers, then grab some or all)
tiles mirror geofabrik data/osm-pbf --dry-run          # list all 554 extracts
tiles mirror geofabrik data/osm-pbf --filter europe/   # one region
tiles mirror versatiles data/vt                      # all current releases
tiles mirror protomaps data/pm --limit 1             # newest planet build only
```

Then open `http://localhost:8080/` for the source index and the built-in viewer.

### Endpoints

| Route | Purpose |
|---|---|
| `GET /health` | liveness |
| `GET /` | HTML index of sources |
| `GET /{name}/tilejson.json` | TileJSON with this server's tile URL |
| `GET /{name}/{z}/{x}/{y}.{ext}` | tile (ext = pbf/mvt for MVT sources) |
| `GET /{name}/style.json` | generated or custom MapLibre style |
| `GET /{name}/view` | embedded MapLibre viewer |

### S3 credentials

Standard AWS environment variables are used (via `object_store`):

```
AWS_REGION or AWS_DEFAULT_REGION
AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN
AWS_ENDPOINT        # S3-compatible services (MinIO, R2, Garage, ...)
AWS_ALLOW_HTTP=true # for non-TLS endpoints
```

## Container (podman)

Multi-stage `Containerfile`: pinned Rust build stage, distroless
`cc-debian12:nonroot` runtime (no shell, no package manager, uid 65532).
Final image is about 49 MB.

```sh
podman build -t tiles -f Containerfile .
podman run --rm -p 8080:8080 -v ./data:/data:ro \
    tiles serve --source osm=/data/firenze.pmtiles
```

Or drop a `config.yaml` into the data dir (the default CMD reads
`/data/config.yaml`).

## Development

```sh
cargo test          # unit + integration tests (local, s3-mock, http-mock)
cargo clippy --all-targets
cargo build --release
```

## Notes

- `.mbtiles` and `.tar` containers need local files (sqlite/seekable file);
  `fetch` them down first if they live on a remote.
- `generate` writes `.pmtiles`, `.versatiles`, `.mbtiles`, or a `postgres://`
  target (one `tiles_<layer>` table per layer, EPSG:4326, tags as jsonb).
  Node/way indexes live on disk (redb, in `--workdir` or the system temp dir),
  so RAM stays flat on large extracts.
- Auth: `--api-key` (or `server.api_key`, `TILES_API_KEY`) guards every route
  but `/health`. Generated tilejson/style/view URLs carry `?key=` so MapLibre
  clients keep working. Outbound: `--header`, `--user-agent`, `--basic-auth`,
  `--bearer`, `--api-key-header` on fetch/mirror/update; `--upstream-header
  "NAME|K: v"` or per-source `headers:` for proxies.
- `update` diffs two `.pmtiles` directories via range requests and downloads
  only blobs whose stored length differs. PMTiles has no per-tile hash, so a
  rebuilt tile of identical length reads as unchanged; use `--full` to force a
  complete download.
- Proxy sources (`{z}/{x}/{y}` in the spec) forward tile requests upstream
  with status, content-type and ETag passthrough. Respect each provider's
  usage policy.
- `generate` layers are
  water, landuse, natural, roads, transit, aeroway, buildings, boundaries
  (admin levels), places, pois. Multipolygon and boundary relations are
  assembled; ocean coastline polygons are not (coastline renders as a line).
- Tiles are re-encoded per request only when the client cannot accept the
  stored encoding; put a caching proxy or CDN in front for heavy traffic.
- Set `--public-url` (or `public_url` in config) when serving behind a
  reverse proxy so generated URLs in tilejson/style are correct.

## License

Quad4 Permissive License, see LICENSE. Permissive: use it for anything, keep the copyright notice, do not claim modified copies are the original.
