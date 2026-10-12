# syntax=docker/dockerfile:1
FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY config.example.toml ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/xolotl /xolotl

# distroless/cc ships glibc and the CA bundle rustls-platform-verifier needs.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /xolotl /usr/local/bin/xolotl
ENV XOLOTL_DATABASE=/data/xolotl.db \
    XOLOTL_LOG_FORMAT=json
VOLUME ["/data"]
EXPOSE 8317
USER nonroot
ENTRYPOINT ["/usr/local/bin/xolotl"]
CMD ["serve", "--config", "/etc/xolotl/config.toml"]
