#!/usr/bin/env python3
"""Address regions of a slopfile session transcript by name, without ever typing a marker.

Rewriting a whole transcript with `head`/`sed`/heredoc works but is brittle: it hardcodes line
numbers, and the markers are escaped on disk (`<!--slopfile:ESC[<<!--/slopfile:ESC-->!-- slopfile:x -->`)
so a targeted `edit` has to reproduce that wrapper exactly. This does the same job structurally:
you name a region, you get or set its body, and this handles the markers and the escaping.

  ./session-edit.py list   SESSION
  ./session-edit.py read   SESSION turn[0].assistant[0].text
  ./session-edit.py set    SESSION turn[0].note[0] --body "..."
  ./session-edit.py append SESSION turn[0].note[0] --body "..."
  ./session-edit.py drop   SESSION 'turn[0]'                 (removes matching regions)
  ./session-edit.py drop   SESSION 'turn[0].*'
  ./session-edit.py check  SESSION
  ./session-edit.py batch  SESSION <<'EOF'
  drop 'turn[*].assistant[*].call[*]'
  set 'turn[*].assistant[*].reasoning' | one-line summary
  add turn[0].note | compaction: dropped dead tool output
  EOF

`batch` is one parse, one write and one line of output however many edits there are, and a failing
line aborts the whole batch without touching the file. Each line is `op path` or `op path | body`;
bodies are one line (use `set ... --body -` for a multi-line body). Paths mean what they did in the
`list` you ran before the batch, so order does not matter and drops cannot renumber later lines.
Prefer it over shell `&&` chains, which re-parse, rewrite and re-print for every edit.

`list` prints a tree. Every addressable segment carries a 0-based index, including the first one:
`turn[0]`, then `turn[0].assistant[0]`, `turn[0].assistant[0].call[0]`. A bare name is shorthand
for `[0]` (`turn.note` means `turn[0].note`), but the tree always writes the index. Indices count
same-named siblings at that level; after a single `drop`, run `list` again. A wildcard applies only
to its own segment, so `[*]` (or a bare `*`) must be repeated on every level you mean to sweep. A
batch resolves every path against the snapshot before it, so order does not matter.

Every write is atomic (temp file plus rename) and leaves the transcript untouched if the edit
would not parse.
"""

import argparse
import json
import os
import re
import sys

OPEN = "<!-- slopfile:"
CLOSE = "<!-- /slopfile -->"
ESC_OPEN = "<!--slopfile:ESC["
ESC_CLOSE = "<!--/slopfile:ESC-->"
BEGIN = "begin"

# Regions the harness gives meaning to. Anything else inside a turn or an assistant is a role the
# model invented and is still addressable.
INPUT = "input"
KNOWN = {
    "system", "turn", "user", "note", "assistant", "reasoning", "text",
    "call", "result", "question", "input", "begin",
}


def escape(body):
    """Wrap every `<` exactly the way the parser expects. Both wrapper delimiters start with `<`,
    so a body cannot assemble a marker and forge structure."""
    return body.replace("<", ESC_OPEN + "<" + ESC_CLOSE)


def unescape(body):
    return body.replace(ESC_OPEN, "").replace(ESC_CLOSE, "")


def to_lines(text):
    """Split into lines that each end in a newline, without inventing a trailing blank line."""
    parts = text.split("\n")
    if parts and parts[-1] == "":
        parts.pop()
    return [p + "\n" for p in parts]


def is_open(line):
    # The escaped form has no space after `<!--`, so it can never be mistaken for structure.
    return line.startswith(OPEN)


def is_close(line):
    return line.strip() == CLOSE


def parse_name(line):
    """`<!-- slopfile:note {"by":"harness"} -->` -> ('note', '{"by":"harness"}')."""
    rest = line[len(OPEN):].rstrip("\r\n")
    if not rest.endswith("-->"):
        return None, None
    rest = rest[:-3].strip()
    m = re.match(r'^([A-Za-z0-9_-]+)\s*(\{.*\})?$', rest, re.S)
    if not m:
        return None, None
    return m.group(1), (m.group(2) or "").strip() or None


