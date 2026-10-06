#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
REPO="$(cd .. && pwd)"

if docker compose version >/dev/null 2>&1; then
  compose() { docker compose "$@"; }
elif command -v docker-compose >/dev/null 2>&1; then
  compose() { docker-compose "$@"; }
else
  echo "error: docker compose is not available (compose plugin or docker-compose binary)" >&2
  exit 1
fi

case "${1:-}" in
  up)
    shift
    export DOCKER_GID=$(stat -c '%g' /var/run/docker.sock 2>/dev/null || echo 998)
    compose up -d "$@"
    ;;
  logs)
    shift
    compose logs -f "${1:-llama-server}"
    ;;
  shell)
    shift
    compose exec slopfile /bin/bash "$@"
    ;;
  down)
    shift
    compose down "$@"
    ;;
  deploy)
    shift
    echo "building slopfile (release, in rust:1-slim)…"
    bin="$(mktemp)"
    trap 'rm -f "$bin"' EXIT
    docker run --rm \
      -v "$REPO":/src:ro \
      -v slopfile-cargo-registry:/usr/local/cargo/registry \
      -v slopfile-cargo-target:/target \
      -e CARGO_TARGET_DIR=/target \
      -w /src \
      rust:1-slim \
      sh -c 'cargo build --release --locked && cat /target/release/slopfile' > "$bin"
    chmod 755 "$bin"
    compose up -d slopfile
    compose cp "$bin" slopfile:/usr/local/bin/slopfile
    compose restart slopfile
    echo "deployed slopfile; container restarted"
    ;;
  *)
    echo "usage: $0 {up|logs [svc]|shell|down|deploy}" >&2
    exit 1
    ;;
esac
