#!/usr/bin/env python3
# Regenerates mail-report-v1.msgpack from the repository root:
#   python3 testdata/generate_mail_report_v1.py
# Hand-rolled MessagePack encoder, independent of rmp-serde, for the v1 mail report golden
# (RFC 0017 §2): a map per struct, keys in declaration order, as `rmp_serde::to_vec_named`
# writes them.
import struct
def s(x):
    b = x.encode()
    assert len(b) < 256
    return (bytes([0xa0 | len(b)]) if len(b) < 32 else bytes([0xd9, len(b)])) + b
def u(n):
    assert n >= 0
    if n < 128: return bytes([n])
    if n < 256: return b'\xcc' + bytes([n])
    if n < 65536: return b'\xcd' + struct.pack('>H', n)
    if n < 2**32: return b'\xce' + struct.pack('>I', n)
    return b'\xcf' + struct.pack('>Q', n)
def f32(x): return b'\xca' + struct.pack('>f', x)
def f64(x): return b'\xcb' + struct.pack('>d', x)
def arr(*xs): assert len(xs) < 16; return bytes([0x90 | len(xs)]) + b''.join(xs)
def mp(*pairs): assert len(pairs) < 16; return bytes([0x80 | len(pairs)]) + b''.join(s(k) + v for k, v in pairs)
NIL = b'\xc0'
RUN = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"
def snapshot(collected_at, info):
    return mp(
        ("collected_at", u(collected_at)),
        ("info", info),
        ("cpu_percent", f32(12.5)),
        ("memory_percent", f32(40.0)),
        ("memory_used_bytes", u(4096)),
        ("memory_total_bytes", u(8192)),
        ("memory_total_display", s("8 KB")),
        ("swap_percent", f32(0.0)),
        ("load_one", f64(0.5)),
        ("load_five", f64(0.25)),
        ("load_fifteen", f64(0.125)),
        ("uptime_seconds", u(3600)),
        ("uptime_display", s("1h 0m")),
        ("disks", arr(mp(("mount_point", s("/")), ("usage_percent", f32(50.0))))),
    )
info = mp(
    ("hostname", s("web-01")),
    ("os_name", s("Debian 12")),
    ("kernel", s("6.1.0")),
    ("cpu_model", s("Xeon")),
    ("cpu_cores", u(4)),
)
report = mp(
    ("kind", s("mail-report.v1")),
    ("run", s(RUN)),
    ("seq", u(7)),
    ("created_at", u(1700000300)),
    ("interval_secs", u(300)),
    ("reason", s("scheduled")),
    ("snapshots", arr(snapshot(1700000240, NIL), snapshot(1700000300, info))),
    ("alerts", arr(mp(
        ("id", s(RUN + "-1")),
        ("metric", s("cpu")),
        ("severity", s("warning")),
        ("current_value", f32(95.0)),
        ("message", s("CPU high")),
        ("fired_at", s("2023-11-14T22:18:20Z")),
    ))),
    ("round", NIL),
)
open("testdata/mail-report-v1.msgpack", "wb").write(report)
print(len(report))
