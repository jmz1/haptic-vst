#!/usr/bin/env python3
"""Headless test client for the haptic server.

Speaks the length-prefixed bincode protocol over /tmp/haptic-vst.sock.
See TESTING.md section 3, and haptic-protocol/src/lib.rs for the schema.

Examples:
    python3 tools/test_note.py
    python3 tools/test_note.py --note 48 --velocity 80 --duration 5 --orbit
    python3 tools/test_note.py --wave-speed 100 --route 0:31 --route 1:13
    python3 tools/test_note.py --panic
"""
import argparse
import math
import socket
import struct
import time

SOCKET_PATH = "/tmp/haptic-vst.sock"
TEST_CHANNEL = 15

# HapticCommand variant tags (declaration order in haptic-protocol)
NOTE_ON, NOTE_OFF, MPE_UPDATE, SET_PARAMETER, PANIC = range(5)
# Parameter variant tags
P_WAVE_SPEED, P_STIMULUS_TYPE, P_MONITOR_ROUTE = range(3)


def frame(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def note_on(note, velocity, pressure, bend, timbre):
    return frame(struct.pack("<IQ3B3f", NOTE_ON, 0, note, velocity, TEST_CHANNEL,
                             pressure, bend, timbre))


def note_off(note):
    return frame(struct.pack("<IQ2B", NOTE_OFF, 0, note, TEST_CHANNEL))


def mpe_update(pressure, bend, timbre):
    return frame(struct.pack("<IQB3f", MPE_UPDATE, 0, TEST_CHANNEL,
                             pressure, bend, timbre))


def set_wave_speed(speed):
    return frame(struct.pack("<IQIf", SET_PARAMETER, 0, P_WAVE_SPEED, speed))


def set_monitor_route(output, source):
    return frame(struct.pack("<IQI2B", SET_PARAMETER, 0, P_MONITOR_ROUTE,
                             output, source))


def panic():
    return frame(struct.pack("<I", PANIC))


class Client:
    """Send-and-drain client: the server broadcasts status to every
    connection and drops clients whose receive buffer fills, so we must
    keep reading even though we ignore the content."""

    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(SOCKET_PATH)
        self.sock.setblocking(False)

    def drain(self):
        try:
            while self.sock.recv(65536):
                pass
        except BlockingIOError:
            pass

    def send(self, data: bytes):
        self.sock.setblocking(True)
        self.sock.sendall(data)
        self.sock.setblocking(False)
        self.drain()

    def sleep(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            self.drain()
            time.sleep(0.01)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--note", type=int, default=60, help="MIDI note (default 60)")
    ap.add_argument("--velocity", type=int, default=100, help="1-127 (default 100)")
    ap.add_argument("--duration", type=float, default=2.0, help="seconds (default 2)")
    ap.add_argument("--pressure", type=float, default=1.0, help="MPE pressure 0-1")
    ap.add_argument("--x", type=float, default=0.0, help="source x as bend -1..1 (0 = centre)")
    ap.add_argument("--y", type=float, default=0.5, help="source y as timbre 0..1 (0.5 = centre)")
    ap.add_argument("--wave-speed", type=float, help="set wave speed (m/s) before the note")
    ap.add_argument("--orbit", action="store_true", help="circle the source during the note")
    ap.add_argument("--orbit-period", type=float, default=4.0, help="seconds per orbit")
    ap.add_argument("--route", action="append", default=[], metavar="OUT:SRC",
                    help="monitor-route physical output OUT to logical channel SRC (repeatable)")
    ap.add_argument("--panic", action="store_true", help="send panic and exit")
    args = ap.parse_args()

    c = Client()

    if args.panic:
        c.send(panic())
        print("panic sent")
        return

    for r in args.route:
        out, src = (int(v) for v in r.split(":"))
        c.send(set_monitor_route(out, src))
        print(f"routed output {out} <- channel {src}")

    if args.wave_speed is not None:
        c.send(set_wave_speed(args.wave_speed))
        print(f"wave speed set to {args.wave_speed} m/s")

    c.send(note_on(args.note, args.velocity, args.pressure, args.x, args.y))
    print(f"note {args.note} on (velocity {args.velocity}), {args.duration}s...")

    if args.orbit:
        start = time.time()
        while time.time() - start < args.duration:
            t = (time.time() - start) / args.orbit_period * 2 * math.pi
            bend = 0.7 * math.cos(t)
            timbre = 0.5 + 0.35 * math.sin(t)
            c.send(mpe_update(args.pressure, bend, timbre))
            c.sleep(0.01)
    else:
        c.sleep(args.duration)

    c.send(note_off(args.note))
    print("note off")
    c.sleep(0.8)  # let the release finish


if __name__ == "__main__":
    main()
