//! The helper injected in front of every program: `aho.call(tool, args)` in Python and Node.
//!
//! Protocol (one request in flight per program; the helper holds a lock around it):
//! * the program's source arrives as the first stdin line (a JSON string), so argv stays small;
//! * a request is `{"id", "tool", "args"}` as ASCII JSON, cut into pieces of at most `PIECE` bytes,
//!   each written with one `write(2)` as the line `\x1eAHO_PTC:<nonce>:<id>:<more>:<piece>\n`
//!   (`more` = 1 until the last piece). stderr is merged into stdout at the fd level, and a pipe
//!   write of at most `PIPE_BUF` bytes is atomic, so no other output can split a piece;
//! * the answer is one stdin line: `{"id", "ok": true, "value"}` or `{"id", "ok": false, "code", "reason"}`.
//!
//! Python moves the channel off fd 0 (stdin reads see EOF); Node cannot `dup2`, so a Node
//! program must not read stdin — the tool description says so.

/// Marker at the start of every piece; the per-run nonce follows.
pub const MARKER: &str = "\u{1e}AHO_PTC:";
/// Bytes of request JSON per piece: marker + nonce + ids + this stays under macOS's 512-byte PIPE_BUF.
pub const PIECE: usize = 400;

pub fn python(nonce: &str) -> String {
    PYTHON.replace("@NONCE@", nonce).replace("@PIECE@", &PIECE.to_string())
}

pub fn node(nonce: &str) -> String {
    NODE.replace("@NONCE@", nonce).replace("@PIECE@", &PIECE.to_string())
}

const PYTHON: &str = r#"import sys as _s, os as _o, json as _j, threading as _t
def _aho_setup():
    src = _j.loads(_s.stdin.buffer.readline())
    ch = _o.dup(0)
    nul = _o.open(_o.devnull, _o.O_RDONLY)
    _o.dup2(nul, 0)
    _o.close(nul)
    rx = _o.fdopen(ch, "rb")
    lock = _t.Lock()
    seq = [0]
    class ToolError(Exception):
        def __init__(self, code, reason):
            super().__init__(code + ": " + reason)
            self.code = code
            self.reason = reason
    def _write(b):
        while b:
            b = b[_o.write(1, b):]
    def call(tool, args=None, **kw):
        a = dict(args or {})
        a.update(kw)
        with lock:
            seq[0] += 1
            i = seq[0]
            body = _j.dumps({"id": i, "tool": tool, "args": a})
            try:
                _s.stdout.flush()
                _s.stderr.flush()
            except Exception:
                pass
            parts = [body[k:k + @PIECE@] for k in range(0, len(body), @PIECE@)]
            for n, part in enumerate(parts):
                more = 1 if n + 1 < len(parts) else 0
                _write(("\x1eAHO_PTC:@NONCE@:%d:%d:%s\n" % (i, more, part)).encode("ascii"))
            line = rx.readline()
        if not line:
            raise ToolError("PTC_CLOSED", "the tool channel closed; the harness stopped answering")
        r = _j.loads(line)
        if r.get("ok"):
            return r.get("value")
        raise ToolError(r.get("code", "PTC_ERROR"), r.get("reason", ""))
    mod = type(_s)("aho")
    mod.call = call
    mod.ToolError = ToolError
    _s.modules["aho"] = mod
    return src, mod
_aho_src, _aho_mod = _aho_setup()
del _aho_setup
_aho_g = {"__name__": "__main__", "__builtins__": __builtins__, "aho": _aho_mod}
exec(compile(_aho_src, "<run_code>", "exec"), _aho_g)
"#;

const NODE: &str = r#""use strict";
const _aho = (() => {
  const fs = require("fs");
  let pending = Buffer.alloc(0);
  const nap = new Int32Array(new SharedArrayBuffer(4));
  function readLine() {
    for (;;) {
      const nl = pending.indexOf(10);
      if (nl >= 0) {
        const line = pending.subarray(0, nl).toString("utf8");
        pending = pending.subarray(nl + 1);
        return line;
      }
      const chunk = Buffer.alloc(65536);
      let n;
      try {
        n = fs.readSync(0, chunk, 0, chunk.length, null);
      } catch (e) {
        if (e.code === "EAGAIN") { Atomics.wait(nap, 0, 0, 5); continue; }
        if (e.code === "EOF") return null;
        throw e;
      }
      if (n === 0) return null;
      pending = Buffer.concat([pending, chunk.subarray(0, n)]);
    }
  }
  function writeAll(s) {
    let b = Buffer.from(s, "ascii");
    while (b.length > 0) {
      try {
        b = b.subarray(fs.writeSync(1, b));
      } catch (e) {
        if (e.code === "EAGAIN") { Atomics.wait(nap, 0, 0, 5); continue; }
        throw e;
      }
    }
  }
  class ToolError extends Error {
    constructor(code, reason) { super(code + ": " + reason); this.name = "ToolError"; this.code = code; this.reason = reason; }
  }
  const src = JSON.parse(readLine());
  let seq = 0;
  function call(tool, args) {
    seq += 1;
    const body = JSON.stringify({ id: seq, tool: tool, args: args || {} })
      .replace(/[\u007f-\uffff]/g, (c) => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0"));
    for (let k = 0; k < body.length || k === 0; k += @PIECE@) {
      const more = k + @PIECE@ < body.length ? 1 : 0;
      writeAll("\x1eAHO_PTC:@NONCE@:" + seq + ":" + more + ":" + body.slice(k, k + @PIECE@) + "\n");
    }
    const line = readLine();
    if (line === null) throw new ToolError("PTC_CLOSED", "the tool channel closed; the harness stopped answering");
    const r = JSON.parse(line);
    if (r.ok) return r.value;
    throw new ToolError(r.code || "PTC_ERROR", r.reason || "");
  }
  return { src: src, api: { call: call, ToolError: ToolError } };
})();
const _aho_fn = require("vm").compileFunction(
  "return (async () => {" + _aho.src + "\n})();",
  ["require", "aho", "module", "exports", "__filename", "__dirname"],
  { filename: "run_code.js" },
);
Promise.resolve(_aho_fn(require, _aho.api, module, exports, "run_code.js", process.cwd())).catch((e) => {
  process.exitCode = 1;
  console.error(e && e.stack ? e.stack : String(e));
});
"#;
