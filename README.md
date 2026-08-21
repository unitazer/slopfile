# slopfile — a file-as-UI coding agent

`slopfile` is a small daemon that turns a single Markdown file into a whole
coding-agent session. You type a request at the bottom of the file, a daemon
streams a `llama-server` response back into the same file — including tool
calls (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, `question`) —
and marks completion inline. No TUI, no database: the `.md` file *is* the UI.

```
clanker/slopfile/            # the single git repo (source + docker + weights)
├── src/  docs/  nvim/  tests/   # the Rust project
├── docker/                    # compose stack + run.sh wrappers
│   ├── docker-compose.yml
│   ├── Dockerfile.slopfile
│   └── run.sh
├── shared/                    # runtime volume (bind-mounted into the containers)
│   ├── models/                # GGUF + mmproj weights (gitignored; mounted in llama-server)
│   ├── slopfile.toml          # daemon app config (tracked)
│   └── transcripts/           # one session .md file per conversation (gitignored)
└── workspace/                 # the model's tool workdir (gitignored)
```

The repo is one git directory: source, docker definitions, the `shared/` runtime
volume, and a private `workspace/` all live under `slopfile/`. The containers
mount only `shared/` and `workspace/` — the slopfile source is **not** mounted
into the agent's container, so the model cannot read or edit its own code. The
`read` tool additionally refuses files containing the `<!-- slopfile` marker
(session transcripts / daemon-owned state).

---

## How the file works

One session = one `.md` file with three regions:

```markdown
```toml
model = "qwen3.8-27b"
thinking = "auto"
tools = ["read", "write", "edit", "bash", "grep", "find", "ls"]
```
# Session                        <- HEADER + system prompt start

System prompt / starting text.   <- this is the first text the model sees

### You
a user turn

### Assistant
streamed assistant text, then tool blocks, then
<!-- @done -->                     done marker (daemon writes it on completion)

anything below the last <!-- @done --> is the TAIL: type your next request here
```

- **Header** — the *first* ` ```toml ` block in the file. Per-session settings.
  Overrides the app config. Only the first one counts; a later ` ```toml `
  (e.g. pasted into a code block) is ordinary text.
- **Middle** — what the LLM actually sees: system prompt, `### You` turns,
  `### Assistant` output with inline tool blocks.
- **Tail** — everything below the last `<!-- @done -->`: your drafts, pending
  `QUESTIONS` blocks, and a daemon-owned status line
  (`<!-- slopfile:live {...} -->`) at the very bottom.

The request context sent to the model is **the middle region only** — header,
tail, comments, status line, and stashed tool JSON are stripped.

### Turn flow

1. Type a request in the tail → hit `:Send` (or `:Resume`).
2. The daemon promotes the tail text to a `### You` turn and streams an
   `### Assistant` response.
3. Tool calls are executed and written as `<!-- tool:... -->` / `<!-- result:... -->`
   blocks, then the next turn continues.
4. On clean completion the daemon appends `<!-- @done -->`.
5. If the model calls the `question` tool, the daemon stops and renders a
   `QUESTIONS` table in the tail. Answer below the `>` line (e.g. `2`, comma
   separated, or free text) and `:Send` again; or ` /skip` for
   defaults/empty. In `confirmations = "yolo"` mode they auto-resolve.

---

## Run 1 — host (quick, no Docker for the daemon)

You need a `llama-server` (or any OpenAI-compatible endpoint) reachable at the
`api_base` from the config.

```bash
# 1. Build the binary
cd slopfile
cargo build --release                 # -> target/release/slopfile (alias slopfile=./target/release/slopfile)

# 2. (optional) create a session file template
slopfile init "$HOME/things/clanker/slopfile/shared/transcripts/t1.md"

# 3. start the daemon (watches the transcripts dir, drives every file)
slopfile
```

The daemon reads `SLOPFILE_CONFIG` (default
`slopfile/shared/slopfile.toml`) and can be overridden per-launch:

```bash
SLOPFILE_CONFIG=/path/to/slopfile.toml            \
SLOPFILE_API_BASE=http://127.0.0.1:8080           \
SLOPFILE_TRANSCRIPTS=/path/to/transcripts         \
SLOPFILE_WORKDIR=/path/to/workspace               \
SLOPFILE_ABORT_RESTART=1                          \
slopfile
```

### nvim plugin

Load the plugin once per nvim session, then open a session `.md`:

```vim
:source ~/things/clanker/slopfile/nvim/plugin/slopfile.lua
```

(Or add `~/things/clanker/slopfile/nvim/plugin` to your `runtimepath` instead,
in which case it loads automatically.) If the `slopfile` binary isn't on
`$PATH`, point the plugin at it first:

```vim
let g:slopfile_bin = '~/things/clanker/slopfile/target/release/slopfile'
```

A 1s timer reloads the buffer while the
daemon streams; the view follows the stream when your cursor is on the last
line. Save before expecting updates — `:checktime` refuses to clobber unsaved
text.

| Command   | Action |
|-----------|--------|
| `:Init`   | save an empty buffer; the daemon formats it as a session |
| `:Send`   | save the buffer + trigger a sweep (tail → `### You`, start/resume) |
| `:Resume` | force a re-parse of dangling output / pending question block |
| `:Abort`  | append the abort marker; daemon stops streaming, closes `@done` |
| `:Status` | re-render the `<!-- slopfile:live ... -->` status line |
| `:Tokens` | run `slopfile tokens <buffer>` and show the token footprint |

