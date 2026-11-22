# syntax=docker/dockerfile:1@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
# Native crypto dependencies require cmake and pkg-config.
# The image carries rustup and the OS, not the compiler: rust-toolchain.toml
# names that, and the layer below installs it, so its tag names no Rust version.
FROM rust:1-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea AS builder
RUN sed -i 's|http://|https://|g' /etc/apt/sources.list.d/*.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
# Its own layer, so the toolchain download stays cached until the pin moves.
COPY rust-toolchain.toml ./
RUN rustup toolchain install
COPY . .
RUN cargo build --release --locked -p proxiima-mcp \
    && cp target/release/proxiima-mcp /proxiima-mcp

# Distroless cc image: glibc + libstdc++ (for aws-lc) + ca-certificates
# (for outbound TLS to Zitadel/S3/embeddings), non-root by default.
FROM gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa AS runtime
ARG VERSION=0.0.0
ARG REVISION=unknown
LABEL org.opencontainers.image.title="proxiima-mcp" \
      org.opencontainers.image.description="Proxiima MCP server" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="https://github.com/Aquilo-Solution-S/proxiima"
COPY --from=builder /proxiima-mcp /usr/local/bin/proxiima-mcp
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/proxiima-mcp"]
