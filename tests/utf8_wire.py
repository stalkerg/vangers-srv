"""Verify the real Rust relay with legacy and negotiated protocol-6 sockets.

Usage: python3 wire_qa.py current-server legacy-server
Only loopback clients and short-lived server child processes are used.
"""
import contextlib
import os
import socket
import struct
import subprocess
import sys
import time

CAPABILITY = b"VANGERS-UTF8/1"
REQ = b"Vivat Sicher, Rock'n'Roll forever!!!\x00\x06"
ACK = b"Enter, my son, please...\x00\x06"

@contextlib.contextmanager
def relay(exe):
    with socket.socket() as temp:
        temp.bind(("127.0.0.1", 0))
        port = temp.getsockname()[1]
    with subprocess.Popen([exe, "--port", str(port)], stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL, env={**os.environ, "RUST_LOG": "error"}) as process:
        try:
            for attempt in range(80):
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=1):
                        break
                except ConnectionRefusedError:
                    time.sleep(.05)
            else:
                raise RuntimeError("Server did not bind")
            yield port
        finally:
            process.terminate()

class Peer:
    def __init__(self, port, offer, expect):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.sock.sendall(REQ)
        assert self.read(len(ACK)) == ACK
        self.send(0x89, CAPABILITY if offer else b"")
        reply = self.receive(0xC6)
        assert reply[4:] == (CAPABILITY if expect else b""), reply
        self.codec = "utf-8" if expect else "cp866"

    def read(self, n):
        data = bytearray()
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            if not chunk:
                raise EOFError("Unexpected close")
            data += chunk
        return bytes(data)

    def send(self, op, data=b""):
        self.sock.sendall(struct.pack("<HB", len(data) + 1, op) + data)

    def receive(self, op):
        for _ in range(32):
            n, = struct.unpack("<H", self.read(2))
            frame = self.read(n)
            if frame[0] == op:
                return frame[1:]
        raise AssertionError(f"Missing event {op:02x}")

    def attach(self, game=0):
        self.send(0x83, struct.pack("<I", game))
        reply = self.receive(0xC3)
        self.id = reply[9]
        return struct.unpack("<I", reply[:4])[0]

    def restore(self, game, station):
        self.send(0x84, struct.pack("<IB", game, station))
        assert self.receive(0xC4) == b"\x01"
        self.id = station

    def register(self, name):
        self.send(0x88, name.encode(self.codec) + b"\x00password\x00")
        self.send(0x94, bytes(52))
        # A reply on this TCP stream fences registration + body setup before
        # peers issue roster requests on other connections.
        self.roster()

    def say(self, text, *peers):
        self.send(0x95, struct.pack("<I", sum(1 << (p.id - 1) for p in peers)) +
                  text.encode(self.codec) + b"\x00")

    def name(self, sender, text):
        assert self.receive(0xD5) == bytes([sender.id]) + text.encode(self.codec) + b"\x00"

    def hears(self, sender, text):
        assert self.receive(0xCE) == bytes([sender.id]) + text.encode(self.codec) + b"\x00"

    def roster(self, *names):
        self.send(0x91)
        roster = self.receive(0xCC)
        for name in names:
            assert name.encode(self.codec) + b"\x00" in roster, (name, roster)

    def reset(self):
        self.sock.close()

def legacy_server(exe):
    with relay(exe) as port:
        current = Peer(port, offer=True, expect=False)
        old = Peer(port, offer=False, expect=False)
        try:
            old.attach(current.attach())
            current.register("Вангер")
            old.name(current, "Вангер")
            old.register("Тест")
            current.name(old, "Тест")
            current.say("Привет?", old)
            old.hears(current, "Привет?")
            old.say("Пока", current)
            current.hears(old, "Пока")
        finally:
            old.reset()
            current.reset()
    print("legacy server: capability probe ignored; CP866 name and chat unchanged")

def mixed_server(exe):
    with relay(exe) as port:
        alice = Peer(port, offer=True, expect=True)
        bob = Peer(port, offer=True, expect=True)
        legacy = Peer(port, offer=False, expect=False)
        try:
            game = alice.attach()
            bob.attach(game)
            legacy.attach(game)

            long_name = "日本語の参加者あいうえおかきくけこ"
            name = long_name[:16]
            alice.register(long_name)
            bob.name(alice, name)
            legacy.name(alice, f"Vanger-{alice.id}")
            bob.register("Éloïse")
            alice.name(bob, "Éloïse")
            legacy.name(bob, f"Vanger-{bob.id}")
            legacy.register("Вангер")
            alice.name(legacy, "Вангер")
            bob.name(legacy, "Вангер")
            alice.roster(name, "Éloïse", "Вангер")
            legacy.roster(f"Vanger-{alice.id}", f"Vanger-{bob.id}", "Вангер")

            alice.say("日本語 É漢字", bob, legacy)
            bob.hears(alice, "日本語 É漢字")
            legacy.hears(alice, "[UTF-8 message]")

            alice.say("Тест?", bob, legacy)
            bob.hears(alice, "Тест?")
            legacy.hears(alice, "Тест?")

            # Expansion on an upgraded receiver does not cut off a legacy chat line.
            legacy.say("Ж" * 139, alice)
            alice.hears(legacy, "Ж" * 139)

            # Forced clipping (client normally sends already width/byte-bounded rows).
            alice.say("x" * 135 + "日" + "abcdef", bob)
            bob.hears(alice, "x" * 135 + "...")

            # The connection dies, but the station/name belongs to the game.
            # The new connection must opt in again before restoring that station.
            station = alice.id
            alice.reset()
            alice = Peer(port, offer=True, expect=True)
            alice.restore(game, station)
            alice.roster(name, "Éloïse", "Вангер")
            alice.say("再接続 É", bob)
            bob.hears(alice, "再接続 É")
            bob.say("復帰", alice, legacy)
            alice.hears(bob, "復帰")
            legacy.hears(bob, "[UTF-8 message]")

            # Mode is not inherited from the station when restoring without opt-in.
            alice.reset()
            alice = Peer(port, offer=False, expect=False)
            alice.restore(game, station)
            alice.roster(f"Vanger-{station}", f"Vanger-{bob.id}", "Вангер")
            alice.say("Привет", bob)
            bob.hears(alice, "Привет")
        finally:
            for peer in (legacy, bob, alice):
                peer.reset()
    print("new server: new/new and mixed names, roster, chat, scalar boundaries and reconnect OK")

if __name__ == "__main__":
    mixed_server(sys.argv[1])
    legacy_server(sys.argv[2])