The plugin finds the binary via `g:slopfile_bin` (default `slopfile`). CLI
equivalents: `slopfile abort <path>` (interrupt a session), `slopfile init
<path>` (create a template), `slopfile tokens <path>` (tokenize a session).

---

## Run 2 — Docker (`llama-server` + daemon in containers)

The compose stack in `docker/` runs the model server and the slopfile
daemon as containers. The `pi` service is a legacy scratch container and is
not needed for slopfile.

### Requirements

- NVIDIA GPU + `nvidia-container-toolkit` installed (llama-server asks for the
  GPU device).
- Host docker daemon, with the docker socket reachable by the `slopfile`
  container, *only* if you want abort → `docker restart clanker-llama`
  (`abort_restart_server = true`). The socket is mounted with the host docker
  group's gid via `DOCKER_GID`.

### Config the container overrides

The compose `slopfile` service sets these env vars (overriding the config
paths below so they work inside the compose network):

| Env                      | Value                    | Meaning |
|--------------------------|--------------------------|---------|
| `SLOPFILE_CONFIG`        | `/shared/slopfile.toml`  | app config, by the container path |
| `SLOPFILE_API_BASE`      | `http://llama-server:8080` | talks to the `llama-server` service by name |
| `SLOPFILE_TRANSCRIPTS`   | `/shared/transcripts`    | session files, on the shared volume |
| `SLOPFILE_WORKDIR`       | `/workspace`    | where tool calls run/edit |
```
Then `workdir` in the compose env matches the `workspace/` bind mount.

`shared/` is mounted read-write in both containers; `models/` is read-only in
`llama-server`.

### Start

```bash
cd slopfile           # repo root
./start.sh            # boot the stack + open nvim (plugin loaded) on the latest session
```

`start.sh` is idempotent — it errors out if the docker daemon is down, uses
`docker compose up -d` (harmless if containers are already running, and waits
for llama-server to be healthy), builds the host `slopfile` binary in release
mode and deploys it into the container (`docker/run.sh deploy`), then opens
nvim with the plugin loaded on the most recent transcript (creating one via
`slopfile init` if none exist).

Or manually:

```bash
cd slopfile/docker
./run.sh up           # build slopfile, pull llama-server, start both
./run.sh logs slopfile   # follow the daemon logs
./run.sh down            # stop
```

`./run.sh up` loads `DOCKER_GID` automatically so the `slopfile` container can
`docker restart clanker-llama` on abort. Then create a transcript and use it
exactly as in the host flow (edit `shared/transcripts/*.md` from the host, or
open it in nvim on the host — it's the same file on the shared volume):

```bash
slopfile init "$HOME/things/clanker/slopfile/shared/transcripts/t1.md"   # from the host
nvim "$HOME/things/clanker/slopfile/shared/transcripts/t1.md"            # edit, :Send
```

### Model weights

`llama-server` runs in **router mode** (`--models-preset`), so models are
declared in `slopfile/shared/models/presets.ini` instead of a single `-m` flag.
Each `[name]` section advertises one model — drop the GGUF (and optional
mmproj) into `slopfile/shared/models/` and add a section. The preset ships with
one model, `qwen3.8-27b` (Qwen3.8-27B multimodal, Q4_K_XL, 131k context).

The default template writes `model = "qwen3.8-27b"` in the session header;
the daemon routes each request to the model named in that header
(`slopfile.toml`'s `model` is just the fallback when the header omits it).
To use a different model in a session, set its preset-section name in the
file's header ` ```toml model = "..." `` block. The server auto-loads on
demand and the daemon best-effort preloads it before streaming.

---

## App config reference (`slopfile.toml`)

```toml
confirmations = "ask"          # "ask" waits for your answer; "yolo" auto-resolves questions
api_base = "http://127.0.0.1:8080"
workdir = "/home/unitazer/things/clanker"      # tool-call root; edits are restricted to it
transcripts_dir = ".../shared/transcripts"     # session files the daemon watches
sweep_interval_ms = 250                        # poll cadence
model = "qwen3.8-27b"                          # served model name
tool_timeout_ms = 30000                        # per-tool bash timeout
abort_restart_server = true                    # docker restart llama-server on abort
llama_container = "clanker-llama"              # container to restart on abort
```

## Troubleshooting

- **No response / stuck** — the status line shows the current phase
  (`idle`, `waiting_answer`, `streaming`, …). Open the file in nvim and run
  `:Status`, or watch `./run.sh logs slopfile`.
- **"waiting_answer"** — the model asked a question; answer below the `> `
  line and `:Send`, or add ` /skip`.
- **Abort does nothing / can't restart server** — the container can't reach
  the docker socket (gid mismatch or socket not mounted). Either `DOCKER_GID`
  is wrong, or set `abort_restart_server = false`.
- **Empty buffer gets clobbered** — `slopfile init` refuses to touch a
  non-empty file; the nvim plugin never overwrites unsaved text.
- **Daemon ignoring a file** — errors are throttled: a failing session is
  skipped for 5s. Check `logs slopfile`.