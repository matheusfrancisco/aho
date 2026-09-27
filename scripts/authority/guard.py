#!/usr/bin/env python3
"""Audit ambient-authority lint inheritance and narrowly declared exceptions.

Clippy resolves methods, including aliases and UFCS, and source matching cannot replace that.
This guard checks the *escape hatches* and ownership declarations. There is no --update:
adding an exception requires an explicit code review.
"""
from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REGISTRY = Path(__file__).with_name("exceptions.json")
REQUIRED_METHODS = {
    "std::fs::read", "std::fs::write", "std::fs::File::open", "std::fs::OpenOptions::open",
    "std::path::Path::canonicalize", "std::path::Path::exists", "std::process::Command::new",
    "std::process::Command::spawn", "std::env::var", "std::env::var_os", "std::env::vars_os",
    "std::net::TcpStream::connect", "std::net::TcpListener::bind", "std::net::UdpSocket::bind",
    "tokio::fs::read", "tokio::fs::write", "tokio::process::Command::spawn",
    "tokio::net::TcpStream::connect", "tokio::net::TcpListener::bind", "reqwest::RequestBuilder::send",
    "tempfile::tempdir", "tempfile::Builder::tempdir_in",
}


def mask_literals(source: str) -> str:
    """Keep offsets/newlines while removing Rust comments and literal contents.

    Nested block comments, raw strings, byte strings, escaped quotes, chars and
    lifetimes are distinguished so a quoted fake attribute cannot grant authority.
    """
    out = list(source)
    i = 0
    while i < len(source):
        start = i
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
        elif source.startswith("/*", i):
            i += 2
            depth = 1
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated block comment")
        else:
            raw = re.match(r'(?:br|cr|r)(#*)"', source[i:])
            if raw and (i == 0 or not (source[i - 1].isalnum() or source[i - 1] == '_')):
                end = source.find('"' + raw[1], i + raw.end())
                if end < 0:
                    raise ValueError("unterminated raw string")
                i = end + 1 + len(raw[1])
            elif source[i] == '"':
                i += 1
                while i < len(source):
                    if source[i] == '\\':
                        i += 2
                    elif source[i] == '"':
                        i += 1
                        break
                    else:
                        i += 1
            elif source[i] == "'" and (char := re.match(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'", source[i:])):
                i += char.end()
            else:
                i += 1
                continue
        for pos in range(start, min(i, len(source))):
            if source[pos] != '\n':
                out[pos] = ' '
    return ''.join(out)


def attributes(source: str, masked: str):
    for match in re.finditer(r'#\s*(!?)\s*\[', masked):
        pos = match.end()
        depth = 1
        while pos < len(masked) and depth:
            depth += (masked[pos] == '[') - (masked[pos] == ']')
            pos += 1
        if depth:
            raise ValueError("unterminated attribute")
        yield match.start(), pos, bool(match[1]), source[match.end():pos - 1]


def item_after(masked: str, pos: int) -> str:
    rest = masked[pos:].lstrip()
    while rest.startswith('#'):
        closing = next(attributes(rest, rest), None)
        if closing is None:
            break
        rest = rest[closing[1]:].lstrip()
    match = re.match(r'(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:async|unsafe|const|default)\s+)*(fn|mod|let)\s+(\w+)', rest)
    if match:
        return f"{match[1]} {match[2]}"
    if rest.startswith('impl'):
        return ' '.join(rest.split('{', 1)[0].split())
    return "<not an audited item>"


def inspect_source(relative: str, source: str, registry: dict, seen: set) -> list[str]:
    errors = []
    masked = mask_literals(source)
    for start, end, inner, body in attributes(source, masked):
        # Test fixtures touch the host by design. The one accepted escape hatch is gated on
        # cfg(test) at a crate root, so no production build is ever exempted.
        if inner and re.fullmatch(r'\s*cfg_attr\s*\(\s*test\s*,\s*allow\s*\(\s*clippy::disallowed_methods\s*\)\s*\)\s*', body):
            if not relative.endswith(('/src/lib.rs', '/src/main.rs')):
                errors.append(f"{relative}:{source.count(chr(10), 0, start) + 1}: the test-only exemption belongs at the crate root")
            continue
        if not re.search(r'\b(?:allow|expect)\s*\(', body):
            continue
        if not re.search(r'\bclippy\s*::\s*(?:disallowed_methods|disallowed_types|restriction|all)\b', body):
            continue
        loc = f"{relative}:{source.count(chr(10), 0, start) + 1}"
        ident = re.search(r'reason\s*=\s*"authority: ([a-z0-9-]+)"', body)
        exact = re.fullmatch(r'\s*allow\s*\(\s*clippy::disallowed_methods\s*,\s*reason\s*=\s*"authority: [a-z0-9-]+"\s*,?\s*\)\s*', body)
        if inner or ident is None or exact is None:
            errors.append(f"{loc}: broad or unaudited authority suppression; use an item-scoped authority ID")
            continue
        key = ident[1]
        if key in seen:
            errors.append(f"{loc}: duplicate authority ID {key}")
        seen.add(key)
        entry = registry.get(key)
        item = item_after(masked, end)
        if entry is None:
            errors.append(f"{loc}: authority ID {key} has no reviewed declaration")
        elif entry.get('path') != relative or entry.get('item') != item:
            errors.append(f"{loc}: {key} moved outside its audited scope ({relative}, {item})")
        elif not entry.get('authority') or not entry.get('reason'):
            errors.append(f"{loc}: {key} must declare its authority and justification")
        if relative.startswith('crates/tools/'):
            errors.append(f"{loc}: tools cannot exempt ambient authority; use bound seams")
        if relative.startswith(('crates/aho-', 'crates/surfaces/')) and not item.startswith(('fn ', 'let ')):
            errors.append(f"{loc}: core/surface exceptions must cover only the I/O item, never a module/impl")
    if relative.startswith(('crates/tools/', 'crates/surfaces/')):
        for match in re.finditer(r'\bstatic\s+(?:mut\s+)?\w+\s*:\s*[^=;]+', masked):
            declaration = match[0]
            if re.search(r'\b(mut|Mutex|RwLock|Cell|RefCell|Atomic\w+)\b', declaration):
                errors.append(f"{relative}: process-global mutable state is not a session capability: {' '.join(declaration.split())}")
    return errors


def check(root: Path = ROOT) -> list[str]:
    errors = []
    manifest = tomllib.loads((root / 'Cargo.toml').read_text())
    level = manifest.get('workspace', {}).get('lints', {}).get('clippy', {}).get('disallowed_methods')
    if isinstance(level, dict):
        level = level.get('level')
    if level not in ('deny', 'forbid'):
        errors.append('workspace must deny clippy::disallowed_methods')
    policy = tomllib.loads((root / 'clippy.toml').read_text())
    methods = {row if isinstance(row, str) else row['path'] for row in policy.get('disallowed-methods', [])}
    for missing in sorted(REQUIRED_METHODS - methods):
        errors.append(f'clippy.toml dropped required authority restriction {missing}')
    registry = json.loads((root / 'scripts/authority/exceptions.json').read_text())
    seen = set()
    for base in ('crates', 'apps'):
        for cargo in sorted((root / base).rglob('Cargo.toml')):
            if any(part in ('target', 'tests', 'fixtures') for part in cargo.relative_to(root).parts):
                continue
            package = tomllib.loads(cargo.read_text())
            if 'package' not in package:
                continue
            if package.get('lints', {}).get('workspace') is not True:
                errors.append(f'{cargo.relative_to(root)} must inherit workspace lints')
            directory = cargo.parent
            for path in sorted(directory.glob('src/**/*.rs')):
                if path.name == 'tests.rs' or 'tests' in path.relative_to(directory).parts:
                    continue
                relative = path.relative_to(root).as_posix()
                errors.extend(inspect_source(relative, path.read_text(), registry, seen))
            if directory.name.startswith(('tool-', 'surface-')):
                crate_root = directory / 'src/lib.rs'
                source = mask_literals(crate_root.read_text())
                if not re.search(r'#!\s*\[\s*forbid\s*\(\s*unsafe_code\s*\)\s*\]', source):
                    errors.append(f'{crate_root.relative_to(root)} must forbid unsafe_code')
    for stale in sorted(set(registry) - seen):
        errors.append(f'authority exception {stale} is stale; remove obsolete declarations')
    return errors


def main() -> int:
    try:
        errors = check()
    except (ValueError, OSError, KeyError) as error:
        print(f'authority: FAIL: {error}', file=sys.stderr)
        return 1
    for error in errors:
        print(f'authority: {error}', file=sys.stderr)
    if errors:
        return 1
    print('authority: reviewed exceptions, unsafe consumer boundary, and lint inheritance verified')
    return 0


if __name__ == '__main__':
    sys.exit(main())
