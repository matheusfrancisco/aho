import asyncio
import json
import os
from typing import Any, AsyncIterator, Mapping, Optional, Sequence, TypedDict, Unpack, cast

from .protocol import VERSION_MAX, VERSION_MIN, Event, HelloOk

# One frame per line; an entry carrying a large tool result is one long line.
_LINE_LIMIT = 64 * 1024 * 1024


class SpawnOptions(TypedDict, total=False):
    """Server launch options: `[command, *globals, *args, "rpc", *session]`, the argv of `aho rpc`."""

    command: str
    """Program to run; default `aho` (looked up on PATH)."""
    project: str
    """`--project <dir>`."""
    sets: Mapping[str, str]
    """`--set KEY=VALUE` per entry; values are raw `--set` text (a TOML scalar such as `true` or `["a"]`, else a string)."""
    disable: Sequence[str]
    """`--disable <id>` per entry."""
    session: str
    """Resume this session (`--session <id>`)."""
    args: Sequence[str]
    """Extra arguments before `rpc`."""
    cwd: str
    env: Mapping[str, str]
    """Added to the inherited environment."""
    client_instance: str
    """`hello.client_instance`; default `python-sdk`."""
    has_local_history: bool
    allow_takeover: bool
    stderr: int
    """Destination of the server's stderr (e.g. `asyncio.subprocess.DEVNULL`); default: inherited."""


class RpcError(Exception):
    """The server answered a request with an `error` frame."""

    def __init__(self, method: str, message: str) -> None:
        super().__init__(f"`{method}` failed: {message}")
        self.method = method
        self.server_message = message


class ConnectionClosed(Exception):
    """The connection ended before a reply arrived, or a request was made after it ended."""


def argv(options: SpawnOptions) -> list[str]:
    """The server's argv (without the program) for `options`."""
    a: list[str] = []
    if "project" in options:
        a += ["--project", options["project"]]
    for k, v in options.get("sets", {}).items():
        a += ["--set", f"{k}={v}"]
    for cap in options.get("disable", ()):
        a += ["--disable", cap]
    a += [*options.get("args", ()), "rpc"]
    if "session" in options:
        a += ["--session", options["session"]]
    return a


class Connection:
    """One NDJSON connection to a server child process."""

    def __init__(self, proc: asyncio.subprocess.Process) -> None:
        self._proc = proc
        self._version = VERSION_MAX
        self._next_id = 1
        self._pending: dict[int, tuple[str, asyncio.Future[Any]]] = {}
        self._events: asyncio.Queue[Optional[Event]] = asyncio.Queue()
        self._closed: Optional[str] = None
        self._hello: Optional[HelloOk] = None
        self._write = asyncio.Lock()
        self._reader = asyncio.create_task(self._read())

    @classmethod
    async def spawn(cls, **options: Unpack[SpawnOptions]) -> Connection:
        """Starts the server and completes the `hello` handshake."""
        command = options.get("command", "aho")
        env = {**os.environ, **options.get("env", {})}
        try:
            proc = await asyncio.create_subprocess_exec(
                command,
                *argv(options),
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=options.get("stderr"),
                cwd=options.get("cwd"),
                env=env,
                limit=_LINE_LIMIT,
            )
        except OSError as e:
            raise ConnectionClosed(f"could not run {command} ({e}); pass `command` with the path of an aho binary") from e
        conn = cls(proc)
        hello = {
            "min": VERSION_MIN,
            "max": VERSION_MAX,
            "client_instance": options.get("client_instance", "python-sdk"),
            "has_local_history": options.get("has_local_history", False),
            "allow_takeover": options.get("allow_takeover", False),
        }
        ok = cast(HelloOk, await conn.request("hello", hello))
        conn._hello = ok
        conn._version = ok["version"]
        return conn

    @property
    def hello(self) -> HelloOk:
        """The server's `hello_ok`."""
        if self._hello is None:
            raise ConnectionClosed("the hello handshake has not completed")
        return self._hello

    async def request(self, method: str, params: Any = None) -> Any:
        """Sends one request; returns its `result` payload, raises `RpcError` on an `error` reply."""
        if self._closed is not None:
            raise ConnectionClosed(f"{self._closed}; `{method}` was not sent")
        stdin = self._proc.stdin
        if stdin is None:
            raise ConnectionClosed("the server process has no piped stdin")
        rid = self._next_id
        self._next_id += 1
        reply: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[rid] = (method, reply)
        line = json.dumps({"v": self._version, "id": rid, "method": method, "params": {} if params is None else params})
        try:
            async with self._write:
                stdin.write(line.encode() + b"\n")
                await stdin.drain()
        except (ConnectionError, RuntimeError) as e:
            self._pending.pop(rid, None)
            raise ConnectionClosed(f"could not send `{method}` ({e})") from e
        return await reply

    async def events(self) -> AsyncIterator[Event]:
        """Server-initiated frames in wire order since connect; ends when the connection closes. One consumer."""
        while True:
            ev = await self._events.get()
            if ev is None:
                self._events.put_nowait(None)
                return
            yield ev

    async def close(self) -> Optional[int]:
        """Closes stdin; the server answers what is in flight and exits. Returns its exit code."""
        stdin = self._proc.stdin
        if stdin is not None and not stdin.is_closing():
            stdin.close()
            try:
                await stdin.wait_closed()
            except ConnectionError:
                pass  # the server already exited; its exit code says why
        code = await self._proc.wait()
        await self._reader
        return code

    async def _read(self) -> None:
        stdout = self._proc.stdout
        reason = "the server closed the connection"
        try:
            while stdout is not None:
                line = await stdout.readline()
                if not line:
                    break
                if not line.strip():
                    continue
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError:
                    text = line[:80].decode(errors="replace")
                    self._events.put_nowait(
                        cast(Event, {"v": self._version, "reply_to": None, "event": "error", "payload": {"message": f"the server sent a line that is not JSON; it was skipped: {text}"}})
                    )
                    continue
                rid = frame.get("reply_to")
                waiting = self._pending.pop(rid, None) if rid is not None else None
                if waiting is None:
                    self._events.put_nowait(cast(Event, frame))
                    continue
                method, reply = waiting
                if reply.done():
                    continue
                if frame.get("event") == "error":
                    payload = frame.get("payload")
                    message = payload.get("message") if isinstance(payload, dict) else None
                    reply.set_exception(RpcError(method, message if isinstance(message, str) else json.dumps(payload)))
                else:
                    reply.set_result(frame.get("payload"))
        except (ValueError, asyncio.LimitOverrunError) as e:
            reason = f"reading from the server failed ({e})"
        finally:
            self._closed = reason
            for method, reply in self._pending.values():
                if not reply.done():
                    reply.set_exception(ConnectionClosed(f"{reason} before answering `{method}`"))
            self._pending.clear()
            self._events.put_nowait(None)
