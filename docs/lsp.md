# Language servers (`tool-lsp`)

`tool-lsp` runs language servers in the bound world (through its `Subprocess` handle, never
`std::process`), one per (language, workspace root): started on first use, initialized once,
kept across calls, stopped at session end. Files are read through the world's `FsRead`.

## Tools

`lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_rename_preview` (read-only diffs; a
server's `workspace/applyEdit` is always refused), `lsp_symbols`, and `lsp_diagnostics{path}`:
the file's current disk content, pulled (`textDocument/diagnostic`) where the server offers it
plus anything it pushed for that content since (rust-analyzer's `cargo check`), else the pushed
list once it settles (`caps.tool-lsp.diagnostics_wait_ms`).

## Diagnostics on write

After every successful `write`, `edit` and `str_replace_editor` (`create`, `str_replace`,
`insert`) whose file a configured server covers, the `post_execute` `ToolLsp::after_write`
spawns one background task per write (`src/on_write.rs`):

1. `didOpen`/`didChange` with the new content, then the server's own diagnostics for it (pulled,
   or the settled push for exactly this version), waiting for indexing if needed;
2. the write's result waits for step 1 at most `caps.tool-lsp.write_diagnostics_wait_ms` (800,
   under `tools.grace_ms` = 1000, so the result is never turned into a late placeholder). Errors
   then warnings, by position, at most 20, as `path:line:col severity [code]: message (source)`
   under a header, are appended; infos and hints are dropped;
3. with `caps.tool-lsp.check_on_save` (true): `didSave`, if the server asked for saves. When the
   server starts work-done progress within 500 ms (rust-analyzer's `cargo check`), the task waits
   for it to end and merges what it pushed for the file;
4. if the file's errors and warnings now differ from what the result showed (the check found
   more, or step 1 missed the result's deadline), a late notice is queued. The capability's
   `context` fn hands queued notices to the session, which logs them as a user message at the
   next request: `Late diagnostics #N for <path> as last written, …` plus the same rows.

Steps 1 and 3 give up `caps.tool-lsp.late_diagnostics_wait_ms` (180000) after the write. A newer
write of the same file drops the older write's notice, so nothing about replaced content
arrives. The result is left unchanged when `caps.tool-lsp.diagnostics_on_write` is off, no
server covers the file, the server fails to start or dies, there are no errors or warnings, or
the runtime is not multi-threaded (the hook blocks its worker with `block_in_place`).

Wording lives in `crates/tools/tool-lsp/prompts/write-diagnostics.md`; the rows are mechanical.
Only the written file's diagnostics are reported, not errors the change causes elsewhere.

## Configuration (`caps.tool-lsp.*`)

| key | default | meaning |
|---|---|---|
| `servers.<language>.{command, globs, roots, init_options, settings, enabled}` | rust | built-in rows for rust, go, python, typescript, c fill unset fields |
| `ready_timeout_ms` | 60000 | wait for indexing before an `lsp_*` call answers anyway |
| `request_timeout_ms` | 30000 | one request |
| `diagnostics_wait_ms` | 15000 | `lsp_diagnostics` wait |
| `max_results` | 50 | rows per `lsp_*` call |
| `diagnostics_on_write` | true | diagnostics on write |
| `write_diagnostics_wait_ms` | 800 | how long a write's result waits for the server's own diagnostics |
| `check_on_save` | true | `didSave` after writes; the rust row's `checkOnSave` |
| `late_diagnostics_wait_ms` | 180000 | how long after a write late diagnostics are still collected |

The rust row's `init_options`/`settings` default to `{"checkOnSave": <check_on_save>}`; setting
either field yourself replaces that default for the field.

Tests: `tests/write_diagnostics.rs` drives tool-fs writes through `after_write` against the
`lsp-fixture` server (`tests/fixture/lsp_fixture.rs`); `tests/rust_analyzer.rs` runs a real
rust-analyzer (and `cargo check`) when it is on PATH.
