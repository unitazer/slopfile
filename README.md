COMEPLETELY vibecoded, this is a throwaway project, if i feel like its worthy and anyone else
expresses some interest in it, i might rewrite it on my own.
the code's definitely questionable at places but if it doesnt work just throw a different clanker at it idgaf


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
into the agent's container, so the model cannot read or edit its own code.

---

## How the file works

One session = one `.md` file that is a **stack of regions**. A region opens with
`<!-- slopfile:NAME -->`, closes with `<!-- /slopfile -->`, and its body is
copied out verbatim — the body is never re-scanned for structure, so nothing a
model writes can turn into structure.

```markdown
```toml
model = "qwen3.8-27b"
thinking = "auto"
tools = ["read", "write", "edit", "bash", "grep", "find", "ls"]
```
<!-- slopfile:begin -->
<!-- slopfile:system -->
You are an expert software engineering assistant…     <- the system prompt
<!-- /slopfile -->
<!-- slopfile:turn -->                                 <- one user turn
<!-- slopfile:user -->
a user turn
<!-- /slopfile -->
<!-- slopfile:assistant -->
<!-- slopfile:reasoning -->
streamed thinking
<!-- /slopfile -->
<!-- slopfile:text -->
streamed reply text
<!-- /slopfile -->
<!-- slopfile:call {"id":"c1","name":"bash","arguments":"{\"command\":\"ls\"}"} -->
<!-- /slopfile -->
<!-- /slopfile -->                                     <- assistant done
<!-- slopfile:result {"id":"c1","name":"bash","ok":true,"ms":9} -->
1: src
<!-- /slopfile -->
<!-- /slopfile -->                                     <- turn finished
<!-- slopfile:input -->
type your next request here                          <- everything below is yours
```

- **The fence** — the first ` ```toml ` block. Per-session settings (model,
  tools, limits); overrides the app config. Human lines around it are kept.
- **`system`** — the system prompt. Required; the daemon refuses edits that drop it.
- **`turn`** — one exchange. It is **finished exactly when its region is closed**,
  which is how the daemon knows a turn ended: no `@done` marker, no `### Assistant`
  headings, no mode tracking.
- **`note`** — free text with an author: your own scratchpad, or
  `<!-- slopfile:note {"by":"harness"} -->` for harness messages (for example
  "you hit the token cap, go prune").
- **`question`** — a `{"call":…,"questions":[…]}` header. The human-facing table
  is rendered into the `input` region below.
- **`input`** — the only opaque region: it runs to the end of the file, so
  whatever you type there can never become structure.

Anything the daemon writes into a body is escaped: a literal `<` is wrapped as
`<!--slopfile:ESC[<<!--/slopfile:ESC-->`. Both wrapper delimiters start with
`<`, so escaping that one character makes it impossible for content to forge a
marker, even by gluing two fragments together.

**All daemon state lives in the daemon, not in the file.** There is no status
line, no send marker, no abort marker: `armed`, `phase`, `parked`, `solved` and
friends are process memory, reachable over the control channel (`sf status`).
The trade-off is that arming a session does not survive a daemon restart — press
send again.

### Turn flow

1. Type a request below the `input` marker → `sf send` (or `:Send`).
2. The daemon promotes that text to a `user` region, opens an `assistant`
   region, and streams `reasoning` / `text` into it as tokens arrive.
3. Tool calls land as `call` + `result` regions; the daemon loops until the
   model stops asking for tools.
4. On clean completion the `turn` region is closed.
5. `question` → the daemon renders a `QUESTIONS` table into `input` and waits.
   Answer below the `>` line (indices, comma separated, or free text) and
   `sf question <session> <answers…>`; `/skip` takes the defaults. In
   `confirmations = "yolo"` they auto-resolve.

## Run 1 — host (quick, no Docker for the daemon)

You need a `llama-server` (or any OpenAI-compatible endpoint) reachable at the
`api_base` from the config.

```bash
# 1. Build both binaries
cd slopfile
cargo build --release                 # -> target/release/{slopfile,sf}

# 2. (optional) create a session file
sf init t1                             # -> shared/transcripts/t1.md

# 3. start the daemon (watches the transcripts dir, drives every file)
slopfile
```

The daemon also speaks the control protocol, so `slopfile send|stop|abort|status`
work identically to `sf` — handy inside the container, where `sf` is not built.

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
in which case it loads automatically.) The plugin shells out to the `sf` CLI; if
it isn't on `$PATH`, point the plugin at it first:

```vim
let g:sf_bin = '~/things/clanker/slopfile/target/release/sf'
```

