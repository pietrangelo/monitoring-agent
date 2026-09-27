#!/usr/bin/env python3
# Regenerates application-frame-v1.msgpack from the repository root:
#   python3 testdata/generate_application_frame_v1.py
# Hand-rolled MessagePack encoder, independent of rmp-serde, for the v1 application frame golden.
import struct
def s(x):
    b = x.encode()
    assert len(b) < 256
    return (bytes([0xa0 | len(b)]) if len(b) < 32 else bytes([0xd9, len(b)])) + b
def u(n):
    assert 0 <= n < 128
    return bytes([n])
def f(x): return b'\xcb' + struct.pack('>d', x)
def arr(*xs): assert len(xs) < 16; return bytes([0x90 | len(xs)]) + b''.join(xs)
def mp(pairs): assert len(pairs) < 16; return bytes([0x80 | len(pairs)]) + b''.join(k + v for k, v in pairs)
NIL = b'\xc0'
frame = arr(
    s("applications.v1"),
    s("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"),
    u(42),
    u(20),
    arr(
        arr(s("orders"), s("up"), s("2.4.1"), mp([(s("heap_used_bytes"), f(300.0)), (s("uptime_seconds"), f(600.5))])),
        arr(s("billing"), s("unreachable"), NIL, mp([])),
    ),
)
open("testdata/application-frame-v1.msgpack", "wb").write(frame)
print(frame.hex())
