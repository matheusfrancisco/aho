#!/usr/bin/env python3
"""Runs unittest discovery and writes one outcome per test id as JSON. Used by run.py.

    python3 unittest_json.py <root> <start dir> <out.json>

<root> goes on sys.path (the package under test lives there). Discovery treats <start dir> as
its own top level unless it is a package, the same as `python3 -m unittest discover -s <start>`
run from <root>. Outcomes: pass, fail, error, skip. A test with a failing subTest is a fail.
"""

import json
import os
import sys
import unittest
from pathlib import Path


class Recorder(unittest.TestResult):
    def __init__(self):
        super().__init__()
        self.outcomes = {}

    def _set(self, test, outcome):
        self.outcomes[test.id()] = outcome

    def addSuccess(self, test):
        self._set(test, "pass")

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self._set(test, "fail")

    def addError(self, test, err):
        super().addError(test, err)
        self._set(test, "error")

    def addSkip(self, test, reason):
        self._set(test, "skip")

    def addExpectedFailure(self, test, err):
        self._set(test, "pass")

    def addUnexpectedSuccess(self, test):
        self._set(test, "fail")

    def addSubTest(self, test, subtest, err):
        if err is not None:
            self._set(test, "fail" if issubclass(err[0], test.failureException) else "error")


def main():
    root, start, out = (Path(a).resolve() for a in sys.argv[1:4])
    top = root if (start / "__init__.py").exists() else start
    sys.path.insert(0, str(root))
    os.chdir(root)
    suite = unittest.defaultTestLoader.discover(str(start), pattern="test*.py", top_level_dir=str(top))
    result = Recorder()
    suite.run(result)
    out.write_text(json.dumps({"outcomes": result.outcomes}, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
