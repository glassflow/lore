#!/usr/bin/env python3
"""Claude Code on its own against Claude Code with lore over MCP, on the same questions.

  agents.py LABEL REPO questions.json... [--source NAME] [--sizes 1,4,8] [--setups none,lore]
                                         [--limit N] [--model M] [--effort E] [--dry-run]

REPO is a checkout of the repo the questions are about, ideally at the commit the set records.
Only the questions whose `repo` is --source (default: REPO's directory name) are asked. Each
workload size n groups them into prompts of n questions (1 is one prompt per question; larger
sizes keep only full groups). Every prompt runs once per setup, the setups side by side:

  none   claude -p with only Read, Grep and Glob, and no MCP servers (--strict-mcp-config)
  lore   the same, plus lore's MCP server (--mcp-config) and the search-first CLAUDE.md line
         (--append-system-prompt); needs LORE_URL and LORE_KEY (read scope) in the environment

Each run is graded against the reference answers by a second claude -p with no tools: correct,
partial or wrong per question. Raw streams, grades, summary.json and summary.md go to
results/agents-LABEL/. This costs money: every prompt is one Claude Code run per setup, plus a
grading call each. --dry-run prints the commands and runs nothing.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter

import retrieval

# The line from docs/usage.md ("Use lore from agents").
SEARCH_FIRST = ("This repository is indexed in the lore MCP server. Use lore's search tool first: "
                "it returns ranked passages (path, line range and the code itself) in well under a "
                "second. Answer from those passages when they are enough, and Read a file only "
                "when they are not.")
TOOLS = "Read,Grep,Glob"
SETUPS = ["none", "lore"]
GRADES = ["correct", "partial", "wrong"]


def claude_bin():
    return os.environ.get("CLAUDE_BIN", "claude")


# Prompts


def prompt_for(questions):
    if len(questions) == 1:
        return f"{questions[0]['q']} Answer from this repository's code and cite file:line."
    lines = ["Answer each of these questions from this repository's code and cite file:line. "
             "Number your answers to match.", ""]
    lines += [f"{i + 1}. {q['q']}" for i, q in enumerate(questions)]
    return "\n".join(lines)


def workloads(questions, sizes):
    """(size, index, questions) for every prompt: each question alone for size 1, else full
    groups of `size` consecutive questions."""
    out = []
    for size in sizes:
        groups = [questions[i:i + size] for i in range(0, len(questions), size)]
        groups = [g for g in groups if len(g) == size]
        out += [(size, i + 1, g) for i, g in enumerate(groups)]
    return out


def mcp_config(url, key):
    return {"mcpServers": {"lore": {
        "type": "http", "url": f"{url.rstrip('/')}/mcp",
        "headers": {"Authorization": f"Bearer {key}"}}}}


def agent_argv(setup, model, effort, mcp_path=None):
    """The claude command of one agent run. The prompt is never an argument: it goes on stdin,
    because a prompt that starts with `-` would be read as an option, and because --tools,
    --allowedTools and --mcp-config take several values and would swallow a trailing prompt."""
    argv = [claude_bin(), "-p", "--model", model, "--output-format", "stream-json", "--verbose",
            "--strict-mcp-config", "--no-session-persistence", "--tools", TOOLS]
    if effort:
        argv += ["--effort", effort]
    if setup == "lore":
        argv += ["--allowedTools", TOOLS + ",mcp__lore", "--mcp-config", mcp_path,
                 "--append-system-prompt", SEARCH_FIRST]
    else:
        argv += ["--allowedTools", TOOLS]
    return argv


def grader_argv(model):
    return [claude_bin(), "-p", "--model", model, "--output-format", "json",
            "--strict-mcp-config", "--no-session-persistence", "--tools", ""]


def grading_prompt(questions, answer):
    parts = [
        "You grade answers about a codebase against reference answers. For each question below, "
        "compare the answer with the reference. correct: it gives every fact of the reference and "
        "contradicts none. partial: it gives some of them, or adds a wrong detail next to right "
        "ones. wrong: it misses the point, contradicts the reference, or does not answer. Ignore "
        "style, length and citations. The answer may answer several questions, numbered in the "
        "order below.",
        "",
        'Reply with JSON only: {"grades": [{"id": "<question id>", "grade": '
        '"correct|partial|wrong", "why": "<one short sentence>"}]}',
        "",
    ]
    for i, q in enumerate(questions):
        parts += [f"Question {i + 1} (id {q['id']}): {q['q']}",
                  f"Reference: {q.get('answer') or '(none)'}", ""]
    parts += ["The answer:", "<<<", answer or "(no answer)", ">>>"]
    return "\n".join(parts)


# Parsing


def usage_tokens(usage):
    """input + cache_creation + cache_read: every input token the model was sent."""
    if not usage:
        return 0
    return sum(int(usage.get(k) or 0)
               for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))


def parse_stream(lines):
    """Metrics of one `--output-format stream-json --verbose` run.

    Claude Code may send one assistant message as several events (one per content block) that
    share the message id and its usage, so tool calls are counted by tool_use id and usage is kept
    per message id. The final context is the input of the last model call of the main agent.
    """
    tools = Counter()
    seen_tools = set()
    usage_by_message = {}
    last_usage = None
    result = None
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(ev, dict):
            continue
        if ev.get("type") == "assistant":
            msg = ev.get("message") or {}
            for block in msg.get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    tid = block.get("id") or f"anon-{len(seen_tools)}"
                    if tid not in seen_tools:
                        seen_tools.add(tid)
                        tools[block.get("name", "?")] += 1
            usage = msg.get("usage")
            if usage:
                usage_by_message[msg.get("id") or f"anon-{len(usage_by_message)}"] = usage
                if not ev.get("parent_tool_use_id"):
                    last_usage = usage
        elif ev.get("type") == "result":
            result = ev
    out = {
        "tool_calls": dict(sorted(tools.items())),
        "tool_calls_total": sum(tools.values()),
        "final_context_tokens": usage_tokens(last_usage),
        "total_input_tokens": sum(usage_tokens(u) for u in usage_by_message.values()),
        "wall_s": None, "cost_usd": None, "num_turns": None, "answer": None,
        "is_error": result is None,
    }
    if result:
        if result.get("usage"):
            out["total_input_tokens"] = usage_tokens(result["usage"])
        if isinstance(result.get("duration_ms"), (int, float)):
            out["wall_s"] = round(result["duration_ms"] / 1000, 1)
        out["cost_usd"] = result.get("total_cost_usd")
        out["num_turns"] = result.get("num_turns")
        out["answer"] = result.get("result")
        out["is_error"] = bool(result.get("is_error")) or result.get("subtype") != "success"
    return out


def parse_grades(text, questions):
    """{id: grade} from the grader's reply; a question it did not grade is "ungraded"."""
    grades = {q["id"]: "ungraded" for q in questions}
    match = re.search(r"\{.*\}", text or "", re.S)
    if not match:
        return grades, []
    try:
        data = json.loads(match.group(0))
    except json.JSONDecodeError:
        return grades, []
    notes = []
    for g in data.get("grades") or []:
        if not isinstance(g, dict) or g.get("id") not in grades:
            continue
        grade = str(g.get("grade", "")).lower()
        if grade in GRADES:
            grades[g["id"]] = grade
            notes.append({"id": g["id"], "grade": grade, "why": g.get("why", "")})
    return grades, notes


