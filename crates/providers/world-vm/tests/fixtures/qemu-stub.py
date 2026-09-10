#!/usr/bin/env python3
"""A stand-in for qemu-system-* (07: tests run against stub QEMU): it honours exactly the argv
world-vm builds. QMP on the -qmp socket (greeting, capabilities, query-uuid, query-status,
system_powerdown, quit), the real vm-guest-agent on the -chardev agent socket (env
AHO_STUB_AGENT; its workspace is $AHO_STUB_ROOT/<-name>), a connection to the -netdev stream
socket, and a serial console on stdio that prints a boot line and echoes every input line."""
import json, os, socket, subprocess, sys, threading, time

args = sys.argv[1:]
def opt(flag):
    return [args[i + 1] for i, a in enumerate(args) if a == flag]
def kv(value):
    return dict(p.split("=", 1) for p in value.split(",") if "=" in p)

uuid = opt("-uuid")[0]
qmp_path = opt("-qmp")[0].split(",")[0].removeprefix("unix:")
agent_path = next(kv(c)["path"] for c in opt("-chardev") if c.startswith("socket,id=agent"))
net_path = kv(opt("-netdev")[0])["addr.path"]
unknown = [a for a in args if a.startswith("-") and a not in (
    "-nodefaults", "-no-user-config", "-name", "-uuid", "-machine", "-accel", "-cpu", "-m", "-smp", "-sandbox",
    "-drive", "-bios", "-kernel", "-initrd", "-append", "-netdev", "-device", "-chardev", "-qmp", "-display", "-vnc", "-serial")]
if unknown:
    sys.exit(f"qemu-stub: unexpected options {unknown}")

workspace = os.path.join(os.environ["AHO_STUB_ROOT"], opt("-name")[0])
os.makedirs(workspace, exist_ok=True)
agent = subprocess.Popen([os.environ["AHO_STUB_AGENT"], "--workspace", workspace, "--socket", agent_path])

def stop(code=0):
    agent.kill()
    for p in (qmp_path, agent_path):
        try: os.unlink(p)
        except OSError: pass
    os._exit(code)

def qmp_client(conn):
    f = conn.makefile("rwb")
    f.write(json.dumps({"QMP": {"version": {"qemu": {"major": 9, "minor": 0, "micro": 0}}, "capabilities": []}}).encode() + b"\r\n"); f.flush()
    for line in f:
        msg = json.loads(line)
        cmd, mid = msg.get("execute"), msg.get("id")
        ret = {}
        if cmd == "query-uuid": ret = {"UUID": uuid}
        elif cmd == "query-status": ret = {"status": "running", "running": True}
        f.write(json.dumps({"return": ret, "id": mid}).encode() + b"\r\n"); f.flush()
        if cmd in ("system_powerdown", "quit"):
            stop(0)

def qmp_server():
    s = socket.socket(socket.AF_UNIX); s.bind(qmp_path); s.listen(8)
    while True:
        c, _ = s.accept()
        threading.Thread(target=qmp_client, args=(c,), daemon=True).start()

def nic():
    # QEMU's reconnecting stream netdev: connect and hold the NIC like a booted guest would.
    while True:
        try:
            n = socket.socket(socket.AF_UNIX); n.connect(net_path)
            while n.recv(65536): pass
        except OSError:
            time.sleep(0.2)

threading.Thread(target=nic, daemon=True).start()
# Wait until the agent listens before announcing QMP: the manager treats QMP as "booted".
for _ in range(200):
    if os.path.exists(agent_path): break
    time.sleep(0.05)
threading.Thread(target=qmp_server, daemon=True).start()
sys.stdout.write("stub boot: aho guest ready\n"); sys.stdout.flush()
pending = b""
while True:
    chunk = os.read(0, 4096)  # raw: a buffered reader would wait for more than one line
    if not chunk:
        break
    pending += chunk
    while b"\n" in pending:
        line, pending = pending.split(b"\n", 1)
        os.write(1, b"console: " + line + b"\n")
stop(0)
