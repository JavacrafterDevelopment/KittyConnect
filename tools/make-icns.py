#!/usr/bin/env python3
"""Draws KittyConnect's icon into assets/KittyConnect.icns for the macOS app.

The same walnut cat head with gilt eyes as src/icon.rs (which draws the
window icon at startup); keep the two in step. Run it again after changing
the drawing:

    python3 tools/make-icns.py
"""
import os
import struct
import zlib

WALNUT = (0x6B, 0x4F, 0x35)
DEEP = (0x4A, 0x35, 0x24)
GILT = (0xD9, 0xAE, 0x58)


def ear(x, y, ax, ay, bx, by, cx, cy):
    def s(px, py, qx, qy):
        return (x - qx) * (py - qy) - (px - qx) * (y - qy)
    d1, d2, d3 = s(ax, ay, bx, by), s(bx, by, cx, cy), s(cx, cy, ax, ay)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def sample(x, y):
    for cx in (0.36, 0.64):
        dx, dy = (x - cx) / 0.075, (y - 0.56) / 0.095
        if dx * dx + dy * dy <= 1:
            px, py = (x - cx) / 0.025, (y - 0.56) / 0.07
            return DEEP if px * px + py * py <= 1 else GILT
    dx, dy = (x - 0.5) / 0.42, (y - 0.6) / 0.34
    if dx * dx + dy * dy <= 1:
        return WALNUT
    if ear(x, y, 0.12, 0.48, 0.16, 0.06, 0.46, 0.32) or ear(x, y, 0.88, 0.48, 0.84, 0.06, 0.54, 0.32):
        return DEEP
    return None


def rgba(size, ss):
    rows = []
    for py in range(size):
        row = bytearray([0])  # PNG filter: none
        for px in range(size):
            r = g = b = hits = 0
            for sy in range(ss):
                for sx in range(ss):
                    c = sample((px + (sx + 0.5) / ss) / size, (py + (sy + 0.5) / ss) / size)
                    if c:
                        r += c[0]; g += c[1]; b += c[2]; hits += 1
            row += bytes([r // hits, g // hits, b // hits, hits * 255 // (ss * ss)]) if hits else b"\0\0\0\0"
        rows.append(bytes(row))
    return b"".join(rows)


def png(size, ss):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(rgba(size, ss), 9)) + chunk(b"IEND", b"")


# icns entry types holding PNGs, by pixel size (the @2x ones reuse a size).
ENTRIES = [(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512),
           (b"ic10", 1024), (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512)]

pngs = {}
body = b""
for kind, size in ENTRIES:
    if size not in pngs:
        pngs[size] = png(size, 4 if size <= 256 else 2)
    body += kind + struct.pack(">I", 8 + len(pngs[size])) + pngs[size]
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "KittyConnect.icns")
with open(out, "wb") as f:
    f.write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
print(f"Wrote assets/KittyConnect.icns ({8 + len(body)} bytes)")
