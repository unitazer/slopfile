# transcript module contract (for subagents)

Read `PLAN.md` first. This file nails down the exact file format and the
public API of `src/transcript.rs` and `src/questions.rs`.

## File layout (one session = one .md)

```
<!-- slopfile:session -->          (optional wrapper comment, ignore)
```toml
model = "qwen3.8-27b"
thinking = "auto"
tool_timeout_ms = 30000
tools = ["read","write","edit","bash","grep","find","ls","question"]
```
                                    (first ```toml fence in the whole file = header)
# Session title (any markdown, ignored)

(the literal first text after the header = SYSTEM PROMPT, until the first ###)

### You
user message text, can contain `@relative/path/img.png` lines

### Assistant
streamed markdown text
<!-- thinking -->
reasoning text (model thinking channel)
<!-- /thinking -->
<!-- tool:bash {"cmd":"ls -la"} -->
```bash output or raw text... ```
<!-- result:bash ok=142ms -->
more text
<!-- @done -->                     (only on successful completion of turn)
```

Tail = everything below the last `<!-- @done -->` line. Contains user draft
text, possible `❓ QUESTIONS` blocks, and the status line comment at the very
bottom.

## Rules for the parser (transcript.rs)

1. **Header**: scan lines; the FIRST line matching ```` ```toml ```` (and its
   closing ```` ``` ```` fence) is the header block. Parse to
   `types::SessionConfig`. If the fence appears anywhere later (model output),
   it's NOT parsed. If no header, defaults.
2. **System prompt**: everything between the end of header and the first line
   starting with `### ` (trimmed, non-empty). It is the FIRST message.
3. **Turns**: sections starting at `### You` / `### Assistant`. Track thinking
   blocks (`<!-- thinking -->` … `<!-- /thinking -->`) and tool blocks.
4. **Tool block format** — exact:
   ```
   <!-- tool:NAME ARGS_JSON -->
   <raw tool output text (may be multi-line, fenced or not)>
   <!-- result:NAME STATUS -->
   ```
   where `ARGS_JSON` is compact JSON, `NAME` matches the tool name,
   `STATUS` is `ok=123ms` or `err=123ms`. Parser reconstructs:
   - assistant ChatMessage with `tool_calls: [{id: <stable hash of
     name+args>, name, arguments}]`
   - tool ChatMessage `tool(tool_call_id, name, output_text)`
5. **Dangling tail detection**: if the file ends while inside an Assistant
   section (i.e. a `### Assistant` whose last content has no following
   `### You` AND no `<!-- @done -->`), then the last assistant turn is
   incomplete: return its location so the agent can drop it and re-ask. The
   dangling section runs to the first daemon status line (or EOF): the live
   line is daemon-owned and must never leak into the resume hint.
6. **@done only** marks clean completion — the agent appends it after the
   final assistant block of a turn completes, and appends nothing after it
   until the next user turn.
7. **Status line**: the daemon owns the last line, `<!-- slopfile:live ... -->`
   serialized JSON in the comment. The parser must EXCLUDE it from history.
   **History end**: history stops right after the last `<!-- @done -->` line.
   Everything below it — drafts, in-flight turns, tool rounds, the status
   line — is the TAIL, appended ONCE to the request by the tail reader, so a
   status line below `@done` must never pull the tail region into `history`
   (that used to double every promoted draft / tool round in the prompt).
   A status line with no `@done` at all only caps history in the fragmented
   bootstrap case.
8. **Abort marker**: `<!-- slopfile:abort -->` is a standalone abort, or may
   be glued directly after the status line
   (`<!-- slopfile:live {...} --><!-- slopfile:abort -->`). The status-line
   prefix requires a trailing space (`<!-- slopfile:live `), so a user/agent
   line that merely *mentions* the abort marker inside flowing text can never
   trigger an abort (and a container restart). A live-prefixed line whose
   payload is itself any `X -->` terminates with the abort glue — this is
   still recognized because the garbled payload is daemon-owned.
9. **Immutability**: parsing must never modify the file. The repo API exposes
   a `Transcript` struct plus methods the agent uses to rewrite the file
   incrementally (append-only below its own pointer).

## Public API (transcript.rs)

```rust
pub struct ParsedFile {
    pub config: SessionConfig,           // header toml (or default)
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,       // includes images resolved later
    pub tail: String,                    // raw text below last @done
    pub pending_questions: Vec<PendingQuestions>, // from QUESTIONS blocks
    pub dangling: Option<DanglingTail>,  // incomplete assistant turn
}

pub struct DanglingTail {
    // byte offset of the END of the dangling `### Assistant` heading line:
    // the file resumes streaming right after it (the heading itself stays
    // put; the partial text below it is dropped and re-asked)
    pub resume_offset: usize,
    // the partial assistant text below the heading, EXCLUDING any daemon
    // status line that cuts the section short
    pub partial_text: String,
}

pub fn parse_file(text: &str) -> ParsedFile;
pub fn header_config(text: &str) -> SessionConfig;
pub fn find_done_marker(text: &str) -> Option<usize>; // last @done index
pub fn is_dangling(text: &str) -> bool;
```

The agent calls `parse_file` each sweep to see if tail changed (new expert
text after the last @done / after dangling resume point).

## QUESTIONS block (questions.rs)

When the model calls the `question` tool, the daemon renders this into the TAIL:

```
❓ QUESTIONS
| # | Question | Options |
| 1 | red or blue | red, blue |
| 2 | how long? | 3, 5, 10 |
ANSWER: (numbers or text, comma separated)
>  (user types below)
```

- The original tool-call JSON is stashed in a comment inside the block:
  `<!-- q:1 {"question":"red or blue","answers":["red","blue"]} -->`
- User answers are parsed from the tail: each line after `>` with
  comma-separated tokens. A token that is a number `k` (1-based) selects
  `answers[k-1]`; anything else is used verbatim (trimmed).
- Result = `ToolResult` with JSON `{"desc": "...", "answers": [...]}` for the
  `question` tool id; the QUESTIONS block then disappears and becomes a normal
  tool result message in model context.

### questions.rs API

```rust
pub struct Question { pub id: u32, pub question: String, pub answers: Vec<String>, pub default: Option<String> }

pub struct QuestionsBlock { pub questions: Vec<Question>, pub raw_json: Vec<String> /* per-question stash */ }

pub fn render_block(qs: &[Question]) -> String;      // markdown table + ANSWER + >
pub fn parse_block(text: &str) -> Option<QuestionsBlock>; // find ❓ QUESTIONS…
pub fn parse_answers(block: &QuestionsBlock, tail_after_below_marker: &str) -> Result<Vec<(u32,String)>, String>;
```

## Testing expectations

- `cargo test` must pass for the module. Add unit tests covering:
  - header only parsed when first fence
  - system prompt extraction
  - tool block round-trip (render → parse → same ChatMessages)
  - dangling detection
  - questions: render + parse + index/text answers + missing answer error
- Do NOT touch `src/main.rs`, `src/types.rs`, `src/api.rs`, `src/tools.rs`,
  `src/sse.rs`, `src/agent.rs`.
- Keep the code clean: `cargo clippy`-clean, no unsafe, respect file conventions.