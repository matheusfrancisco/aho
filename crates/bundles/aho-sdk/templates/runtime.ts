import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";
import { VERSION_MAX, VERSION_MIN } from "./protocol.ts";
import type { Event, Hello, HelloOk, ServerFrame } from "./protocol.ts";

/** Server launch options: `[command, ...globals, ...args, "rpc", ...session]`, the argv of `aho rpc`. */
export interface SpawnOptions {
  /** Program to run; default `aho` (looked up on PATH). */
  command?: string;
  /** `--project <dir>`. */
  project?: string;
  /** `--set KEY=VALUE` per entry; values are raw `--set` text (a TOML scalar such as `true` or `["a"]`, else a string). */
  sets?: Record<string, string>;
  /** `--disable <id>` per entry. */
  disable?: string[];
  /** Resume this session (`--session <id>`). */
  session?: string;
  /** Extra arguments before `rpc`. */
  args?: string[];
  cwd?: string;
  /** Added to the inherited environment; `undefined` removes a variable. */
  env?: Record<string, string | undefined>;
  /** `hello.client_instance`; default `ts-sdk`. */
  clientInstance?: string;
  hasLocalHistory?: boolean;
  allowTakeover?: boolean;
  /** Destination of the server's stderr; default `inherit`. */
  stderr?: "inherit" | "ignore";
}

/** The server answered a request with an `error` frame. */
export class RpcError extends Error {
  readonly method: string;
  readonly serverMessage: string;

  constructor(method: string, message: string) {
    super(`\`${method}\` failed: ${message}`);
    this.name = "RpcError";
    this.method = method;
    this.serverMessage = message;
  }
}

/** The connection ended before a reply arrived, or a request was made after it ended. */
export class ConnectionClosed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConnectionClosed";
  }
}

interface Waiting {
  method: string;
  resolve: (payload: unknown) => void;
  reject: (error: Error) => void;
}

/** The server's argv (without the program) for `options`. */
export function argv(options: SpawnOptions): string[] {
  const a: string[] = [];
  if (options.project !== undefined) a.push("--project", options.project);
  for (const [k, v] of Object.entries(options.sets ?? {})) a.push("--set", `${k}=${v}`);
  for (const id of options.disable ?? []) a.push("--disable", id);
  a.push(...(options.args ?? []), "rpc");
  if (options.session !== undefined) a.push("--session", options.session);
  return a;
}

function environment(extra: Record<string, string | undefined>): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries({ ...process.env, ...extra })) {
    if (v !== undefined) env[k] = v;
  }
  return env;
}

/** One NDJSON connection to a server child process. */
export class Connection {
  #child: ChildProcess;
  #version: number = VERSION_MAX;
  #nextId = 1;
  #pending = new Map<number, Waiting>();
  #queue: Event[] = [];
  #waiters: Array<(r: IteratorResult<Event, undefined>) => void> = [];
  #closed: string | null = null;
  #exit: Promise<number | null>;
  #hello: HelloOk | null = null;

