#!/bin/bash

# Exit on error
set -e

# Configuration
# GitHub Container Registry requires lowercased image names
REGISTRY="ghcr.io"
USERNAME="phrontizo"
IMAGE_NAME="debridmoviemapper"
FULL_IMAGE_NAME="$REGISTRY/$USERNAME/$IMAGE_NAME"
TAG="latest"

echo "=== DebridMovieMapper Multi-Platform Build ==="

# Check if Docker is running
if ! docker info > /dev/null 2>&1; then
    echo "Error: Docker is not running. Please start Docker and try again."
    exit 1
fi

# Check for buildx
if ! docker buildx version > /dev/null 2>&1; then
    echo "Error: Docker buildx is not installed. Please install it to build multi-platform images."
    exit 1
fi

# Ensure we are logged into GHCR (optional check, will fail at push if not logged in)
echo "Ensuring you are logged into $REGISTRY..."
# We don't force login here to avoid interrupting the script, but we remind the user.
echo "Note: If you haven't logged in, run: echo \$GITHUB_TOKEN | docker login $REGISTRY -u $USERNAME --password-stdin"

# Set up buildx builder if needed
BUILDER_NAME="debrid-builder"
if ! docker buildx inspect "$BUILDER_NAME" > /dev/null 2>&1; then
    echo "Creating new buildx builder: $BUILDER_NAME"
    docker buildx create --name "$BUILDER_NAME" --driver docker-container --use
    docker buildx inspect --bootstrap
else
    echo "Using existing buildx builder: $BUILDER_NAME"
    docker buildx use "$BUILDER_NAME"
fi

echo "Building for linux/amd64 and linux/arm64..."
echo "Target Image: $FULL_IMAGE_NAME:$TAG"

# Build and push
# --platform specifies the target architectures
# --push tells buildx to push the resulting manifest list to the registry
docker buildx build \
    --platform linux/amd64,linux/arm64 \
    -t "$FULL_IMAGE_NAME:$TAG" \
    --push \
    .

echo "=== Build and Push Complete ==="
echo "Image available at: $FULL_IMAGE_NAME:$TAG"
