#!/usr/bin/env python3
"""Tests for session-edit.py. Run with `python3 -m unittest shared/skills/test_session_edit.py`."""

import contextlib
import importlib.util
import io
import pathlib
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("session_edit", HERE / "session-edit.py")
se = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(se)

FENCE = '```toml\nmodel = "m"\n```\n'
BEGIN = "<!-- slopfile:begin -->\n"
END = "<!-- /slopfile -->\n"
INPUT = "<!-- slopfile:input -->\n"
SYSTEM = "<!-- slopfile:system -->\nsys\n" + END
ECHO_ARGS = '{"command":"echo hi"}'
CALL = (
    '<!-- slopfile:call {"id":"c1","name":"bash","arguments":'
    + '"{\\"command\\":\\"echo hi\\"}"} -->\n'
    + END
)


def balanced():
    """Two closed turns; the first has a call, a result and a note. 56 content chars."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\nhello\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nr0\n" + END
        + "<!-- slopfile:text -->\nt0\n" + END
        + CALL
        + END  # assistant
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true,"ms":3} -->\nout\n' + END
        + "<!-- slopfile:note -->\nn0\n" + END
        + END  # turn
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\nagain\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nr1\n" + END
        + "<!-- slopfile:text -->\nt1\n" + END
        + END  # assistant
        + END  # turn
        + INPUT
    )


def many_turns(count):
    out = [FENCE, BEGIN, SYSTEM]
    for i in range(count):
        out.append(
            "<!-- slopfile:turn -->\n"
            f"<!-- slopfile:user -->\nu{i}\n" + END
            + "<!-- slopfile:assistant -->\n"
            f"<!-- slopfile:reasoning -->\nr{i}\n" + END
            + f'<!-- slopfile:call {{"id":"c{i}","name":"bash","arguments":"{{}}"}} -->\n' + END
            + END  # assistant
            + f'<!-- slopfile:result {{"id":"c{i}","name":"bash","ok":true}} -->\no{i}\n' + END
            + END  # turn
        )
    out.append(INPUT)
    return "".join(out)


def duplicate_ids_live():
    """Two calls sharing one id, the second running `list`: pairing must fall back to order."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nthought\n" + END
        + '<!-- slopfile:call {"id":"c1","name":"bash","arguments":'
        + '"{\\\"command\\\":\\\"echo first\\\"}"} -->\n' + END
        + '<!-- slopfile:call {"id":"c1","name":"bash","arguments":'
        + '"{\\\"command\\\":\\\"python3 session-edit.py list s.md\\\"}"} -->\n' + END
        + END
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true} -->\nfirst out\n' + END
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true} -->\nlisting output\n' + END
        + END
        + INPUT
    )


def live():
    """Tool-time state: assistant open, its reasoning closed, generation over."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + '<!-- slopfile:call {"id":"c2","name":"bash","arguments":'
        + '"{\\"command\\":\\"python3 /shared/skills/session-edit.py list '
        + '/shared/transcripts/s.md\\"}"} -->\n'
        + END
        + INPUT
    )


def live_no_call():
    """The real tool-time state: assistant open, reasoning closed, no call yet.

    The daemon writes calls when the round settles, after the tool returns, so a listing
    taken from inside a call cannot see its own call.
    """
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + INPUT
    )


def live_with_previous():
    """One settled round, then a live round with no call yet (the batch being planned)."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nfirst thought\n" + END
        + CALL
        + END  # assistant
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true} -->\nfirst out\n' + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + INPUT
    )


