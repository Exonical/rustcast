#!/usr/bin/env python3
"""Left-click at pixel X Y through flux-server's input port (127.0.0.1:8556).

  podman exec -i flux-host su flux -c 'python3 - X Y [W H]' < containers/test-click.py
"""
import json, socket, struct, sys, time

x, y = float(sys.argv[1]), float(sys.argv[2])
w, h = (float(sys.argv[3]), float(sys.argv[4])) if len(sys.argv) > 4 else (1920.0, 1080.0)
s = socket.create_connection(("127.0.0.1", 8556))


def send(ev):
    payload = json.dumps(ev).encode()
    s.sendall(b"\x02" + struct.pack(">I", len(payload)) + payload)
    time.sleep(0.3)


send({"Mouse": {"MoveAbsolute": {"x": x / (w - 1), "y": y / (h - 1)}}})
send({"Mouse": {"ButtonDown": {"button": "Left"}}})
send({"Mouse": {"ButtonUp": {"button": "Left"}}})
time.sleep(0.5)
