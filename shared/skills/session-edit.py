#!/usr/bin/env python3
"""Address regions of a slopfile session transcript by name, without ever typing a marker.

Rewriting a whole transcript with `head`/`sed`/heredoc works but is brittle: it hardcodes line
numbers, and the markers are escaped on disk (`<!--slopfile:ESC[<<!--/slopfile:ESC-->!-- slopfile:x -->`)
so a targeted `edit` has to reproduce that wrapper exactly. This does the same job structurally:
you name a region, you get or set its body, and this handles the markers and the escaping.

  ./session-edit.py list   SESSION
  ./session-edit.py read   SESSION turn[3].assistant.text
  ./session-edit.py set    SESSION turn[3].note --body "..."
  ./session-edit.py append SESSION turn[3].note --body "..."
  ./session-edit.py drop   SESSION turn[1].rounds            (removes matching regions)
  ./session-edit.py check  SESSION

A path is `name` or `name[n]` segments joined by dots, outermost first. `list` prints every path
you can address. Repeated names need an index; the ones that always repeat can use `[*]` to mean
"all of them" for `drop`.

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
    while node is not None and node.parent is not None:
        n = index_of[node]
        parts.append(node.name if n == 0 else f"{node.name}[{n}]")
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


def walk(nodes, index_of, out, counters):
    for node in nodes:
        idx = counters.get(node.name, 0)
        counters[node.name] = idx + 1
        index_of[node] = idx
        p = path_of(node, index_of)
        if node.parent is not None:
            out.append((p, node))
        walk(node.children, index_of, out, counters)


def resolve(entries, spec):
    """`turn[3].assistant.text` -> that node. A bare name matches its first occurrence."""
    parts = []
    for seg in spec.split("."):
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
            name = got.split("[")[0]
            if name != want:
                ok = False
                break
            if ix is not None and ix != "*" and got != f"{want}[{ix}]":
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
IN_TURN = {"user", "note", "assistant"}
IN_ASSISTANT = {"reasoning", "text", "call"}


def check(lines, original):
    """Refuse anything the harness would roll back, so a bad edit never reaches disk."""
    if not lines:
        return "edit produced an empty file"
    if not original.split("---")[0].strip() == "".join(lines[:1]).strip() and "```toml" not in original:
        pass
    stack = []
    for line in lines:
        if is_open(line):
            name, _ = parse_name(line)
            if name is None:
                return f"unreadable region marker: {line.strip()[:60]}"
            if name in ("call", "result", "question"):
                pass
            stack.append(name)
        elif is_close(line):
            if not stack:
                return f"{CLOSE} closes a region that was never opened"
            stack.pop()
    # `begin` and `input` are legitimately open: a live session always ends inside the input
    # region so the user can keep typing. Anything else left open is a real break.
    leftover = [n for n in stack if n not in ("begin", "input")]
    if leftover:
        return f"unclosed region(s): {', '.join(leftover)}"
    text = "".join(lines)
    if text.count("```toml") == 1 and not text.lstrip().startswith("```toml"):
        head = text.split("```toml")[0].strip()
        if head:
            return "the ```toml fence must stay at the top of the file"
    # Placement, not just balance: the harness refuses a region that sits somewhere it has no
    # meaning, and an edit that gets rolled back is worse than one that never ran.
    stack = []
    for line in lines:
        if is_open(line):
            name, payload = parse_name(line)
            if payload:
                try:
                    json.loads(payload)
                except Exception as exc:
                    return f"the {name} header is not valid JSON: {exc}"
            parent = stack[-1] if stack else None
            if parent is None:
                if name not in TOP_LEVEL:
                    return f"`{name}` may not sit at the top level (allowed: {', '.join(sorted(TOP_LEVEL - {'begin'}))})"
            elif parent == "turn":
                if name not in IN_TURN and name in KNOWN:
                    return f"`{name}` does not belong inside a `turn`; allowed: {', '.join(sorted(IN_TURN))}, or a role of your own"
            elif parent == "assistant":
                if name not in IN_ASSISTANT and name in KNOWN:
                    return f"`{name}` does not belong inside an `assistant`; allowed: {', '.join(sorted(IN_ASSISTANT))}, or a role of your own"
            stack.append(name)
        elif is_close(line) and stack:
            stack.pop()
    return None


def write_atomic(path, text):
    tmp = f"{path}.session-edit.{os.getpid()}.tmp"
    with open(tmp, "w") as fh:
        fh.write(text)
    os.replace(tmp, path)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["list", "read", "set", "append", "add", "drop", "check"])
    ap.add_argument("session")
    ap.add_argument("path", nargs="?")
    ap.add_argument("--body", help="new body text; use '-' to read the body from stdin")
    args = ap.parse_args()

    if args.cmd not in ("list", "check") and not args.path:
        ap.error(f"{args.cmd} needs a region path")

    with open(args.session) as fh:
        original = fh.read()
    lines = original.splitlines(keepends=True)
    roots, stack = build_tree(lines)
    entries, index_of, counters = [], {}, {}
    walk(roots, index_of, entries, counters)

    if args.cmd == "list":
        print(f"{len(lines)} lines, {'unclosed: ' + ','.join(n.name for n in stack) if stack else 'balanced'}")
        for path, node in entries:
            payload = f"  {node.payload[:60]}" if node.payload else ""
            print(f"  {path}{payload}")
        return
    if args.cmd == "check":
        why = check(lines, original)
        print(why or "ok")
        raise SystemExit(1 if why else 0)

    if args.cmd == "add":
        parent_spec, _, new_name = args.path.rpartition(".")
        if not new_name:
            raise SystemExit("add needs a full path like turn.note or turn.assistant.scratchpad")
        raw = sys.stdin.read() if args.body == "-" else (args.body or "")

        def reindex():
            nonlocal lines, original, roots, entries, index_of, counters
            original = "".join(lines)
            roots, stack = build_tree(lines)
            entries, index_of, counters = [], {}, {}
            walk(roots, index_of, entries, counters)
            return stack

        def insert_into(holder, seg, body_lines):
            """Put a new region inside `holder`, above the input region when it sits under begin."""
            if holder.name == BEGIN:
                inp = find_input(roots)
                at = inp.open_line if inp is not None else (holder.close_line or len(lines))
            else:
                at = holder.close_line
            if at is None:
                raise SystemExit(f"cannot find where to create {seg!r}")
            lines[at:at] = body_lines

        # Walk the path one segment at a time, creating any container that is missing, so
        # `turn.assistant.scratchpad` works on a session that has neither yet.
        segments = parent_spec.split(".") if parent_spec else []
        holder = None
        for depth, seg in enumerate(segments):
            spec = ".".join(segments[: depth + 1])
            try:
                holder = resolve(entries, spec)[0]
                continue
            except SystemExit:
                pass
            if seg not in ("turn", "assistant"):
                raise SystemExit(
                    f"cannot create {seg!r}; only `turn` and `assistant` are created for you"
                )
            if holder is None:
                tops = [n for n in roots if n.name == BEGIN]
                if not tops:
                    raise SystemExit("this file has no <!-- slopfile:begin --> region")
                holder = tops[0]
            insert_into(holder, seg, [f"{OPEN}{seg} -->\n", f"{CLOSE}\n"])
            reindex()
            holder = resolve(entries, spec)[0]
        if holder is None:
            tops = [n for n in roots if n.name == BEGIN]
            if not tops:
                raise SystemExit("this file has no <!-- slopfile:begin --> region")
            holder = tops[0]

        if new_name not in KNOWN and holder.name not in ("turn", "assistant"):
            raise SystemExit(
                f"{new_name!r} may only be invented inside a turn or an assistant, not inside "
                f"{holder.name!r}"
            )
        block = [f"{OPEN}{new_name} -->\n"] + to_lines(escape(raw)) + [f"{CLOSE}\n"]
        insert_into(holder, new_name, block)
        original = "".join(lines)
        why = check(lines, original)
        if why:
            raise SystemExit(f"refusing to write: {why}")
        write_atomic(args.session, original)
        print(f"added {new_name} inside {parent_spec or 'the top level'}")
        return

    targets = resolve(entries, args.path)
    if args.cmd == "read":
        for node in targets:
            sys.stdout.write(unescape(body_text(lines, node)))
        return

    if args.cmd == "drop":
        # Both markers go, or the open is left dangling and the file stops parsing.
        doomed = set()
        for node in targets:
            if node.close_line is None:
                raise SystemExit(f"{args.path} is unclosed; the file is mid-generation, try again shortly")
            end = node.close_line
            for i in range(node.open_line, end + 1):
                doomed.add(i)
        keep = [l for i, l in enumerate(lines) if i not in doomed]
        why = check(keep, original)
        if why:
            raise SystemExit(f"refusing to write: {why}")
        write_atomic(args.session, "".join(keep))
        print(f"dropped {len(targets)} region(s)")
        return

    raw = sys.stdin.read() if args.body == "-" else (args.body or "")
    edits = []
    for node in targets:
        if node.close_line is None:
            raise SystemExit(f"{args.path} is unclosed; the file is mid-generation, try again shortly")
        new = escape(raw) if args.cmd == "set" else body_text(lines, node).rstrip("\n") + "\n" + escape(raw)
        edits.append((node.body_start, node.close_line, to_lines(new)))
    keep = list(lines)
    for start, end, body in sorted(edits, reverse=True):
        keep[start:end] = body
    why = check(keep, original)
    if why:
        raise SystemExit(f"refusing to write: {why}")
    write_atomic(args.session, "".join(keep))
    verb = "set" if args.cmd == "set" else "appended to"
    print(f"{verb} {args.path} ({len(targets)} region(s))")


if __name__ == "__main__":
    main()