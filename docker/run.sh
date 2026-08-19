#!/bin/bash
# Convenience wrapper for the slopfile + llama-server stack.
set -euo pipefail
cd "$(dirname "$0")"

case "${1:-}" in
  up)
    shift
    # Export the docker group gid so the slopfile container can talk to the
    # docker socket (abort → restart llama-server).
    export DOCKER_GID=$(stat -c '%g' /var/run/docker.sock 2>/dev/null || echo 998)
    exec docker compose up -d "$@"
    ;;
  logs)
    shift
    exec docker compose logs -f "${1:-llama-server}"
    ;;
  shell)
    shift
    exec docker compose exec slopfile /bin/bash "$@"
    ;;
  down)
    shift
    exec docker compose down "$@"
    ;;
  deploy)
    shift
    # Build a release slopfile binary and drop it into the running container
    # (faster than rebuilding the image), then restart so it takes effect.
    echo "building slopfile (release)…"
    (cd .. && cargo build --release)
    bin=../target/release/slopfile
    docker compose up -d slopfile
    docker compose cp "$bin" slopfile:/usr/local/bin/slopfile
    docker compose restart slopfile
    echo "deployed slopfile; container restarted"
    ;;
  *)
    echo "usage: $0 {up|logs [svc]|shell|down|deploy}" >&2
    exit 1
    ;;
esac