# Running


def run_claude(argv, prompt, cwd, out_path, err_path):
    """Runs claude with the prompt on stdin, saving stdout and stderr. Returns (exit code,
    wall seconds)."""
    start = time.monotonic()
    with open(out_path, "w", encoding="utf-8") as out, open(err_path, "w", encoding="utf-8") as err:
        proc = subprocess.run(argv, input=prompt, cwd=cwd, stdout=out, stderr=err, text=True)
    return proc.returncode, time.monotonic() - start


def run_agent(setup, questions, repo, out_dir, name, opts, mcp_path=None):
    argv = agent_argv(setup, opts.model, opts.effort, mcp_path)
    prompt = prompt_for(questions)
    stream = os.path.join(out_dir, f"{name}-{setup}.jsonl")
    code, wall = run_claude(argv, prompt, repo, stream, stream[:-len(".jsonl")] + ".err")
    with open(stream, encoding="utf-8") as f:
        metrics = parse_stream(f)
    if metrics["wall_s"] is None:
        metrics["wall_s"] = round(wall, 1)
    metrics["exit_code"] = code
    return metrics


def grade(questions, answer, out_dir, name, opts):
    path = os.path.join(out_dir, f"{name}.grade.json")
    code, _ = run_claude(grader_argv(opts.grader_model), grading_prompt(questions, answer),
                         out_dir, path, path[:-len(".json")] + ".err")
    try:
        with open(path, encoding="utf-8") as f:
            reply = json.load(f)
    except (OSError, json.JSONDecodeError):
        reply = {}
    grades, notes = parse_grades(reply.get("result", ""), questions)
    return {"grades": grades, "notes": notes, "cost_usd": reply.get("total_cost_usd"),
            "exit_code": code}


