FROM rust:1.89-bookworm AS builder

# Install build dependencies with cache mount
RUN <<EOT
    apt-get update
    apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        cmake \
        clang \
        git \
        curl \
        protobuf-compiler \
        libprotobuf-dev
EOT

WORKDIR /app

# Copy dependency manifests and project structure
COPY gridtokenx-chain-bridge/ gridtokenx-chain-bridge/
COPY gridtokenx-blockchain-core/ gridtokenx-blockchain-core/

WORKDIR /app/gridtokenx-chain-bridge

# Build in release mode with cargo cache mounts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/gridtokenx-chain-bridge/target \
    cargo build --release --bin gridtokenx-chain-bridge && \
    cp target/release/gridtokenx-chain-bridge /app/gridtokenx-chain-bridge-bin

# Stage 2: Runtime
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies
RUN <<EOT
    apt-get update
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        tzdata \
        curl
EOT

WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /app/gridtokenx-chain-bridge-bin /app/chain-bridge

# Expose port (gRPC: 5040)
EXPOSE 5040

# Run the binary
ENTRYPOINT ["/app/chain-bridge"]
