# slopfile — a file that *is* the app

> Plan / design doc. Update this file whenever a decision changes so a fresh
> agent (or a context-resuming one) can pick up exactly where the last one
> stopped. DO NOT delete sections; append changes at the end.

## Status / Progress

- [x] Decisions locked (see below)
- [x] `cargo init` + git init in `slopfile/`
- [x] Deps added (tokio full, serde derive, serde_json, toml, tracing, tracing-subscriber, clap derive, reqwest no-default + json,stream,rustls, futures-util)
- [x] Scaffold transcript.rs (3-region parser, first-```toml-fence-only config)
- [x] Spike llama-server: reasoning_content / tool_calls / image_url parts (curl)
- [x] Headless turn loop + dangling-tail resume (live smoke-tested: draft → promote → stream → tool → @done)
- [x] 7 tools + question tool round-trip + QUESTIONS rendering
- [x] QUESTION block answer resolution (ask mode) end-to-end: block rendered → user answered `1,2,2` → resolved to tool block → daemon continued → final answer → @done
- [ ] yolo confirmations path live test
- [ ] nvim plugin `:Send` `:Resume` `:Abort` + autoread live reload
- [ ] compose `slopfile` service + host run
- [ ] Polish: abort, truncation, yolo, /skip

## Notes from live testing (2026-08-18)

- llama-server REQUIRES the nested OpenAI tool-call shape on the wire:
  `{"id", "type":"function", "function":{"name","arguments"}}` — a flat
  `{id,name,arguments}` is rejected with 500. ToolCall now has custom
  (De)serialize for this; `type_` always "function".
- Streamed assistant output lives BELOW the last `@done`; after a tool round
  the file is re-parsed and the below-`@done` sections are re-appended to the
  LLM context via `transcript::tail_sections` (scan_lines extracted for reuse).
- Dangling detection refined: an assistant section that closes all its tool
  blocks (`<!-- result:` for each `<!-- tool:`) is NOT dangling — the daemon
  continues the turn without cutting it. Only unclosed tool blocks or plain
  partial text are cut on resume.
- `app` config file lives at `docker-pi/shared/slopfile.toml` (host path
  `[app]` section optional; flat fields now: confirmations, api_base, workdir,
  transcripts_dir, sweep_interval_ms, model, tool_timeout_ms,
  max_context_tokens).
- Error throttle: a session that errors is skipped for 5 s (was hammering the
  API every 250 ms on a bad file).
- The daemon status line (`<!-- slopfile:live ... -->`) is daemon-owned and
  MUST NOT enter LLM history. It did (last line parsed as trailing Assistant
  after a closed tool round → has_work() false → daemon silently stalled after
  a QUESTION block was resolved). `parse_file()` now truncates history before
  the status line; regression test
  `status_line_after_closed_tool_block_is_not_history`.

## What we are building

No TUI binary. The user edits one `.md` file in nvim; a Rust daemon in the
container watches it, talks to llama-server (OpenAI-compatible API), streams
deltas back into the file. The file is the entire UI.

- Project name: **slopfile** (NOT pi-anything; user explicitly forbade the
  Pi-prefix for commands/features: no `:PiSend`, `:PiResume`, `:PiAbort`).
- One `.md` per session = the whole app. No DB, no sqlite, no index.
- No undo system (docker mount makes it pointless).
- Borrow conventions from opencode (the harness the user is comfortable with):
  LLMs overfit to harnesses; keep the transcript shaped like what they know.

## File format (one session = one .md)

Three regions, markers are HTML comments; everything human readable.

```
┌─ HEADER: first ```toml fence in the entire file.
   settings for the session (model, thinking, tool_timeout_ms, tools=[...]).
   Parsed ONLY if it is the FIRST fenced ```toml block in the file, so a model
   writing ```toml later does not get parsed as config. STRIPPED from LLM
   context (excluded from the middle block).
├─ MIDDLE: what the LLM sees, verbatim and in order:
   - system prompt = the literal first text of the middle region
   - `### You` user turns
   - `### Assistant` streamed output, tool calls inline, `⋯` thinking blocks
   - `<!-- @done -->` end-of-turn anchor written on clean completion
├─ TAIL: everything below the last `@done`:
   - user drafts (typed below the history)
   - ❓ QUESTIONS blocks (pending model questions)
   - `<!-- slopfile:live ... -->` status line (daemon-rewritten ~1 Hz)
```

LLM request context = middle region ONLY (system + history). Header / tail /
comments / status / stashed tool JSON are stripped.

Alt layout reminder (user's wording): "header with ```toml config here, a
middle section with the llm reasoning and history, and a tail section where i
type. so that i could edit the entire thing from 1 file. the llm only gets the
middle block with its system prompt (which should be the starting text in the
middle block)."

## Turn flow

1. User types in tail → `:Send` (nvim, no Pi prefix) → daemon moves tail text
   into history as `### You`.
2. Daemon POSTs /v1/chat/completions with stream=true, messages built from
   middle region, tools = 7 file tools + `question`.
3. Deltas appended live to the file (`### Assistant`), token-by-token (or
   chunk-by-chunk if flushing is a problem — as fresh as possible, never laggy;
   append-only, no full-file rewrite).
4. Tool calls execute with pi-like semantics; results written as marked blocks.
5. `question` tool call → daemon stops, renders QUESTIONS table in tail,
   phase=waiting_answer, stashes original tool-call JSON as comment inside the
   block (state recoverable from file alone, no sidecar).
6. User answers below `>` with `2` (index) or free text, comma-separated.
   On next `:Send`: index → items[parse], else raw string. QUESTION block then
   disappears; model sees a normal tool result. If unresolved on resume → error
   note in file + refuse (or `/skip` → defaults/empty).
7. yolo mode (app settings): question auto-answered with default / first, never
   blocks.
8. If a QUESTION block is pending and user sends new text: answers win the
   turn (parse tail; remaining text becomes the new user message).

## Resume without "please continue"

- Dangling tail: last `### Assistant` has no `@done` → on daemon wake, rebuild
  request from completed history, drop partial tail, stream a fresh response.
- Pending QUESTIONS → re-answer on next send or `/skip`.
- Daemon NEVER touches text above its own stream pointer; never deletes user
  text. Insertion of assistant content while user edits tail: keep append-only
  semantics, re-anchor status line to bottom.

## App settings — `docker-pi/shared/slopfile.toml` (separate file)

```toml
[app]
confirmations = "ask"        # ask | yolo  (user: "should confirmations even happen or just yolo it")
api_base = "http://llama-server:8080"
workdir = "/home/unitazer/things/clanker"
transcripts_dir = "/shared/transcripts"
sweep_interval_ms = 250
```

Header (file) settings override app settings per-session. Model params in the
header toml; app behavior params in slopfile.toml.

## Question tool schema (model-visible)

```json
{
  "name": "question",
  "description": "Ask the user multiple questions. Blocks until answered.",
  "parameters": {
    "type": "object",
    "properties": {
      "desc": { "type": "string" },
      "data": {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "question": { "type": "string" },
            "answers": { "type": "array", "items": { "type": "string" } },
            "default": { "type": "string" }
          },
          "required": ["question", "answers"]
        }
      }
    },
    "required": ["desc", "data"]
  }
}
```

User's sketch of the experience (verbatim-ish):

> the llm does {"tool":"question","desc":"...","data":[{"question":"red or blue","answers":["red","blue"]}]}
> and what i see in the file is:
> QUESTIONS: <markdown table>
> ANSWER: (type below)
> > and then i type 1,2,3 (indices) or random string comma separated
> and then the entire thing disappears from the file and it just becomes a
> standard tool call thing for what the model sees, and if i resume while this
> special formatted block still exists without giving an answer, either the
> tool has special handling for it, or it doesnt let me resume and prints an
> error into the file

## Tools (v1)

read, write, edit, bash (timeout), grep, find, ls — mirror pi's semantics.
Config-gated via header tools list. Plus the special `question` tool.

## Images

`@relative/path/img.png` in the user's current turn → resolved against
workdir → base64 data-URL image_url content part (llama-server OpenAI API).
Failure → error note block in the file.

## opencode/pi conventions to copy (LLM familiar-shape)

- OpenAI message shape throughout (role/content/tool_calls/reasoning_content).
- Slash-style commands in tail (`/skip`, etc.).
- `AGENTS.md`-like guidance as system text start (the "starting text in the
  middle block" is the user's AGENTS). Config file with model+provider (like
  opencode.json: model, provider.baseURL, provider.models[].name).
- Status line answers "is it even working": provider rtt, phase, tok/s, in/out,
  active tool (user: "i want special stuff like provider status (is it even
  working?), tokens in the file, etc.").
- No undo (skip it, docker).

## Execution order

1. Scaffold transcript.rs (3-region parser + markers + status block)
2. Spike llama-server (reasoning_content, tool_calls, image parts)
3. Headless turn loop: send tail → stream → `@done`, dangling-tail resume
4. 7 tools + question round-trip + QUESTIONS rendering
5. nvim plugin `:Send` `:Resume` `:Abort`, autoread/CursorHold live reload
6. compose `slopfile` service, host run wiring
7. Polish: abort, truncation, yolo, /skip

## Env facts (verify before assuming)

- llama-server (container `clanker-llama`) at `127.0.0.1:8080` on host,
  `http://llama-server:8080` in compose network. Model alias `qwen3.8-27b`.
  Serves OpenAI-compatible `/v1/chat/completions`, `/v1/models`.
- The host also has rustc/cargo 1.96; python3 3.14 (no httpx/textual).
- pi (pi_agent_rust) has RPC mode & its own session JSONL — NOT used by
  slopfile; slopfile talks to llama-server directly.
- Host opencode config shows llama-local provider = same server + qwen3.8-27b
  plus cloud fallbacks (nullsink) — the model naming convention to mimic.
- The daemon runs in a container sharing `docker-pi/shared/` (transcripts) and
  `/home/unitazer/things/clanker` (workdir) so it can edit the source.

## Commands / UX glossary

- `:Send`   — move tail into history, start/resume generation (vim cmd via plugin)
- `:Resume` — force resume of dangling tail / pending question parsing
- `:Abort`  — stop current generation (daemon cleans up, leaves @done)
- `:Status` — re-render/refresh the status line
- `/skip`   — in tail, answer pending questions with defaults/empty
- No Pi prefix anywhere in user-facing commands or file markers
  (file markers use `slopfile:` prefix, e.g. `<!-- slopfile:live ... -->`)

## Constraints / anti-features

- No DB, no sqlite, no index, no undo.
- Append-only daemon writes below its stream pointer; user ownership above.
- Never delete user text; if a QUESTION block is removed it first must be
  converted to a normal tool result in model context.
- NO FILE DELETION without permission (AGENTS.md rule from pi repo applies.)