A 1s timer reloads the buffer while the
daemon streams; the view follows the stream when your cursor is on the last
line. Save before expecting updates — `:checktime` refuses to clobber unsaved
text.

| Command     | Action |
|-------------|--------|
| `:Init`     | `sf init <session>` for this buffer |
| `:Send`     | save the buffer, then `sf send` — arms and starts/resumes generating |
| `:Resume`   | alias for `:Send` |
| `:Stop`     | `sf stop` — let the current turn finish, then go idle |
| `:Abort`    | `sf abort` — kill the in-flight stream now |
| `:Status`   | `sf status` — one line of live daemon state |
| `:Question` | prompt for answers, then `sf question` |
| `:Tokens`   | `sf tokens` — token/budget report |

The plugin finds the CLI via `g:sf_bin` (or the older `g:slopfile_bin`),
defaulting to `sf` on `$PATH`.

### The `sf` CLI

```bash
sf init <session>                      # create a session file (local)
sf send <session>                      # arm + start generating
sf stop <session>                      # finish the current turn, then idle
sf abort <session>                     # kill the in-flight stream now
sf status <session>                    # phase/running/armed/parked/solved/…
sf question <session> <answers…>       # one argument per pending question
sf tokens <session>                    # token footprint vs the context budget
```

`<session>` is a bare name (`1`, `1.md`) resolved inside the daemon's
transcripts dir; path separators are refused. `--addr` (or `SF_ADDR`, default
`127.0.0.1:8787`) points at the daemon, `--transcripts` (or
`SLOPFILE_TRANSCRIPTS`) at the local session dir used by `init` and `tokens`.

The wire is newline-delimited JSON, one request per TCP connection, on
`127.0.0.1:8787` inside the container's compose network — see the
`ports:`/`SF_ADDR:` entries in `docker/docker-compose.yml`.

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
for llama-server to be healthy), builds the `slopfile` binary in release mode
inside a throwaway `rust:1-slim` container (no host toolchain needed, so it
works on NixOS) and deploys it into the running container
(`docker/run.sh deploy`), then opens nvim with the plugin loaded on the most
recent transcript (creating one via `slopfile init` if none exist). The nvim
plugin also needs a host-side binary (`target/release/slopfile`); `start.sh`
builds one via `cargo` or `nix develop -c cargo` when it's missing.

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

## The model owns its own context

The session file *is* the model's context — it is parsed back into the request on
every call — so the model is told its file's path in the system prompt and may
edit it directly with `read`/`edit`/`write` or with bash. It decides what to drop,
compress or delete, and the edit becomes its context on the next call. There is no
summarizer, no successor transcript, and no compaction step: nothing is ever
rewritten behind the model's back.

- Compress a long turn, delete a dead tool round, keep a running `note` region, or
  wipe whole turns — whatever it leaves in the file is what it gets back.
- Subagents get their own file, so they manage their own context too.
- If the model fills the context mid-response, a harness `note` tells it to prune
  the file itself; if it doesn't, the usual drop-oldest truncation keeps the
  request inside the budget.
- The harness owns the structure and nothing else. An edit that would leave the
  file unparseable (an unknown region, a `<!-- /slopfile -->` with nothing open, a
  corrupt `call`/`result`/`question` header) or that drops the ` ```toml ` fence or
  the `system` region is refused by `edit`/`write` with the reason, and rolled back
  after a bash edit with the reason appended to that tool's result.

## Troubleshooting

- **No response / stuck** — ask the daemon: `sf status <session>` (or `:Status`)
  reports `phase` (`idle`, `generating`, `running_tools`, `waiting_answer`,
  `waiting_subagents`) plus `armed`, `abort`, `parked`, `solved`, `context_full`.
  Watch `./run.sh logs slopfile` for what the daemon is doing.
- **The model never starts** — nothing arms a session on its own. `sf send` (or
  `:Send`) is what sets `armed`, and arming does not survive a daemon restart.
- **`failed to parse session file`** — the daemon refuses to drive a file whose
  regions are broken; the message names the region. Fix it by hand, then send.
- **"waiting_answer"** — the model asked a question; answer below the `> `
  line and `:Send`, or add ` /skip`.
- **Abort does nothing / can't restart server** — the container can't reach
  the docker socket (gid mismatch or socket not mounted). Either `DOCKER_GID`
  is wrong, or set `abort_restart_server = false`.
- **Empty buffer gets clobbered** — `slopfile init` refuses to touch a
  non-empty file; the nvim plugin never overwrites unsaved text.
- **Daemon ignoring a file** — errors are throttled: a failing session is
  skipped for 5s. Check `logs slopfile`.
