#!/usr/bin/env python3
"""Send input events to flux-server's TCP frame port (default 127.0.0.1:8556).

Run inside the host container:
  podman exec -i flux-host python3 - TEXT < containers/test-input.py
Types TEXT (plus Enter) after clicking at the screen centre and scrolling.
"""
import json, socket, struct, sys, time

SC = {c: s for c, s in zip("qwertyuiop", range(0x10, 0x1A))}
SC.update({c: s for c, s in zip("asdfghjkl", range(0x1E, 0x27))})
SC.update({c: s for c, s in zip("zxcvbnm", range(0x2C, 0x33))})
SC.update({" ": 0x39, "/": 0x35, ".": 0x34, "-": 0x0C, "\n": 0x1C})
SC.update({c: s for c, s in zip("1234567890", range(0x02, 0x0C))})

text = sys.argv[1] if len(sys.argv) > 1 else "touch /tmp/flux-key-ok"
s = socket.create_connection(("127.0.0.1", 8556))


def send(ev):
    payload = json.dumps(ev).encode()
    s.sendall(b"\x02" + struct.pack(">I", len(payload)) + payload)
    time.sleep(0.02)


def key(sc, down):
    k = "KeyDown" if down else "KeyUp"
    send({"Keyboard": {k: {"scan_code": sc, "key_code": None, "modifiers": 0}}})


send({"Mouse": {"MoveAbsolute": {"x": 0.5, "y": 0.5}}})
time.sleep(0.3)
send({"Mouse": {"ButtonDown": {"button": "Left"}}})
send({"Mouse": {"ButtonUp": {"button": "Left"}}})
send({"Mouse": {"Move": {"dx": 5, "dy": 5}}})
send({"Mouse": {"Scroll": {"dx": 0, "dy": 120}}})
send({"Mouse": {"Scroll": {"dx": 0, "dy": -120}}})
for ch in text + "\n":
    key(SC[ch], True)
    key(SC[ch], False)
time.sleep(0.5)
