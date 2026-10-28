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
  assistant[0]  301c  ← live (self)
    reasoning[0]  250c
    call[0]  51c  bash("cargo test --all")
```

The counts are exact content characters, not tokens. Every segment has a 0-based index, including
the first: `turn[0].assistant[0].call[0]`. A bare name means `[0]`; a wildcard applies only to its
own segment, so `turn[*].assistant[*].call[*]` is every call in every turn.

Sizes shrink after an edit, so one `drop` can renumber later siblings. Inside one `batch`, every
path means what it meant in the listing before it.

## self, last, ranges

- `self` is the round this command runs in: the last assistant of the last turn. It is not "the
  block you last saw". If a user message arrived after your listing, `self` is a newer, smaller
  round.
- Old blocks do not move. A user message appends a turn; it does not renumber the old ones, so
  `turn[2]` from your listing is still `turn[2]`. Target old blocks by turn:
  `set 'turn[2].assistant[*].reasoning' | ...`.
- `turn[-1]` and `turn[last]` mean "now", and so does an open range like `turn[8..]`. For a cut
  from a listing use an explicit end (`turn[8..11]`); an open range would include the live turn
  and be refused.
- `[a..b]` is inclusive; `[last]` and `[-1]` pick the last sibling (`result[-1]`).

If the session runs without thinking there is no reasoning block; summarize `self.text` instead.

If a receipt shows `self` is smaller than you expected, an interruption moved it. The old block is
still at the turn you listed; target it by turn.

## While your tool runs

`live: turn[N]` is normal, not a half-written turn. Generation has ended; the reasoning is closed
and editable, and only the current *assistant* is left open so the harness can attach this call's
result to it.

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
EOF
```

Drop the calls with the results; a call left without a result becomes a "not executed" stub in the
next request. The note's own call would create another reasoning block to clean up, which is why
the summary and the cleanup are one batch. The note body is multi-line; the outer shell heredoc
must use a different tag (`EOF`).

The note is memory, not your reply. Do not paste its body into `text` or `reasoning` as well, and
do not treat the batch result as the end of the turn: keep working with the next tool call. The
note is written before this call's result, so later rounds are appended after it; you do not need
to add it again. A note duplicated into prose is how the model talks itself into stopping.

Each line is `drop PATH`, `set PATH | BODY`, `append PATH | BODY`, `add PATH | BODY`, or
`add PATH <<'TAG'`. Quotes around a path are stripped. If any line fails, the whole batch is
abandoned and the file is untouched. Do not chain single commands with `&&`.

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
- Writes while the file is `streaming:`.
- Writes when the file changed after the tool read it (a user message or a subagent report landed).
  Nothing is written; run `list` again and re-apply.

Escaping is handled for you: `<` in a body is wrapped automatically, so a note containing
`<!-- slopfile:note -->` stays text. Do not write that wrapper yourself.

## Trust the output

`set`, `append`, `add`, `drop` and `batch` print the resolved paths (including
`self → turn[2].assistant[3].reasoning (92c)`) and exact line/character deltas. If it printed, it
wrote. Do not read or grep the session file to verify: that re-injects it as fresh context and
defeats the point of dropping it.

Keep stderr visible: a validation error means it refused to write, and `2>/dev/null` turns that
into a silent no-op. `ctx:N` lags one call; judge by trend. `note` lives at the turn level; inside
an assistant use your own role name.
