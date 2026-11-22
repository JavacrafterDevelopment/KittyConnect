#!/usr/bin/env python3
"""Packet capture for the end-to-end encryption check (run by run.sh).

  capture.py capture <out.pickle> <stop-file>
      Records every IPv4/IPv6 TCP and UDP packet on every interface of this
      machine (a raw AF_PACKET socket, so it needs root) until <stop-file>
      appears. Meanwhile it notes, from /proc, which program owns each socket
      and what address it's bound to.

  capture.py check <capture.pickle> <e2e-report.json> <relay-derp-port> <label>
      Sorts the packets into
        - tunnel: anything to or from a TailCat program's network socket
          (the WireGuard UDP socket each tailcat/kc-voip binds on all
          addresses) or the relay. This is what leaves a real PC.
          Everything in it must be WireGuard, disco (TailCat's path
          finding, NaCl-encrypted), STUN, or TailCat's "meow" introduction
          (public keys only).
        - local: everything else, all on this machine between KittyConnect
          and its own TailCat programs (127.0.0.1 / ::1). This never leaves
          a real PC, so plaintext is expected here.
      Then checks that no marker or code the test sent appears anywhere in
      tunnel traffic, that every tunnel UDP packet is WireGuard, disco
      (TailCat's NaCl-encrypted path finding) or STUN, that everything the
      relay carried is WireGuard or disco, and how random the WireGuard
      payload bytes are. Exits 1 if anything fails.
"""
import json
import math
import os
import pickle
import socket
import struct
import sys
import threading
import time
from collections import Counter, defaultdict

TUNNEL_PROGRAMS = {"tailcat", "kc-voip"}
DISCO_MAGIC = "TS\U0001f4ac".encode()  # 'TS' + speech balloon, 6 bytes
DERP_MAGIC = "DERP\U0001f511".encode()


# ------------------------------------------------------------------ capture

def proc_sockets():
    """{inode: (proto, local_ip_hex, port)} from /proc/net."""
    out = {}
    for proto, files in (("tcp", ("tcp", "tcp6")), ("udp", ("udp", "udp6"))):
        for f in files:
            try:
                lines = open(f"/proc/net/{f}").read().splitlines()[1:]
            except OSError:
                continue
            for line in lines:
                cols = line.split()
                ip, port = cols[1].split(":")
                out[cols[9]] = (proto, ip, int(port, 16))
    return out


def owners_snapshot(owners):
    socks = proc_sockets()
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            comm = open(f"/proc/{pid}/comm").read().strip()
            fds = os.listdir(f"/proc/{pid}/fd")
        except OSError:
            continue
        for fd in fds:
            try:
                link = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                continue
            if link.startswith("socket:["):
                s = socks.get(link[8:-1])
                if s:
                    proto, ip, port = s
                    wildcard = set(ip) == {"0"}
                    owners[(proto, port)].add((comm, wildcard))


def capture(out_path, stop_path):
    packets = []
    owners = defaultdict(set)
    stop = threading.Event()

    def poll():
        while not stop.is_set():
            owners_snapshot(owners)
            time.sleep(0.1)

    threading.Thread(target=poll, daemon=True).start()
    s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.ntohs(0x0003))
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 64 * 1024 * 1024)
    s.settimeout(0.2)
    seen_out = set()
    while not os.path.exists(stop_path):
        try:
            frame, sa = s.recvfrom(262144)
        except socket.timeout:
            continue
        # Loopback shows each packet twice (outgoing and incoming); keep one.
        if sa[2] == socket.PACKET_OUTGOING and sa[0] == "lo":
            continue
        eth = struct.unpack("!H", frame[12:14])[0]
        if eth == 0x0800:
            ip = frame[14:]
            ihl = (ip[0] & 0xF) * 4
            proto, body = ip[9], ip[ihl:]
            src, dst = socket.inet_ntop(socket.AF_INET, ip[12:16]), socket.inet_ntop(socket.AF_INET, ip[16:20])
        elif eth == 0x86DD:
            ip = frame[14:]
            proto, body = ip[6], ip[40:]
            src, dst = socket.inet_ntop(socket.AF_INET6, ip[8:24]), socket.inet_ntop(socket.AF_INET6, ip[24:40])
        else:
            continue
        if proto == 6 and len(body) >= 20:
            sp, dp, seq = struct.unpack("!HHI", body[:8])
            off = (body[12] >> 4) * 4
            packets.append(("tcp", src, sp, dst, dp, seq, bytes(body[off:])))
        elif proto == 17 and len(body) >= 8:
            sp, dp = struct.unpack("!HH", body[:4])
            packets.append(("udp", src, sp, dst, dp, 0, bytes(body[8:])))
    stop.set()
    owners_snapshot(owners)
    with open(out_path, "wb") as f:
        pickle.dump({"packets": packets, "owners": dict(owners)}, f)
    print(f"captured {len(packets)} packets")


