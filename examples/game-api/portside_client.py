#!/usr/bin/env python3
"""A small client for Portside's game API, using nothing but Python's own
library. Copy the Portside class into your own tools, or run this file:

    python portside_client.py info          # your hub, who's in it, your voice channel
    python portside_client.py host 7777     # hosts a UDP+TCP echo server as a session
    python portside_client.py join          # joins the first session and pings it over UDP
    python portside_client.py chat hello    # sends a message to this game on friends' PCs
    python portside_client.py listen        # prints every event

The game API is on by default in Portside (Settings > Game API). The first run asks
you to allow it in Portside; the key it gets is saved in
portside-token.json beside this file. See docs/game-api.md.
"""

import json
import os
import socket
import sys
import threading
import time

APP = {"id": "portside.example", "name": "Portside example"}
PORTS = range(47800, 47810)
TOKEN_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "portside-token.json")


class PortsideError(Exception):
    pass


class Portside:
    """One connection to Portside. Requests wait for their reply; events
    go to on_event (on the reader thread)."""

    def __init__(self, app=APP, token_file=TOKEN_FILE, on_event=None):
        self.app = app
        self.token_file = token_file
        self.on_event = on_event or (lambda name, data: None)
        self.sock = None
        self.next_id = 1
        self.waiting = {}
        self.lock = threading.Lock()

    def connect(self):
        for port in PORTS:
            try:
                s = socket.create_connection(("127.0.0.1", port), timeout=2)
            except OSError:
                continue
            f = s.makefile("rb")
            try:
                hi = json.loads(f.readline())
            except (OSError, ValueError):
                s.close()
                continue
            if hi.get("op") == "portside":
                self.sock, self.file = s, f
                break
            s.close()
        else:
            raise PortsideError("Portside isn't running, or its game API is off (Settings > Game API).")
        self.sock.settimeout(None)
        self._send({"op": "hello", "v": 1, "app": self.app, "token": self._load_token()})
        while True:
            msg = self._read()
            op = msg.get("op")
            if op == "asking":
                print("Portside is asking whether to allow this. Answer in the Portside window.")
            elif op == "authorized":
                self._save_token(msg["token"])
            elif op == "ready":
                break
            elif op == "denied":
                raise PortsideError(msg.get("error", "Not allowed."))
        threading.Thread(target=self._reader, daemon=True).start()
        return self

    def request(self, cmd, timeout=90, **args):
        with self.lock:
            rid = self.next_id
            self.next_id += 1
            done = threading.Event()
            self.waiting[rid] = [done, None]
        self._send({"id": rid, "cmd": cmd, **args})
        if not done.wait(timeout):
            raise PortsideError(f"{cmd}: no reply")
        reply = self.waiting.pop(rid)[1]
        if not reply.get("ok"):
            raise PortsideError(reply.get("error", "failed"))
        return reply.get("data")

    def _reader(self):
        while True:
            try:
                msg = self._read()
            except (OSError, ValueError):
                msg = None
            if msg is None:
                self.on_event("closed", None)
                return
            if "evt" in msg:
                self.on_event(msg["evt"], msg.get("data"))
            elif "id" in msg and msg["id"] in self.waiting:
                w = self.waiting[msg["id"]]
                w[1] = msg
                w[0].set()

    def _send(self, obj):
        self.sock.sendall((json.dumps(obj) + "\n").encode())

    def _read(self):
        line = self.file.readline()
        if not line:
            raise PortsideError("Portside closed the connection.")
        return json.loads(line)

    def _load_token(self):
        try:
            with open(self.token_file) as f:
                return json.load(f).get(self.app["id"], "")
        except (OSError, ValueError):
            return ""

    def _save_token(self, token):
        try:
            with open(self.token_file) as f:
                all_tokens = json.load(f)
        except (OSError, ValueError):
            all_tokens = {}
        all_tokens[self.app["id"]] = token
        with open(self.token_file, "w") as f:
            json.dump(all_tokens, f)


def echo_server(port):
    """A tiny game server: echoes UDP and TCP on 127.0.0.1:port."""
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.bind(("127.0.0.1", port))
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    tcp.bind(("127.0.0.1", port))
    tcp.listen()

    def udp_loop():
        while True:
            data, addr = udp.recvfrom(65535)
            print(f"  UDP from {addr[1]}: {data[:60]!r}")
            udp.sendto(b"echo: " + data, addr)

    def tcp_loop():
        while True:
            c, addr = tcp.accept()
            threading.Thread(target=lambda c=c: [c.sendall(b"echo: " + d) for d in iter(lambda: c.recv(65535), b"")], daemon=True).start()

    threading.Thread(target=udp_loop, daemon=True).start()
    threading.Thread(target=tcp_loop, daemon=True).start()


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "info"
    events = lambda name, data: print(f"event {name}: {json.dumps(data)}")
    p = Portside(on_event=events if what in ("listen", "host") else None).connect()

    if what == "info":
        print("me:   ", json.dumps(p.request("getMe")))
        print("hub:  ", json.dumps(p.request("getHub"), indent=2))
        print("voice:", json.dumps(p.request("getVoice"), indent=2))
        print("saved:", json.dumps(p.request("getSavedHubs")))
    elif what == "host":
        port = int(sys.argv[2]) if len(sys.argv) > 2 else 7777
        echo_server(port)
        s = p.request("hostSession", port=port, title="Echo test", info={"players": 1})
        p.request("setActivity", text="Hosting an echo test")
        print(f"Hosting session {s['sid']} on port {port}. Friends in your hub can join it now. Ctrl+C to stop.")
        while True:
            time.sleep(1)
    elif what == "join":
        sessions = p.request("getSessions")
        if not sessions:
            print("No sessions in your hub for this game yet. Run 'host' on another PC first.")
            return
        s = sessions[0]
        print(f"Joining {s['host']['name']}'s session \"{s['title']}\"...")
        port = p.request("joinSession", session=s["id"])["port"]
        print(f"Through Portside at 127.0.0.1:{port}")
        u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        u.settimeout(1)
        # The tunnel comes up with the first packets: keep sending until one comes back.
        for i in range(30):
            t = time.time()
            u.sendto(f"ping {i}".encode(), ("127.0.0.1", port))
            try:
                data, _ = u.recvfrom(65535)
                print(f"  {data.decode()} in {(time.time() - t) * 1000:.0f} ms")
                time.sleep(0.5)
            except socket.timeout:
                print("  (waiting for the tunnel)")
        with socket.create_connection(("127.0.0.1", port), timeout=10) as c:
            c.sendall(b"hello over TCP")
            print(" ", c.recv(65535).decode())
    elif what == "chat":
        p.request("sendMessage", data={"text": " ".join(sys.argv[2:]) or "hi"})
        print("Sent.")
    elif what == "listen":
        print("Listening for events. Ctrl+C to stop.")
        while True:
            time.sleep(1)
    else:
        print(__doc__)


if __name__ == "__main__":
    try:
        main()
    except PortsideError as e:
        print(e)
        sys.exit(1)
    except KeyboardInterrupt:
        pass
