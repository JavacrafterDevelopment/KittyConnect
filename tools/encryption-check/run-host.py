#!/usr/bin/env python3
"""The end-to-end encryption check on Windows and macOS (run by
.github/workflows/encryption.yml; Linux has run.sh).

  run-host.py <mode> --app <KittyConnect built with --features e2e>
              --out <dir> [--relay <localrelay>] [--kc-voip <test kc-voip>]

Two Portsides (the app's own code, src/e2e.rs, started as `--kc-e2e`) connect
to each other through TailCat while every packet on this computer is
recorded, on loopback and on the network card:

  - macOS: tcpdump on lo0 and on the default route's interface.
  - Windows: raw sockets with SIO_RCVALL (the same as RawCap), one on
    127.0.0.1 and one on each of this computer's IPv4 addresses. Packet
    Monitor and the like miss loopback, where the plaintext side is.

Which program owns each port is polled meanwhile (lsof / netstat -ano), so
the check (capture.py's, plus the rules below) can tell TailCat's WireGuard
sockets from the rest.

Modes:
  local-direct, local-relay   a relay on this computer (Tailscale's DERP
      server code over plain HTTP, so the capture sees what a relay sees) and
      kc-voip pointed at it; relay forces everything through it.
  tailscale-direct, tailscale-relay   the shipped programs as they are,
      embedded in the app, on Tailscale's real relays over TLS; relay forces
      everything through them, so it all crosses the network card.

Fails if any marker or code the test sent shows up outside traffic between
127.0.0.1 and 127.0.0.1 (or ::1), if TailCat's sockets carry anything but
WireGuard, disco or STUN, or if Portside itself sends anything off this
computer. Nothing but Python's standard library and the OS's own tools.
"""
import argparse
import importlib.util
import json
import os
import platform
import re
import socket
import struct
import subprocess
import sys
import threading
import time
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("kc_capture", os.path.join(HERE, "capture.py"))
cap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cap)

MAC = platform.system() == "Darwin"
WIN = platform.system() == "Windows"


def norm(comm):
    """Owner names as capture.py knows them (the embedded programs are
    unpacked as tailcat-<version>[.exe] and kc-voip-<version>-tc<version>)."""
    c = comm.lower()
    for k in ("tailcat", "kc-voip", "localrelay"):
        if c.startswith(k):
            return k
    if c.startswith("kittyconn"):
        return "KittyConnect"
    return comm


def loopback(a):
    return a.startswith("127.") or a == "::1"


# ------------------------------------------------------------------ owners

def owners_mac():
    out = subprocess.run(["sudo", "lsof", "-nP", "-iTCP", "-iUDP", "-F", "pcPn"], capture_output=True, text=True).stdout
    res = []
    comm = proto = ""
    for line in out.splitlines():
        if not line:
            continue
        t, v = line[0], line[1:]
        if t == "c":
            comm = v
        elif t == "P":
            proto = v.lower()
        elif t == "n":
            local = v.split("->")[0]
            host, _, port = local.rpartition(":")
            if port.isdigit():
                res.append((proto, int(port), norm(comm), host in ("*", "[::]", "0.0.0.0")))
    return res


def owners_win():
    names = {}
    for line in subprocess.run(["tasklist", "/fo", "csv", "/nh"], capture_output=True, text=True).stdout.splitlines():
        cols = [c.strip('"') for c in line.split('","')]
        if len(cols) >= 2 and cols[1].isdigit():
            names[cols[1]] = cols[0]
    res = []
    for proto in ("TCP", "UDP", "TCPv6", "UDPv6"):
        out = subprocess.run(["netstat", "-ano", "-p", proto], capture_output=True, text=True).stdout
        for line in out.splitlines():
            cols = line.split()
            if len(cols) < 4 or cols[0] not in ("TCP", "UDP"):
                continue
            host, _, port = cols[1].rpartition(":")
            if not port.isdigit():
                continue
            res.append((cols[0].lower(), int(port), norm(names.get(cols[-1], "?")), host in ("0.0.0.0", "[::]")))
    return res


class Owners:
    def __init__(self):
        self.map = defaultdict(set)
        self.stop = threading.Event()
        self.t = threading.Thread(target=self.run, daemon=True)
        self.t.start()

    def run(self):
        while not self.stop.is_set():
            try:
                for proto, port, comm, wild in (owners_mac() if MAC else owners_win()):
                    self.map[(proto, port)].add((comm, wild))
            except Exception as e:  # keep polling
                print("owners:", e, file=sys.stderr)
            time.sleep(0.2)

    def done(self):
        self.stop.set()
        self.t.join()
        return dict(self.map)


# ------------------------------------------------------------------ capture

