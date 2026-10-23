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
    no_deps=""
    if [ "${1:-}" = "--no-deps" ]; then
      no_deps="--no-deps"
      shift
    fi
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
    if [ -n "$no_deps" ]; then
      compose up -d --no-deps slopfile
    else
      compose up -d slopfile
    fi
    compose cp "$bin" slopfile:/usr/local/bin/slopfile
    compose restart slopfile
    echo "deployed slopfile; container restarted"
    ;;
  *)
    echo "usage: $0 {up|logs [svc]|shell|down|deploy [--no-deps]}" >&2
    exit 1
    ;;
esac
