#!/usr/bin/env python3
# Regenerates snapshot-frame-v1.msgpack from the repository root:
#   python3 testdata/generate_snapshot_frame_v1.py
# Hand-rolled MessagePack encoder, independent of rmp-serde, for the snapshot frame golden
# (RFC 0014). Every same-typed field holds a different value, so a frame that moves one
# field onto another's position can't match these bytes.
import struct
def s(x):
    b = x.encode()
    assert len(b) < 32
    return bytes([0xa0 | len(b)]) + b
def u(n):
    if n < 128: return bytes([n])
    if n < 1 << 16: return b'\xcd' + struct.pack('>H', n)
    return b'\xce' + struct.pack('>I', n)
def f32(x): return b'\xca' + struct.pack('>f', x)
def f64(x): return b'\xcb' + struct.pack('>d', x)
def arr(*xs):
    if len(xs) < 16: return bytes([0x90 | len(xs)]) + b''.join(xs)
    return b'\xdc' + struct.pack('>H', len(xs)) + b''.join(xs)
frame = arr(
    s("system-s"),            # system_id
    s("host-h"),              # hostname
    s("os-pretty"),           # os_name
    s("kernel-k"),            # kernel
    f32(12.5),                # cpu_percent
    u(8),                     # cpu_cores
    s("cpu-model"),           # cpu_model
    f32(33.25),               # memory_percent
    s("mem-used"),            # memory_used_display
    s("mem-total"),           # memory_total_display
    u(1111),                  # memory_used_bytes
    u(2222),                  # memory_total_bytes
    f32(7.75),                # swap_percent
    f64(0.5),                 # load_one
    f64(0.25),                # load_five
    f64(0.125),               # load_fifteen
    u(3333),                  # uptime_seconds
    s("uptime-u"),            # uptime_display
    arr(                      # disks
        arr(s("disk-mount"), f32(50.5), s("disk-total"), s("disk-used")),
        arr(s("disk2-mount"), f32(60.25), s("disk2-total"), s("disk2-used")),
    ),
    arr(arr(u(1), s("proc-1"), f32(1.5), s("proc-mem"), f32(0.75))),        # top_processes
    u(4444444),               # timestamp
)
open("testdata/snapshot-frame-v1.msgpack", "wb").write(frame)
print(frame.hex())
