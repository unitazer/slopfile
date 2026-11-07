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

Never `tail`, `head`, `sed`, `grep` or `cat` the transcript: re-reading it puts the same bytes back
in your context. `list`, `read` and `check` are the only exceptions.

## Rule 1: every `list` is a debt the next batch pays

A listing is not reference material you keep. It is a working set that is wrong the instant you
edit: sizes shrink, siblings renumber, and a stale listing sends you on long reasoning chains about
regions that no longer exist.

So the rule is mechanical: **the batch that follows a `list` must drop that listing, and every
earlier listing still in the window.** A compaction that removes output but leaves listings behind
has not finished.

```bash
python3 /shared/skills/session-edit.py batch FILE <<'EOF'
drop 'turn[3].round[1]'
set 'self.reasoning' | pruned turn[3]; state in the note
droplists
EOF
```

`droplists` is the whole cleanup in one line: it finds every round that ran `session-edit.py list`
and drops the round and its listing. Use it instead of hunting for indices.

Every `list` prints, at the bottom, the exact drop lines for the listings it can see. For the round
currently running the list it synthesizes the lines even though that call is not in the file yet:

```
# every list round is dead once you edit: drop it in the next batch
# this listing is running inside the live round: turn[12].assistant[18]
#   drop 'turn[12].assistant[18]'
#   drop 'turn[12].result[31]'   (next result in the turn; exact for a one-call round)
#   drop 'turn[11].assistant[4]'   (an older listing)
#   drop 'turn[11].result[9]'   (that listing)
# ...or the whole job in one line: droplists
```

You cannot delete a stale listing without running `list`. That is fine: the new listing pays for
the old one. List once, and in the batch it plans drop the old list round *and* this one.

- **Never run `list` to check that an edit worked.** The receipt already printed the resolved paths
  and the new size; that is the verification. A verification listing costs more context than the
  edit saved and leaves another debt.
- **Run `list` bare.** `list | head -20` is still a listing, and piping is exactly what hides the
  cleanup hint and the `← live (self)` marker. If the tree is too big to read, use
  `list --turns 6` (older turns are folded) rather than a pipe.
- **Do not hand-drop the listing round with `self`.** The round running `list` is `self` now, but
  its call is not in the file, and results are turn-level. Use the `drop 'turn[N].assistant[M]'`
  and `drop 'turn[N].result[K]'` lines the listing printed, or just `droplists`.
- If you also want to drop the round that ran a write between the listing and the batch, use
  `drop 'prev'`: that is the last settled round before the live one, and it removes the assistant
  and its results together.
- A batch that dropped output but left a listing prints `warning: N list round(s) still in
  context`. That warning is the point: read it, then run `droplists`.

## List first

```bash
python3 /shared/skills/session-edit.py list /shared/transcripts/<session>.md
```

```
3590 lines, 74,210 content chars; live: turn[2]
system[0]  1,204c
turn[1]  4,112c
  user[0]  196c
  assistant[0]  2,930c
    reasoning[0]  2,714c
    call[0]  158c  bash("cargo test --all")
  note[0]  745c
  result[0]  241c  bash ok 149ms
turn[2]  512c
  user[0]  88c
  assistant[0]  301c
    reasoning[0]  250c
    call[0]  51c  bash("cargo test --all")
  result[0]  241c  bash ok 149ms
  assistant[1]  250c  ← live (self)
    reasoning[0]  250c
```

The round marked `← live (self)` is the one running your command. Its `call` is **not in the
file yet**: the harness writes calls when the round settles, after the tool returns. So `self` has
`reasoning`/`text` but no `call` and no `result` while a tool runs. `turn[2].assistant[0]` above is
a previous round: it has its call and result because it already settled.

The counts are exact content characters, not tokens. Every segment has a 0-based index, including
the first: `turn[0].assistant[0].call[0]`. A bare name means `[0]`; a wildcard applies only to its
own segment, so `turn[*].assistant[*].call[*]` is every call in every turn.

Sizes shrink after an edit, so one `drop` can renumber later siblings. Inside one `batch`, every
path means what it meant in the listing before it.

## Rounds, self, prev, ranges

A **round** is one assistant generation: one `assistant` block plus the `result` blocks for its
calls. Results are siblings of the assistant in the turn, never children. Every tool call you make
is a new round, so `self` moves forward on every call even when no user message arrives.

- `self` is the round this command runs in: the last assistant of the last turn. It is not "the
  block you last saw". If a user message arrived after your listing, `self` is a newer, smaller
  round.
- **`self` has only `reasoning` and `text` while your tool runs.** The call running this command is
  written when the round settles, after the tool returns, so `self.call[*]` never matches. Results
  are children of the turn, so `self.result[*]` never matches either. Use `prev`, `round[M]`, or
  `droplists` instead.
