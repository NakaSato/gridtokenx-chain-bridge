# syntax=docker/dockerfile:1.7
# Chain Bridge — distroless image: binary + its shared libs only.
# No Rust toolchain, no target/ in the image (target lives in a BuildKit cache mount).
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
        libprotobuf-dev \
        busybox-static
EOT

WORKDIR /app

# Copy dependency manifests and project structure
COPY gridtokenx-chain-bridge/ gridtokenx-chain-bridge/
COPY gridtokenx-blockchain-core/ gridtokenx-blockchain-core/
COPY gridtokenx-telemetry/ gridtokenx-telemetry/

WORKDIR /app/gridtokenx-chain-bridge

# Build in release mode with cargo cache mounts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/gridtokenx-chain-bridge/target \
    cargo build --release --bin gridtokenx-chain-bridge && \
    cp target/release/gridtokenx-chain-bridge /app/gridtokenx-chain-bridge-bin

# Collect the binary + its non-glibc shared libs into a flat lib/ folder.
# glibc core + the dynamic loader come from the distroless/cc base — skip them.
RUN set -eux; \
    BIN=/app/gridtokenx-chain-bridge-bin; \
    mkdir -p /out/lib; \
    cp "$BIN" /out/chain-bridge; \
    cp /bin/busybox /out/busybox; \
    ldd "$BIN" | awk '/=>/{print $3} !/=>/{print $1}' | grep -E '^/' | sort -u | while read -r lib; do \
        case "$lib" in \
            */ld-linux*|*/libc.so*|*/libm.so*|*/libpthread*|*/libdl.so*|*/librt.so*) continue;; \
        esac; \
        cp -Lv "$lib" /out/lib/; \
    done

# -----------------------------------------------------------------------------
# Stage 2: Runtime (distroless)
# -----------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12 AS runtime

WORKDIR /app

COPY --from=builder /out/chain-bridge /app/chain-bridge
COPY --from=builder /out/lib/ /app/lib/
COPY --from=builder /out/busybox /usr/bin/busybox

ENV LD_LIBRARY_PATH=/app/lib

# Expose port (gRPC: 5040)
EXPOSE 5040

# Run the binary
ENTRYPOINT ["/app/chain-bridge"]
