#!/usr/bin/env python3
"""A harness without a model, for checking run.py's scoring end to end.

    python3 fake_harness.py <mode> <task dir> <work dir> <package>

reference     the reference implementation + the hidden tests: correctness 100%, every mutant
              killed, no test passes against the stub.
tautology     the reference implementation + tests that pass whatever the code does: correctness
              100%, no mutant killed, every test passes against the stub.
first-mutant  the task's first mutant as the implementation + the hidden tests: correctness below
              100% (its own tests fail on it), every mutant still killed.
"""

import ast
import json
import sys
from pathlib import Path

TAUTOLOGY = '''import unittest

import {package}


class Tautologies(unittest.TestCase):
    def test_truth(self):
        self.assertTrue(True)
{per_function}
'''

PER_FUNCTION = '''
    def test_{name}_exists(self):
        self.assertTrue(callable({package}.{name}))

    def test_{name}_rejects_missing_arguments(self):
        with self.assertRaises(Exception):
            {package}.{name}()
'''


def public_functions(source):
    tree = ast.parse(source)
    return [n.name for n in tree.body if isinstance(n, ast.FunctionDef) and not n.name.startswith("_")]


def main():
    mode, task, work, package = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4]
    reference = (task / "reference.py").read_text()
    hidden = (task / "hidden_test.py").read_text()
    if mode == "reference":
        impl, tests = reference, hidden
    elif mode == "tautology":
        names = public_functions((task / "stub.py").read_text())
        per = "".join(PER_FUNCTION.format(name=n, package=package) for n in names)
        impl, tests = reference, TAUTOLOGY.format(package=package, per_function=per)
    elif mode == "first-mutant":
        first = json.loads((task / "mutants.json").read_text())[0]
        impl, tests = reference.replace(first["find"], first["replace"], 1), hidden
    else:
        sys.exit(f"unknown mode {mode!r}")
    (work / package / "__init__.py").write_text(impl)
    (work / "tests").mkdir(exist_ok=True)
    (work / "tests" / f"test_{package}.py").write_text(tests)
    print(f"fake harness ({mode}) wrote {package}/__init__.py and tests/test_{package}.py")


if __name__ == "__main__":
    main()
