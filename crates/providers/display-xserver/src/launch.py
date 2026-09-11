# Executed by the bound world's python3, never by an ambient host process.
# stdout is exclusively one readiness JSON line followed by x11vnc's RFB stream.
import json
import os
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time


def main():
    if os.name != "posix":
        raise RuntimeError("display-xserver requires a POSIX world with Xvfb and x11vnc")
    for binary in ("Xvfb", "x11vnc"):
        if shutil.which(binary) is None:
            raise RuntimeError("display-xserver requires %s installed inside the bound world" % binary)
    width, height = map(int, sys.argv[1:3])
    os.umask(0o077)
    directory = os.path.abspath(".aho-desktop-%d" % os.getpid())
    os.mkdir(directory, 0o700)
    authority = os.path.join(directory, "Xauthority")
    # FamilyWild cookie works before Xvfb has chosen its free display number.
    field = lambda data: struct.pack("!H", len(data)) + data
    cookie = os.urandom(16)
    with open(authority, "xb") as out:
        out.write(struct.pack("!H", 65535) + field(b"") + field(b"")
                  + field(b"MIT-MAGIC-COOKIE-1") + field(cookie))
    ready_read, ready_write = os.pipe()
    xvfb = None
    vnc = None

    def stop(_signal=None, _frame=None):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        xvfb = subprocess.Popen(
            ["Xvfb", "-displayfd", str(ready_write), "-screen", "0", "%dx%dx24" % (width, height),
             "-nolisten", "tcp", "-auth", authority, "-noreset"],
            stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr,
            pass_fds=(ready_write,))
        os.close(ready_write)
        ready_write = None
        deadline = time.monotonic() + 8
        number = bytearray()
        while not number.endswith(b"\n"):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([ready_read], [], [], remaining)[0]:
                raise RuntimeError("Xvfb did not report a display within eight seconds; check its diagnostics")
            byte = os.read(ready_read, 1)
            if not byte or len(number) >= 16:
                raise RuntimeError("Xvfb exited before reporting a display; check its diagnostics")
            number.extend(byte)
        display = ":%d" % int(number)
        env = dict(os.environ, DISPLAY=display, XAUTHORITY=authority)
        os.write(1, (json.dumps({"display": display, "xauthority": authority}) + "\n").encode())
        # inetd never opens a TCP listener. Use a private Unix socketpair because x11vnc
        # expects a bidirectional socket on fd 0, not two separate anonymous pipes.
        local, remote = socket.socketpair()
        vnc = subprocess.Popen(
            ["x11vnc", "-inetd", "-display", display, "-auth", authority, "-nopw", "-quiet", "-noxdamage"],
            stdin=remote, stdout=remote, stderr=sys.stderr, env=env)
        remote.close()
        # Bounded full-duplex relay; backpressure remains in the OS, no output ring.
        os.set_blocking(0, False)
        os.set_blocking(1, False)
        local.setblocking(False)
        to_vnc = bytearray()
        to_parent = bytearray()
        parent_open = True
        server_open = True
        while server_open or to_parent:
            readers = []
            if parent_open and len(to_vnc) < 65536:
                readers.append(0)
            if server_open and len(to_parent) < 65536:
                readers.append(local)
            writers = ([local] if to_vnc and server_open else []) + ([1] if to_parent else [])
            readable, writable, _ = select.select(readers, writers, [], 1)
            if 0 in readable:
                data = os.read(0, 65536 - len(to_vnc))
                if data:
                    to_vnc.extend(data)
                else:
                    parent_open = False
            if local in writable:
                sent = local.send(to_vnc)
                del to_vnc[:sent]
            if not parent_open and not to_vnc and server_open:
                local.shutdown(socket.SHUT_WR)
                break
            if local in readable:
                data = local.recv(65536 - len(to_parent))
                if data:
                    to_parent.extend(data)
                else:
                    server_open = False
            if 1 in writable:
                sent = os.write(1, to_parent)
                del to_parent[:sent]
            if xvfb.poll() is not None:
                raise RuntimeError("Xvfb exited while the session desktop was active")
        local.close()
    finally:
        os.close(ready_read)
        if ready_write is not None:
            os.close(ready_write)
        for child in (vnc, xvfb):
            if child is not None and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        os.unlink(authority)
        os.rmdir(directory)


try:
    main()
except Exception as error:
    print("display-xserver: %s" % error, file=sys.stderr, flush=True)
    sys.exit(1)