- `prev` is the last settled round before the live one: `prev` = `turn[-1].round[-2]`. Use it for
  the round that ran your previous tool call (the write you want to drop, for example).
- `round[M]` expands to one assistant block **and its results**, so `drop 'turn[0].round[2..23]'`
  drops both halves of every round in the range. `round` must be the last segment. Use it instead
  of computing `assistant[a..b]` and `result[c..d]` separately.
- Old blocks do not move. A user message appends a turn; it does not renumber the old ones, so
  `turn[2]` from your listing is still `turn[2]`. Target old blocks by turn:
  `set 'turn[2].assistant[*].reasoning' | ...`.
- `turn[-1]` and `turn[last]` mean "now", and so does an open range like `turn[8..]`. For a cut
  from a listing use an explicit end (`turn[8..11]`); an open range would include the live turn
  and be refused.
- `[a..b]` is inclusive; `[last]` and `[-1]` pick the last sibling (`result[-1]`, `round[-2]`).

If the session runs without thinking there is no reasoning block; summarize `self.text` instead.

If a receipt shows `self` is smaller than you expected, an interruption moved it. The old block is
still at the turn you listed; target it by turn.

Removing your own reasoning about the session-edit plan is generally a good idea: the batch's own
receipt is the record.

## While your tool runs

`live: turn[N]` is normal, not a half-written turn. Generation has ended; the reasoning is closed
and editable, and only the current *assistant* is left open so the harness can attach this call's
result to it.

**The live round's call and result are not in the file yet.** The harness writes them when the round
settles, after the tool returns. That is why `self.call[*]` and `self.result[*]` cannot resolve:
there is nothing there to match. A `list` prints a synthesized cleanup hint for the live round
anyway, so copy the `turn[N].assistant[M]` / `turn[N].result[K]` lines it gives you, or use
`droplists`.

`streaming:` means the file is being written right now. `list`, `read` and `check` still work;
every write is refused until generation ends.

Never drop the live round or the live turn: this call's result is attached to it, and dropping it
makes the harness overwrite the previous round instead. Summarize `self` in place.

## Notes are checkpoints

A note records what you know at one point: what you tried, what failed, what is still open. It sits
where you wrote it — after the call that wrote it, before that call's result — and later rounds are
appended after it, so it stays at the cut instead of drifting to the end of the turn. Work after a
note in the same turn is provisional: you can drop it and continue from the note.

- Compact: drop the dead output and add one note at the cut. The note is memory, not your reply;
  do not paste its body into `text` or `reasoning` as well.
- Dead end: find the note that marks the last good point, drop the failed rounds/results/notes
  after it, and write one note saying what failed and where to go next. Notes are small, so
  prefer dropping the old checkpoint and writing a fresh one over carefully amending it.
- Editing a note in place has one use: extending a checkpoint you just rolled back to. Never edit
  an old note to "update memory"; add a new note and drop the old one in the same batch.
- Keep every `user` message.

To roll back to `turn[2].note[0]` when the live round is `assistant[4]`, take the indices from
`list` and stop before the live round:

```bash
python3 /shared/skills/session-edit.py batch FILE <<'EOF'
drop 'turn[2].assistant[2..3]'
drop 'turn[2].result[1..]'
drop 'turn[2].note[1..]'
drop 'turn[2].note[0]'
add 'turn[-1].note' <<'END'
## Checkpoint
- tried X: no results
- tried Y: no results
- continue in the direction of Z
END
EOF
```

The note you add goes after the live assistant, so the live call's result still follows it and the
loop does not mistake the note for your reply.

## Batch: one call for the whole edit

Run `list` once, then one batch. Do not narrate the plan in your reasoning: that narration becomes
context you have to prune again.

```bash
python3 /shared/skills/session-edit.py batch /shared/transcripts/<session>.md <<'EOF'
drop 'turn[1].assistant[*].call[*]'
drop 'turn[1].result[*]'
set 'turn[1].assistant[*].reasoning' | searched/fetched benchmarks
set 'self.reasoning' | pruned turn[1]; findings in the note
add 'turn[-1].note' <<'END'
## Findings
- ...
END
droplists
EOF
```

Drop the calls with the results; a call left without a result becomes a "not executed" stub in the
next request. Use `drop 'turn[N].round[M]'` when you want to remove the whole round (assistant and
results); use the separate call/result drops when you want to keep the assistant's reasoning and
only remove its output. If you wrote a file to preserve state during compaction, drop that write
round too (`drop 'prev'`), because the file body is in the transcript as the call's arguments.
The note's own call would create another reasoning block to clean up, which is why the summary and
the cleanup are one batch. The note body is multi-line; the outer shell heredoc must use a
different tag (`EOF`).

