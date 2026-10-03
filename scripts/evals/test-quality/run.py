#!/usr/bin/env python3
"""Test-quality eval: does a harness make the model write correct code and tests that catch bugs?

Operator-invoked. It spends real model tokens, so it never runs in scripts/gate.sh or CI.

    python3 scripts/evals/test-quality/run.py --out DIR --harness aho-new --harness omp \\
        [--task intervals] [--repeat N] [--var key=value ...] [--harnesses FILE] [--python PY]
    python3 scripts/evals/test-quality/run.py --out DIR --dry-run

Every invocation writes under DIR/<UTC timestamp>/. For each harness x task x repeat it seeds a
fresh git work dir (the task's README.md as the spec, a stub package), runs the harness command
from harnesses.json non-interactively in it, then scores what the model left behind:

  correctness   the hidden reference suite run against the model's package (passed / total)
  mutation      the model's own tests run against each hidden mutant of the reference
                implementation; a mutant is killed when a test that passes on the reference
                fails or errors on it (killed / total)
  tautological  the model's tests that pass on the reference and also on the stub, which only
                raises NotImplementedError: they cannot catch anything

The model's tests run under pytest when the --python interpreter can import it, else unittest
discovery (tests/ if present, else the work root). results.json records every command, variable
and outcome; summary.md has one table per harness.

Template variables (harnesses.json `vars`, overridden by --var): model, thinking_budget,
omp_thinking, aho_baseline, aho_new, omp. Always set: repo, eval_dir, python, work, run_dir,
task, task_dir, package, prompt.

--dry-run only checks the tasks: the reference passes every hidden test, the stub fails every
hidden test, and every mutant applies exactly once and is killed by the hidden suite. A real run
performs the same check first and refuses to start on a broken task.
"""

import argparse
import datetime
import json
import os
import shutil
import signal
import string
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from pathlib import Path

EVAL = Path(__file__).resolve().parent
REPO = EVAL.parents[2]
TASKS = EVAL / "tasks"
HARNESS_STATE = shutil.ignore_patterns(".git", ".aho", ".omp", "__pycache__", ".pytest_cache")
TEST_TIMEOUT = 120
NOT_A_KILL = ("pass", "skip")
SEVERITY = {"pass": 0, "skip": 1, "fail": 2, "error": 3}


class Task:
    def __init__(self, name):
        d = TASKS / name
        if not (d / "reference.py").is_file():
            raise SystemExit(f"no task {name!r} under {TASKS}")
        self.name = self.package = name
        self.dir = d
        self.spec = (d / "README.md").read_text()
        self.stub = (d / "stub.py").read_text()
        self.reference = (d / "reference.py").read_text()
        self.hidden = (d / "hidden_test.py").read_text()
        self.mutants = []
        for m in json.loads((d / "mutants.json").read_text()):
            n = self.reference.count(m["find"])
            if n != 1:
                raise SystemExit(f"{name}: mutant {m['name']!r} must match the reference exactly once, matched {n}")
            self.mutants.append((m["name"], self.reference.replace(m["find"], m["replace"])))

    def seed(self, work):
        (work / self.package).mkdir(parents=True)
        (work / "README.md").write_text(self.spec)
        (work / self.package / "__init__.py").write_text(self.stub)


def run_process(argv, cwd, env, stdout, stderr, timeout):
    """(exit code, timed out, seconds). The whole process group dies on timeout."""
    began = time.monotonic()
    proc = subprocess.Popen(
        argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True
    )
    try:
        code, timed_out = proc.wait(timeout=timeout), False
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        code, timed_out = proc.wait(), True
    return code, timed_out, round(time.monotonic() - began, 1)


def variant(src, dst, package, impl):
    """Copy of a work dir without harness state; with `impl`, the package is only that module."""

    def ignore(dirpath, names):
        skipped = set(HARNESS_STATE(dirpath, names))
        if impl is not None and Path(dirpath) == src:
            skipped |= {package, f"{package}.py"}
        return skipped

    shutil.copytree(src, dst, ignore=ignore)
    if impl is not None:
        (dst / package).mkdir()
        (dst / package / "__init__.py").write_text(impl)
    return dst


