"""End-to-end check of the Python SDK against a real server (`aho rpc`, or `aho-sdk rpc`) driven
by the deterministic scripted model: open a session, prompt, answer the approval card for
`write`, and receive the tool calls, their results, and the final text.

    PYTHONPATH=sdk/python python3 sdk/python/tests/e2e.py --bin <path to aho> --project <empty dir>

Prints a JSON summary on stdout; exits 1 with the reason on stderr when a check fails.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

from aho_sdk import Client, protocol


def text_of(blocks: list[protocol.ContentBlock]) -> str:
    return "".join(b["text"] for b in blocks if b["type"] == "text")


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="aho")
    ap.add_argument("--project", required=True)
    args = ap.parse_args()
    project = Path(args.project)
    (project / "a.txt").write_text("hello from disk\n")
    script = project / "script.json"
    script.write_text(
        json.dumps(
            {
                "steps": [
                    {"tool_calls": [{"name": "read", "args": {"path": "a.txt"}}], "stop": "tool_use"},
                    {
                        "expect_tool_result_contains": "hello from disk",
                        "tool_calls": [{"name": "write", "args": {"path": "b.txt", "content": "written by the python sdk\n"}}],
                        "stop": "tool_use",
                    },
                    {"expect_tool_result_contains": "b.txt", "text": "Read a.txt and wrote b.txt."},
                ]
            }
        )
    )

    client = await Client.spawn(
        command=args.bin,
        project=str(project),
        sets={"model.provider": "scripted", "provider.scripted.script": str(script), "policy.rules": '["ask write(*)"]'},
        client_instance="python-e2e",
    )
    events = client.events()
    subscribed = await client.subscribe()
    turn = asyncio.create_task(client.prompt({"text": "read a.txt then write b.txt"}))

    entries: list[protocol.Entry] = []
    approvals: list[protocol.ApprovalCard] = []
    streamed = ""
    async for ev in events:
        if turn.done() and turn.exception() is not None:
            raise SystemExit(f"e2e: the prompt failed: {turn.exception()}")
        if ev["event"] == "entry":
            entries.append(ev["payload"])
        elif ev["event"] == "stream" and ev["payload"]["kind"] == "text":
            streamed += ev["payload"]["text"]
        elif ev["event"] == "approval":
            approvals.append(ev["payload"])
            await client.answer_approval({"id": ev["payload"]["id"], "outcome": "allow"})
        elif ev["event"] == "idle":
            break
    result = await turn
    fold = await client.fold()
    code = await client.close()

    kinds = [e["kind"] for e in entries]
    tool_calls = [{"call": k["call"], "tool": k["tool"], "args": k["args"]} for k in kinds if k["type"] == "tool_call"]
    tool_results = [{"call": k["call"], "outcome": k["outcome"]["outcome"], "text": text_of(k["content"])} for k in kinds if k["type"] == "tool_result"]
    finals = [text_of(k["content"]) for k in kinds if k["type"] == "assistant_message" and k["stop"] == "end_turn"]

    checks = [
        (result["transition"]["transition"] == "completed", f"transition {result['transition']}"),
        (subscribed["replayed"] > 0, "the bootstrap entries are replayed on subscribe"),
        ([c["tool"] for c in tool_calls] == ["read", "write"], f"tool calls {tool_calls}"),
        ([r["outcome"] for r in tool_results] == ["ok", "ok"], f"tool results {tool_results}"),
        (bool(tool_results) and "hello from disk" in tool_results[0]["text"], f"read result {tool_results[:1]}"),
        ([(a["tool"], a["class"]) for a in approvals] == [("write", "workspace_write")], f"approvals {approvals}"),
        (finals[-1:] == ["Read a.txt and wrote b.txt."], f"final text {finals}"),
        ((project / "b.txt").read_text() == "written by the python sdk\n", "b.txt content"),
        (fold["turns"] == 1, f"fold turns {fold['turns']}"),
        (code == 0, f"server exit code {code}"),
    ]
    failed = [why for ok, why in checks if not ok]
    if failed:
        raise SystemExit("e2e: " + "; ".join(failed))
    print(
        json.dumps(
            {
                "sdk": "python",
                "version": client.hello["version"],
                "session": client.hello["session"],
                "replayed": subscribed["replayed"],
                "tool_calls": tool_calls,
                "tool_results": tool_results,
                "approvals": [{"id": a["id"], "tool": a["tool"], "reason": a["reason"]} for a in approvals],
                "final_text": finals[-1],
                "streamed_text": streamed,
                "transition": result["transition"]["transition"],
                "exit": code,
            }
        )
    )


if __name__ == "__main__":
    try:
        asyncio.run(asyncio.wait_for(main(), timeout=60))
    except TimeoutError:
        print("e2e: no idle within 60 s", file=sys.stderr)
        sys.exit(1)
