# Stage 1: Build the application
FROM rust:1.84-slim AS builder

# Install build dependencies:
# - musl-tools for static linking
# - ca-certificates for fetching dependencies via HTTPS
RUN apt-get update && apt-get install -y \
    musl-tools \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Determine the Rust target based on the build architecture
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "x86_64" ]; then \
        rustup target add x86_64-unknown-linux-musl; \
    elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
        rustup target add aarch64-unknown-linux-musl; \
    fi

WORKDIR /app

# Copy dependency manifests and source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build the application statically for the determined target
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "x86_64" ]; then \
        TARGET="x86_64-unknown-linux-musl"; \
    elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
        TARGET="aarch64-unknown-linux-musl"; \
    fi && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/debridmoviemapper .

# Stage 2: Final runtime image (minimal 'scratch' image)
FROM scratch

# Copy the statically linked binary
COPY --from=builder /app/debridmoviemapper /debridmoviemapper

# Copy system CA certificates so that the app can make HTTPS requests
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Expose the WebDAV port (default 8080)
EXPOSE 8080

# The application requires RD_API_TOKEN and TMDB_API_KEY environment variables.
# It uses 'metadata.db' in the current directory for persistence.
# It is recommended to mount a volume for 'metadata.db' if persistence across container recreations is desired.

ENTRYPOINT ["/debridmoviemapper"]
