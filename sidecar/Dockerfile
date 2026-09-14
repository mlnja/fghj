# Builds fghj's in-network TLS proxy sidecar (see `sidecar_image.rs`, which
# materializes this alongside the rest of the crate into a scratch build
# context — `fghjd` embeds this file at compile time, so it never has to
# exist on the target machine's filesystem on its own).
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY ui/dist ./ui/dist
COPY sidecar ./sidecar
RUN cargo build --release --bin fghj-sidecar

FROM debian:bookworm-slim
COPY --from=build /app/target/release/fghj-sidecar /usr/local/bin/fghj-sidecar
ENTRYPOINT ["/usr/local/bin/fghj-sidecar"]