class Node:
    __slots__ = ("name", "payload", "open_line", "close_line", "body_start", "children", "parent")

    def __init__(self, name, payload, open_line, parent):
        self.name = name
        self.payload = payload
        self.open_line = open_line
        self.close_line = None
        self.body_start = open_line + 1
        self.children = []
        self.parent = parent


def build_tree(lines):
    """Scan for real markers only. Returns (root_nodes, stack). Anything before the first marker is
    the preamble (the toml fence and any prose) and is not addressable."""
    roots, stack = [], []
    for i, line in enumerate(lines):
        if is_open(line):
            name, payload = parse_name(line)
            if name is None:
                continue
            # `input` is the human's tail: a root that runs to EOF. The harness stops reading
            # structure at that marker, so the live assistant stays open above it without the
            # input becoming its child. Treating it as a child instead puts an extra close on the
            # stack, and every edit made through that stack closes one container too many.
            if name == INPUT:
                while stack and stack[-1].name not in CONTAINERS:
                    stack.pop()
                roots.append(Node(name, payload, i, None))
                break
            parent = stack[-1] if stack else None
            node = Node(name, payload, i, parent)
            (parent.children if parent else roots).append(node)
            stack.append(node)
        elif is_close(line) and stack:
            stack[-1].close_line = i
            stack.pop()
    return roots, stack


def path_of(node, index_of):
    parts = []
    while node is not None:
        if node.parent is None and node.name in (BEGIN, INPUT):
            break
        parts.append(f"{node.name}[{index_of[node]}]")
        node = node.parent
    return ".".join(reversed(parts))


def find_input(nodes):
    """The top-level `input` region, wherever it sits under `begin`."""
    for node in nodes:
        if node.name == INPUT:
            return node
        hit = find_input(node.children)
        if hit is not None:
            return hit
    return None


def walk(nodes, index_of, out):
    for node in nodes:
        # Index among same-named siblings, not globally: deleting a result in one turn must not
        # renumber the results of every turn after it.
        idx = [n for n in nodes if n.name == node.name].index(node)
        index_of[node] = idx
        p = path_of(node, index_of)
        if p:
            out.append((p, node))
        walk(node.children, index_of, out)


def parse_payload(node):
    if not node.payload:
        return {}
    try:
        payload = json.loads(node.payload)
    except Exception:
        return {}
    return payload if isinstance(payload, dict) else {}


def parse_arguments(payload):
    raw = payload.get("arguments")
    if isinstance(raw, str):
        try:
            raw = json.loads(raw)
        except Exception:
            return {}
    return raw if isinstance(raw, dict) else {}


def short_words(value, limit=20):
    words = str(value).split()
    if len(words) > limit:
        return " ".join(words[:limit]) + " ..."
    return " ".join(words)


def quoted(value):
    return json.dumps(str(value))


def call_summary(node, limit=20):
    """Render a call as `bash("first words")` or `read("/path")`, never as raw JSON."""
    payload = parse_payload(node)
    name = payload.get("name") or "?"
    args = parse_arguments(payload)
    if name == "bash":
        return f"bash({quoted(short_words(args.get('command') or '', limit))})"
    if name in ("read", "write", "edit"):
        return f"{name}({quoted(args.get('path') or '')})"
    if name in ("grep", "find"):
        pattern = short_words(args.get("pattern") or "", limit)
        path = args.get("path") or "."
        if path != ".":
            return f"{name}({quoted(pattern)}, {quoted(path)})"
        return f"{name}({quoted(pattern)})"
    if name == "ls":
        return f"ls({quoted(args.get('path') or '.')})"
    if name == "question":
        data = args.get("data")
        if isinstance(data, list) and data and isinstance(data[0], dict):
            question = data[0].get("question") or ""
            return f"question({quoted(short_words(question, limit))})"
        return "question()"
    if name == "subagent":
        return f"subagent({quoted(short_words(args.get('task') or '', limit))})"
    if args:
        return f"{name}({quoted(short_words(json.dumps(args, sort_keys=True), min(limit, 12)))})"
    return f"{name}()"


