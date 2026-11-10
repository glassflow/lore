#!/usr/bin/env python3
"""Labelled search eval for lore.

  retrieval.py LABEL URL KEY questions.json...   ask every question, write results/LABEL.json
  retrieval.py --diff A B                        compare two results question by question
  retrieval.py --check REPO questions.json...    print each gold range as it reads at the set's commit

`bench/eval/eval` runs this file. Each question is one `GET /v1/search?q=<q>&k=50&source=<repo>`.
A hit is a result in the question's source and file whose lines overlap a gold range. With
`need: any` the question's rank is the best rank of its gold ranges; with `need: all` it is the
worst, and the question is a miss unless every range is found. A and B in --diff are labels
(results/<label>.json) or paths to result files. Python 3.9 or newer, standard library only.
"""

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

CATEGORIES = ["enumeration", "schema", "where", "config", "how"]
NEEDS = ["any", "all"]
SEARCH_K = 50
# hit@12 is fast mode's k: the passages a fast answer is written from.
CUTOFFS = [5, 12, 50]
HERE = os.path.dirname(os.path.abspath(__file__))
LABEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")


def results_dir():
    return os.environ.get("EVAL_RESULTS") or os.path.join(HERE, "results")


# Question sets


def load_set(path):
    """Reads a question set: a list of questions, or {"commit": ..., "questions": [...]}.

    `commit` is the sha the gold ranges were written against: one string for the whole set, or an
    object of {repo: sha} for a set over several repos. Returns (commits, questions), with commits
    a dict of repo to sha ("*" for every repo). Raises ValueError naming the first problem.
    """
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    return parse_set(data, path)


def parse_set(data, path="<set>"):
    commits = {}
    if isinstance(data, list):
        questions = data
    elif isinstance(data, dict) and isinstance(data.get("questions"), list):
        questions = data["questions"]
        commit = data.get("commit")
        if isinstance(commit, str):
            commits["*"] = commit
        elif isinstance(commit, dict):
            commits.update({str(k): str(v) for k, v in commit.items()})
        elif commit is not None:
            raise ValueError(f"{path}: commit must be a sha or an object of repo to sha")
    else:
        raise ValueError(f"{path}: expected a list of questions or an object with \"questions\"")
    seen = set()
    for i, q in enumerate(questions):
        where = f"{path}: question {i + 1}"
        if not isinstance(q, dict):
            raise ValueError(f"{where} is not an object")
        for field in ("id", "repo", "category", "q", "gold"):
            if not q.get(field):
                raise ValueError(f"{where} has no {field}")
        where = f"{path}: {q['id']}"
        if q["id"] in seen:
            raise ValueError(f"{where} appears twice")
        seen.add(q["id"])
        if q["category"] not in CATEGORIES:
            raise ValueError(f"{where}: category must be one of {', '.join(CATEGORIES)}")
        q.setdefault("need", "any")
        if q["need"] not in NEEDS:
            raise ValueError(f"{where}: need must be any or all")
        if not isinstance(q["gold"], list):
            raise ValueError(f"{where}: gold must be a list")
        for g in q["gold"]:
            if not isinstance(g, dict) or not g.get("path"):
                raise ValueError(f"{where}: every gold range needs a path")
            start, end = g.get("start"), g.get("end")
            if (start is None) != (end is None):
                raise ValueError(f"{where}: a gold range has both start and end, or neither")
            if start is not None and not (isinstance(start, int) and isinstance(end, int)
                                          and 1 <= start <= end):
                raise ValueError(f"{where}: gold lines must be integers with 1 <= start <= end")
    return commits, questions


def commit_for(commits, repo):
    return commits.get(repo) or commits.get("*")


# Scoring


def overlaps(hit, gold):
    """True when the hit is in the gold file and its lines overlap the gold range. A gold range
    without lines stands for the whole file."""
    if hit.get("path") != gold["path"]:
        return False
    if gold.get("start") is None:
        return True
    start, end = hit.get("start_line"), hit.get("end_line")
    if start is None or end is None:
        return False
    return start <= gold["end"] and gold["start"] <= end


def gold_rank(hits, repo, gold):
    """The 1-based rank of the first hit in `repo` that overlaps `gold`, or None."""
    for i, hit in enumerate(hits):
        if hit.get("source") == repo and overlaps(hit, gold):
            return i + 1
    return None


def question_rank(hits, question):
    """The question's rank: the best gold rank for need any, the worst for need all."""
    ranks = [gold_rank(hits, question["repo"], g) for g in question["gold"]]
    found = [r for r in ranks if r is not None]
    if question.get("need", "any") == "all":
        return max(ranks) if ranks and len(found) == len(ranks) else None
    return min(found) if found else None


def summarize(ranks):
    """n, hit@k for each cutoff (a count) and MRR over a list of ranks (None is a miss)."""
    n = len(ranks)
    out = {"n": n}
    for k in CUTOFFS:
        out[f"hit@{k}"] = sum(1 for r in ranks if r is not None and r <= k)
    out["mrr"] = round(sum(1.0 / r for r in ranks if r) / n, 3) if n else 0.0
    return out