def parse_junit(xml):
    outcomes = {}
    if not xml.exists():
        return outcomes
    for case in ET.parse(xml).getroot().iter("testcase"):
        tid = f"{case.get('classname', '')}::{case.get('name', '')}"
        tags = {child.tag for child in case}
        got = "error" if "error" in tags else "fail" if "failure" in tags else "skip" if "skipped" in tags else "pass"
        if SEVERITY[got] >= SEVERITY[outcomes.get(tid, "pass")]:
            outcomes[tid] = got
    return outcomes


def run_suite(python, runner, root, start, report):
    """({test id: pass|fail|error|skip}, timed out) for the tests under `start`, `root` importable."""
    if runner == "pytest":
        xml = report.with_suffix(".xml")
        cmd = [python, "-m", "pytest", "-q", "-p", "no:cacheprovider", f"--junitxml={xml}", str(start)]
    else:
        cmd = [python, str(EVAL / "unittest_json.py"), str(root), str(start), str(report)]
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    with report.with_suffix(".log").open("w") as log:
        _, timed_out, _ = run_process(cmd, root, env, log, log, TEST_TIMEOUT)
    if timed_out:
        return {}, True
    if runner == "pytest":
        return parse_junit(xml), False
    return (json.loads(report.read_text())["outcomes"] if report.exists() else {}), False


def hidden_suite(python, src, scratch, label, task, impl):
    root = variant(src, scratch / label, task.package, impl)
    d = root / "_hidden"
    d.mkdir(exist_ok=True)
    (d / f"test_{task.package}_hidden.py").write_text(task.hidden)
    outcomes, _ = run_suite(python, "unittest", root, d, scratch / f"{label}.json")
    return outcomes


def check_task(task, scratch, python):
    """(report, hidden test ids). The report's `problems` is empty when the task is sound."""
    seed = scratch / "seed"
    task.seed(seed)
    ref = hidden_suite(python, seed, scratch, "reference", task, task.reference)
    ids = sorted(ref)
    problems = [] if ids else ["the hidden suite found no tests"]
    problems += [f"reference {o}s hidden {t}" for t, o in sorted(ref.items()) if o != "pass"]
    stub = hidden_suite(python, seed, scratch, "stub", task, task.stub)
    problems += [f"stub passes hidden {t}" for t in ids if stub.get(t) == "pass"]
    killers = {}
    for name, source in task.mutants:
        got = hidden_suite(python, seed, scratch, f"mutant-{name}", task, source)
        killers[name] = sum(1 for t in ids if got.get(t) != "pass")
        if not killers[name]:
            problems.append(f"mutant {name} survives the hidden suite")
    report = {"task": task.name, "hidden_tests": len(ids), "killing_tests_per_mutant": killers, "problems": problems}
    return report, ids


def score(task, work, scratch, python, runner, hidden_ids):
    got = hidden_suite(python, work, scratch, "hidden-on-model", task, None)
    passed = [t for t in hidden_ids if got.get(t) == "pass"]

    def own(label, impl):
        root = variant(work, scratch / label, task.package, impl)
        start = root / "tests" if (root / "tests").is_dir() else root
        return run_suite(python, runner, root, start, scratch / f"{label}.json")

    on_model, _ = own("own-on-model", None)
    on_ref, ref_timed_out = own("own-on-reference", task.reference)
    on_stub, _ = own("own-on-stub", task.stub)
    valid = sorted(t for t, o in on_ref.items() if o == "pass")
    killed, survivors = [], []
    for name, source in task.mutants:
        outcomes, _ = own(f"own-on-mutant-{name}", source)
        hit = any(outcomes.get(t) not in NOT_A_KILL for t in valid)
        (killed if hit else survivors).append(name)
    total = len(hidden_ids)
    return {
        "correctness": {
            "passed": len(passed),
            "total": total,
            "rate": len(passed) / total,
            "failed": sorted(set(hidden_ids) - set(passed)),
        },
        "own_tests": {
            "runner": runner,
            "total": len(on_model),
            "pass_on_own_impl": sum(1 for o in on_model.values() if o == "pass"),
            "valid": len(valid),
            "invalid_on_reference": sorted(t for t, o in on_ref.items() if o != "pass"),
            "timed_out_on_reference": ref_timed_out,
        },
        "mutation": {"killed": len(killed), "total": len(task.mutants), "score": len(killed) / len(task.mutants), "survivors": survivors},
        "tautological": sorted(t for t in valid if on_stub.get(t) == "pass"),
    }


