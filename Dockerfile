# Stage 1: Build the application
# Use BUILDPLATFORM so the builder always runs natively (no QEMU emulation)
FROM --platform=$BUILDPLATFORM rust:slim AS builder

# Install build dependencies including cross-compilation toolchain for arm64
RUN apt-get update && apt-get install -y \
    musl-tools \
    ca-certificates \
    gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

# Use buildx provided TARGETARCH to determine Rust target
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "amd64" ]; then \
        echo "x86_64-unknown-linux-musl" > /target_triple; \
    elif [ "$TARGETARCH" = "arm64" ]; then \
        echo "aarch64-unknown-linux-musl" > /target_triple; \
    else \
        echo "Unsupported architecture: $TARGETARCH" && exit 1; \
    fi && \
    rustup target add $(cat /target_triple)

# Configure cross-linker for arm64 targets
RUN mkdir -p /app/.cargo && \
    printf '[target.aarch64-unknown-linux-musl]\nlinker = "aarch64-linux-gnu-gcc"\n' \
    > /app/.cargo/config.toml

WORKDIR /app

# Cache dependency build: copy manifests, create dummy source, build deps only
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > src/mapper.rs && \
    TARGET=$(cat /target_triple) && \
    cargo build --release --target $TARGET && \
    rm -rf src

# Copy real source and build the application
COPY src ./src
RUN TARGET=$(cat /target_triple) && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/debridmoviemapper .

# Create empty data directory owned by nobody for the scratch stage
RUN mkdir -p /empty-data && chown 65534:65534 /empty-data

# Stage 2: Final runtime image (minimal 'scratch' image)
FROM scratch

# Copy system CA certificates so that the app can make HTTPS requests
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /app/debridmoviemapper /debridmoviemapper

# Expose the WebDAV port (default 8080)
EXPOSE 8080

# Create /data owned by nobody (65534) for the redb database.
# On scratch there is no mkdir/chown, so we COPY an empty dir from the builder.
COPY --from=builder --chown=65534:65534 /empty-data /data
WORKDIR /data
USER 65534:65534

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/debridmoviemapper", "--healthcheck"]

ENTRYPOINT ["/debridmoviemapper"]