def summary_of(rows):
    """The overall summary and one per category, in CATEGORIES order."""
    by_cat = {}
    for c in CATEGORIES:
        ranks = [r["rank"] for r in rows if r["category"] == c]
        if ranks:
            by_cat[c] = summarize(ranks)
    return {"overall": summarize([r["rank"] for r in rows]), "categories": by_cat}


def median(values):
    return round(statistics.median(values), 1) if values else None


# Running


def get_json(url, key=None, timeout=60):
    req = urllib.request.Request(url)
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return json.load(res)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:300]
        raise RuntimeError(f"GET {url.split('?')[0]} answered {e.code}: {body}") from None


def search(url, key, q, source, k=SEARCH_K):
    """One search. Returns (hits, milliseconds)."""
    params = urllib.parse.urlencode({"q": q, "k": k, "source": source})
    start = time.perf_counter()
    body = get_json(f"{url.rstrip('/')}/v1/search?{params}", key)
    return body["hits"], (time.perf_counter() - start) * 1000


def short_hit(hit):
    where = hit.get("path") or hit.get("title") or "?"
    if hit.get("start_line") is not None:
        where += f":{hit['start_line']}-{hit['end_line']}"
    return where


def run(label, url, key, paths, out_dir=None, log=print):
    sets, questions = [], []
    for path in paths:
        commits, qs = load_set(path)
        rel = os.path.relpath(os.path.abspath(path))
        sets.append({"file": os.path.abspath(path) if rel.startswith("..") else rel,
                     "commits": commits, "questions": len(qs)})
        questions.extend(qs)
    ids = [q["id"] for q in questions]
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes:
        raise ValueError(f"question ids appear in more than one set: {', '.join(dupes)}")
    if not questions:
        raise ValueError("no questions")

    try:
        info = get_json(f"{url.rstrip('/')}/v1/info")
    except Exception as e:  # noqa: BLE001 - the eval still runs against an older server
        info = {"error": str(e)}
    # The first search pays for warm-up (connections, the embedder's first call); not timed.
    search(url, key, questions[0]["q"], questions[0]["repo"])

    rows = []
    for q in questions:
        hits, ms = search(url, key, q["q"], q["repo"])
        rank = question_rank(hits, q)
        rows.append({
            "id": q["id"], "repo": q["repo"], "category": q["category"], "need": q["need"],
            "q": q["q"], "rank": rank,
            "gold_ranks": [gold_rank(hits, q["repo"], g) for g in q["gold"]],
            "latency_ms": round(ms, 1),
            "top": [short_hit(h) for h in hits[:5]],
        })
        log(f"  {q['id']:<12} {q['category']:<12} rank {fmt_rank(rank):>3}  {ms:6.0f} ms")

    result = {
        "label": label,
        "date": datetime.now(timezone.utc).replace(microsecond=0).isoformat(),
        "url": url,
        "k": SEARCH_K,
        "server": info,
        "sets": sets,
        "latency_ms_median": median([r["latency_ms"] for r in rows]),
        "summary": summary_of(rows),
        "questions": rows,
    }
    out_dir = out_dir or results_dir()
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, f"{label}.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2)
        f.write("\n")
    return result, out


# Reporting


def fmt_rank(rank):
    return "-" if rank is None else str(rank)


def report(result):
    lines = []
    server = result.get("server") or {}
    lines.append(f"{result['label']}: {len(result['questions'])} questions, "
                 f"median latency {result['latency_ms_median']} ms, "
                 f"embedder {server.get('embedder', '?')}")
    lines.append(f"{'':<12} {'n':>3} {'hit@5':>7} {'hit@12':>7} {'hit@50':>7} {'MRR':>6}")
    summary = result["summary"]
    for name, s in [("overall", summary["overall"])] + list(summary["categories"].items()):
        n = s["n"]
        cells = [f"{s[f'hit@{k}']}/{n}" for k in CUTOFFS]
        lines.append(f"{name:<12} {n:>3} {cells[0]:>7} {cells[1]:>7} {cells[2]:>7} {s['mrr']:>6.3f}")
    return "\n".join(lines)


def load_result(name):
    path = name if name.endswith(".json") and os.path.exists(name) else \
        os.path.join(results_dir(), f"{name}.json")
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def rank_key(rank):
    return float("inf") if rank is None else rank


def diff(a, b):
    """Question-by-question changes from result a to result b: (id, category, rank a, rank b,
    "better" or "worse"), in b's order then a's, plus the ids found in only one of them."""
    ra = {r["id"]: r for r in a["questions"]}
    rb = {r["id"]: r for r in b["questions"]}
    changed = []
    for qid, row in rb.items():
        if qid not in ra:
            continue
        x, y = ra[qid]["rank"], row["rank"]
        if x != y:
            changed.append((qid, row["category"], x, y,
                            "better" if rank_key(y) < rank_key(x) else "worse"))
    only_a = [i for i in ra if i not in rb]
    only_b = [i for i in rb if i not in ra]
    return changed, only_a, only_b