def git_seed(work):
    git = shutil.which("git")
    if not git:
        return False
    ident = ["-c", "user.name=eval", "-c", "user.email=eval@localhost", "-c", "commit.gpgsign=false"]
    for cmd in ([git, "init", "-q"], [git, "add", "-A"], [git, *ident, "commit", "-qm", "seed"]):
        if subprocess.run(cmd, cwd=work, capture_output=True).returncode != 0:
            return False
    return True


def placeholders(text):
    return {field for _, field, _, _ in string.Formatter().parse(text) if field}


def pct(x):
    return f"{100 * x:.0f}%"


def summary(stamp, meta, results):
    v = meta["vars"]
    lines = [
        f"# test-quality eval {stamp}",
        "",
        f"model `{v.get('model')}`, thinking_budget `{v.get('thinking_budget')}` (aho), "
        f"omp_thinking `{v.get('omp_thinking')}`, own tests run with `{meta['runner']}`. "
        "Commands, variables and per-test detail: results.json.",
    ]
    for h in meta["harnesses"]:
        rows = [r for r in results if r["harness"] == h]
        if not rows:
            continue
        lines += [
            "",
            f"## {h}",
            "",
            "| task | run | exit | s | correctness | own tests valid/total | mutants killed | tautological | survivors |",
            "|---|---|---|---|---|---|---|---|---|",
        ]
        for r in rows:
            c, o, m = r["correctness"], r["own_tests"], r["mutation"]
            code = f"{r['exit']}{' timeout' if r['timed_out'] else ''}"
            lines.append(
                f"| {r['task']} | {r['repeat']} | {code} | {r['seconds']:.0f} | {c['passed']}/{c['total']} ({pct(c['rate'])}) "
                f"| {o['valid']}/{o['total']} | {m['killed']}/{m['total']} ({pct(m['score'])}) | {len(r['tautological'])} "
                f"| {', '.join(m['survivors']) or '-'} |"
            )
        n = len(rows)
        lines.append(
            f"| **mean** | {n} runs | | | {pct(sum(r['correctness']['rate'] for r in rows) / n)} | | "
            f"{pct(sum(r['mutation']['score'] for r in rows) / n)} | {sum(len(r['tautological']) for r in rows) / n:.1f} | |"
        )
    return "\n".join(lines) + "\n"


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", required=True, help="results root; each invocation adds a UTC-timestamped folder")
    p.add_argument("--harness", action="append", default=[], help="a harnesses.json entry; repeatable")
    p.add_argument("--task", action="append", default=[], help="a tasks/ entry; repeatable (default: all)")
    p.add_argument("--repeat", type=int, default=1, help="runs per harness and task")
    p.add_argument("--var", action="append", default=[], metavar="KEY=VALUE", help="template variable; repeatable")
    p.add_argument("--harnesses", default=str(EVAL / "harnesses.json"))
    p.add_argument("--python", default=sys.executable, help="interpreter the tests run under")
    p.add_argument("--runner", choices=["auto", "unittest", "pytest"], default="auto", help="runner for the model's tests")
    p.add_argument("--dry-run", action="store_true", help="validate the tasks only; no harness runs")
    return p.parse_args()