def result_summary(node, lines):
    """Show the outcome, never the id or raw JSON; failures also show the first words of the error."""
    payload = parse_payload(node)
    name = payload.get("name") or "?"
    failed = payload.get("ok") is False
    status = "FAILED" if failed else "ok"
    ms = payload.get("ms")
    if isinstance(ms, int):
        status += f" {ms}ms"
    if failed:
        preview = short_words(unescape(body_text(lines, node)), 12)
        if preview:
            status += f": {preview}"
    return f"{name} {status}"


def render_tree(nodes, depth, index_of, lines, out):
    for node in nodes:
        detail = ""
        if node.name == "call":
            detail = f"  {call_summary(node)}"
        elif node.name == "result":
            detail = f"  {result_summary(node, lines)}"
        out.append(f"{'  ' * depth}{node.name}[{index_of[node]}]{detail}")
        render_tree(node.children, depth + 1, index_of, lines, out)


def split_segment(seg):
    m = re.match(r"^([A-Za-z0-9_-]+)(?:\[(\d+)\])?$", seg)
    if not m:
        raise SystemExit(f"bad path segment: {seg!r}")
    return m.group(1), m.group(2)


def resolve(entries, spec):
    """`turn[3].assistant[0].text` -> that node. A bare name matches its first occurrence."""
    parts = []
    for seg in spec.split("."):
        if seg == "*":
            parts.append((None, "*"))
            continue
        m = re.match(r"^([A-Za-z0-9_-]+)(?:\[(\d+|\*)\])?$", seg)
        if not m:
            raise SystemExit(f"bad path segment: {seg!r}")
        parts.append((m.group(1), m.group(2)))
    hits = []
    for path, node in entries:
        segs = path.split(".")
        if len(segs) != len(parts):
            continue
        ok = True
        for got, (want, ix) in zip(segs, parts):
            m = re.match(r"^([A-Za-z0-9_-]+)(?:\[(\d+)\])?$", got)
            name, index = m.group(1), int(m.group(2) or 0)
            if want is not None and name != want:
                ok = False
                break
            # A bare name is the first occurrence, not every occurrence. Without this a
            # `set turn.assistant.reasoning` rewrites the first reasoning of every turn.
            if ix is None and index != 0:
                ok = False
                break
            if ix not in (None, "*") and index != int(ix):
                ok = False
                break
        if ok:
            hits.append(node)
    if not hits:
        raise SystemExit(f"no region matches {spec!r}; run `list` to see the paths")
    return hits


def body_text(lines, node):
    end = node.close_line if node.close_line is not None else len(lines)
    return "".join(lines[node.body_start:end])


TOP_LEVEL = {"system", "turn", "input", "begin"}
IN_TURN = {"user", "note", "assistant", "result", "question"}
IN_ASSISTANT = {"reasoning", "text", "call"}
CONTAINERS = {"turn", "assistant"}
# The parser reports a second `user` in a turn, or a second `reasoning`/`text` in an assistant, and
# a write carrying one would be rolled back. Same names are fine in different containers.
ONCE_PER_TURN = {"user"}
ONCE_PER_ASSISTANT = {"reasoning", "text"}
# The harness renders a live session with its last turn, current assistant and the section being
# streamed still open, then the human's `input` after them.
LIVE_TAILS = {
    (),
    ("turn",),
    ("turn", "assistant"),
    ("turn", "assistant", "reasoning"),
    ("turn", "assistant", "text"),
}


