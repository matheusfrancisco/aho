"""Semantic Versioning 2.0.0 precedence: see README.md."""

import re

_NUMBER = re.compile(r"0|[1-9][0-9]*")
_IDENT = re.compile(r"[0-9A-Za-z-]+")


def _number(text, version):
    if not _NUMBER.fullmatch(text):
        raise ValueError(f"invalid number {text!r} in {version!r}")
    return int(text)


def _idents(text, version):
    parts = text.split(".")
    for part in parts:
        if not _IDENT.fullmatch(part):
            raise ValueError(f"invalid identifier {part!r} in {version!r}")
    return parts


def _parse(version):
    rest, plus, build = version.partition("+")
    if plus:
        _idents(build, version)
    core, dash, pre = rest.partition("-")
    numbers = core.split(".")
    if len(numbers) != 3:
        raise ValueError(f"expected MAJOR.MINOR.PATCH in {version!r}")
    key = tuple(_number(n, version) for n in numbers)
    pre_ids = []
    if dash:
        for ident in _idents(pre, version):
            pre_ids.append(_number(ident, version) if ident.isdigit() else ident)
    return key, pre_ids


def _cmp(x, y):
    return (x > y) - (x < y)


def _cmp_ident(a, b):
    a_num, b_num = isinstance(a, int), isinstance(b, int)
    if a_num and b_num:
        return _cmp(a, b)
    if a_num != b_num:
        return -1 if a_num else 1
    return _cmp(a, b)


def compare(a, b):
    (key_a, pre_a), (key_b, pre_b) = _parse(a), _parse(b)
    if key_a != key_b:
        return _cmp(key_a, key_b)
    if not pre_a or not pre_b:
        return _cmp(len(pre_b), len(pre_a))
    for x, y in zip(pre_a, pre_b):
        c = _cmp_ident(x, y)
        if c:
            return c
    return _cmp(len(pre_a), len(pre_b))