def main():
    args = parse_args()
    config = json.loads(Path(args.harnesses).read_text())
    unknown = [h for h in args.harness if h not in config["harnesses"]]
    if unknown:
        raise SystemExit(f"unknown harness {unknown}; known: {sorted(config['harnesses'])}")
    if not args.dry_run and not args.harness:
        raise SystemExit("name at least one --harness (or pass --dry-run)")
    names = args.task or sorted(p.name for p in TASKS.iterdir() if (p / "reference.py").is_file())
    tasks = [Task(n) for n in names]

    variables = {"repo": str(REPO), "eval_dir": str(EVAL), "python": args.python}
    for key, value in config["vars"].items():
        variables[key] = value.format_map(variables)
    for kv in args.var:
        key, sep, value = kv.partition("=")
        if not sep:
            raise SystemExit(f"--var wants KEY=VALUE, got {kv!r}")
        variables[key] = value
    for h in args.harness:
        spec = config["harnesses"][h]
        for field in sorted(set().union(*(placeholders(a) for a in spec["argv"]))):
            if variables.get(field) == "":
                raise SystemExit(f"harness {h} needs --var {field}=...")

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    session = Path(args.out).resolve() / stamp
    session.mkdir(parents=True)
    runner = args.runner
    if runner == "auto":
        has_pytest = subprocess.run([args.python, "-c", "import pytest"], capture_output=True).returncode == 0
        runner = "pytest" if has_pytest else "unittest"

    reports, hidden_ids = [], {}
    for task in tasks:
        report, hidden_ids[task.name] = check_task(task, session / "tasks" / task.name, args.python)
        reports.append(report)
        state = "ok" if not report["problems"] else "BROKEN: " + "; ".join(report["problems"])
        kills = report["killing_tests_per_mutant"]
        print(
            f"task {task.name}: {report['hidden_tests']} hidden tests, "
            f"{sum(1 for k in kills.values() if k)}/{len(kills)} mutants killed by them: {state}"
        )
    (session / "tasks.json").write_text(json.dumps(reports, indent=1))
    if any(r["problems"] for r in reports):
        raise SystemExit(1)
    if args.dry_run:
        print(f"dry run ok: {session}")
        return

    meta = {"argv": sys.argv, "vars": variables, "prompt": config["prompt"], "runner": runner, "harnesses": args.harness}
    results = []
    for h in args.harness:
        spec = config["harnesses"][h]
        for task in tasks:
            for r in range(1, args.repeat + 1):
                run_dir = session / h / task.name / f"r{r}"
                work = run_dir / "work"
                task.seed(work)
                v = dict(variables, work=str(work), run_dir=str(run_dir), task=task.name, task_dir=str(task.dir), package=task.package)
                v["prompt"] = config["prompt"].format_map(v)
                argv = [a.format_map(v) for a in spec["argv"]]
                cwd = spec.get("cwd", "{work}").format_map(v)
                env = dict(os.environ, **{k: x.format_map(v) for k, x in spec.get("env", {}).items()})
                print(f"[{h}] {task.name} r{r}: running", flush=True)
                result = {"harness": h, "task": task.name, "repeat": r, "argv": argv, "cwd": cwd, "work": str(work), "git": git_seed(work)}
                with (run_dir / "harness.stdout").open("w") as out, (run_dir / "harness.stderr").open("w") as err:
                    try:
                        code, timed_out, secs = run_process(argv, cwd, env, out, err, spec.get("timeout", 1800))
                    except OSError as e:
                        code, timed_out, secs = None, False, 0.0
                        err.write(f"could not start the harness: {e}\n")
                result.update(exit=code, timed_out=timed_out, seconds=secs)
                result.update(score(task, work, run_dir / "score", args.python, runner, hidden_ids[task.name]))
                results.append(result)
                c, m = result["correctness"], result["mutation"]
                print(
                    f"[{h}] {task.name} r{r}: exit {code}, correctness {c['passed']}/{c['total']}, "
                    f"mutants killed {m['killed']}/{m['total']}, tautological {len(result['tautological'])}",
                    flush=True,
                )
                (session / "results.json").write_text(json.dumps({**meta, "tasks": reports, "results": results}, indent=1))
    (session / "summary.md").write_text(summary(stamp, meta, results))
    print(f"results: {session / 'results.json'}\nsummary: {session / 'summary.md'}")


if __name__ == "__main__":
    main()