def ip_packet(ip, label, out):
    """IPv4/IPv6 packet -> capture.py's tuple, plus where it was seen."""
    if not ip:
        return
    v = ip[0] >> 4
    if v == 4:
        ihl = (ip[0] & 0xF) * 4
        proto, body = ip[9], ip[ihl:struct.unpack("!H", ip[2:4])[0] or len(ip)]
        src, dst = socket.inet_ntop(socket.AF_INET, ip[12:16]), socket.inet_ntop(socket.AF_INET, ip[16:20])
    elif v == 6:
        proto, body = ip[6], ip[40:]
        src, dst = socket.inet_ntop(socket.AF_INET6, ip[8:24]), socket.inet_ntop(socket.AF_INET6, ip[24:40])
    else:
        return
    if proto == 6 and len(body) >= 20:
        sp, dp, seq = struct.unpack("!HHI", body[:8])
        off = (body[12] >> 4) * 4
        out.append((label, ("tcp", src, sp, dst, dp, seq, bytes(body[off:]))))
    elif proto == 17 and len(body) >= 8:
        sp, dp = struct.unpack("!HH", body[:4])
        out.append((label, ("udp", src, sp, dst, dp, 0, bytes(body[8:]))))
    else:
        out.append((label, (f"ip{proto}", src, 0, dst, 0, 0, bytes(body))))


def read_pcap(path, label, out):
    data = open(path, "rb").read()
    if len(data) < 24:
        return
    e = "<" if struct.unpack("<I", data[:4])[0] in (0xA1B2C3D4, 0xA1B23C4D) else ">"
    link = struct.unpack(e + "I", data[20:24])[0]
    i = 24
    while i + 16 <= len(data):
        incl = struct.unpack(e + "I", data[i + 8:i + 12])[0]
        f = data[i + 16:i + 16 + incl]
        i += 16 + incl
        if link in (0, 108):  # loopback: 4-byte address family
            ip_packet(f[4:], label, out)
        elif link == 1 and len(f) > 14 and f[12:14] in (b"\x08\x00", b"\x86\xdd"):
            ip_packet(f[14:], label, out)
        elif link in (12, 101):
            ip_packet(f, label, out)


class MacCapture:
    def __init__(self, outdir):
        r = subprocess.run(["route", "-n", "get", "default"], capture_output=True, text=True).stdout
        m = re.search(r"interface:\s*(\S+)", r)
        self.ifaces = ["lo0"] + ([m.group(1)] if m else [])
        self.files = {i: os.path.join(outdir, f"{i}.pcap") for i in self.ifaces}
        self.procs = [subprocess.Popen(["sudo", "tcpdump", "-i", i, "-s", "0", "-U", "-n", "-w", f], stderr=subprocess.DEVNULL) for i, f in self.files.items()]
        time.sleep(2)

    def done(self):
        time.sleep(1)
        subprocess.run(["sudo", "pkill", "-INT", "-x", "tcpdump"])
        for p in self.procs:
            p.wait(timeout=20)
        out = []
        for i, f in self.files.items():
            read_pcap(f, "loopback" if i == "lo0" else f"network card ({i})", out)
        return out


class WinCapture:
    def __init__(self, outdir):
        addrs = {"127.0.0.1"}
        for line in subprocess.run(["ipconfig"], capture_output=True, text=True).stdout.splitlines():
            m = re.search(r"IPv4[^:]*:\s*([\d.]+)", line)
            if m:
                addrs.add(m.group(1))
        self.outdir = outdir
        self.stop = threading.Event()
        self.raw = defaultdict(list)
        self.threads = []
        for a in sorted(addrs):
            s = socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_IP)
            s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 64 * 1024 * 1024)
            s.bind((a, 0))
            s.setsockopt(socket.IPPROTO_IP, socket.IP_HDRINCL, 1)
            s.ioctl(socket.SIO_RCVALL, socket.RCVALL_ON)
            s.settimeout(0.2)
            t = threading.Thread(target=self.run, args=(s, a), daemon=True)
            t.start()
            self.threads.append(t)
        print("capturing on " + ", ".join(sorted(addrs)))
        time.sleep(1)

    def run(self, s, a):
        while not self.stop.is_set():
            try:
                self.raw[a].append((time.time(), s.recv(65535)))
            except socket.timeout:
                continue
            except OSError:
                continue

    def done(self):
        time.sleep(1)
        self.stop.set()
        for t in self.threads:
            t.join()
        out = []
        for a, pkts in self.raw.items():
            # Kept as a pcap (raw IP) for Wireshark.
            with open(os.path.join(self.outdir, f"{a}.pcap"), "wb") as f:
                f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 262144, 101))
                for ts, p in pkts:
                    f.write(struct.pack("<IIII", int(ts), int((ts % 1) * 1e6), len(p), len(p)) + p)
            for _, p in pkts:
                ip_packet(p, "loopback" if a == "127.0.0.1" else f"network card ({a})", out)
        return out


# -------------------------------------------------------------------- check

