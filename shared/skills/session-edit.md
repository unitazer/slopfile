---
name: session-edit
description: >
  read this every time you need to edit your own context.
  Read and change regions of your own session file by name, without typing markers. Use it to
  compact context, keep a ledger or scratchpad, drop dead tool output, or record state for later.
  Prefer it over head/sed/heredoc, which hardcodes line numbers. Triggers: compact the context,
  prune, drop old turns, keep notes, write down, scratchpad, ledger, remember this, shorten the
  transcript, edit my context file.
---

## List first

```bash
python3 /shared/skills/session-edit.py list /shared/transcripts/<session>.md
```

The output is a tree of addressable regions. Every segment has a 0-based index, including the
first: `turn[0]`, `turn[0].assistant[0]`, `turn[0].assistant[0].call[0]`.

```
643 lines, balanced
system[0]
turn[0]
  user[0]
  assistant[0]
    reasoning[0]
    text[0]
    call[0]  bash("cargo test --all")
  result[0]  bash ok 149ms
  note[0]
turn[1]
...
```

Indices count same-named siblings at that level. A bare name is shorthand for `[0]`
(`turn.note` means `turn[0].note`), but prefer the indexed form. A call shows the command or path;
the result shows only its status, plus the first words of the error on failure, so the arguments
are not repeated.

A wildcard applies only to its own segment: `turn[*].assistant[*].call[*]` means every call in
every turn. Put `[*]` on every level you mean to sweep.

A single `drop` can renumber later siblings, so run `list` again before targeting a re-indexed
path. A `batch` resolves every path against the listing from before it, so order inside a batch
does not matter.

## Batch: one call for the whole edit

Run `list` once, send one batch, get back to work. Do not narrate the plan in your reasoning; that
narration becomes context you have to prune again.

A compaction is one heredoc, one parse, one write, and one line of output:

```bash
python3 /shared/skills/session-edit.py batch /shared/transcripts/<session>.md <<'EOF'
drop 'turn[*].assistant[*].call[*]'
drop 'turn[*].result[*]'
set 'turn[*].assistant[*].reasoning' | reran tokenize: 25k -> 6k
add turn[0].note | compaction: dropped dead tool output
EOF
```

Each line is `drop PATH`, `set PATH | BODY`, `append PATH | BODY`, or `add PATH | BODY`. Bodies are
one line; use `set FILE PATH --body -` for a multi-line body. Quotes around a path are stripped.
If any line fails, the whole batch is abandoned and the file is untouched. Do not chain single
commands with `&&`: each one re-parses and rewrites the file.

## Single edits

```bash
python3 /shared/skills/session-edit.py read FILE 'turn[0].note[0]'
python3 /shared/skills/session-edit.py set  FILE 'turn[0].note[0]' --body '...'
python3 /shared/skills/session-edit.py add  FILE 'turn[0].assistant[0].scratchpad' --body 'trying two approaches'
python3 /shared/skills/session-edit.py drop FILE 'turn[0].*'
```

`--body -` reads the body from stdin, which is how you pass something long or generated:

```bash
python3 /shared/skills/session-edit.py set FILE 'turn[0].note[0]' --body - <<'EOF'
## STATE
- 7 repos verified; search still running (pid in ghsearch.log)
- next: curate candidates, then shallow-clone
EOF
```

## Adding your own roles

Any name you invent inside a turn or an assistant is kept and handed back under that name, so you
can keep a scratchpad or a ledger without cramming it into prose. `add` creates the region, and
creates the turn/assistant it needs if they are missing:

```bash
python3 /shared/skills/session-edit.py add FILE 'turn[0].assistant[0].scratchpad' --body 'trying two approaches'
python3 /shared/skills/session-edit.py add FILE 'turn[0].ledger' --body -   # kept verbatim
```

The new leaf has no index: `add` appends it and the listing shows the index it landed on. A batch
`add` needs its parent path to exist already. Use a single `add` for a session that has no turn or
assistant yet.

