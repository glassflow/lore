"""Tests of the search eval and the agent comparison. No server, no model, no network beyond
localhost:

  python3 -m unittest discover -s bench/eval -p 'test_*.py'
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import agents  # noqa: E402
import retrieval  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def hit(path, start, end, source="lore"):
    return {"source": source, "path": path, "start_line": start, "end_line": end}


def question(gold, need="any", qid="q1", category="where"):
    return {"id": qid, "repo": "lore", "category": category, "q": "where?", "need": need,
            "gold": gold}


class Overlap(unittest.TestCase):
    def test_lines_must_overlap_in_the_same_file(self):
        g = {"path": "src/a.rs", "start": 10, "end": 20}
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", 15, 16), g))
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", 1, 10), g))
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", 20, 30), g))
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", 1, 100), g))
        self.assertFalse(retrieval.overlaps(hit("src/a.rs", 1, 9), g))
        self.assertFalse(retrieval.overlaps(hit("src/a.rs", 21, 30), g))
        self.assertFalse(retrieval.overlaps(hit("src/b.rs", 15, 16), g))

    def test_a_gold_range_without_lines_is_the_whole_file(self):
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", 500, 510), {"path": "src/a.rs"}))
        self.assertTrue(retrieval.overlaps(hit("src/a.rs", None, None), {"path": "src/a.rs"}))

    def test_a_hit_without_lines_never_overlaps_a_range(self):
        g = {"path": "src/a.rs", "start": 1, "end": 5}
        self.assertFalse(retrieval.overlaps(hit("src/a.rs", None, None), g))


class Rank(unittest.TestCase):
    hits = [
        hit("src/a.rs", 1, 50, source="other"),  # right file, wrong source
        hit("docs/x.md", 1, 10),
        hit("src/a.rs", 1, 50),
        hit("src/b.rs", 100, 120),
    ]

    def test_gold_rank_is_1_based_and_checks_the_source(self):
        self.assertEqual(retrieval.gold_rank(self.hits, "lore", {"path": "src/a.rs", "start": 3,
                                                                 "end": 3}), 3)
        self.assertEqual(retrieval.gold_rank(self.hits, "other", {"path": "src/a.rs", "start": 3,
                                                                  "end": 3}), 1)
        self.assertIsNone(retrieval.gold_rank(self.hits, "lore", {"path": "src/c.rs"}))
        self.assertIsNone(retrieval.gold_rank([], "lore", {"path": "src/a.rs"}))

    def test_need_any_takes_the_best_gold_rank(self):
        gold = [{"path": "src/b.rs", "start": 110, "end": 110},
                {"path": "src/a.rs", "start": 3, "end": 3},
                {"path": "src/missing.rs", "start": 1, "end": 1}]
        self.assertEqual(retrieval.question_rank(self.hits, question(gold, "any")), 3)

    def test_need_all_takes_the_worst_and_misses_when_one_is_missing(self):
        gold = [{"path": "src/b.rs", "start": 110, "end": 110},
                {"path": "src/a.rs", "start": 3, "end": 3}]
        self.assertEqual(retrieval.question_rank(self.hits, question(gold, "all")), 4)
        gold.append({"path": "src/missing.rs", "start": 1, "end": 1})
        self.assertIsNone(retrieval.question_rank(self.hits, question(gold, "all")))

    def test_need_defaults_to_any(self):
        q = question([{"path": "src/a.rs"}])
        del q["need"]
        self.assertEqual(retrieval.question_rank(self.hits, q), 3)


class Summary(unittest.TestCase):
    def test_hits_at_each_cutoff_and_mrr(self):
        s = retrieval.summarize([1, 5, 6, 12, 13, 50, None, None])
        self.assertEqual(s["n"], 8)
        self.assertEqual(s["hit@5"], 2)
        self.assertEqual(s["hit@12"], 4)
        self.assertEqual(s["hit@50"], 6)
        want = (1 + 1 / 5 + 1 / 6 + 1 / 12 + 1 / 13 + 1 / 50) / 8
        self.assertAlmostEqual(s["mrr"], round(want, 3))

    def test_empty_and_all_misses(self):
        self.assertEqual(retrieval.summarize([])["mrr"], 0.0)
        self.assertEqual(retrieval.summarize([None, None])["hit@50"], 0)

    def test_categories_keep_their_order_and_skip_empty_ones(self):
        rows = [{"category": "how", "rank": 1}, {"category": "enumeration", "rank": None}]
        s = retrieval.summary_of(rows)
        self.assertEqual(list(s["categories"]), ["enumeration", "how"])
        self.assertEqual(s["overall"]["hit@5"], 1)
        self.assertEqual(s["overall"]["mrr"], 0.5)


class Sets(unittest.TestCase):
    q = {"id": "a", "repo": "lore", "category": "how", "q": "how?",
         "gold": [{"path": "x.rs", "start": 1, "end": 2}]}

    def test_a_wrapper_with_one_commit(self):
        commits, qs = retrieval.parse_set({"commit": "abc", "questions": [dict(self.q)]})
        self.assertEqual(commits, {"*": "abc"})
        self.assertEqual(retrieval.commit_for(commits, "lore"), "abc")
        self.assertEqual(qs[0]["need"], "any")

    def test_a_wrapper_with_a_commit_per_repo(self):
        commits, _ = retrieval.parse_set({"commit": {"lore": "abc", "app": "def"},
                                          "questions": [dict(self.q)]})
        self.assertEqual(retrieval.commit_for(commits, "app"), "def")
        self.assertIsNone(retrieval.commit_for(commits, "nope"))

    def test_a_bare_list_has_no_commit(self):
        commits, qs = retrieval.parse_set([dict(self.q)])
        self.assertEqual(commits, {})
        self.assertEqual(len(qs), 1)

    def test_bad_sets_are_refused(self):
        bad = [
            {"questions": [dict(self.q, category="why")]},
            {"questions": [dict(self.q), dict(self.q)]},
            {"questions": [dict(self.q, need="most")]},
            {"questions": [dict(self.q, gold=[{"path": "x.rs", "start": 5, "end": 2}])]},
            {"questions": [dict(self.q, gold=[{"path": "x.rs", "start": 5}])]},
            {"questions": [dict(self.q, gold=[])]},
            {"questions": [{k: v for k, v in self.q.items() if k != "repo"}]},
            {"commit": 7, "questions": [dict(self.q)]},
            {"nothing": []},
        ]
        for data in bad:
            with self.assertRaises(ValueError, msg=json.dumps(data)):
                retrieval.parse_set(json.loads(json.dumps(data)))

    def test_the_public_set_loads_and_spreads_over_every_category(self):
        commits, qs = retrieval.load_set(os.path.join(HERE, "questions", "lore.json"))
        self.assertRegex(commits["*"], r"^[0-9a-f]{40}$")
        self.assertTrue(15 <= len(qs) <= 20)
        self.assertEqual({q["category"] for q in qs}, set(retrieval.CATEGORIES))
        self.assertTrue(all(q["repo"] == "lore" and q.get("answer") for q in qs))


def result(label, ranks):
    rows = [{"id": qid, "category": "how", "rank": r} for qid, r in ranks.items()]
    return {"label": label, "latency_ms_median": 20.0, "questions": rows,
            "summary": retrieval.summary_of(rows)}


class Diff(unittest.TestCase):
    def test_changes_question_by_question(self):
        a = result("a", {"q1": 3, "q2": None, "q3": 1, "q4": 7, "gone": 1})
        b = result("b", {"q1": 1, "q2": 40, "q3": None, "q4": 7, "new": 2})
        changed, only_a, only_b = retrieval.diff(a, b)
        self.assertEqual(changed, [("q1", "how", 3, 1, "better"), ("q2", "how", None, 40, "better"),
                                   ("q3", "how", 1, None, "worse")])
        self.assertEqual(only_a, ["gone"])
        self.assertEqual(only_b, ["new"])
        text = retrieval.diff_report(a, b)
        self.assertIn("q3", text)
        self.assertIn("1 -> -", text)
        self.assertNotIn("q4", text)

    def test_identical_results(self):
        a = result("a", {"q1": 3})
        self.assertEqual(retrieval.diff(a, a), ([], [], []))
        self.assertIn("no question changed rank", retrieval.diff_report(a, a))


class FakeLore(BaseHTTPRequestHandler):
    """Answers /v1/search with a fixed ranking and records the queries."""
    seen = []

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/v1/info":
            body = {"embedder": "hash", "version": "test"}
        elif url.path == "/v1/search":
            if self.headers.get("Authorization") != "Bearer lk_test":
                self.send_response(401)
                self.end_headers()
                return
            params = {k: v[0] for k, v in parse_qs(url.query).items()}
            FakeLore.seen.append(params)
            body = {"hits": [hit("docs/a.md", 1, 9), hit("src/a.rs", 10, 30),
                             hit("src/b.rs", 1, 5)]}
        else:
            self.send_response(404)
            self.end_headers()
            return
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Run(unittest.TestCase):
    def setUp(self):
        self.server = HTTPServer(("127.0.0.1", 0), FakeLore)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.tmp = tempfile.mkdtemp()
        FakeLore.seen = []

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        shutil.rmtree(self.tmp)

    def test_run_writes_the_result_file(self):
        qs = [question([{"path": "src/a.rs", "start": 20, "end": 20}], qid="q1"),
              question([{"path": "src/b.rs", "start": 1, "end": 1},
                        {"path": "src/a.rs", "start": 11, "end": 11}], need="all", qid="q2",
                       category="how"),
              question([{"path": "src/zzz.rs"}], qid="q3", category="config")]
        path = os.path.join(self.tmp, "set.json")
        with open(path, "w") as f:
            json.dump({"commit": "c0ffee", "questions": qs}, f)
        res, out = retrieval.run("t", self.url, "lk_test", [path], out_dir=self.tmp,
                                 log=lambda *_: None)
        self.assertEqual(out, os.path.join(self.tmp, "t.json"))
        with open(out) as f:
            saved = json.load(f)
        self.assertEqual([r["rank"] for r in saved["questions"]], [2, 3, None])
        self.assertEqual(saved["questions"][1]["gold_ranks"], [3, 2])
        self.assertEqual(saved["sets"][0]["commits"], {"*": "c0ffee"})
        self.assertEqual(saved["server"]["embedder"], "hash")
        self.assertEqual(saved["summary"]["overall"]["hit@5"], 2)
        self.assertEqual(saved["summary"]["categories"]["config"]["hit@50"], 0)
        # One warm-up search, then one per question, each with k=50 and the question's source.
        self.assertEqual(len(FakeLore.seen), 4)
        self.assertTrue(all(p["k"] == "50" and p["source"] == "lore" for p in FakeLore.seen))
        self.assertIn("overall", retrieval.report(res))

    def test_a_bad_key_is_an_error(self):
        path = os.path.join(self.tmp, "set.json")
        with open(path, "w") as f:
            json.dump([question([{"path": "src/a.rs"}])], f)
        with self.assertRaises(RuntimeError):
            retrieval.run("t", self.url, "lk_wrong", [path], out_dir=self.tmp, log=lambda *_: None)


@unittest.skipUnless(shutil.which("git"), "git is not installed")
class Check(unittest.TestCase):
    def test_check_reports_missing_files_and_ranges_past_the_end(self):
        tmp = tempfile.mkdtemp()
        try:
            git = ["git", "-C", tmp, "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                   "-c", "commit.gpgsign=false"]
            subprocess.run(git + ["init", "-q"], check=True)
            with open(os.path.join(tmp, "a.rs"), "w") as f:
                f.write("one\ntwo\nthree\n")
            subprocess.run(git + ["add", "a.rs"], check=True)
            subprocess.run(git + ["commit", "-q", "-m", "c"], check=True)
            sha = subprocess.run(git + ["rev-parse", "HEAD"], capture_output=True, text=True,
                                 check=True).stdout.strip()
            qs = [question([{"path": "a.rs", "start": 2, "end": 3}], qid="ok"),
                  question([{"path": "a.rs", "start": 9, "end": 9}], qid="past"),
                  question([{"path": "b.rs"}], qid="missing")]
            path = os.path.join(tmp, "set.json")
            with open(path, "w") as f:
                json.dump({"commit": sha, "questions": qs}, f)
            out = []
            self.assertEqual(retrieval.check(tmp, [path], out=out.append), 2)
            self.assertTrue(any("two" in line for line in out))
        finally:
            shutil.rmtree(tmp)


class Stream(unittest.TestCase):
    def test_metrics_of_a_recorded_run(self):
        with open(os.path.join(HERE, "testdata", "stream.jsonl")) as f:
            m = agents.parse_stream(f)
        self.assertEqual(m["tool_calls"], {"Grep": 1, "Read": 1, "mcp__lore__search": 1})
        self.assertEqual(m["tool_calls_total"], 3)
        self.assertEqual(m["final_context_tokens"], 2 + 900 + 14000)
        self.assertEqual(m["total_input_tokens"], 10 + 14900 + 26000)
        self.assertEqual(m["wall_s"], 12.3)
        self.assertEqual(m["cost_usd"], 0.0712)
        self.assertEqual(m["num_turns"], 3)
        self.assertFalse(m["is_error"])
        self.assertTrue(m["answer"].startswith("Four kinds"))

    def test_without_a_result_usage_input_is_summed_per_message(self):
        lines = [
            json.dumps({"type": "assistant", "message": {"id": "m1", "content": [], "usage": {
                "input_tokens": 1, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 0}}}),
            json.dumps({"type": "assistant", "message": {"id": "m1", "content": [], "usage": {
                "input_tokens": 1, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 0}}}),
            json.dumps({"type": "assistant", "message": {"id": "m2", "content": [], "usage": {
                "input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 11}}}),
            json.dumps({"type": "result", "subtype": "error_max_turns", "is_error": True}),
        ]
        m = agents.parse_stream(lines)
        self.assertEqual(m["total_input_tokens"], 11 + 13)
        self.assertEqual(m["final_context_tokens"], 13)
        self.assertTrue(m["is_error"])

    def test_a_run_that_died_has_no_result(self):
        m = agents.parse_stream(["", "garbage"])
        self.assertTrue(m["is_error"])
        self.assertIsNone(m["answer"])
        self.assertEqual(m["tool_calls"], {})


class Grades(unittest.TestCase):
    qs = [{"id": "a", "q": "?", "answer": "x"}, {"id": "b", "q": "?", "answer": "y"},
          {"id": "c", "q": "?", "answer": "z"}]

    def test_grades_from_a_fenced_reply(self):
        reply = ('```json\n{"grades": [{"id": "a", "grade": "Correct", "why": "ok"}, '
                 '{"id": "b", "grade": "partial"}, {"id": "zzz", "grade": "wrong"}, '
                 '{"id": "c", "grade": "great"}]}\n```')
        grades, notes = agents.parse_grades(reply, self.qs)
        self.assertEqual(grades, {"a": "correct", "b": "partial", "c": "ungraded"})
        self.assertEqual([n["id"] for n in notes], ["a", "b"])

    def test_an_unreadable_reply_grades_nothing(self):
        for reply in ["", "no json here", "{broken"]:
            grades, _ = agents.parse_grades(reply, self.qs)
            self.assertEqual(set(grades.values()), {"ungraded"})

    def test_the_grading_prompt_holds_every_reference(self):
        p = agents.grading_prompt(self.qs, "an answer")
        for q in self.qs:
            self.assertIn(f"(id {q['id']})", p)
            self.assertIn(f"Reference: {q['answer']}", p)
        self.assertIn("an answer", p)


class Prompts(unittest.TestCase):
    qs = [{"id": f"q{i}", "q": f"question {i}?"} for i in range(1, 10)]

    def test_workloads_keep_full_groups(self):
        w = agents.workloads(self.qs, [1, 4, 8])
        sizes = [(s, i, len(g)) for s, i, g in w]
        self.assertEqual(sizes[:9], [(1, i, 1) for i in range(1, 10)])
        self.assertEqual(sizes[9:], [(4, 1, 4), (4, 2, 4), (8, 1, 8)])

    def test_prompts_number_the_questions(self):
        self.assertTrue(agents.prompt_for(self.qs[:1]).startswith("question 1? Answer from"))
        p = agents.prompt_for(self.qs[:4])
        self.assertIn("\n1. question 1?\n", p)
        self.assertTrue(p.endswith("4. question 4?"))

    def test_the_prompt_is_never_an_argument(self):
        argv = agents.agent_argv("lore", "m", "medium", "/tmp/mcp.json")
        self.assertIn("--strict-mcp-config", argv)
        self.assertEqual(argv[argv.index("--tools") + 1], "Read,Grep,Glob")
        self.assertEqual(argv[argv.index("--mcp-config") + 1], "/tmp/mcp.json")
        self.assertEqual(argv[argv.index("--append-system-prompt") + 1], agents.SEARCH_FIRST)
        none = agents.agent_argv("none", "m", "", None)
        self.assertNotIn("--mcp-config", none)
        self.assertNotIn("--effort", none)
        self.assertIn("--strict-mcp-config", none)
        grader = agents.grader_argv("m")
        self.assertEqual(grader[grader.index("--tools") + 1], "")

    def test_the_mcp_config_points_at_mcp_with_the_key(self):
        c = agents.mcp_config("http://h:8080/", "lk_x")["mcpServers"]["lore"]
        self.assertEqual(c["url"], "http://h:8080/mcp")
        self.assertEqual(c["headers"]["Authorization"], "Bearer lk_x")


FAKE_CLAUDE = r"""#!/usr/bin/env python3
import json, os, sys
log = os.environ["FAKE_CLAUDE_LOG"]
with open(log, "a") as f:
    f.write(json.dumps({"argv": sys.argv[1:], "stdin": sys.stdin.read()}) + "\n")
