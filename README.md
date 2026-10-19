COMEPLETELY vibecoded, this is a throwaway project, if i feel like its worthy and anyone else
expresses some interest in it, i might rewrite it on my own.
the code's definitely questionable at places but if it doesnt work just throw a different clanker at it idc

# slopfile

A coding agent that lives in one Markdown file.

Write a request at the bottom of `1.md`. A daemon streams the reply back into
that file: reasoning, text, tool calls, tool results, as the tokens arrive. The
file is the whole UI. Edit it while the model works.

The file is also the model's context. It gets parsed into every request, and the
model is told its own path, so it can `edit`, `write` or `bash` its way through
the file and delete what it no longer needs. Whatever it leaves is what it gets
back. No summarizer sits in the middle.

## Run it

Run the daemon in Docker. Nothing here has any security in it, so the container
is the only thing standing between the agent and your machine.

- The control port has no authentication whatsoever. Anything that can reach it
  can arm a session, start it generating, or abort it.
- `bash` has no path sandbox. `read`, `write` and `edit` do check paths against
  `workdir` (canonicalized, symlinks refused), but `bash` is unrestricted, so
  the agent gets anywhere the daemon's user can get.
- An API key in a fence is readable by the model, since it can `cat` its own
  file.

On the host, keep the control port on loopback and do not publish it. `sf` is a
plain TCP client with no login step, so it reaches the daemon over the compose
network or on localhost without caring which.

Needs a Rust toolchain and something that speaks the OpenAI chat completions API.

```bash
cargo build --release

export SLOPFILE_CONFIG=shared/slopfile.toml
export SLOPFILE_API_BASE=http://127.0.0.1:8080
export SLOPFILE_WORKDIR=/path/to/a/codebase
export SLOPFILE_TRANSCRIPTS=./transcripts

./target/release/sf init 1
./target/release/slopfile &

# write a request at the bottom of the file, then:
./target/release/sf send 1
```

The daemon watches the transcripts directory and waits. Nothing happens until
`sf send` arms a session.

```bash
sf init <session>                # create a session file
sf send <session>                # arm + generate
sf stop <session>                # let the turn finish, then idle
sf abort <session>               # kill the in-flight stream
sf status <session>              # one line of daemon state
sf question <session> <answers…> # answer a question
sf tokens <session>              # token footprint
```

`sf` reaches the daemon over `--addr`, `SF_ADDR`, or the default
`127.0.0.1:8787`.

Docker also works, and needs an NVIDIA GPU:

```bash
./start.sh                  # daemon + llama-server, then nvim on the newest session
docker/run.sh logs slopfile # follow the daemon
docker/run.sh down          # stop
```

`start.sh` wants a running docker daemon and `nvim` on `$PATH`. Skipping it
entirely and pointing at a hosted API is the easier path.

## OpenRouter

In `slopfile.toml`:

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
reasoning_style = "object"
headers = { Authorization = "Bearer ${OPENROUTER_API_KEY}" }
```

Then in the session's fence, the `` ```toml `` block at the top of the transcript:

````markdown
```toml
provider = "openrouter"
model = "dots-studio/dots-3-note-preview:free"
thinking = "high"
tools = ["read", "write", "edit", "bash", "grep", "find", "ls", "question", "subagent"]
```
````

`base_url` can carry `/v1` or not, it normalizes either way. `${VAR}` is
expanded by the daemon from its own environment, so under Docker that means the
container's. Add this to the `slopfile` service:

```yaml
OPENROUTER_API_KEY: ${OPENROUTER_API_KEY:-}
```

The free tier, which covers ids ending in `:free` like the one above, is 50
requests a day and one tool-calling turn spends several. Ten credits buys
1000/day. When you hit it the session parks and a note in the transcript says
so.

An unset or empty `${VAR}` is a hard error rather than a dropped header, since
an empty `Authorization: Bearer ` comes back as a 401 that reads like a wrong key
rather than a missing one.

## Any OpenAI-compatible endpoint

Same shape, any provider with `POST /chat/completions`:

```toml
[providers.openai]
base_url = "https://api.openai.com/v1"
headers = { Authorization = "Bearer ${OPENAI_API_KEY}" }

[providers.mylocal]
base_url = "http://192.168.1.50:8080"
```

A fence naming `provider = "openai"` picks it. Leaving `provider` out falls back
to the app-wide `provider`, then to `api_base`. An unknown name errors out rather
than falling back, since quietly answering on the wrong model costs money.

A provider that is a llama-server wants `llama = true`, which turns back
on the llama.cpp routes (`/models/load`, `/abort`, `/tokenize`) that hosted
endpoints skip.

### Keys

Two places, and they differ in one way that matters.

The environment, via `${VAR}` in `slopfile.toml` as above. The model cannot read
it from any file.

Or in the fence:

```toml
provider = "openrouter"
headers = { Authorization = "Bearer sk-or-v1-..." }
```

Fences are gitignored and go out in no request. The model can still read them
though, since `bash` has no path sandbox, so treat this as convenience rather
than secrecy.

## Config

These only work in `slopfile.toml`:

| key | |
|---|---|
| `api_base` | implicit endpoint, assumed to be a llama-server |
| `workdir` | root the tools stay inside |
| `transcripts_dir` | session files the daemon watches |
| `default_session_model` | model stamped into new sessions |
| `confirmations` | `ask` waits for answers, `yolo` picks the default. Gates nothing else, `bash` runs either way. A misspelling parses as `ask` |
| `abort_restart_server` | restart the llama container on abort |

These also work per session, in the fence:

| key | meaning |
|---|---|
| `provider` | endpoint for that session |
| `model` | model for that session |
| `headers` | sent on top of the provider's |
| `thinking` | reasoning effort |
| `tools` | which tools the model gets |
| `max_context_tokens` | see below |
| `tell_model_context_size` | tell the model how full it is (default true) |
| `burst_max_turns` | turns per send, 0 for no ceiling |

A fence key the daemon does not know is ignored without a word, so a typo there
does nothing rather than erroring.

### max_context_tokens

The one setting slopfile and the model server can quietly disagree about. It is
the window slopfile assumes, so match the server's real one, which for
llama-server is `ctx-size` in `shared/models/presets.ini`. Set it too high and
requests overflow the KV cache and get rejected mid-session with nothing in the
log; too low and slopfile prunes while the server has room.

Up to 40,000 tokens go to the reply, the rest to the conversation. 65,536 leaves
about 25,500.

The model gets told where it stands. From the second tool call on, each result
carries a `ctx:N` tag, and the system prompt explains that the number is a
snapshot rather than a live meter. `tell_model_context_size = false` turns it
off.

### Counting tokens

llama-server has `/tokenize`, so slopfile asks it directly for local models.
Hosted endpoints have nothing like it, so it measures instead: it keeps the bytes
it sent next to the provider's `usage.prompt_tokens` and takes the ratio from
those. No fixed bytes-per-token constant, which would be wrong for most models.

A sample only counts for the endpoint and model that produced it. The first turn
against a new endpoint, or after switching model, has nothing to compare against:
slopfile sends the transcript whole and logs `tokens=?` rather than guessing.
Budget enforcement starts on the turn after.

### thinking

`none`, `low`, `medium`, `high`, `xhigh`, or `auto` to leave it alone. The wire
name differs: OpenAI and llama-server read `reasoning_effort`, OpenRouter reads
a `reasoning` object. `reasoning_style` picks which, per provider.

Reasoning tokens come out of the same allowance as the reply, so a model can
spend it all thinking and return nothing. slopfile spots that case and tells the
model its transcript was fine, since the advice is the opposite.

## File format

One session, one `.md`, built from regions. `<!-- slopfile:NAME -->` opens one,
`<!-- /slopfile -->` closes it, and the body is copied out verbatim. Nothing in
it gets re-read as structure, so the model cannot forge a region by writing
one.

```
system      the system prompt, required
turn        one exchange; finished when its region closes
  assistant reasoning, text, call
  result    paired to its call by id
  note      free text, yours or the harness's
input       everything below, runs to end of file
```

Names you invent inside a `turn` or `assistant` are kept and handed back under
that name, so `scratchpad` works as a scratchpad.

Every `<` in a body is escaped. That is the whole of the forgery defence, since
a marker needs one and content cannot produce an unescaped one. Edits that would
break the structure are refused with a reason, then rolled back.

The nvim plugin in `nvim/plugin/slopfile.lua` adds `:Send`, `:Stop`, `:Abort`,
`:Status`, `:Question` and `:Tokens` over `sf`, and reloads the buffer while the
daemon streams.

`subagent` spawns a worker with its own file, `<parent>.sub-01.md`, and its
report lands back in the parent. Workers get no `question` tool; if one calls it
anyway it gets told to decide for itself.

`shared/skills/*.md` gets indexed into the system prompt by name and
description, and the model reads one when a task matches. A skill it decides not
to read is a skill that does not happen.

## When it breaks

`sf status` first. It reports phase (`idle`, `generating`, `running_tools`,
`waiting_answer`, `waiting_subagents`) plus `armed`, `parked`, `blocked` and
`stopped`.

- Nothing after `sf send`: check status. `blocked` means the provider refused
  outright, and `sf send` clears it. `parked` means five failures in a row, and
  editing the file clears that one.
- `waiting_answer`: answer under the `>` line in the file, or add ` /skip`.
- `failed to parse session file`: regions are broken and the message names the
  region. Fix by hand, then send.
- Provider refused the request: a `[harness]` note in the transcript explains
  it. Read it before retrying.

MIT licensed.
