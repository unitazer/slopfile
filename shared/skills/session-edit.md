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

Compaction is a tail operation: pick a cut point at a turn boundary, drop everything after it in
one batch, and write a summary `note` at the tail of the surviving prefix (e.g. the last
surviving turn's note) covering everything you deleted — including the user's latest request and
any open state, because the cut removes those. Finished subtasks become `result + one-liner`;
verbose reasoning becomes a one-liner; dead `result`s and the `call`s that produced them are gone.

Keep the planning out of the transcript: run `list` once, send one batch, get back to work.

Do this before the window fills, not after. A failed search, a huge directory listing, or a wall of
repeated logs belongs in the bin immediately, before it rides along in every future request.

## Why the tail: the file is a prompt prefix

The harness sends this file as your prompt, and the provider caches the KVs of whatever prefix it
has already prefilled. A byte that changes at position P invalidates the cache from P to the end:
the next request re-prefills everything after P. Deletion is nearly free; only the new bytes cost.
So:

- Compact at the tail. Cut at a turn boundary, drop everything after it, write the summary at the
  tail of the surviving prefix. You pay the prefill of the summary no matter how much you dropped,
  and everything before the cut stays cached for the rest of the session.
- Do not scatter drops across turns. Each mid-file drop makes every later request re-prefill from
  that drop to the end of the file. If something in the middle must die, collect all such removals
  into one compaction.
- Treat the top as read-only. The toml fence, the system region, and already-summarized old turns
  are your deepest, longest-cached prefix. Never reword them in place; if an old turn needs
  fixing, cut before it and summarize.
- Notes get buried. A note added at the tail of turn N is at the tail only until turn N+1 is
  appended; after that it is mid-file, and `set`-ing it means re-prefilling from there to the end.
  Write running notes in the current turn and never go back to update a buried one — stale content
  in a buried note is cheaper to leave than to fix. The permanent layer is the compaction summary
  at the tail of the surviving prefix, which becomes stable cached prefix once the next turn lands.
- Cut where it hurts least. The summary must restate the user's latest request and every open
  thread, file path, PID, and decision the next turn will need — those are the bytes you are about
  to delete, and the next request will need them most.

## Trust the one-line output; never re-read to check

The file is your context; you can already see what is in it. If a batch printed `applied N edits`
and a region you no longer remember is gone from `list`, it worked. Do not `read` or grep the
session file to verify: that re-injects the file as brand-new context that rides along for the rest
of the session, and it defeats the point of dropping it. Record what you dropped and why in the
summary note of the same batch — that is the provenance, and it costs nothing extra.

Hygiene:
- Keep session-edit's stderr visible. A validation error is the tool telling you it refused to
  write; hiding it with `2>/dev/null` turns a refused edit into a silent no-op you will not notice.
- `ctx:N` lags one call and only moves on the next result. Judge by trend across calls.
- Placement: `note` lives at the turn level; inside an `assistant` use your own role name
  (`scratchpad`, `ledger`). The tool refuses anything it will not keep parseable and says why.

## Compaction is two stages

Stage 1, the cut: one batch that drops the cut turns, plus the summary `add` at the tail of the
surviving prefix (a separate `--body -` invocation when the summary is multi-line; batch bodies are
one line). Stage 2, the cleanup: the compaction's own reasoning — 2-4k tokens of cut-point math and
path mapping — lands in the surviving file as dead weight. Drop it in one small batch of
`drop '<turn>.assistant[N].reasoning'` right after stage 1, while the cut turn is still the region
just below the live turn; left to a later turn it becomes a mid-file edit. Keep the user messages,
the `text` lines, the `call`s and their one-line `result`s — that is the provenance.

## What a cut changes (and does not)

- `drop turn[N]` (or bare `turn`) deletes the whole region, children included; no `turn[N].*`
  needed. A six-turn drop batch applied atomically in one call.
- Deleting turns re-indexes every survivor: the turn that was `turn[6]` is `turn` the moment the
  batch lands. Paths from the pre-batch listing are stale for anything after the batch, so re-`list`
  before a follow-up `add`/`set` targeting a re-indexed path. Inside one batch the old paths are
  fine — it resolves against the pre-batch listing.
- The summary note goes on the last surviving turn (`turn.note` after a full cut). If the live turn
  has already been appended, the `add` lands just before it — one small insertion, and it is stable
  cached prefix from the next request on.
- Distill, don't narrate: if you spent time deriving something (cut point, sizes, path mapping),
  the result belongs in this skill or the summary note, not in the transcript. Re-deriving it next
  session is the failure mode this skill exists to prevent.
