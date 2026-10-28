#!/usr/bin/env python3
"""Address regions of a slopfile session transcript by name, without ever typing a marker.

Rewriting a whole transcript with `head`/`sed`/heredoc works but is brittle: it hardcodes line
numbers, and the markers are escaped on disk (`<!--slopfile:ESC[<<!--/slopfile:ESC-->!-- slopfile:x -->`)
so a targeted `edit` has to reproduce that wrapper exactly. This does the same job structurally:
you name a region, you get or set its body, and this handles the markers and the escaping.

  ./session-edit.py list   SESSION
  ./session-edit.py read   SESSION turn[0].assistant[0].text
  ./session-edit.py set    SESSION self.reasoning --body "..."
  ./session-edit.py append SESSION turn[0].note[0] --body "..."
  ./session-edit.py drop   SESSION 'turn[0]'                 (removes matching regions)
  ./session-edit.py drop   SESSION 'turn[0].*'
  ./session-edit.py check  SESSION
  ./session-edit.py batch  SESSION <<'EOF'
  drop 'turn[*].result[*]'
  set 'self.reasoning' | one-line summary
  add 'turn[-1].note' <<'END'
  ## Summary
  - ...
  END
  EOF

Selectors:

  self           the assistant round running this command: the last assistant of the last turn
  [-1], [last]   the last sibling of that name (turn[-1], result[-1])
  [a..b]         an inclusive range (turn[3..5], turn[8..], turn[..2])
  [*]            every sibling (unchanged)

Turns are stable: a user interruption appends a turn, it does not renumber the old ones, so an
old block stays at the turn you listed it in. Only `self` and `turn[-1]` mean "now".

`batch` is one parse, one write and one receipt however many edits there are, and a failing line
aborts the whole batch without touching the file. Each line is `op path`, `op path | body`, or an
`add` with a `<<'TAG'` heredoc body. Paths mean what they did in the `list` you ran before the
batch, so order does not matter and drops cannot renumber later lines. A note's own call would
create another reasoning block to clean up, so the summary and the cleanup belong to one batch.

`list` prints a tree with exact content-character counts. Every addressable segment carries a
0-based index, including the first one: `turn[0]`, then `turn[0].assistant[0]`,
`turn[0].assistant[0].call[0]`. A bare name is shorthand for `[0]` (`turn.note` means
`turn[0].note`), but the tree always writes the index. Indices count same-named siblings at that
level.

Every write is atomic (temp file plus rename) and leaves the transcript untouched if the edit
would not parse or if the file changed while the edit was being prepared.
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
    """Scan for real markers only. Returns (root_nodes, container_stack, open_nodes).

    `container_stack` is what a leaf may be inserted into: unclosed leaves are dropped from it, as
    they are closed by the first inserted region. `open_nodes` is the true open path before that
    trimming, which is how a still-streaming `reasoning`/`text` is recognised.
    """
    roots, stack = [], []
    open_nodes = None
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
                open_nodes = list(stack)
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
    if open_nodes is None:
        open_nodes = list(stack)
    return roots, stack, open_nodes


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


def body_text(lines, node):
    end = node.close_line if node.close_line is not None else len(lines)
    return "".join(lines[node.body_start:end])


def content_size(lines, node):
    """Exact characters the region contributes to the prompt.

    A call keeps its arguments in the header payload; every other leaf keeps them in its body.
    Containers are the sum of their children and carry no text of their own.
    """
    if node.name in CONTAINERS or node.name == BEGIN:
        return 0
    if node.name == "call":
        args = parse_payload(node).get("arguments")
        return len(args) if isinstance(args, str) else 0
    return len(unescape(body_text(lines, node)))


def size_map(editor):
    sizes = {}

    def visit(node):
        own = content_size(editor.lines, node)
        sizes[node] = own + sum(visit(child) for child in node.children)
        return sizes[node]

    for root in editor.roots:
        visit(root)
    return sizes


def top_nodes(editor):
    out = []
    for root in editor.roots:
        if root.name == BEGIN:
            out.extend(root.children)
        elif root.name != INPUT:
            out.append(root)
    return out


def live_turn(editor):
    turns = [n for n in top_nodes(editor) if n.name == "turn"]
    return turns[-1] if turns else None


def live_assistant(editor):
    turn = live_turn(editor)
    if turn is None:
        return None
    assistants = [n for n in turn.children if n.name == "assistant"]
    return assistants[-1] if assistants else None


def open_state(editor):
    """balanced, live (tool time: assistant open, generation over), or streaming."""
    names = [n.name for n in editor.open_nodes]
    if names and names[-1] in ("reasoning", "text"):
        return "streaming"
    if "turn" in names:
        return "live"
    return "balanced"


def describe_state(editor):
    state = open_state(editor)
    if state == "balanced":
        return "balanced"
    if state == "streaming":
        node = editor.open_nodes[-1]
        return f"streaming: {path_of(node, editor.index_of)}"
    turn = live_turn(editor)
    return f"live: {path_of(turn, editor.index_of)}" if turn is not None else "live"


def total_chars(editor, sizes=None):
    sizes = sizes if sizes is not None else size_map(editor)
    return sum(sizes[node] for node in top_nodes(editor))


def render_tree(nodes, depth, index_of, lines, sizes, out, live_path=None, stream_path=None):
    for node in nodes:
        detail = ""
        if node.name == "call":
            detail = f"  {call_summary(node)}"
        elif node.name == "result":
            detail = f"  {result_summary(node, lines)}"
        mark = ""
        path = path_of(node, index_of)
        if path == live_path:
            mark = "  ← live (self)"
        elif path == stream_path:
            mark = "  ← streaming"
        out.append(
            f"{'  ' * depth}{node.name}[{index_of[node]}]  {sizes[node]:,}c{detail}{mark}"
        )
        render_tree(
            node.children, depth + 1, index_of, lines, sizes, out, live_path, stream_path
        )


def list_output(editor):
    sizes = size_map(editor)
    header = (
        f"{len(editor.lines)} lines, {total_chars(editor, sizes):,} content chars; "
        f"{describe_state(editor)}"
    )
    live_path = None
    stream_path = None
    state = open_state(editor)
    if state == "live":
        assistant = live_assistant(editor)
        if assistant is not None:
            live_path = path_of(assistant, editor.index_of)
    elif state == "streaming":
        stream_path = path_of(editor.open_nodes[-1], editor.index_of)
    out = [header]
    render_tree(
        top_nodes(editor), 0, editor.index_of, editor.lines, sizes, out, live_path, stream_path
    )
    inp = find_input(editor.roots)
    if inp is not None:
        size = sizes.get(inp, 0)
        if size:
            out.append(f"input  {size:,}c")
    return "\n".join(out)


def split_spec(spec):
    """Split on dots that are outside `[...]`, so `turn[4..5].note` has two segments."""
    parts, depth, start = [], 0, 0
    for i, ch in enumerate(spec):
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth = max(0, depth - 1)
        elif ch == "." and depth == 0:
            parts.append(spec[start:i])
            start = i + 1
    parts.append(spec[start:])
    return parts


def parse_segment(seg):
    m = re.match(r"^([A-Za-z0-9_-]+|\*)(?:\[([^\]]*)\])?$", seg)
    if not m:
        raise SystemExit(f"bad path segment: {seg!r}")
    return m.group(1), m.group(2)


def indices(selector, count):
    """Expand `None`, `*`, `last`, `N`, `-N` or an inclusive `a..b` against a sibling list."""
    if count <= 0:
        return []
    if selector is None:
        return [0]
    selector = selector.strip()
    if selector in ("", None):
        return [0]
    if selector in ("*", ".."):
        return list(range(count))
    if selector in ("last", "-1"):
        return [count - 1]
    if ".." in selector:
        left, _, right = selector.partition("..")
        start = int(left) if left.strip() else 0
        end = int(right) if right.strip() else count - 1
        if start < 0:
            start += count
        if end < 0:
            end += count
        start = max(0, start)
        end = min(count - 1, end)
        if start > end:
            return []
        return list(range(start, end + 1))
    try:
        index = int(selector)
    except ValueError:
        raise SystemExit(f"bad index: {selector!r}")
    if index < 0:
        index += count
    return [index] if 0 <= index < count else []


def resolve(editor, spec):
    """Turn a spec into nodes against the tree as it is right now.

    `self` is resolved here, at execution time, never from a listing the model remembers.
    """
    spec = spec.strip()
    if not spec:
        raise SystemExit("empty region path")
    segments = split_spec(spec)
    start = ""
    if segments[0] == "self":
        node = live_assistant(editor)
        if node is None:
            raise SystemExit("there is no live round to address with self")
        start = path_of(node, editor.index_of)
        segments = segments[1:]
    if any(seg == "self" for seg in segments):
        raise SystemExit("self must be the first segment: use self.reasoning, not turn[0].self")
    if not segments:
        node = editor.by_path.get(start)
        if node is None:
            raise SystemExit(f"no region matches {spec!r}; run `list` to see the paths")
        return [node]

    parents = [start]
    for seg in segments:
        name, selector = parse_segment(seg)
        found = []
        for parent in parents:
            groups = {}
            for child_name, child_path in editor.by_parent.get(parent, []):
                groups.setdefault(child_name, []).append(child_path)
            if name == "*":
                for paths in groups.values():
                    for i in indices(selector if selector is not None else "*", len(paths)):
                        found.append(paths[i])
            else:
                paths = groups.get(name, [])
                for i in indices(selector, len(paths)):
                    found.append(paths[i])
        parents = found
        if not parents:
            raise SystemExit(f"no region matches {spec!r}; run `list` to see the paths")
    return [editor.by_path[path] for path in parents]


def self_label(editor, spec, nodes):
    bits = []
    for node in nodes:
        bits.append(
            f"{path_of(node, editor.index_of)} ({content_size(editor.lines, node):,}c)"
        )
    return "self → " + ", ".join(bits)


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
            self.initial_text = fh.read()
        self.lines = self.initial_text.splitlines(keepends=True)
        self.reindex()
        self.initial_lines = len(self.lines)
        self.initial_chars = total_chars(self)

    def reindex(self):
        self.roots, self.stack, self.open_nodes = build_tree(self.lines)
        self.entries, self.index_of = [], {}
        walk(self.roots, self.index_of, self.entries)
        self.by_path = dict(self.entries)
        self.by_parent = {}
        for path, node in self.entries:
            parent = path.rsplit(".", 1)[0] if "." in path else ""
            self.by_parent.setdefault(parent, []).append((node.name, path))

    def text(self):
        return "".join(self.lines)

    def is_live(self):
        return "turn" in [node.name for node in self.open_nodes]

    def ensure_writable(self):
        if open_state(self) == "streaming":
            raise SystemExit(
                "the transcript is streaming right now (reasoning/text open); "
                "wait for the generation to end"
            )

    def resolve(self, spec):
        return resolve(self, spec)

    def targets(self, spec, closed=True):
        hits = self.resolve(spec)
        if closed:
            turn = live_turn(self)
            assistant = live_assistant(self)
            for node in hits:
                if node.close_line is not None:
                    continue
                if node is turn:
                    raise SystemExit(
                        f"{spec} is the live turn; its tool call is still running."
                    )
                if node is assistant:
                    raise SystemExit(
                        f"{spec} is the live round; its call is still running. "
                        "Summarize it with `set 'self.reasoning' | ...` instead."
                    )
                raise SystemExit(
                    f"{spec} is still open (streaming); wait for the generation to end"
                )
        return hits

    def commit(self, new_lines):
        """Check the whole file, adopt it and reindex, all before anything reaches disk."""
        why = check(new_lines)
        if why:
            raise SystemExit(f"refusing to write: {why}")
        self.lines = new_lines
        self.reindex()

    def stats(self):
        return len(self.lines), total_chars(self)

    def delta_suffix(self):
        lines, chars = self.stats()
        return (
            f"{self.initial_lines} → {lines} lines, "
            f"{self.initial_chars:,} → {chars:,} content chars; {describe_state(self)}"
        )

    def drop(self, spec):
        self.ensure_writable()
        targets = self.targets(spec)
        if self.is_live():
            assistant = live_assistant(self)
            turn = live_turn(self)
            if assistant is not None and assistant in targets:
                raise SystemExit(
                    f"{spec} is the live round; its call is still running. "
                    "Summarize it with `set 'self.reasoning' | ...` instead."
                )
            if turn is not None and turn in targets:
                raise SystemExit(f"{spec} is the live turn; its tool call is still running.")
        # Both markers go, or the open is left dangling and the file stops parsing.
        doomed = set()
        for node in targets:
            doomed.update(range(node.open_line, node.close_line + 1))
        self.commit([line for i, line in enumerate(self.lines) if i not in doomed])
        return len(targets), f"dropped {len(targets)} region(s): {self.delta_suffix()}"

    def write(self, spec, raw, append=False):
        self.ensure_writable()
        targets = self.targets(spec)
        for node in targets:
            if node.name in CONTAINERS:
                raise SystemExit(
                    f"{spec} is a whole {'round' if node.name == 'assistant' else 'turn'}; "
                    "target reasoning/text/note instead"
                )
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
        if split_spec(spec)[0] == "self":
            label = self_label(self, spec, targets)
        else:
            label = f"{spec} ({len(targets)} region(s))"
        self.commit(keep)
        verb = "appended to" if append else "set"
        return len(targets), f"{verb} {label}: {self.delta_suffix()}"

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
        self.ensure_writable()
        parent_spec, _, new_name = spec.rpartition(".")
        if not new_name:
            raise SystemExit("add needs a full path like turn.note or turn.assistant.scratchpad")
        new_base, new_index = parse_segment(new_name)
        if new_base == "*" or new_index is not None:
            raise SystemExit(
                "add creates a new region; omit its index (use set or drop for an existing one)"
            )
        if new_base in ("input", "begin"):
            raise SystemExit(f"{new_base!r} is a structural region; it cannot be added")
        # Walk the path one segment at a time, creating any container that is missing, so
        # `turn[0].assistant[0].scratchpad` works on a session that has neither yet.
        segments = split_spec(parent_spec) if parent_spec else []
        holder = None
        for depth, seg in enumerate(segments):
            prefix = ".".join(segments[: depth + 1])
            try:
                found = self.resolve(prefix)
            except SystemExit:
                found = []
            if found:
                if len(found) > 1:
                    raise SystemExit(
                        f"{prefix} matches {len(found)} regions; add needs one parent"
                    )
                holder = found[0]
                continue
            base, _ = parse_segment(seg)
            if base not in CONTAINERS:
                raise SystemExit(
                    f"cannot create {base!r}; only `turn` and `assistant` are created for you"
                )
            if holder is None:
                holder = self.begin()
            self.insert_into(holder, base, [f"{OPEN}{base} -->\n", f"{CLOSE}\n"])
            self.reindex()
            try:
                holder = self.resolve(prefix)[0]
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
        holder_path = path_of(holder, self.index_of)
        block = [f"{OPEN}{new_base} -->\n"] + to_lines(escape(raw)) + [f"{CLOSE}\n"]
        self.insert_into(holder, new_base, block)
        self.commit(list(self.lines))
        created = [
            path for name, path in self.by_parent.get(holder_path, []) if name == new_base
        ]
        new_path = created[-1] if created else (
            f"{holder_path}.{new_base}" if holder_path else new_base
        )
        size = content_size(self.lines, self.by_path[new_path])
        return 1, f"added {new_path} ({size:,}c): {self.delta_suffix()}"

    def save(self):
        with open(self.path) as fh:
            current = fh.read()
        if current != self.initial_text:
            raise SystemExit(
                "the transcript changed while this edit was being prepared "
                "(a user message or a subagent report landed). Nothing was written; "
                "run `list` again and re-apply."
            )
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
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z0-9_]+)\1\s*$")


def parse_batch(text):
    """`drop PATH` / `set PATH | BODY` / `add PATH <<'END'` lines -> ops, in one pass."""
    raw_lines = text.splitlines()
    ops = []
    i = 0
    while i < len(raw_lines):
        raw = raw_lines[i]
        lineno = i + 1
        i += 1
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        head, sep, body = raw.partition("|")
        heredoc_body = None
        match = HEREDOC.search(head)
        if match:
            tag = match.group(2)
            head = head[: match.start()].rstrip()
            chunk = []
            closed = False
            while i < len(raw_lines):
                candidate = raw_lines[i]
                i += 1
                if candidate.strip() == tag:
                    closed = True
                    break
                chunk.append(candidate)
            if not closed:
                raise SystemExit(f"batch line {lineno}: heredoc {tag!r} is never closed")
            heredoc_body = "\n".join(chunk)
            if heredoc_body and not heredoc_body.endswith("\n"):
                heredoc_body += "\n"
        op, _, spec = head.strip().partition(" ")
        op, spec = op.strip(), spec.strip()
        if op not in BATCH_OPS:
            raise SystemExit(
                f"batch line {lineno}: unknown op {op!r}; use drop, set, append or add"
            )
        if not spec:
            raise SystemExit(f"batch line {lineno}: {op} needs a region path")
        if len(spec) > 1 and spec[0] == spec[-1] and spec[0] in "'\"":
            spec = spec[1:-1]
        if heredoc_body is not None and sep:
            raise SystemExit(
                f"batch line {lineno}: use either `| body` or a <<'TAG' heredoc, not both"
            )
        if op == "drop":
            if sep or heredoc_body is not None:
                raise SystemExit(f"batch line {lineno}: drop takes no body")
        elif heredoc_body is None:
            if not sep:
                raise SystemExit(
                    f"batch line {lineno}: {op} needs `| body` or a <<'TAG' heredoc"
                )
            if body.startswith(" "):
                body = body[1:]
        else:
            body = heredoc_body
        ops.append((lineno, op, spec, body))
    return ops


def plan_batch(editor, ops):
    """Turn the whole batch into line replacements against the pre-batch snapshot.

    Resolving every op up front is the point: paths keep the meaning they had in the `list` that
    preceded the batch, so dropping `turn[0].result` cannot renumber `turn[1].result`.
    """
    editor.ensure_writable()
    drops = []  # (start, end, lineno) end exclusive
    edits = []  # (start, end, seq, lines, lineno)
    inserts = []  # (at, seq, closes, block, lineno, holder)
    labels = []
    regions = 0
    for seq, (lineno, op, spec, body) in enumerate(ops):
        try:
            if op == "drop":
                nodes = editor.targets(spec)
                if split_spec(spec)[0] == "self":
                    labels.append(self_label(editor, spec, nodes))
                drops.extend((n.open_line, n.close_line + 1, lineno) for n in nodes)
                regions += len(nodes)
            elif op in ("set", "append"):
                nodes = editor.targets(spec)
                if split_spec(spec)[0] == "self":
                    labels.append(self_label(editor, spec, nodes))
                for node in nodes:
                    if node.name in CONTAINERS:
                        raise SystemExit(
                            f"{spec} is a whole "
                            f"{'round' if node.name == 'assistant' else 'turn'}; "
                            "target reasoning/text/note instead"
                        )
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
                new_base, new_index = parse_segment(new_name)
                if new_base == "*" or new_index is not None:
                    raise SystemExit(
                        "add creates a new region; omit its index (use set or drop for an existing one)"
                    )
                if new_base in ("input", "begin"):
                    raise SystemExit(f"{new_base!r} is a structural region; it cannot be added")
                if parent_spec:
                    found = editor.resolve(parent_spec)
                    if len(found) > 1:
                        raise SystemExit(
                            f"{parent_spec} matches {len(found)} regions; add needs one parent"
                        )
                    holder = found[0]
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
    return out, regions, labels


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
        new_lines, regions, labels = plan_batch(editor, ops)
        for label in labels:
            print(label)
        editor.commit(new_lines)
        editor.save()
        print(
            f"applied {len(ops)} {'edit' if len(ops) == 1 else 'edits'} ({regions} regions): "
            f"{editor.delta_suffix()}"
        )
        return

    if args.cmd == "list":
        print(list_output(editor))
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
