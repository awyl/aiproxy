#!/usr/bin/env bash
set -euo pipefail

DOCKER_USER="${DOCKER_USER:?Set DOCKER_USER to your Docker Hub username}"
IMAGE="${DOCKER_USER}/aiproxy"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

echo "Building ${IMAGE}:${VERSION}..."
docker build -t "${IMAGE}:${VERSION}" -t "${IMAGE}:latest" .

echo "Pushing ${IMAGE}:${VERSION}..."
docker push "${IMAGE}:${VERSION}"
docker push "${IMAGE}:latest"

echo "Done — ${IMAGE}:${VERSION} + ${IMAGE}:latest pushed to Docker Hub"