def check(lines):
    """Refuse anything the harness would roll back, so a bad edit never reaches disk."""
    if not lines:
        return "edit produced an empty file"
    text = "".join(lines)
    if text.count("```toml") == 1 and not text.lstrip().startswith("```toml"):
        head = text.split("```toml")[0].strip()
        if head:
            return "the ```toml fence must stay at the top of the file"
    stack = []
    # Identity of the enclosing turn/assistant for the singleton checks: two siblings with the
    # same name are different containers, and both may hold a `reasoning`.
    ids = []
    next_id = 0
    opened = set()
    for line in lines:
        if is_close(line):
            if not stack:
                return f"{CLOSE} closes a region that was never opened"
            stack.pop()
            ids.pop()
            continue
        if not is_open(line):
            continue
        name, payload = parse_name(line)
        if name is None:
            return f"unreadable region marker: {line.strip()[:60]}"
        if name == "begin":
            continue
        # `input` is the human's tail and runs to EOF. The renderer can put it inside the live
        # turn/assistant, so stop here rather than reading that as nesting.
        if name == "input":
            break
        parent = stack[-1] if stack else None
        if parent is not None and parent not in CONTAINERS:
            return f"`{parent}` was left unclosed before `{name}`"
        if payload:
            try:
                json.loads(payload)
            except Exception as exc:
                return f"the {name} header is not valid JSON: {exc}"
        if parent is None:
            if name not in TOP_LEVEL:
                return f"`{name}` may not sit at the top level (allowed: system, turn, input)"
        elif parent == "turn":
            if name not in IN_TURN and name in KNOWN:
                return f"`{name}` does not belong inside a `turn`; allowed: {', '.join(sorted(IN_TURN))}, or a role of your own"
            if name in ONCE_PER_TURN and (ids[-1], name) in opened:
                return f"a second `{name}` in the same `turn`"
        elif parent == "assistant":
            if name not in IN_ASSISTANT and name in KNOWN:
                return f"`{name}` does not belong inside an `assistant`; allowed: {', '.join(sorted(IN_ASSISTANT))}, or a role of your own"
            if name in ONCE_PER_ASSISTANT and (ids[-1], name) in opened:
                return f"a second `{name}` in the same `assistant`"
        opened.add((ids[-1] if ids else None, name))
        stack.append(name)
        if name in CONTAINERS:
            ids.append(next_id)
            next_id += 1
        else:
            ids.append(None)
    if tuple(stack) not in LIVE_TAILS:
        return f"unclosed region(s): {', '.join(stack)}"
    return None


def write_atomic(path, text):
    tmp = f"{path}.session-edit.{os.getpid()}.tmp"
    with open(tmp, "w") as fh:
        fh.write(text)
    os.replace(tmp, path)