def check(seen, owners, report, derp_port, label, outdir):
    # 1. capture.py's own check, on everything: markers never in tunnel or
    #    relay traffic, tunnel UDP only WireGuard/disco/STUN, WireGuard
    #    random, and the markers *are* seen on loopback (the positive control).
    pk = os.path.join(outdir, "capture.pickle")
    import pickle
    with open(pk, "wb") as f:
        pickle.dump({"packets": [p for _, p in seen if p[0] in ("tcp", "udp")], "owners": owners}, f)
    rp = os.path.join(outdir, "report.json")
    ok = cap.check(pk, rp, str(derp_port), label)

    # 2. Stricter, as on two real PCs: a marker or code may only ever travel
    #    between loopback addresses (Portside and its own TailCat); and
    #    nothing Portside itself owns may talk to another address.
    needles = {k: v.encode() for k, v in report["markers"].items()}
    for k, v in report["codes"].items():
        needles[f"code:{k}"] = v.encode()
    needles["any KCMARK"] = b"KCMARK"

    def owner(proto, port):
        return {c for c, _ in owners.get((proto, port), ())}

    fail = []
    offbox = Counter()
    where = Counter()
    for lab, (proto, src, sp, dst, dp, seq, pay) in seen:
        where[lab] += 1
        both_lo = loopback(src) and loopback(dst)
        if not both_lo:
            for k, n in needles.items():
                if n in pay:
                    fail.append(f"{k} between {src}:{sp} and {dst}:{dp} ({lab})")
            if proto in ("tcp", "udp"):
                mine = owner(proto, sp) | owner(proto, dp)
                if "KittyConnect" in mine:
                    fail.append(f"Portside itself sent {proto} {src}:{sp} -> {dst}:{dp}")
                tun = mine & {"tailcat", "kc-voip"}
                if tun:
                    kind = cap.udp_kind(pay) if proto == "udp" and pay else ("tls" if pay[:1] in (b"\x16", b"\x17", b"\x14", b"\x15") else ("http" if pay[:4] in (b"GET ", b"HTTP", b"POST") else "tcp"))
                    remote = dp if src == dst or owner(proto, sp) & tun else sp
                    offbox[f"{'/'.join(sorted(tun))} {proto} {kind}" + (f" port {remote}" if proto == "tcp" else "")] += 1
                    if proto == "udp" and pay and kind == "OTHER":
                        fail.append(f"TailCat sent non-WireGuard UDP {src}:{sp} -> {dst}:{dp}")
                elif mine & {"localrelay"}:
                    offbox[f"local relay {proto}"] += 1
                else:
                    offbox[f"other programs ({', '.join(sorted(mine)) or 'owner unknown'}) {proto}"] += 1
            else:
                offbox[f"{proto} (kernel)"] += 1
    print("\npackets seen: " + ", ".join(f"{k} {v}" for k, v in sorted(where.items())))
    print("not between loopback addresses: " + (", ".join(f"{k} {v}" for k, v in sorted(offbox.items())) or "nothing"))
    print("strict check (markers only between loopback addresses; Portside never off this PC): " + ("PASS" if not fail else "FAIL\n  " + "\n  ".join(fail[:30])))
    return ok and not fail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["local-direct", "local-relay", "tailscale-direct", "tailscale-relay"])
    ap.add_argument("--app", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--relay")
    ap.add_argument("--kc-voip")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    env = dict(os.environ)
    env["KC_E2E_OUT"] = os.path.join(a.out, "report.json")
    relay = None
    derp_port = 0
    if a.mode.startswith("local"):
        relay = subprocess.Popen([a.relay], stdout=subprocess.PIPE, text=True)
        info = {}
        while len(info) < 3:
            k, _, v = relay.stdout.readline().strip().partition("=")
            info[k] = v
        derp_port = int(info["DERPPORT"])
        env.update(TAILCAT_DERPMAP_URL=info["DERPMAP"], KC_TEST_DERP_REGION=info["REGION"], TS_DEBUG_USE_DERP_HTTP="1", KC_TEST_KC_VOIP=os.path.abspath(a.kc_voip))
    if a.mode.endswith("relay"):
        env["TS_DEBUG_ALWAYS_USE_DERP"] = "1"

    owners = Owners()
    capture = MacCapture(a.out) if MAC else WinCapture(a.out)
    print(f"--- {a.mode}: running the app's end-to-end test", flush=True)
    t0 = time.time()
    with open(os.path.join(a.out, "app.log"), "w") as log:
        rc = subprocess.run([a.app, "--kc-e2e"], env=env, stdout=log, stderr=subprocess.STDOUT, timeout=1500).returncode
    print(f"app exited {rc} after {time.time() - t0:.0f} s")
    seen = capture.done()
    own = owners.done()
    if relay:
        relay.kill()
    if not os.path.exists(env["KC_E2E_OUT"]):
        print(open(os.path.join(a.out, "app.log"), errors="replace").read()[-6000:])
        print("The test itself failed, before it could write its report.")
        sys.exit(1)
    report = json.load(open(env["KC_E2E_OUT"]))
    print(json.dumps(report["results"], indent=2))
    if report.get("error"):
        # A step failed (a feature didn't work, not a leak): the capture of
        # everything up to it is still checked below.
        print(f"\nA TEST STEP FAILED: {report['error']}")
    ok = check(seen, own, report, derp_port, f"{platform.system()} {a.mode}", a.out)
    print(f"\n{platform.system()} {a.mode}: encryption " + ("PASS" if ok else "FAILED") + ("; but a test step failed (above)" if report.get("error") else "; every step worked"))
    ok = ok and not report.get("error") and rc == 0
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