def live_multi():
    """A live round that ran `list` alongside another call; results settle after both."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + '<!-- slopfile:call {"id":"c2","name":"bash","arguments":'
        + '"{\\"command\\":\\"python3 /shared/skills/session-edit.py list '
        + '/shared/transcripts/s.md\\"}"} -->\n'
        + END
        + '<!-- slopfile:call {"id":"c3","name":"bash","arguments":'
        + '"{\\"command\\":\\"echo other\\"}"} -->\n'
        + END
        + INPUT
    )


def live_multi_after_list():
    """The next round after a multi-call round that contained `list`."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + '<!-- slopfile:call {"id":"c2","name":"bash","arguments":'
        + '"{\\"command\\":\\"python3 /shared/skills/session-edit.py list '
        + '/shared/transcripts/s.md\\"}"} -->\n'
        + END
        + '<!-- slopfile:call {"id":"c3","name":"bash","arguments":'
        + '"{\\"command\\":\\"echo other\\"}"} -->\n'
        + END
        + END  # assistant
        + '<!-- slopfile:result {"id":"c2","name":"bash","ok":true} -->\nlisting output\n' + END
        + '<!-- slopfile:result {"id":"c3","name":"bash","ok":true} -->\nother output\n' + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nread the listing\n" + END
        + '<!-- slopfile:call {"id":"c4","name":"bash","arguments":'
        + '"{\\"command\\":\\"batch\\"}"} -->\n'
        + END
        + INPUT
    )


def live_with_result():
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nfirst thought\n" + END
        + "<!-- slopfile:call {\"id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{}\"} -->\n" + END
        + END  # assistant
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true} -->\nfirst out\n' + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nsecond thought\n" + END
        + "<!-- slopfile:call {\"id\":\"c2\",\"name\":\"bash\",\"arguments\":\"{}\"} -->\n" + END
        + INPUT
    )


def streaming():
    """Actually mid-generation: the reasoning section is still open."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nstill writing\n"
        + INPUT
    )


def noted():
    """Live turn with a note: the harness closes the assistant, the turn stays open."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\ndo it\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:reasoning -->\nthought\n" + END
        + END  # assistant
        + "<!-- slopfile:note -->\nsummary\n" + END
        + INPUT
    )


def checkpoint():
    """A note pinned between a call and its result, then a failed round and a live round."""
    return (
        FENCE
        + BEGIN
        + SYSTEM
        + "<!-- slopfile:turn -->\n"
        + "<!-- slopfile:user -->\nresearch\n" + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:text -->\nsearch X\n" + END
        + '<!-- slopfile:call {"id":"c1","name":"bash","arguments":"{}"} -->\n' + END
        + END  # assistant
        + "<!-- slopfile:note -->\ntried X\n" + END
        + '<!-- slopfile:result {"id":"c1","name":"bash","ok":true} -->\nout\n' + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:text -->\nsearch Y\n" + END
        + '<!-- slopfile:call {"id":"c2","name":"bash","arguments":"{}"} -->\n' + END
        + END  # assistant
        + '<!-- slopfile:result {"id":"c2","name":"bash","ok":true} -->\nout\n' + END
        + "<!-- slopfile:assistant -->\n"
        + "<!-- slopfile:text -->\nsearch Z\n" + END
        + '<!-- slopfile:call {"id":"c3","name":"bash","arguments":"{}"} -->\n' + END
        + INPUT
    )


