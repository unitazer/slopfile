#!/bin/bash
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
BIN="$REPO/target/release/slopfile"

TRANSCRIPTS="$REPO/shared/transcripts"
shopt -s nullglob
sessions=( "$TRANSCRIPTS"/*.md )
shopt -u nullglob
if [ "${#sessions[@]}" -eq 0 ]; then
  SESSION="$TRANSCRIPTS/1.md"
  "$BIN" init "$SESSION"
else
  SESSION="$(ls -t -- "${sessions[@]}" | head -1)"
fi

exec nvim -c "let g:slopfile_bin='$BIN'" -c "luafile $REPO/nvim/plugin/slopfile.lua" "$SESSION"