def run_prompt(size, index, questions, repo, out_dir, opts, mcp_path):
    """Runs every setup on one prompt at the same time, then grades each answer."""
    name = f"n{size}-p{index}"
    runs = {}

    def one(setup):
        try:
            m = run_agent(setup, questions, repo, out_dir, name, opts, mcp_path)
            m["grading"] = grade(questions, m["answer"], out_dir, f"{name}-{setup}", opts)
        except Exception as e:  # noqa: BLE001 - one failed run must not lose the others
            m = {"error": str(e)}
        runs[setup] = m

    threads = [threading.Thread(target=one, args=(s,)) for s in opts.setups]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return {"size": size, "prompt": index, "ids": [q["id"] for q in questions], "runs": runs}


def totals(prompts, setups):
    """Per workload size and setup: summed wall time, tool calls, input tokens and cost, the
    largest final context, and the grade counts."""
    out = {}
    for size in sorted({p["size"] for p in prompts}):
        out[size] = {}
        for setup in setups:
            runs = [p["runs"].get(setup) or {} for p in prompts if p["size"] == size]
            tools = Counter()
            grades = Counter()
            for r in runs:
                tools.update(r.get("tool_calls") or {})
                grades.update(((r.get("grading") or {}).get("grades") or {}).values())
            out[size][setup] = {
                "prompts": len(runs),
                "errors": sum(1 for r in runs if r.get("error") or r.get("is_error")),
                "wall_s": round(sum(r.get("wall_s") or 0 for r in runs), 1),
                "tool_calls": dict(sorted(tools.items())),
                "final_context_tokens_max": max([r.get("final_context_tokens") or 0 for r in runs],
                                                default=0),
                "total_input_tokens": sum(r.get("total_input_tokens") or 0 for r in runs),
                "cost_usd": round(sum(r.get("cost_usd") or 0 for r in runs), 2),
                "grades": {g: grades.get(g, 0) for g in GRADES + ["ungraded"]},
            }
    return out


def kilo(n):
    return f"{round(n / 1000)}k"


def summary_markdown(label, tot):
    lines = [f"# {label}", "",
             "| Questions per prompt | Setup | Prompts | Wall | Tool calls | Final context (max) "
             "| Input tokens | Cost | Correct | Partial | Wrong |",
             "|---|---|---|---|---|---|---|---|---|---|---|"]
    for size, by_setup in tot.items():
        for setup, t in by_setup.items():
            tools = ", ".join(f"{k} {v}" for k, v in t["tool_calls"].items()) or "none"
            g = t["grades"]
            n = sum(g.values())
            err = f" ({t['errors']} failed)" if t["errors"] else ""
            lines.append(
                f"| {size} | {'lore search-first' if setup == 'lore' else 'no lore'} | "
                f"{t['prompts']}{err} | {t['wall_s']:.0f} s | {tools} | "
                f"{kilo(t['final_context_tokens_max'])} | {kilo(t['total_input_tokens'])} | "
                f"${t['cost_usd']:.2f} | {g['correct']}/{n} | {g['partial']}/{n} | {g['wrong']}/{n} |")
    return "\n".join(lines)


def head_commit(repo):
    p = subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 else None


