import test from "node:test";
import assert from "node:assert/strict";
import type { Machine } from "./api.js";
import { canJoinMachine, dedupeDefaultMachine, filterMachines, machineState } from "./machines.js";

const machine = (overrides: Partial<Machine> = {}): Machine => ({
  id: "host-1",
  name: "Linux Host",
  display_name: "Workstation",
  frame_endpoint: "192.0.2.10:8556",
  os: "Linux",
  gpu_vendor: "AMD",
  encoder_backend: "Software",
  virtual_display: true,
  width: 1920,
  height: 1080,
  target_fps: 60,
  default_fps: 48,
  viewers: 0,
  last_seen: new Date(0).toISOString(),
  status: "online",
  upstream_status: "idle",
  ...overrides,
});

test("machine state prioritizes offline then active viewers", () => {
  assert.equal(machineState(machine()), "available");
  assert.equal(machineState(machine({ viewers: 1 })), "in-use");
  assert.equal(machineState(machine({ status: "offline", viewers: 1 })), "offline");
});

test("search covers display, platform, GPU and encoder fields", () => {
  const host = machine();
  assert.deepEqual(filterMachines([host], "workstation", "all"), [host]);
  assert.deepEqual(filterMachines([host], "amd", "all"), [host]);
  assert.deepEqual(filterMachines([host], "h264", "all"), []);
});

test("filters and hides a duplicate static endpoint when the real host is online", () => {
  const duplicate = machine({
    id: "default",
    name: "Configured Flux Server",
    frame_endpoint: "192.0.2.10:8556",
  });
  const available = machine();
  const busy = machine({ id: "host-2", frame_endpoint: "192.0.2.11:8556", viewers: 1 });
  const offline = machine({ id: "host-3", frame_endpoint: "192.0.2.12:8556", status: "offline" });
  assert.deepEqual(dedupeDefaultMachine([duplicate, available]).map((m) => m.id), ["host-1"]);
  assert.deepEqual(filterMachines([duplicate, available, busy, offline], "", "available").map((m) => m.id), ["host-1"]);
  assert.deepEqual(filterMachines([available, busy, offline], "", "in-use").map((m) => m.id), ["host-2"]);
  assert.deepEqual(filterMachines([available, busy, offline], "", "offline").map((m) => m.id), ["host-3"]);
});

test("machineState: locked login state maps to locked", () => {
  assert.equal(machineState(machine({ login_state: "locked", logged_in_user: "devuser" })), "locked");
  assert.equal(machineState(machine({ login_state: "active", logged_in_user: "devuser" })), "available");
});

test("canJoinMachine: same os user, admin, or idle only", () => {
  const locked = machine({ login_state: "locked", logged_in_user: "devuser" });
  assert.equal(canJoinMachine(locked, { username: "devuser" }), true);
  assert.equal(canJoinMachine(locked, { username: "other" }), false);
  assert.equal(canJoinMachine(locked, { username: "other", admin: true }), true);
  const activeOther = machine({ login_state: "active", logged_in_user: "devuser" });
  assert.equal(canJoinMachine(activeOther, { username: "other" }), false);
  const idle = machine({ login_state: "idle" });
  assert.equal(canJoinMachine(idle, { username: "anyone" }), true);
});
