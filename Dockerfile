# Stage 1: Build the application
FROM rust:slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    musl-tools \
    ca-certificates \
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

WORKDIR /app

# Copy dependency manifests and source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build the application statically for the determined target
RUN TARGET=$(cat /target_triple) && \
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