# -------------------------------------------------------------------- check

def entropy(data):
    if not data:
        return 0.0
    c = Counter(data)
    n = len(data)
    return -sum(v / n * math.log2(v / n) for v in c.values())


def udp_kind(p):
    # TailCat's introduction through the relay (tailcat/disco.go): "meow",
    # 0x01, the joiner's node public key and disco public key (69 bytes), and
    # the host's 5-byte "meow" 0x02 reply. Public keys and nothing else.
    if p[:4] == b"meow":
        if p[4:5] == b"\x01" and len(p) == 69:
            return "tailcat-meow (2 public keys)"
        if p[4:5] == b"\x02" and len(p) == 5:
            return "tailcat-meowed (ack)"
        return "OTHER"
    if len(p) >= 4 and p[0] in (1, 2, 3, 4) and p[1:4] == b"\0\0\0":
        return {1: "wireguard-handshake-init", 2: "wireguard-handshake-reply", 3: "wireguard-cookie", 4: "wireguard-data"}[p[0]]
    if p.startswith(DISCO_MAGIC):
        return "disco"
    if len(p) >= 8 and p[4:8] == b"\x21\x12\xa4\x42":
        return "stun"
    return "OTHER"


def reassemble(segments):
    """TCP payloads of one direction, in sequence order, overlaps dropped."""
    segments = sorted((s for s in segments if s[1]), key=lambda s: s[0])
    if not segments:
        return b""
    out = bytearray()
    start = segments[0][0]
    for seq, data in segments:
        at = (seq - start) & 0xFFFFFFFF
        if at + len(data) <= len(out):
            continue
        if at > len(out):
            out += b"\0" * (at - len(out))  # a gap: shouldn't happen on one machine
        out += data[len(out) - at:]
    return bytes(out)


def derp_frames(stream):
    """The relay's frames after the HTTP upgrade: (type, payload)."""
    i = stream.find(b"\r\n\r\n")
    # Only the relay connection itself (not the relay map's download).
    if i < 0 or not (stream.startswith(b"GET /derp") or stream.startswith(b"HTTP/1.1 101")):
        return []
    i += 4
    frames = []
    while i + 5 <= len(stream):
        t, n = stream[i], struct.unpack("!I", stream[i + 1:i + 5])[0]
        if i + 5 + n > len(stream):
            break
        frames.append((t, stream[i + 5:i + 5 + n]))
        i += 5 + n
    return frames