if "stream-json" in sys.argv:
    sys.stdout.write(open(os.environ["FAKE_CLAUDE_STREAM"]).read())
else:
    reply = '{"grades": [{"id": "q1", "grade": "correct", "why": "ok"}]}'
    sys.stdout.write(json.dumps({"type": "result", "result": reply, "total_cost_usd": 0.01}))
"""


class FakeClaude(unittest.TestCase):
    def test_a_prompt_starting_with_a_dash_goes_on_stdin(self):
        tmp = tempfile.mkdtemp()
        try:
            exe = os.path.join(tmp, "claude")
            with open(exe, "w") as f:
                f.write(FAKE_CLAUDE)
            os.chmod(exe, 0o755)
            log = os.path.join(tmp, "log.jsonl")
            env = {"CLAUDE_BIN": exe, "FAKE_CLAUDE_LOG": log,
                   "FAKE_CLAUDE_STREAM": os.path.join(HERE, "testdata", "stream.jsonl")}
            old = {k: os.environ.get(k) for k in env}
            os.environ.update(env)
            try:
                opts = type("Opts", (), {"model": "m", "effort": "medium", "grader_model": "g",
                                         "setups": ["none", "lore"]})()
                qs = [{"id": "q1", "q": "--version: which flag prints it?", "answer": "-V"}]
                p = agents.run_prompt(1, 1, qs, tmp, tmp, opts, "/tmp/mcp.json")
            finally:
                for k, v in old.items():
                    if v is None:
                        os.environ.pop(k, None)
                    else:
                        os.environ[k] = v
            with open(log) as f:
                calls = [json.loads(line) for line in f]
            self.assertEqual(len(calls), 4)  # two agent runs and two gradings
            for call in calls:
                self.assertFalse(any("--version: which" in a for a in call["argv"]))
            agent_calls = [c for c in calls if "stream-json" in c["argv"]]
            self.assertEqual(len(agent_calls), 2)
            for c in agent_calls:
                self.assertTrue(c["stdin"].startswith("--version: which flag prints it?"))
            for setup in ("none", "lore"):
                run = p["runs"][setup]
                self.assertEqual(run["tool_calls_total"], 3)
                self.assertEqual(run["grading"]["grades"], {"q1": "correct"})
            tot = agents.totals([p], ["none", "lore"])
            self.assertEqual(tot[1]["lore"]["grades"]["correct"], 1)
            self.assertIn("lore search-first", agents.summary_markdown("t", tot))
        finally:
            shutil.rmtree(tmp)


if __name__ == "__main__":
    unittest.main()