  private constructor(child: ChildProcess, command: string) {
    this.#child = child;
    const exit = Promise.withResolvers<number | null>();
    this.#exit = exit.promise;
    child.once("close", (code: number | null) => exit.resolve(code));
    child.once("error", (e: Error) => {
      this.#shutdown(`could not run ${command} (${e.message}); pass \`command\` with the path of an aho binary`);
      exit.resolve(null);
    });
    child.stdin?.on("error", (e: Error) => this.#shutdown(`writing to the server failed (${e.message})`));
    if (child.stdout === null) throw new ConnectionClosed("the server process has no piped stdout");
    const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
    lines.on("line", (line: string) => this.#frame(line));
    lines.on("close", () => this.#shutdown("the server closed the connection"));
  }

  /** Starts the server and completes the `hello` handshake. */
  static async spawn(options: SpawnOptions = {}): Promise<Connection> {
    const command = options.command ?? "aho";
    const child = spawn(command, argv(options), {
      cwd: options.cwd,
      env: environment(options.env ?? {}),
      stdio: ["pipe", "pipe", options.stderr ?? "inherit"],
    });
    const c = new Connection(child, command);
    const hello: Hello = {
      min: VERSION_MIN,
      max: VERSION_MAX,
      client_instance: options.clientInstance ?? "ts-sdk",
      has_local_history: options.hasLocalHistory ?? false,
      allow_takeover: options.allowTakeover ?? false,
    };
    const ok = (await c.request("hello", hello)) as HelloOk;
    c.#hello = ok;
    c.#version = ok.version;
    return c;
  }

  /** The server's `hello_ok`. */
  get hello(): HelloOk {
    if (this.#hello === null) throw new ConnectionClosed("the hello handshake has not completed");
    return this.#hello;
  }

  /** Sends one request; resolves its `result` payload, rejects with `RpcError` on an `error` reply. */
  request(method: string, params: unknown): Promise<unknown> {
    if (this.#closed !== null) return Promise.reject(new ConnectionClosed(`${this.#closed}; \`${method}\` was not sent`));
    const stdin = this.#child.stdin;
    if (stdin === null) return Promise.reject(new ConnectionClosed("the server process has no piped stdin"));
    const id = this.#nextId++;
    const line = JSON.stringify({ v: this.#version, id, method, params: params ?? {} }) + "\n";
    const reply = Promise.withResolvers<unknown>();
    this.#pending.set(id, { method, resolve: reply.resolve, reject: reply.reject });
    stdin.write(line, (err?: Error | null) => {
      if (err) {
        this.#pending.delete(id);
        reply.reject(new ConnectionClosed(`could not send \`${method}\` (${err.message})`));
      }
    });
    return reply.promise;
  }

  /** Server-initiated frames in wire order since connect; ends when the connection closes. One consumer. */
  events(): AsyncIterableIterator<Event> {
    const next = (): Promise<IteratorResult<Event, undefined>> => {
      const ev = this.#queue.shift();
      if (ev !== undefined) return Promise.resolve({ value: ev, done: false });
      if (this.#closed !== null) return Promise.resolve({ value: undefined, done: true });
      const waiter = Promise.withResolvers<IteratorResult<Event, undefined>>();
      this.#waiters.push(waiter.resolve);
      return waiter.promise;
    };
    const it: AsyncIterableIterator<Event> = {
      next,
      [Symbol.asyncIterator]() {
        return it;
      },
    };
    return it;
  }

  /** Ends stdin; the server answers what is in flight and exits. Resolves its exit code. */
  close(): Promise<number | null> {
    this.#child.stdin?.end();
    return this.#exit;
  }

  #frame(line: string): void {
    if (line.trim() === "") return;
    let f: ServerFrame;
    try {
      f = JSON.parse(line) as ServerFrame;
    } catch {
      this.#push({ v: this.#version, reply_to: null, event: "error", payload: { message: `the server sent a line that is not JSON; it was skipped: ${line.slice(0, 80)}` } });
      return;
    }
    const id = f.reply_to;
    const waiting = id === null || id === undefined ? undefined : this.#pending.get(id);
    if (waiting !== undefined && id !== null && id !== undefined) {
      this.#pending.delete(id);
      const message = (f.payload as { message?: unknown } | null)?.message;
      if (f.event === "error") waiting.reject(new RpcError(waiting.method, typeof message === "string" ? message : JSON.stringify(f.payload)));
      else waiting.resolve(f.payload);
      return;
    }
    this.#push(f as unknown as Event);
  }

  #push(ev: Event): void {
    const w = this.#waiters.shift();
    if (w !== undefined) w({ value: ev, done: false });
    else this.#queue.push(ev);
  }

  #shutdown(reason: string): void {
    if (this.#closed !== null) return;
    this.#closed = reason;
    for (const w of this.#pending.values()) w.reject(new ConnectionClosed(`${reason} before answering \`${w.method}\``));
    this.#pending.clear();
    for (const w of this.#waiters.splice(0)) w({ value: undefined, done: true });
  }
}
