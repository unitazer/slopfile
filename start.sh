#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO"

if ! docker info >/dev/null 2>&1; then
  echo "error: docker daemon is not running (or not usable)." >&2
  echo "  start it first, then re-run:  $0" >&2
  exit 1
fi

"$REPO/docker/run.sh" up

"$REPO/docker/run.sh" deploy
SF="$REPO/target/release/sf"
if [ ! -x "$SF" ]; then
  if command -v cargo >/dev/null 2>&1; then
    cargo build --release --bin sf
  elif command -v nix >/dev/null 2>&1; then
    nix develop -c cargo build --release --bin sf
  else
    echo "error: $SF missing and neither cargo nor nix is available to build it (the nvim plugin needs the host sf CLI)" >&2
    exit 1
  fi
fi

TRANSCRIPTS="$REPO/shared/transcripts"
shopt -s nullglob
sessions=( "$TRANSCRIPTS"/*.md )
shopt -u nullglob
if [ "${#sessions[@]}" -eq 0 ]; then
  SESSION="$TRANSCRIPTS/1.md"
  # Seed via the container: a host-run init would freeze host paths into the
  # session's system prompt (skills dir, cwd) that the container can't resolve.
  "$REPO/docker/run.sh" shell -c 'slopfile init /shared/transcripts/1.md'
else
  SESSION="$(ls -t -- "${sessions[@]}" | head -1)"
fi

exec nvim -c "let g:sf_bin='$SF'" -c "luafile $REPO/nvim/plugin/slopfile.lua" "$SESSION"