def check(cap_path, report_path, derp_port, label):
    cap = pickle.load(open(cap_path, "rb"))
    report = json.load(open(report_path))
    packets, owners = cap["packets"], cap["owners"]
    derp_port = int(derp_port)

    def tunnel_socket(proto, port):
        """A TailCat program's network socket: bound to every address."""
        return proto == "udp" and any(c in TUNNEL_PROGRAMS and wild for c, wild in owners.get((proto, port), ()))

    def relay_socket(proto, port):
        return any(c == "localrelay" for c, _ in owners.get((proto, port), ())) or (proto == "tcp" and port == derp_port)

    secrets = dict(report["markers"])
    for k, v in report["codes"].items():
        secrets[f"code:{k}"] = v
    needles = {k: v.encode() for k, v in secrets.items()}
    needles["any KCMARK"] = b"KCMARK"

    found_tunnel = Counter()
    found_local = Counter()
    found_local_udp = Counter()
    kinds = Counter()
    tunnel_bytes = 0
    wg_payload = bytearray()
    derp_streams = defaultdict(list)
    for proto, src, sp, dst, dp, seq, pay in packets:
        is_relay = relay_socket(proto, sp) or relay_socket(proto, dp)
        is_tunnel = is_relay or tunnel_socket(proto, sp) or tunnel_socket(proto, dp)
        hits = [k for k, n in needles.items() if n in pay]
        if is_tunnel:
            tunnel_bytes += len(pay)
            for k in hits:
                found_tunnel[k] += 1
            if proto == "udp" and pay:
                kind = udp_kind(pay)
                kinds[kind] += 1
                if kind == "wireguard-data":
                    wg_payload += pay[16:]
            if proto == "tcp" and (sp == derp_port or dp == derp_port):
                derp_streams[(src, sp, dst, dp)].append((seq, pay))
        else:
            for k in hits:
                found_local[k] += 1
                if proto == "udp":
                    found_local_udp[k] += 1

    # What the relay carried, frame by frame.
    relay = Counter()
    odd = []
    for segs in derp_streams.values():
        for t, body in derp_frames(reassemble(segs)):
            if t in (0x04, 0x05):  # send / receive: 32B peer key + packet
                kind = udp_kind(body[32:])
                relay[("send:" if t == 0x04 else "recv:") + kind.replace("OTHER", "OTHER!")] += 1
                if kind == "wireguard-data" and t == 0x04:
                    wg_payload += body[32 + 16:]
                if kind == "OTHER" and len(odd) < 6:
                    odd.append(body[32:])
            elif t == 0x01:
                relay["server-key" + ("" if body.startswith(DERP_MAGIC) else " (bad magic!)")] += 1
            elif t in (0x02, 0x03):
                relay["client/server info (NaCl box)"] += 1
            else:
                relay[f"control 0x{t:02x}"] += 1

    print(f"\n=== {label} ===")
    print(f"packets captured: {len(packets)}; tunnel and relay traffic: {tunnel_bytes:,} bytes")
    print("tunnel UDP packets by kind: " + ", ".join(f"{k} {v}" for k, v in sorted(kinds.items())))
    if relay:
        print("relay frames: " + ", ".join(f"{k} {v}" for k, v in sorted(relay.items())))
    for o in odd:
        print(f"  unrecognised relayed packet: {len(o)} bytes, starts {o[:24].hex()}")
    print(f"WireGuard payload: {len(wg_payload):,} bytes, entropy {entropy(bytes(wg_payload)):.4f} bits/byte (8 = random)")
    print("\nmarker / code                  seen on this PC (plaintext, expected)   seen in tunnel/relay traffic")
    fail = []
    for k in sorted(needles):
        local, udp, tun = found_local[k], found_local_udp[k], found_tunnel[k]
        note = f"{local} packets" + (f" ({udp} UDP)" if udp else "")
        print(f"  {k:30} {note:40} {tun}")
        if tun:
            fail.append(f"{k} appeared in tunnel/relay traffic")
        # Codes never need to travel at all (only the room's, in a hub invite).
        if k != "any KCMARK" and not k.startswith("code:") and not local:
            fail.append(f"{k} never seen at all (the capture missed it, or it was never sent)")
    for k in ("VoiceFriend", "VoiceHost", "Video", "NativeVideo", "NativeShareAudio"):
        if found_local_udp[k] < 2:
            fail.append(f"{k} wasn't seen as UDP on both sides of a tunnel")
    if kinds.get("OTHER"):
        fail.append(f"{kinds['OTHER']} tunnel UDP packets weren't WireGuard, disco or STUN")
    if any(k.endswith("OTHER!") for k in relay):
        fail.append("the relay carried something that isn't WireGuard or disco")
    if tunnel_bytes == 0:
        fail.append("no tunnel traffic captured")
    if len(wg_payload) > 10000 and entropy(bytes(wg_payload)) < 7.9:
        fail.append("WireGuard payload doesn't look random")
    print("\nresult: " + ("PASS" if not fail else "FAIL\n  " + "\n  ".join(fail)))
    return not fail


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "capture":
        capture(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 6 and sys.argv[1] == "check":
        sys.exit(0 if check(*sys.argv[2:6]) else 1)
    else:
        print(__doc__)
        sys.exit(2)