class Base(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def editor(self, text, name="s.md"):
        path = pathlib.Path(self.dir.name, name)
        path.write_text(text)
        return se.Editor(str(path)), path


class TestList(Base):
    def test_states(self):
        for text, want in (
            (balanced(), "balanced"),
            (live(), "live: turn[0]"),
            (streaming(), "streaming: turn[0].assistant[0].reasoning[0]"),
        ):
            editor, _ = self.editor(text)
            output = se.list_output(editor)
            self.assertIn(want, output.splitlines()[0])
            self.assertNotIn("unclosed:", output)

    def test_live_markers(self):
        editor, _ = self.editor(live())
        output = se.list_output(editor)
        self.assertIn("← live (self)", output)
        self.assertNotIn("← streaming", output)

        editor, _ = self.editor(streaming())
        output = se.list_output(editor)
        self.assertIn("← streaming", output)
        self.assertNotIn("← live (self)", output)

    def test_compaction_hint_only_when_live(self):
        editor, _ = self.editor(live())
        output = se.list_output(editor)
        self.assertIn("turn[0].assistant[0]", output)
        self.assertIn("turn[0].result[0]", output)
        editor, _ = self.editor(balanced())
        self.assertNotIn("compacting?", se.list_output(editor))

    def test_compaction_hint_drops_only_the_list_call_when_the_round_has_others(self):
        editor, _ = self.editor(live_multi())
        output = se.list_output(editor)
        self.assertIn("turn[0].assistant[0].call[0]", output)
        self.assertIn("turn[0].result[0]", output)
        self.assertIn("2 calls", output)
        self.assertNotIn("#   drop 'turn[0].assistant[0]'", output)

    def test_hint_for_the_live_round_without_a_call(self):
        editor, _ = self.editor(live_no_call())
        output = se.list_output(editor)
        self.assertIn("this listing is running inside the live round", output)
        self.assertIn("turn[0].assistant[0]", output)
        self.assertIn("turn[0].result[0]", output)

    def test_hint_for_a_live_round_after_a_settled_round(self):
        editor, _ = self.editor(live_with_previous())
        output = se.list_output(editor)
        self.assertIn("this listing is running inside the live round", output)
        self.assertIn("turn[0].assistant[1]", output)
        self.assertIn("turn[0].result[1]", output)
        self.assertNotIn("#   drop 'turn[0].assistant[0]'", output)

    def test_multi_call_cleanup_keeps_the_other_call_and_result(self):
        editor, _ = self.editor(live_multi_after_list())
        ops = se.parse_batch(
            "set 'self.reasoning' | summary\n"
            "drop 'turn[0].assistant[0].call[0]'\n"
            "drop 'turn[0].result[0]'\n"
        )
        new_lines, _, _ = se.plan_batch(editor, ops)
        editor.commit(new_lines)
        text = editor.text()
        self.assertNotIn("session-edit.py list", text)
        self.assertNotIn("listing output", text)
        self.assertIn("echo other", text)
        self.assertIn("other output", text)
        self.assertIsNone(se.check(editor.lines))

    def test_exact_characters(self):
        editor, _ = self.editor(balanced())
        output = se.list_output(editor)
        self.assertIn("56 content chars; balanced", output.splitlines()[0])
        self.assertIn("system[0]  4c", output)
        self.assertIn("turn[0]  40c", output)
        self.assertIn("assistant[0]  27c", output)
        self.assertIn(f"call[0]  {len(ECHO_ARGS)}c", output)
        self.assertIn("result[0]  4c", output)
        self.assertIn("turn[1]  12c", output)


class TestSelectors(Base):
    def test_self(self):
        editor, _ = self.editor(live())
        self.assertEqual(
            se.path_of(editor.resolve("self.reasoning")[0], editor.index_of),
            "turn[0].assistant[0].reasoning[0]",
        )

        editor, _ = self.editor(balanced())
        self.assertEqual(
            se.path_of(editor.resolve("self.text")[0], editor.index_of),
            "turn[1].assistant[0].text[0]",
        )

    def test_ranges_and_last(self):
        editor, _ = self.editor(many_turns(6))

        def paths(spec):
            return [se.path_of(n, editor.index_of) for n in editor.resolve(spec)]

        self.assertEqual(paths("turn[-1]"), ["turn[5]"])
        self.assertEqual(paths("turn[last]"), ["turn[5]"])
        self.assertEqual(paths("turn[4..]"), ["turn[4]", "turn[5]"])
        self.assertEqual(paths("turn[2..3]"), ["turn[2]", "turn[3]"])
        self.assertEqual(paths("turn[..1]"), ["turn[0]", "turn[1]"])
        self.assertEqual(len(paths("turn[*].result[*]")), 6)

    def test_bare_name_is_first(self):
        editor, _ = self.editor(many_turns(3))
        self.assertEqual(
            [se.path_of(n, editor.index_of) for n in editor.resolve("turn")], ["turn[0]"]
        )
        self.assertEqual(
            [se.path_of(n, editor.index_of) for n in editor.resolve("turn.user")],
            ["turn[0].user[0]"],
        )

    def test_self_must_be_first(self):
        editor, _ = self.editor(live())
        with self.assertRaises(SystemExit) as caught:
            editor.resolve("turn[0].self.reasoning")
        self.assertIn("self must be the first segment", str(caught.exception))

    def test_self_call_explains_the_in_flight_call(self):
        editor, _ = self.editor(live_no_call())
        with self.assertRaises(SystemExit) as caught:
            editor.resolve("self.call[0]")
        message = str(caught.exception)
        self.assertIn("not written until the tool returns", message)
        self.assertIn("prev", message)

    def test_self_result_explains_turn_level_results(self):
        editor, _ = self.editor(live_no_call())
        with self.assertRaises(SystemExit) as caught:
            editor.resolve("self.result[0]")
        message = str(caught.exception)
        self.assertIn("children of the turn", message)
        self.assertIn("turn[N].result[M]", message)

    def test_prev_is_the_last_settled_round(self):
        editor, _ = self.editor(live_with_previous())
        self.assertEqual(
            [se.path_of(n, editor.index_of) for n in editor.resolve("prev")],
            ["turn[0].assistant[0]", "turn[0].result[0]"],
        )
        self.assertEqual(
            se.path_of(editor.resolve("prev.reasoning")[0], editor.index_of),
            "turn[0].assistant[0].reasoning[0]",
        )

    def test_round_expands_to_assistant_and_results(self):
        editor, _ = self.editor(live_with_previous())
        self.assertEqual(
            [se.path_of(n, editor.index_of) for n in editor.resolve("turn[0].round[0]")],
            ["turn[0].assistant[0]", "turn[0].result[0]"],
        )

    def test_round_must_be_last(self):
        editor, _ = self.editor(live_with_previous())
        with self.assertRaises(SystemExit) as caught:
            editor.resolve("turn[0].round[0].reasoning")
        self.assertIn("must be the last segment", str(caught.exception))

    def test_drop_prev_removes_the_whole_round(self):
        editor, _ = self.editor(live_with_previous())
        editor.drop("prev")
        text = editor.text()
        self.assertNotIn("first thought", text)
        self.assertNotIn("first out", text)
        self.assertIn("second thought", text)
        self.assertIsNone(se.check(editor.lines))

    def test_dropping_a_result_alone_warns(self):
        editor, _ = self.editor(balanced())
        with contextlib.redirect_stderr(io.StringIO()) as err:
            editor.drop("turn[0].result[0]")
        self.assertIn("without its call", err.getvalue())


class TestLiveGuards(Base):
    def test_drop_live_round(self):
        editor, _ = self.editor(live())
        with self.assertRaises(SystemExit) as caught:
            editor.drop("turn[0].assistant[0]")
        self.assertIn("live round", str(caught.exception))
        self.assertIn("self.reasoning", str(caught.exception))

    def test_drop_live_round_when_noted(self):
        editor, _ = self.editor(noted())
        with self.assertRaises(SystemExit) as caught:
            editor.drop("turn[0].assistant[0]")
        self.assertIn("live round", str(caught.exception))

    def test_drop_live_turn(self):
        editor, _ = self.editor(live())
        with self.assertRaises(SystemExit) as caught:
            editor.drop("turn[0]")
        self.assertIn("live turn", str(caught.exception))

    def test_add_note_closes_live_assistant(self):
        editor, path = self.editor(live())
        _, message = editor.add("turn[-1].note", "summary")
        self.assertIn("turn[0].note[0]", message)
        self.assertIsNone(se.check(editor.lines))
        self.assertNotIn("assistant[0]  ← live", se.list_output(editor))
        editor.save()
        self.assertIsNone(se.check(pathlib.Path(path).read_text().splitlines(keepends=True)))

    def test_streaming_refuses_writes(self):
        editor, _ = self.editor(streaming())
        self.assertIn("streaming", se.list_output(editor))
        reasoning = editor.resolve("turn[0].assistant[0].reasoning")[0]
        self.assertIn("still writing", se.unescape(se.body_text(editor.lines, reasoning)))
        with self.assertRaises(SystemExit) as caught:
            editor.write("turn[0].assistant[0].reasoning", "x")
        self.assertIn("streaming right now", str(caught.exception))
        with self.assertRaises(SystemExit):
            editor.add("turn[-1].note", "x")
        with self.assertRaises(SystemExit):
            editor.drop("turn[0].user[0]")
        with self.assertRaises(SystemExit):
            se.plan_batch(editor, se.parse_batch("set 'turn[0].user[0]' | x\n"))


class TestReceipts(Base):
    def test_add_reports_path(self):
        editor, _ = self.editor(balanced())
        _, message = editor.add("turn[1].note", "remember")
        self.assertIn("added turn[1].note[0]", message)
        self.assertIn("content chars", message)
        self.assertIsNone(se.check(editor.lines))

    def test_self_receipt_resolves_path(self):
        editor, _ = self.editor(live())
        _, message = editor.write("self.reasoning", "pruned")
        self.assertIn("self → turn[0].assistant[0].reasoning[0]", message)
        self.assertIn("(15c)", message)
        self.assertIn("content chars", message)


class TestBatch(Base):
    def test_heredoc_and_self_in_one_batch(self):
        editor, path = self.editor(live_with_result())
        batch = (
            "drop 'turn[0].result[*]'\n"
            "set 'self.reasoning' | pruned\n"
            "add 'turn[-1].note' <<'END'\n"
            "## Findings\n"
            "- a\n"
            "- b\n"
            "END\n"
        )
        ops = se.parse_batch(batch)
        new_lines, regions, labels = se.plan_batch(editor, ops)
        self.assertTrue(any("self → turn[0].assistant[1].reasoning[0]" in l for l in labels))
        editor.commit(new_lines)
        self.assertIsNone(se.check(editor.lines))
        editor.save()

        reloaded = se.Editor(str(path))
        note = reloaded.resolve("turn[0].note[0]")[0]
        self.assertEqual(se.unescape(se.body_text(reloaded.lines, note)),
                         "## Findings\n- a\n- b\n")
        self.assertEqual(
            [p for name, p in reloaded.by_parent.get("turn[0]", []) if name == "result"], []
        )
        self.assertEqual(
            se.unescape(se.body_text(reloaded.lines, reloaded.resolve("self.reasoning")[0])),
            "pruned\n",
        )

    def test_heredoc_never_closed(self):
        with self.assertRaises(SystemExit) as caught:
            se.parse_batch("add 'turn[0].note' <<'END'\nbody\n")
        self.assertIn("never closed", str(caught.exception))

    def test_heredoc_and_pipe_conflict(self):
        with self.assertRaises(SystemExit) as caught:
            se.parse_batch("add 'turn[0].note' <<'END' | body\nbody\nEND\n")
        self.assertIn("not both", str(caught.exception))

    def test_rollback_to_a_checkpoint_rewrites_one_note(self):
        editor, path = self.editor(checkpoint())
        batch = (
            "drop 'turn[0].assistant[1]'\n"
            "drop 'turn[0].result[1]'\n"
            "drop 'turn[0].note[0]'\n"
            "add 'turn[-1].note' <<'END'\n"
            "tried X and Y: no results; continue with Z\n"
            "END\n"
        )
        new_lines, _, _ = se.plan_batch(editor, se.parse_batch(batch))
        editor.commit(new_lines)
        self.assertIsNone(se.check(editor.lines))
        editor.save()

        reloaded = se.Editor(str(path))
        self.assertEqual(len(reloaded.resolve("turn[0].assistant[*]")), 2)
        self.assertEqual(len(reloaded.resolve("turn[0].result[*]")), 1)
        notes = reloaded.resolve("turn[0].note[*]")
        self.assertEqual(len(notes), 1)
        self.assertIn(
            "continue with Z", se.unescape(se.body_text(reloaded.lines, notes[0]))
        )
        # The fresh note follows the live assistant, so the live call's result will land after it.
        live = reloaded.resolve("turn[0].assistant[1]")[0]
        self.assertGreater(notes[0].open_line, live.open_line)

    def test_append_extends_a_kept_checkpoint(self):
        editor, _ = self.editor(checkpoint())
        editor.write("turn[0].note[0]", "tried Y too", append=True)
        note = editor.resolve("turn[0].note[0]")[0]
        body = se.unescape(se.body_text(editor.lines, note))
        self.assertIn("tried X", body)
        self.assertIn("tried Y too", body)
        self.assertLess(body.index("tried X"), body.index("tried Y too"))

    def test_tail_cut_without_self(self):
        editor, _ = self.editor(many_turns(6))
        batch = "drop 'turn[4..5]'\nadd 'turn[3].note' <<'END'\nsummary\nEND\n"
        new_lines, _, _ = se.plan_batch(editor, se.parse_batch(batch))
        editor.commit(new_lines)
        self.assertIsNone(se.check(editor.lines))
        self.assertEqual(len(editor.resolve("turn[*]")), 4)

    def test_prev_drops_the_whole_round_in_a_batch(self):
        editor, _ = self.editor(live_with_previous())
        new_lines, _, _ = se.plan_batch(editor, se.parse_batch("drop 'prev'\n"))
        editor.commit(new_lines)
        text = editor.text()
        self.assertNotIn("first thought", text)
        self.assertNotIn("first out", text)
        self.assertIn("second thought", text)
        self.assertIsNone(se.check(editor.lines))

    def test_round_range_drops_assistants_and_results(self):
        editor, _ = self.editor(many_turns(3))
        batch = "drop 'turn[1..2].round[*]'\n"
        new_lines, _, _ = se.plan_batch(editor, se.parse_batch(batch))
        editor.commit(new_lines)
        self.assertEqual(len(editor.resolve("turn[*].assistant[*]")), 1)
        self.assertEqual(len(editor.resolve("turn[*].result[*]")), 1)
        self.assertIsNone(se.check(editor.lines))


class TestFreshness(Base):
    def test_refuses_when_file_changed(self):
        editor, path = self.editor(balanced())
        path.write_text(balanced() + "\n")
        editor.write("turn[0].note[0]", "replacement")
        with self.assertRaises(SystemExit) as caught:
            editor.save()
        self.assertIn("changed while this edit was being prepared", str(caught.exception))
        self.assertEqual(path.read_text(), balanced() + "\n")


class TestCheck(Base):
    def test_successful_writes_stay_valid(self):
        editor, _ = self.editor(balanced())
        editor.write("turn[0].note[0]", "new note")
        self.assertIsNone(se.check(editor.lines))
        editor.add("turn[1].scratchpad", "state")
        self.assertIsNone(se.check(editor.lines))
        editor.drop("turn[0].result[*]")
        self.assertIsNone(se.check(editor.lines))


class TestDropReminder(Base):
    def test_drop_prints_the_skill_reminder(self):
        editor, _ = self.editor(balanced())
        with contextlib.redirect_stderr(io.StringIO()) as err:
            editor.drop("turn[0].result[*]")
            editor.save()
            se.drop_reminder()
        self.assertIn("session-edit.md", err.getvalue())
        self.assertIn("removed the session-edit skill", err.getvalue())

    def test_write_without_a_drop_prints_nothing(self):
        editor, _ = self.editor(balanced())
        with contextlib.redirect_stderr(io.StringIO()) as err:
            editor.write("turn[0].note[0]", "note")
            editor.save()
            se.stale_warning(editor)
        self.assertEqual(err.getvalue(), "")

    def test_droplists_batch_prints_the_reminder(self):
        text = balanced().replace(
            "echo hi", "python3 /shared/skills/session-edit.py list s.md"
        )
        editor, path = self.editor(text)
        with contextlib.redirect_stderr(io.StringIO()) as err:
            ops = se.parse_batch("droplists\n")
            new_lines, _, labels = se.plan_batch(editor, ops)
            editor.commit(new_lines)
            editor.save()
        self.assertIn("droplists: dropped 1 list round", "\n".join(labels))
        self.assertIn("session-edit.md", err.getvalue())
        self.assertNotIn("session-edit.py list", path.read_text())
        self.assertIsNone(se.check(se.Editor(str(path)).lines))


    def test_duplicate_ids_pair_by_order(self):
        editor, _ = self.editor(duplicate_ids_live())
        rounds = se.list_rounds(editor)
        self.assertEqual(len(rounds), 1)
        self.assertEqual(rounds[0]["round"], "turn[0].assistant[0].call[1]")
        self.assertEqual(rounds[0]["result"], "turn[0].result[1]")
        self.assertTrue(rounds[0]["settled"])

    def test_droplists_with_duplicate_ids(self):
        editor, path = self.editor(duplicate_ids_live())
        ops = se.parse_batch("droplists\n")
        new_lines, _, labels = se.plan_batch(editor, ops)
        editor.commit(new_lines)
        editor.save()
        self.assertIn("droplists: dropped 1 list round", "\n".join(labels))
        text = path.read_text()
        self.assertIn("first out", text)
        self.assertNotIn("listing output", text)
        self.assertNotIn("session-edit.py list", text)
        self.assertIsNone(se.check(se.Editor(str(path)).lines))

if __name__ == "__main__":
    unittest.main()