## Why not rewrite the file

`head -48 f.md > f.new && cat >> f.new <<'EOF' ... EOF && mv` works, and it is how you lose an
afternoon: it hardcodes line numbers, and those numbers move every time the harness regenerates the
fence or the file gains a line. It also makes you retype every region above the cut, which is the
part you were trying to keep.

## What the tool refuses

- Edits that would not parse, or that put a region where it has no meaning. It says why and leaves
  the file alone.
- Removing the ```toml fence at the top.
- Part of a batch: one bad line means none of it is written.

Escaping is handled for you: `<` in a body is wrapped automatically, so a note containing
`<!-- slopfile:note -->` stays text instead of becoming structure. Do not write that wrapper
yourself.

## Compaction

Compaction is a tail operation. Pick a cut at a turn boundary, drop everything after it in one
batch, and write a summary note at the tail of the surviving prefix. The summary must restate the
user's latest request and every open thread, path, PID, and decision, because the cut is about to
delete those bytes. Finished subtasks become a one-line result; verbose reasoning becomes a
one-line summary; dead results and the calls that produced them are removed.

Do this before the window fills. A failed search, a huge directory listing, or a wall of repeated
logs belongs in the bin immediately.

## Why the tail

The harness sends this file as the prompt, and the provider caches the KV of the prefilled prefix.
A byte changed at position P invalidates the cache from P to the end. Deletion is nearly free; only
new bytes cost.

- Cut at the tail. Everything before the cut stays cached for the rest of the session.
- Do not scatter drops across turns. Each mid-file drop makes every later request re-prefill from
  that drop to the end. Collect mid-file removals into one compaction.
- Treat the top as read-only. The toml fence, the system region, and summarized old turns are the
  deepest cached prefix. Never reword them in place; cut before an old turn and summarize it.
- Notes get buried. A note at the tail of turn N is mid-file once turn N+1 lands, and setting it
  re-prefills from there to the end. Write running notes in the current turn; do not update a
  buried one. The permanent layer is the compaction summary at the tail of the surviving prefix.
- Cut where it hurts least. The summary must restate the bytes the next turn needs most.

## Trust the output

If a batch printed `applied N edits`, it wrote the file. Do not read or grep the session file to
verify: that re-injects it as fresh context and defeats the point of dropping it. Record what you
dropped and why in the summary note of the same batch.

Hygiene:
- Keep session-edit's stderr visible. A validation error means it refused to write; hiding it with
  `2>/dev/null` turns a refused edit into a silent no-op.
- `ctx:N` lags one call and only moves on the next result. Judge by trend across calls.
- `note` lives at the turn level. Inside an assistant, use your own role name (`scratchpad`,
  `ledger`). The tool refuses anything it cannot keep parseable and says why.

## Compaction is two stages

Stage 1, the cut: one batch drops the cut turns and adds the summary at the tail of the surviving
prefix. If the summary is multi-line, add it with a separate `--body -` call; batch bodies are one
line.

Stage 2, the cleanup: the compaction's own reasoning (cut-point math, path mapping) lands in the
surviving file as dead weight. Drop it in one small batch right after stage 1, while the cut turn is
still next to the live turn. Keep the user messages, text lines, calls, and one-line results as
provenance.

## What a cut changes

- `drop 'turn[3]'` deletes the whole turn, children included. No `.*` needed.
- Deleting turns renumbers survivors: if `turn[6]` becomes the first survivor, it is `turn[0]` the
  moment the batch lands. Re-list before a follow-up add/set. Inside one batch, old paths resolve
  against the pre-batch tree.
- The summary note goes on the last surviving turn (`turn[0].note` if one survives). If the live
  turn already exists, the add lands just before it and becomes stable cached prefix on the next
  request.
- Distill, do not narrate: put derived cut points, sizes, and path mappings in this skill or the
  summary note, not in the transcript. Re-deriving them next session is the failure this skill
  exists to prevent.
