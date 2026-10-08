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
python3 /shared/skills/session-edit.py list  /shared/transcripts/<session>.md
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

Repeated names get an index: `turn[2].assistant.call[1]`. Omit the index for the first one.

## The four things you actually need

```bash
# what is in there, and where
python3 /shared/skills/session-edit.py list  FILE

# read one region (markers unescaped, so you see your own text)
python3 /shared/skills/session-edit.py read  FILE turn[1].note

# replace a region's body
python3 /shared/skills/session-edit.py set   FILE turn[1].note --body '...'

# keep a running record: replace a region each turn instead of appending a new one
python3 /shared/skills/session-edit.py set   FILE turn.note --body "$(cat ledger.md)"
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

To drop something you no longer need, remove its regions:

```bash
python3 /shared/skills/session-edit.py drop FILE 'turn[1].*'
```

## Why not just rewrite the file

`head -48 f.md > f.new && cat >> f.new <<'EOF' ... EOF && mv` works, and it is how you will lose
an afternoon: it hardcodes line numbers, and the line numbers move every time the harness
regenerates the fence or the file gains a line. Rewriting from `head` also means retyping every
region above the cut, which is exactly the part you were trying to delete.

## Things this refuses to do

- Writing an edit that would not parse, or that puts a region somewhere it has no meaning. It
  says so and leaves the file alone, which is safer than a rolled-back edit you have to unpick.
- Removing the ```toml fence at the top.

Escaping is handled for you: `<` in a body is wrapped automatically, so a note containing
`<!-- slopfile:note -->` stays text instead of becoming structure. You never write that wrapper
yourself.

## Compacting, which is the point

The whole reason the file exists is that you can shrink it. When a turn is finished and its output
is no longer useful, drop it and keep a short `note` in its place:

```bash
python3 /shared/skills/session-edit.py drop FILE 'turn[4]'
python3 /shared/skills/session-edit.py set   FILE turn.note --body 'turn 4: cloned 7 repos, kept'
```

Do this before the window fills, not after. If a turn's text or a tool result stops mattering,
it is dead weight in every future request.

once you are done editting the session file, you may want to edit out the part where you called the tools
to edit it as well if it was more than one tool call

also, its best to apply these edits immediately after an accidental context spill, where you injest too much
information from a tool call that did what you did not intend it to do, so failed web searches, find attempts
that dumped a bunch of useless directories and hundreds of lines of repeating logs that only really have a few
lines that are worth paying attention to should be editted out / compacted as soon as possible as to not waste compute
