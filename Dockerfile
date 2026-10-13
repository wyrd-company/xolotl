# syntax=docker/dockerfile:1
# The web UI is plain static files, so build it once on the build platform.
FROM --platform=$BUILDPLATFORM node:22-slim AS ui
WORKDIR /ui
RUN corepack enable
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store pnpm install --frozen-lockfile
COPY ui ./
RUN pnpm build

FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY config.example.toml ./
# Embedded into the release binary.
COPY --from=ui /ui/dist ./ui/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/xolotl /xolotl
# An empty data directory for the runtime image, which has no shell to create it.
RUN mkdir /data

# distroless/cc ships glibc and the CA bundle rustls-platform-verifier needs.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /xolotl /usr/local/bin/xolotl
# Owned by nonroot (65532) so a fresh named volume mounted here is writable.
COPY --from=build --chown=65532:65532 /data /data
ENV XOLOTL_DATABASE=/data/xolotl.db \
    XOLOTL_LOG_FORMAT=json
VOLUME ["/data"]
EXPOSE 8317
USER nonroot
ENTRYPOINT ["/usr/local/bin/xolotl"]
CMD ["serve", "--config", "/etc/xolotl/config.toml"]
