# tiles: self-hosted PMTiles/VersaTiles server
# podman build -t tiles -f Containerfile .
# podman run --rm -p 8080:8080 -v ./data:/data:ro tiles serve --source osm=/data/firenze.pmtiles

FROM docker.io/library/rust:1.99-bookworm@sha256:37f847a196b12e0c6298d265b99075a8bc9176f2bb22e434b4340ab7342ca3af AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY assets ./assets
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/build/target \
	cargo build --release --locked && cp target/release/tiles /tiles-bin

# Distroless runtime: no shell, no package manager, nonroot user (uid 65532).
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:777e96cf322c46bc32aca926c263624c4dc8d7cf37e2fa65ba2c7e697318ebbb
COPY --from=build /tiles-bin /usr/local/bin/tiles
VOLUME ["/data"]
EXPOSE 8080
LABEL org.opencontainers.image.title="tiles" \
	org.opencontainers.image.description="Self-hosted PMTiles/VersaTiles tile server, fetcher, and .osm.pbf tile generator" \
	org.opencontainers.image.source="https://github.com/Quad4-Software/tiles" \
	org.opencontainers.image.licenses="LicenseRef-Quad4Permissive"
ENTRYPOINT ["tiles"]
CMD ["serve", "--config", "/data/config.yaml"]