Each line is `drop PATH`, `droplists`, `set PATH | BODY`, `append PATH | BODY`, `add PATH | BODY`,
or `add PATH <<'TAG'`. Quotes around a path are stripped; `drop` and `droplists` take no body. If
any line fails, the whole batch is abandoned and the file is untouched. Do not chain single
commands with `&&`. Nested heredocs inside a batch do not work: for a short body use `op PATH |
body`, or write the body to a temp file and pass it with `--body -`.

Use ranges (`2..7`, inclusive) instead of repeating the same operation.

To cut a tail, drop the old turns with an explicit end and summarize the live turn in the same
batch. If the live turn is `turn[12]`:

```bash
python3 /shared/skills/session-edit.py batch FILE <<'EOF'
drop 'turn[8..11]'
set 'self.reasoning' | compacted turns 8-11; summary in the note
add 'turn[-1].note' <<'END'
## Summary
- latest request: ...
- open threads / paths / pids / decisions: ...
END
EOF
```

Keep every user message unless the user says to drop it.

The note is memory, not your reply. Do not paste its body into `text` or `reasoning` as well, and
do not treat the batch result as the end of the turn: keep working with the next tool call. The
note is written before this call's result, so later rounds are appended after it; you do not need
to add it again. A note duplicated into prose is how the model talks itself into stopping.

## Compacting under a `ctx:` warning

When the `ctx:N` tag says compact, the next call must be the compaction batch. Do not write a large
file first: a `write` call's body is part of the transcript as the call's arguments, so it adds the
same bytes you are trying to remove. Put the state in the note instead, or if you do write a file,
drop that round (`drop 'prev'`) in the same batch.

A failed batch is atomic: nothing is written. If you are already at the budget, a failed batch can
push the next request over the limit, so keep the batch small and use the paths the listing
printed.

## Single edits

```bash
python3 /shared/skills/session-edit.py read FILE 'turn[0].note[0]'
python3 /shared/skills/session-edit.py set  FILE 'self.reasoning' --body '...'
python3 /shared/skills/session-edit.py add  FILE 'turn[0].assistant[0].scratchpad' --body '...'
python3 /shared/skills/session-edit.py drop FILE 'turn[0].*'
```

`--body -` reads the body from stdin for something long or generated.

## Adding your own roles

Any name you invent inside a turn or an assistant is kept and handed back under that name, so you
can keep a scratchpad or ledger without cramming it into prose. `add` creates the region and the
turn/assistant it needs, and its receipt shows the indexed path it landed on. A batch `add` needs
its parent path to exist already.

## What the tool refuses

- Edits that would not parse, or that put a region where it has no meaning. It says why and leaves
  the file alone.
- Removing the ```toml fence at the top.
- Part of a batch: one bad line means none of it is written.
- Dropping the live round or the live turn, because this call's result will land there.
- `self.call[*]` and `self.result[*]`: the live round's call is not written yet, and results are
  turn-level. The error names the live round and points at `prev`, `round[M]`, or `droplists`.
- `round` anywhere except as the last segment; `round[M]` already names a whole round.
- Writes while the file is `streaming:`.
- Writes when the file changed after the tool read it (a user message or a subagent report landed).
  Nothing is written; run `list` again and re-apply.

Escaping is handled for you: `<` in a body is wrapped automatically, so a note containing
`<!-- slopfile:note -->` stays text. Do not write that wrapper yourself.

## Trust the output

`set`, `append`, `add`, `drop` and `batch` print the resolved paths (including
`self → turn[2].assistant[3].reasoning (92c)`) and the file's size after the edit. If it printed,
it wrote. Do not read or grep the session file to verify: that re-injects it as fresh context and
defeats the point of dropping it.

Sizes are snapshots, not deltas. The harness writes your reasoning, text and call into the
transcript as you generate them, so the file grows between a `list` and the next command. Do not
compare a count from an earlier `list` to a later receipt: the difference is your own output plus
whatever tool results landed since, not what the edit did. The receipt's count is the size now.

## `ctx:N` is one call behind

The token tag on a tool result was sampled **before** that call ran, not after. So a tag on a
compaction receipt shows the size the file had when the batch was issued — the drops it reports
have already happened and are not in that number. Do not read a big `ctx` tag on a batch receipt as
"still full, compact again now": the next call's tag is the one that reflects the edit. Judge by
trend across two calls, and trust the receipt's own line count.

## Keep this skill in the window

Dropping this skill out of context is a bad idea: after a compaction the agent gets confused by
exactly the things this file says are not worth reasoning about. A batch can drop the very text
that explains why listings must be dropped.

After any edit that drops regions, the tool prints a reminder to stderr:

```
reminder: a drop may have removed the session-edit skill from your context. if these rules are no
longer in your window, read /shared/skills/session-edit.md before editing again.
```

If that reminder fires and the rules are gone, re-read this file before the next edit. Prefer
keeping it: a whole-turn drop that removes the skill's read round is usually a net loss.

Keep stderr visible: a validation error means it refused to write, and `2>/dev/null` turns that
into a silent no-op.
