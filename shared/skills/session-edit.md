---
name: session-edit
description: >
  Read and change regions of your own session file by name, without ever typing a marker. Use this
  whenever you want to compact the context, keep a running ledger or scratchpad, drop dead tool
  output, or record state you will need later. Prefer this over rewriting the file with
  head/sed/heredoc, which hardcodes line numbers and breaks as soon as the file shifts. Triggers
  on: compact the context, prune, drop old turns, keep notes, write down, scratchpad, ledger,
  remember this, shorten the transcript, edit my context file.
---

## Your context file, addressed by name

```bash
python3 /shared/skills/session-edit.py list /shared/transcripts/<session>.md
```

That prints every addressable path. Start there — do not guess a path.

```
  turn
  turn.user
  turn.note
  turn.assistant
  turn.assistant.reasoning
  turn.assistant.text
  turn.result  {"id":"c1","name":"bash","ok":true,...}
  turn.assistant.call  {"id":"c1","name":"bash","arguments":"..."}
```

Repeated names get an index: `turn[2].assistant.call[1]`. Omit the index for the first one. Indices
count same-named siblings at that spot. A single command renumbers the remaining siblings, so run
`list` again after a `drop`; a `batch` resolves every path against the listing from before it, so
there order does not matter. A wildcard applies to its own segment only: `turn[*].assistant.call`
means every turn but only the first assistant and first call in each, so put `[*]` on every level
you mean, e.g. `drop 'turn[*].assistant[*].call[*]'`.

## Batch: one call for the whole edit

Run `list` once, send one batch, get back to work. Do not narrate the edit plan in your reasoning;
that narration becomes context you will have to compact again.

A compaction is one heredoc, one parse and one write, with one line of output:

```bash
python3 /shared/skills/session-edit.py batch /shared/transcripts/<session>.md <<'EOF'
drop 'turn[*].assistant[*].call[*]'
drop 'turn[*].result[*]'
set 'turn[*].assistant.reasoning' | reran tokenize: 25k -> 6k
add turn.note | compaction: dropped dead tool output
EOF
```

Every line is `drop PATH`, `set PATH | BODY`, `append PATH | BODY` or `add PATH | BODY`. Paths keep
the meaning they had in the `list` you ran just before the batch, so you can drop in any order and
one line cannot renumber another; quotes around a path are stripped. Bodies are one line; for a
multi-line body use `set FILE PATH --body -`. If any line fails, the whole batch is abandoned and
the file is untouched. Do not chain the single commands with `&&`: each one re-parses, rewrites and
prints, which is the expensive way to do the same job.

## Single edits

```bash
python3 /shared/skills/session-edit.py read  FILE turn[1].note
python3 /shared/skills/session-edit.py set   FILE turn.note --body '...'
python3 /shared/skills/session-edit.py add   FILE turn.assistant.scratchpad --body 'trying two approaches'
python3 /shared/skills/session-edit.py drop  FILE 'turn[1].*'
```

`--body -` reads the body from stdin, which is how you pass something long or generated:

```bash
python3 /shared/skills/session-edit.py set FILE turn.note --body - <<'EOF'
## STATE
- 7 repos verified; search still running (pid in ghsearch.log)
- next: curate candidates, then shallow-clone
EOF
```

## Adding your own roles

Any name you invent inside a `turn` or an `assistant` is kept and handed back to you under that
name, so you can keep a scratchpad or a ledger without cramming it into prose. `add` creates the
region, and creates the `turn` / `assistant` it needs if they are not there yet:

```bash
python3 /shared/skills/session-edit.py add FILE turn.assistant.scratchpad --body 'trying two approaches'
python3 /shared/skills/session-edit.py add FILE turn.ledger --body -        # your own role, kept verbatim
```

A batch `add` needs its parent path to exist already; use a single `add` for the rare session that
has no turn or assistant yet.

## Why not just rewrite the file

`head -48 f.md > f.new && cat >> f.new <<'EOF' ... EOF && mv` works, and it is how you will lose
an afternoon: it hardcodes line numbers, and the line numbers move every time the harness
regenerates the fence or the file gains a line. Rewriting from `head` also means retyping every
region above the cut, which is exactly the part you were trying to delete.

## Things this refuses to do

- Writing an edit that would not parse, or that puts a region somewhere it has no meaning. It
  says so and leaves the file alone, which is safer than a rolled-back edit you have to unpick.
- Removing the ```toml fence at the top.
- Committing part of a batch: one bad line means none of it is written.

Escaping is handled for you: `<` in a body is wrapped automatically, so a note containing
`<!-- slopfile:note -->` stays text instead of becoming structure. You never write that wrapper
yourself.

## Compacting, which is the point

One batch is enough: drop the `result`s that no longer matter, replace verbose reasoning with
one-liners, drop the calls that produced them, and leave a `note` with anything worth remembering.
Keep the planning out of the transcript: run `list` once, send one batch, get back to work.

Do this before the window fills, not after. A failed search, a huge directory listing, or a wall of
repeated logs belongs in the bin immediately, before it rides along in every future request.