def diff_report(a, b):
    w = max(12, len(a["label"]), len(b["label"]))
    lines = [f"{'':<10} {a['label']:>{w}} {b['label']:>{w}}"]
    sa, sb = a["summary"]["overall"], b["summary"]["overall"]
    for k in CUTOFFS:
        key = f"hit@{k}"
        cells = [f"{sa[key]}/{sa['n']}", f"{sb[key]}/{sb['n']}"]
        lines.append(f"{key:<10} {cells[0]:>{w}} {cells[1]:>{w}} ({sb[key] - sa[key]:+d})")
    lines.append(f"{'MRR':<10} {sa['mrr']:>{w}.3f} {sb['mrr']:>{w}.3f} "
                 f"({sb['mrr'] - sa['mrr']:+.3f})")
    latency = [f"{a['latency_ms_median']} ms", f"{b['latency_ms_median']} ms"]
    lines.append(f"{'latency':<10} {latency[0]:>{w}} {latency[1]:>{w}}")
    changed, only_a, only_b = diff(a, b)
    if changed:
        lines.append("")
        lines.append("changed:")
        for qid, cat, x, y, way in changed:
            lines.append(f"  {qid:<12} {cat:<12} {fmt_rank(x):>3} -> {fmt_rank(y):<3} {way}")
    else:
        lines.append("")
        lines.append("no question changed rank")
    if only_a:
        lines.append(f"only in {a['label']}: {', '.join(only_a)}")
    if only_b:
        lines.append(f"only in {b['label']}: {', '.join(only_b)}")
    return "\n".join(lines)


def check(repo_dir, paths, at=None, out=print):
    """Prints every gold range as the file reads at the set's commit (or `at`). Returns the
    number of problems: a missing commit, a missing file, a range past the end of the file."""
    problems = 0
    for path in paths:
        commits, questions = load_set(path)
        for q in questions:
            rev = at or commit_for(commits, q["repo"])
            if not rev:
                out(f"{q['id']}: {path} records no commit for {q['repo']}; pass --at")
                problems += 1
                continue
            out(f"{q['id']} [{q['category']}, need {q['need']}] {q['q']}")
            out(f"  answer: {q.get('answer', '')}")
            for g in q["gold"]:
                shown = subprocess.run(["git", "-C", repo_dir, "show", f"{rev}:{g['path']}"],
                                       capture_output=True, text=True)
                if shown.returncode != 0:
                    out(f"  PROBLEM {g['path']} not found at {rev[:12]}: {shown.stderr.strip()}")
                    problems += 1
                    continue
                lines = shown.stdout.split("\n")
                if g.get("start") is None:
                    out(f"  {g['path']} (whole file, {len(lines)} lines)")
                    continue
                if g["end"] > len(lines):
                    out(f"  PROBLEM {g['path']}:{g['start']}-{g['end']} is past the end "
                        f"({len(lines)} lines)")
                    problems += 1
                    continue
                out(f"  {g['path']}:{g['start']}-{g['end']}")
                span = list(range(g["start"], g["end"] + 1))
                if len(span) > 12:
                    span = span[:8] + [None] + span[-3:]
                for n in span:
                    out("      ..." if n is None else f"    {n:>5}  {lines[n - 1]}")
    return problems


def main(argv=None):
    p = argparse.ArgumentParser(
        prog="eval", description="Labelled search eval for lore.",
        usage="eval LABEL URL KEY questions.json...\n"
              "       eval --diff A B\n"
              "       eval --check REPO questions.json... [--at REV]")
    p.add_argument("--diff", nargs=2, metavar=("A", "B"), help="compare two results")
    p.add_argument("--check", metavar="REPO", help="print the gold ranges from a git checkout")
    p.add_argument("--at", metavar="REV", help="with --check: read this revision, not the set's commit")
    p.add_argument("args", nargs="*")
    a = p.parse_args(argv)
    try:
        if a.diff:
            print(diff_report(load_result(a.diff[0]), load_result(a.diff[1])))
            return 0
        if a.check:
            if not a.args:
                p.error("--check needs at least one questions file")
            problems = check(a.check, a.args, a.at)
            print(f"{problems} problem(s)")
            return 1 if problems else 0
        if len(a.args) < 4:
            p.error("need LABEL URL KEY and at least one questions file")
        label, url, key, paths = a.args[0], a.args[1], a.args[2], a.args[3:]
        if not LABEL_RE.match(label):
            p.error("LABEL may hold only letters, digits, '.', '_' and '-'")
        result, out = run(label, url, key, paths)
        print(report(result))
        print(f"wrote {out}")
        return 0
    except (OSError, ValueError, RuntimeError, KeyError, json.JSONDecodeError) as e:
        print(f"eval: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
