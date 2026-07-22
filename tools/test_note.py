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
import os

SOCKET_PATH = "/tmp/haptic-vst.sock"
TEST_CHANNEL = 15
DEFAULT_TEST_NOTE = 36  # Ableton C1, 65.4 Hz without transposition

PROTOCOL_VERSION = 3

# HapticCommand variant tags (declaration order in haptic-protocol)
HELLO, NOTE_ON, NOTE_OFF, MPE_UPDATE, SET_PARAMETER, PANIC = range(6)
# Parameter variant tags
P_WAVE_SPEED, P_STIMULUS_TYPE, P_MONITOR_ROUTE, P_TW_SCALE_MODE, \
    P_TW_WAVELENGTH, P_ATTEN_D0, P_ATTEN_EXPONENT = range(7)
# ClientRole / StimulusType variant tags
ROLE_CONTROLLER = 0
STIMULUS_WAVE = 0
STIMULUS_TW = 1
SCALE_SPEED = 0
SCALE_WAVELENGTH = 1


def frame(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def recv_exact(sock, length):
    chunks = bytearray()
    while len(chunks) < length:
        chunk = sock.recv(length - len(chunks))
        if not chunk:
            raise ConnectionError("server closed during handshake")
        chunks.extend(chunk)
    return bytes(chunks)


def hello(instance_id):
    return frame(struct.pack("<IHQIIfIffff", HELLO, PROTOCOL_VERSION, instance_id,
                             ROLE_CONTROLLER, STIMULUS_WAVE, 20.0,
                             SCALE_SPEED, 20.0, 0.2, 0.5, 1.0))


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


def set_stimulus_type(stimulus_type):
    return frame(struct.pack("<IQII", SET_PARAMETER, 0, P_STIMULUS_TYPE,
                             stimulus_type))


def set_scale_mode(mode):
    return frame(struct.pack("<IQII", SET_PARAMETER, 0, P_TW_SCALE_MODE, mode))


def set_wavelength(wavelength_m):
    return frame(struct.pack("<IQIf", SET_PARAMETER, 0, P_TW_WAVELENGTH,
                             wavelength_m))


def set_atten_d0(d0_m):
    return frame(struct.pack("<IQIf", SET_PARAMETER, 0, P_ATTEN_D0, d0_m))


def set_atten_exponent(exponent):
    return frame(struct.pack("<IQIf", SET_PARAMETER, 0, P_ATTEN_EXPONENT,
                             exponent))


def set_monitor_route(output, source):
    return frame(struct.pack("<IQI2B", SET_PARAMETER, 0, P_MONITOR_ROUTE,
                             output, source))


def panic():
    return frame(struct.pack("<I", PANIC))


class Client:
    """Versioned controller client for the server's framed Unix socket."""

    def __init__(self, socket_path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(socket_path)
        instance_id = (time.time_ns() ^ (os.getpid() << 32)) & ((1 << 64) - 1)
        instance_id = instance_id or 1
        self.sock.sendall(hello(instance_id))
        payload_len = struct.unpack("<I", recv_exact(self.sock, 4))[0]
        payload = recv_exact(self.sock, payload_len)
        status, version, accepted_id = struct.unpack("<IHQ", payload)
        if status != 0 or version != PROTOCOL_VERSION or accepted_id != instance_id:
            raise ConnectionError("server rejected or mismatched the handshake")
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
    ap.add_argument("--note", type=int, default=DEFAULT_TEST_NOTE,
                    help="MIDI note (default 36 / Ableton C1 / 65.4 Hz)")
    ap.add_argument("--velocity", type=int, default=100, help="1-127 (default 100)")
    ap.add_argument("--duration", type=float, default=2.0, help="seconds (default 2)")
    ap.add_argument("--pressure", type=float, default=1.0, help="MPE pressure 0-1")
    ap.add_argument("--x", type=float, default=0.0, help="source x as bend -1..1 (0 = centre)")
    ap.add_argument("--y", type=float, default=0.5, help="source y as timbre 0..1 (0.5 = centre)")
    ap.add_argument("--wave-speed", type=float, help="set wave speed (m/s) before the note")
    ap.add_argument("--type", choices=("wave", "tw"), default="wave",
                    help="stimulus type (default wave)")
    ap.add_argument("--scale-mode", choices=("speed", "wavelength"), default="speed",
                    help="TW spatial scale representation")
    ap.add_argument("--wavelength", type=float, default=0.2,
                    help="TW fixed wavelength in metres (default 0.2)")
    ap.add_argument("--atten-d0", type=float, default=0.5,
                    help="distance-decay knee in metres (default 0.5)")
    ap.add_argument("--atten-p", type=float, default=1.0,
                    help="distance-decay exponent (default 1.0)")
    ap.add_argument("--orbit", action="store_true", help="circle the source during the note")
    ap.add_argument("--orbit-period", type=float, default=4.0, help="seconds per orbit")
    ap.add_argument("--route", action="append", default=[], metavar="OUT:SRC",
                    help="monitor-route physical output OUT to logical channel SRC (repeatable)")
    ap.add_argument("--panic", action="store_true", help="send panic and exit")
    ap.add_argument("--socket", default=os.environ.get("HAPTIC_SOCKET_PATH", SOCKET_PATH),
                    help="server Unix socket (or set HAPTIC_SOCKET_PATH)")
    args = ap.parse_args()

    c = Client(args.socket)

    if args.panic:
        c.send(panic())
        print("panic sent")
        return

    for r in args.route:
        out, src = (int(v) for v in r.split(":"))
        c.send(set_monitor_route(out, src))
        print(f"routed output {out} <- channel {src}")

    stimulus_type = STIMULUS_TW if args.type == "tw" else STIMULUS_WAVE
    scale_mode = SCALE_WAVELENGTH if args.scale_mode == "wavelength" else SCALE_SPEED
    c.send(set_stimulus_type(stimulus_type))
    c.send(set_scale_mode(scale_mode))
    c.send(set_wavelength(args.wavelength))
    c.send(set_atten_d0(args.atten_d0))
    c.send(set_atten_exponent(args.atten_p))

    if args.wave_speed is not None:
        c.send(set_wave_speed(args.wave_speed))
        print(f"wave speed set to {args.wave_speed} m/s")

    c.send(note_on(args.note, args.velocity, args.pressure, args.x, args.y))
    print(f"{args.type} note {args.note} on (velocity {args.velocity}), {args.duration}s...")

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