class Editor:
    """A parsed transcript mutated in memory; `save` is the only thing that touches disk."""

    def __init__(self, path):
        self.path = path
        with open(path) as fh:
            self.lines = fh.read().splitlines(keepends=True)
        self.reindex()

    def reindex(self):
        self.roots, self.stack = build_tree(self.lines)
        self.entries, self.index_of = [], {}
        walk(self.roots, self.index_of, self.entries)

    def text(self):
        return "".join(self.lines)

    def targets(self, spec, closed=True):
        hits = resolve(self.entries, spec)
        if closed:
            for node in hits:
                if node.close_line is None:
                    raise SystemExit(
                        f"{spec} is unclosed; the file is mid-generation, try again shortly"
                    )
        return hits

    def commit(self, new_lines):
        """Check the whole file, adopt it and reindex, all before anything reaches disk."""
        why = check(new_lines)
        if why:
            raise SystemExit(f"refusing to write: {why}")
        self.lines = new_lines
        self.reindex()

    def drop(self, spec):
        targets = self.targets(spec)
        # Both markers go, or the open is left dangling and the file stops parsing.
        doomed = set()
        for node in targets:
            doomed.update(range(node.open_line, node.close_line + 1))
        self.commit([line for i, line in enumerate(self.lines) if i not in doomed])
        return len(targets), f"dropped {len(targets)} region(s)"

    def write(self, spec, raw, append=False):
        targets = self.targets(spec)
        edits = []
        for node in targets:
            if append:
                new = body_text(self.lines, node).rstrip("\n") + "\n" + escape(raw)
            else:
                new = escape(raw)
            edits.append((node.body_start, node.close_line, to_lines(new)))
        keep = list(self.lines)
        for start, end, body in sorted(edits, reverse=True):
            keep[start:end] = body
        self.commit(keep)
        verb = "appended to" if append else "set"
        return len(targets), f"{verb} {spec} ({len(targets)} region(s))"

    def begin(self):
        tops = [n for n in self.roots if n.name == BEGIN]
        if not tops:
            raise SystemExit("this file has no <!-- slopfile:begin --> region")
        return tops[0]

    def insert_into(self, holder, seg, body_lines):
        """Put a new region inside `holder`, above the input region when holder is open."""
        at, closes = insert_point(self, holder)
        if at is None:
            raise SystemExit(f"cannot find where to create {seg!r}")
        self.lines[at:at] = closes + body_lines

    def add(self, spec, raw):
        parent_spec, _, new_name = spec.rpartition(".")
        if not new_name:
            raise SystemExit("add needs a full path like turn.note or turn.assistant.scratchpad")
        new_base, new_index = split_segment(new_name)
        if new_index is not None:
            raise SystemExit(
                "add creates a new region; omit its index (use set or drop for an existing one)"
            )
        # Walk the path one segment at a time, creating any container that is missing, so
        # `turn[0].assistant[0].scratchpad` works on a session that has neither yet.
        segments = parent_spec.split(".") if parent_spec else []
        holder = None
        for depth, seg in enumerate(segments):
            prefix = ".".join(segments[: depth + 1])
            try:
                holder = resolve(self.entries, prefix)[0]
                continue
            except SystemExit:
                pass
            base, _ = split_segment(seg)
            if base not in CONTAINERS:
                raise SystemExit(
                    f"cannot create {base!r}; only `turn` and `assistant` are created for you"
                )
            if holder is None:
                holder = self.begin()
            self.insert_into(holder, base, [f"{OPEN}{base} -->\n", f"{CLOSE}\n"])
            self.reindex()
            try:
                holder = resolve(self.entries, prefix)[0]
            except SystemExit:
                raise SystemExit(
                    f"created {base!r}, but {prefix!r} does not exist; check the index"
                )
        if holder is None:
            holder = self.begin()

        if new_base not in KNOWN and holder.name not in CONTAINERS:
            raise SystemExit(
                f"{new_base!r} may only be invented inside a turn or an assistant, not inside "
                f"{holder.name!r}"
            )
        block = [f"{OPEN}{new_base} -->\n"] + to_lines(escape(raw)) + [f"{CLOSE}\n"]
        self.insert_into(holder, new_base, block)
        self.commit(list(self.lines))
        return 1, f"added {new_base} inside {parent_spec or 'the top level'}"

    def save(self):
        write_atomic(self.path, self.text())


def insert_point(view, holder):
    """Where a new child of `holder` goes, plus the closes needed to leave the live tail."""
    if holder.name == BEGIN:
        inp = find_input(view.roots)
        return (inp.open_line if inp is not None else len(view.lines)), []
    if holder.close_line is not None:
        return holder.close_line, []
    if holder.name == INPUT:
        raise SystemExit("cannot add a region inside the input region")
    inp = find_input(view.roots)
    at = inp.open_line if inp is not None else len(view.lines)
    # The new region must be a child of `holder`, not of whatever section is still open under it,
    # so close that tail first. Otherwise a note added mid-turn lands inside the assistant and the
    # harness rejects it on the next parse.
    at_holder = [i for i, node in enumerate(view.stack) if node is holder]
    if not at_holder:
        raise SystemExit(f"`{holder.name}` is not on the open path")
    closes = max(0, len(view.stack) - at_holder[0] - 1)
    if view.stack and view.stack[-1].name == INPUT:
        closes = max(0, closes - 1)
    return at, [CLOSE + "\n"] * closes


