#!/bin/bash
# Boot the slopfile + llama-server stack and drop you into nvim with the
# slopfile plugin loaded, on the latest session (creating one if needed).
#
# Idempotent: if the containers are already up, this just re-verifies and opens
# nvim. Errors out if the docker daemon isn't running.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO"

# 0) The docker daemon must be reachable.
if ! docker info >/dev/null 2>&1; then
  echo "error: docker daemon is not running (or not usable)." >&2
  echo "  start it first, then re-run:  $0" >&2
  exit 1
fi

# 1) Boot the stack. docker/run.sh runs `docker compose up -d`, which is
#    idempotent — already-running containers are left alone, and composer
#    waits for llama-server to become healthy before slopfile starts.
"$REPO/docker/run.sh" up

# 2) Build the release binary and push a fresh copy into the slopfile
#    container (rebuild + copy + restart via docker/run.sh deploy). This also
#    rebuilds the host copy at target/release/slopfile, used by nvim :Tokens
#    and `slopfile init` below — so no separate host build is needed.
"$REPO/docker/run.sh" deploy
BIN="$REPO/target/release/slopfile"

# 3) Pick a session: most recently modified transcript, else create one.
#    Guarded against the empty case — a failing glob must not trip `set -e`
#    and skip nvim.
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

# 4) Open nvim with the user's normal config loaded, the slopfile plugin
#    luafile'd on top, and g:slopfile_bin pointing at the repo binary.
exec nvim -c "let g:slopfile_bin='$BIN'" -c "luafile $REPO/nvim/plugin/slopfile.lua" "$SESSION"