def main(argv=None):
    p = argparse.ArgumentParser(prog="agents.py", description=__doc__.split("\n\n")[0])
    p.add_argument("label")
    p.add_argument("repo")
    p.add_argument("questions", nargs="+")
    p.add_argument("--source", help="the questions' repo name (default: REPO's directory name)")
    p.add_argument("--sizes", default="1,4,8", help="questions per prompt (default 1,4,8)")
    p.add_argument("--setups", default="none,lore", help="none, lore or both (default)")
    p.add_argument("--limit", type=int, help="ask only the first N questions")
    p.add_argument("--model", default=os.environ.get("BENCH_MODEL", "claude-opus-5-5"))
    p.add_argument("--effort", default="medium", help="Claude Code's --effort; empty for none")
    p.add_argument("--grader-model", default=None, help="default: --model")
    p.add_argument("--dry-run", action="store_true")
    opts = p.parse_args(argv)
    opts.grader_model = opts.grader_model or opts.model
    if not retrieval.LABEL_RE.match(opts.label):
        p.error("LABEL may hold only letters, digits, '.', '_' and '-'")
    try:
        opts.sizes = sorted({int(s) for s in opts.sizes.split(",") if s.strip()})
    except ValueError:
        p.error("--sizes is a comma-separated list of positive integers")
    if not opts.sizes or min(opts.sizes) < 1:
        p.error("--sizes is a comma-separated list of positive integers")
    opts.setups = [s.strip() for s in opts.setups.split(",") if s.strip()]
    if not opts.setups or any(s not in SETUPS for s in opts.setups):
        p.error("--setups takes none, lore or none,lore")

    repo = os.path.abspath(opts.repo)
    if not os.path.isdir(repo):
        p.error(f"no such directory: {opts.repo}")
    for path in opts.questions:
        if os.path.realpath(path).startswith(os.path.realpath(repo) + os.sep):
            p.error(f"{path} is inside {opts.repo}, so Claude Code could read the answers: use a "
                    "checkout at the set's commit (git worktree add <dir> <commit>)")
    source = opts.source or os.path.basename(repo.rstrip("/"))
    questions, commit = [], None
    for path in opts.questions:
        commits, qs = retrieval.load_set(path)
        qs = [q for q in qs if q["repo"] == source]
        if qs:
            commit = commit or retrieval.commit_for(commits, source)
        questions += qs
    if opts.limit:
        questions = questions[:opts.limit]
    if not questions:
        p.error(f"no questions for the source {source!r} (--source picks another)")
    head = head_commit(repo)
    if commit and head and head != commit:
        print(f"note: {repo} is at {head[:12]}, the set was written against {commit[:12]}",
              file=sys.stderr)

    url, key = os.environ.get("LORE_URL"), os.environ.get("LORE_KEY")
    if "lore" in opts.setups and not (url and key) and not opts.dry_run:
        p.error("the lore setup needs LORE_URL and LORE_KEY (a read key of a tenant with this repo)")
    if not opts.dry_run and not shutil.which(claude_bin()):
        p.error(f"{claude_bin()} is not on PATH")

    work = workloads(questions, opts.sizes)
    if not work:
        p.error("no workload: every size is larger than the number of questions")

    if opts.dry_run:
        for size, index, qs in work:
            for setup in opts.setups:
                argv = agent_argv(setup, opts.model, opts.effort, "<mcp.json>")
                print(f"# n{size}-p{index} {setup}: {', '.join(q['id'] for q in qs)}")
                print("  " + " ".join(a if a and " " not in a else json.dumps(a) for a in argv))
        print(f"{len(work)} prompts x {len(opts.setups)} setups, prompts on stdin")
        return 0

    out_dir = os.path.join(retrieval.results_dir(), f"agents-{opts.label}")
    os.makedirs(out_dir, exist_ok=True)
    mcp_path = None
    if "lore" in opts.setups:
        # The key is in this file, so it lives in a private temp dir, not in the results.
        tmp = tempfile.mkdtemp(prefix="lore-agents-")
        mcp_path = os.path.join(tmp, "mcp.json")
        with open(os.open(mcp_path, os.O_WRONLY | os.O_CREAT, 0o600), "w") as f:
            json.dump(mcp_config(url, key), f)

    prompts = []
    try:
        for size, index, qs in work:
            print(f"n{size}-p{index}: {', '.join(q['id'] for q in qs)}", file=sys.stderr)
            prompts.append(run_prompt(size, index, qs, repo, out_dir, opts, mcp_path))
    finally:
        if mcp_path:
            shutil.rmtree(os.path.dirname(mcp_path), ignore_errors=True)

    tot = totals(prompts, opts.setups)
    summary = {"label": opts.label, "repo": repo, "source": source, "head": head,
               "set_commit": commit, "model": opts.model, "effort": opts.effort,
               "grader_model": opts.grader_model, "totals": tot, "prompts": prompts}
    with open(os.path.join(out_dir, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2)
        f.write("\n")
    md = summary_markdown(opts.label, tot)
    with open(os.path.join(out_dir, "summary.md"), "w", encoding="utf-8") as f:
        f.write(md + "\n")
    print(md)
    print(f"results in {out_dir}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