BATCH_OPS = ("drop", "set", "append", "add")


def parse_batch(text):
    """`drop PATH` / `set PATH | BODY` lines -> ops. One line per edit, no shell chaining."""
    ops = []
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        head, sep, body = raw.partition("|")
        op, _, spec = head.strip().partition(" ")
        op, spec = op.strip(), spec.strip()
        if op not in BATCH_OPS:
            raise SystemExit(
                f"batch line {lineno}: unknown op {op!r}; use drop, set, append or add"
            )
        if not spec:
            raise SystemExit(f"batch line {lineno}: {op} needs a region path")
        if op == "drop" and sep:
            raise SystemExit(f"batch line {lineno}: drop takes no body")
        if op != "drop" and not sep:
            raise SystemExit(f"batch line {lineno}: {op} needs `| body`")
        if body.startswith(" "):
            body = body[1:]
        if len(spec) > 1 and spec[0] == spec[-1] and spec[0] in "'\"":
            spec = spec[1:-1]
        ops.append((lineno, op, spec, body))
    return ops


def plan_batch(editor, ops):
    """Turn the whole batch into line replacements against the pre-batch snapshot.

    Resolving every op up front is the point: paths keep the meaning they had in the `list` that
    preceded the batch, so dropping `turn[0].result` cannot renumber `turn[1].result`.
    """
    drops = []  # (start, end, lineno) end exclusive
    edits = []  # (start, end, seq, lines, lineno)
    inserts = []  # (at, seq, closes, block, lineno, holder)
    regions = 0
    for seq, (lineno, op, spec, body) in enumerate(ops):
        try:
            if op == "drop":
                nodes = editor.targets(spec)
                drops.extend((n.open_line, n.close_line + 1, lineno) for n in nodes)
                regions += len(nodes)
            elif op in ("set", "append"):
                nodes = editor.targets(spec)
                for node in nodes:
                    if op == "append":
                        new = body_text(editor.lines, node).rstrip("\n") + "\n" + escape(body)
                    else:
                        new = escape(body)
                    edits.append((node.body_start, node.close_line, seq, to_lines(new), lineno))
                regions += len(nodes)
            else:
                parent_spec, _, new_name = spec.rpartition(".")
                if not new_name:
                    raise SystemExit("add needs a full path like turn.note")
                new_base, new_index = split_segment(new_name)
                if new_index is not None:
                    raise SystemExit(
                        "add creates a new region; omit its index (use set or drop for an existing one)"
                    )
                if parent_spec:
                    holder = resolve(editor.entries, parent_spec)[0]
                else:
                    holder = editor.begin()
                if new_base not in KNOWN and holder.name not in CONTAINERS:
                    raise SystemExit(
                        f"{new_base!r} may only be invented inside a turn or an assistant"
                    )
                at, closes = insert_point(editor, holder)
                block = [f"{OPEN}{new_base} -->\n"] + to_lines(escape(body)) + [f"{CLOSE}\n"]
                inserts.append((at, seq, closes, block, lineno, holder))
                regions += 1
        except SystemExit as exc:
            raise SystemExit(f"batch line {lineno} ({op} {spec}): {exc}")

    # Edits must not touch a region the batch drops, and no two edits may share lines.
    for start, end, _, _, lineno in edits:
        for dstart, dend, dlineno in drops:
            if start < dend and dstart < end:
                raise SystemExit(
                    f"batch lines {dlineno} and {lineno}: one drops a region the other edits"
                )
    for i, (start, end, _, _, lineno) in enumerate(edits):
        for ostart, oend, _, _, olineno in edits[i + 1 :]:
            if start < oend and ostart < end:
                raise SystemExit(f"batch lines {lineno} and {olineno}: edits overlap")
    for at, _, _, _, lineno, _ in inserts:
        for start, end, dlineno in drops:
            if start <= at < end:
                raise SystemExit(f"batch lines {dlineno} and {lineno}: add lands in a dropped region")
        for start, end, _, _, elineno in edits:
            if start <= at < end:
                raise SystemExit(f"batch lines {elineno} and {lineno}: add lands in an edited region")

    # Nested drops (a turn and a child) are fine, but they must become disjoint line ranges before
    # they are applied: two slice assignments computed on the original numbering would otherwise
    # delete past the first one's end.
    merged = []
    for start, end, _ in sorted(drops):
        if merged and start < merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])

    # Additions at the same point are one insert; they must all go into the same holder, or the
    # first close-marker block would already have closed the second one's parent.
    grouped = {}
    for at, seq, closes, block, lineno, holder in inserts:
        grouped.setdefault(at, []).append((seq, closes, block, lineno, holder))
    mods = [(start, end, 0, []) for start, end in merged]
    mods += [(start, end, seq, lines) for start, end, seq, lines, _ in edits]
    for at, group in grouped.items():
        group.sort()
        holders = {id(entry[4]) for entry in group}
        if len(holders) > 1:
            linenos = ", ".join(str(entry[3]) for entry in group)
            raise SystemExit(
                f"batch lines {linenos}: add to different regions at the same spot; split the batch"
            )
        seq, closes, _, _, _ = group[0]
        lines = list(closes)
        for _, _, block, _, _ in group:
            lines.extend(block)
        mods.append((at, at, seq, lines))

    out = list(editor.lines)
    for start, end, seq, lines in sorted(mods, key=lambda m: (m[0], m[2]), reverse=True):
        out[start:end] = lines
    return out, regions


