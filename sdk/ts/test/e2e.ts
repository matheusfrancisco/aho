// End-to-end check of the TypeScript SDK against a real server (`aho rpc`, or `aho-sdk rpc`)
// driven by the deterministic scripted model: open a session, prompt, answer the approval card
// for `write`, and receive the tool calls, their results, and the final text.
//
//   node --experimental-strip-types test/e2e.ts --bin <path to aho> --project <empty dir>
//
// Prints a JSON summary on stdout; exits 1 with the reason on stderr when a check fails.

import { strict as assert } from "node:assert";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { Client } from "../src/index.ts";
import type { ApprovalCard, Entry } from "../src/index.ts";

const { values } = parseArgs({ options: { bin: { type: "string", default: "aho" }, project: { type: "string" } } });
const project = values.project;
if (project === undefined) throw new Error("pass --project <empty dir>");
setTimeout(() => {
  console.error("e2e: no idle within 60 s");
  process.exit(1);
}, 60_000).unref();

writeFileSync(join(project, "a.txt"), "hello from disk\n");
const script = join(project, "script.json");
writeFileSync(
  script,
  JSON.stringify({
    steps: [
      { tool_calls: [{ name: "read", args: { path: "a.txt" } }], stop: "tool_use" },
      {
        expect_tool_result_contains: "hello from disk",
        tool_calls: [{ name: "write", args: { path: "b.txt", content: "written by the ts sdk\n" } }],
        stop: "tool_use",
      },
      { expect_tool_result_contains: "b.txt", text: "Read a.txt and wrote b.txt." },
    ],
  }),
);

const client = await Client.spawn({
  command: values.bin,
  project,
  sets: { "model.provider": "scripted", "provider.scripted.script": script, "policy.rules": '["ask write(*)"]' },
  clientInstance: "ts-e2e",
});
const events = client.events();
const subscribed = await client.subscribe();
const turn = client.prompt({ text: "read a.txt then write b.txt" });
turn.catch((e: unknown) => {
  console.error(`e2e: the prompt failed: ${String(e)}`);
  process.exit(1);
});

const entries: Entry[] = [];
const approvals: ApprovalCard[] = [];
let streamed = "";
for await (const ev of events) {
  if (ev.event === "entry") entries.push(ev.payload);
  else if (ev.event === "stream" && ev.payload.kind === "text") streamed += ev.payload.text;
  else if (ev.event === "approval") {
    approvals.push(ev.payload);
    await client.answerApproval({ id: ev.payload.id, outcome: "allow" });
  } else if (ev.event === "idle") break;
}
const { transition } = await turn;
const fold = await client.fold();
const exit = await client.close();

const toolCalls = entries.flatMap((e) => (e.kind.type === "tool_call" ? [{ call: e.kind.call, tool: e.kind.tool, args: e.kind.args }] : []));
const toolResults = entries.flatMap((e) =>
  e.kind.type === "tool_result"
    ? [{ call: e.kind.call, outcome: e.kind.outcome.outcome, text: e.kind.content.flatMap((b) => (b.type === "text" ? [b.text] : [])).join("") }]
    : [],
);
const finals = entries.flatMap((e) =>
  e.kind.type === "assistant_message" && e.kind.stop === "end_turn" ? [e.kind.content.flatMap((b) => (b.type === "text" ? [b.text] : [])).join("")] : [],
);

assert.equal(transition.transition, "completed");
assert.ok(subscribed.replayed > 0, "the bootstrap entries are replayed on subscribe");
assert.deepEqual(toolCalls.map((c) => c.tool), ["read", "write"]);
assert.deepEqual(toolResults.map((r) => r.outcome), ["ok", "ok"]);
assert.ok(toolResults[0].text.includes("hello from disk"), toolResults[0].text);
assert.deepEqual(approvals.map((a) => [a.tool, a.class]), [["write", "workspace_write"]]);
assert.equal(finals.at(-1), "Read a.txt and wrote b.txt.");
assert.equal(readFileSync(join(project, "b.txt"), "utf8"), "written by the ts sdk\n");
assert.equal(fold.turns, 1);
assert.equal(exit, 0);

console.log(
  JSON.stringify({
    sdk: "ts",
    version: client.hello.version,
    session: client.hello.session,
    replayed: subscribed.replayed,
    tool_calls: toolCalls,
    tool_results: toolResults,
    approvals: approvals.map((a) => ({ id: a.id, tool: a.tool, reason: a.reason })),
    final_text: finals.at(-1),
    streamed_text: streamed,
    transition: transition.transition,
    exit,
  }),
);