def body_arg(value):
    return sys.stdin.read() if value == "-" else (value or "")


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "cmd", choices=["list", "read", "set", "append", "add", "drop", "check", "batch"]
    )
    ap.add_argument("session")
    ap.add_argument("path", nargs="?")
    ap.add_argument("--body", help="new body text; use '-' to read the body from stdin")
    args = ap.parse_args()

    if args.cmd not in ("list", "check", "batch") and not args.path:
        ap.error(f"{args.cmd} needs a region path")

    editor = Editor(args.session)

    if args.cmd == "batch":
        if args.path or args.body:
            ap.error("batch reads its edits from stdin; pass only the session")
        ops = parse_batch(sys.stdin.read())
        if not ops:
            raise SystemExit("batch: no edits on stdin")
        new_lines, regions = plan_batch(editor, ops)
        editor.commit(new_lines)
        editor.save()
        state = (
            "unclosed: " + ",".join(node.name for node in editor.stack)
            if editor.stack
            else "balanced"
        )
        print(
            f"applied {len(ops)} {'edit' if len(ops) == 1 else 'edits'} ({regions} regions): "
            f"{len(editor.lines)} lines, {state}"
        )
        return

    if args.cmd == "list":
        state = (
            "unclosed: " + ",".join(node.name for node in editor.stack)
            if editor.stack
            else "balanced"
        )
        print(f"{len(editor.lines)} lines, {state}")
        top = []
        for root in editor.roots:
            if root.name == BEGIN:
                top.extend(root.children)
            elif root.name != INPUT:
                top.append(root)
        out = []
        render_tree(top, 0, editor.index_of, editor.lines, out)
        print("\n".join(out))
        return

    if args.cmd == "check":
        why = check(editor.lines)
        print(why or "ok")
        raise SystemExit(1 if why else 0)

    if args.cmd == "read":
        for node in editor.targets(args.path, closed=False):
            sys.stdout.write(unescape(body_text(editor.lines, node)))
        return

    if args.cmd == "add":
        _, message = editor.add(args.path, body_arg(args.body))
    elif args.cmd == "drop":
        _, message = editor.drop(args.path)
    else:
        _, message = editor.write(args.path, body_arg(args.body), append=(args.cmd == "append"))
    editor.save()
    print(message)


if __name__ == "__main__":
    main()