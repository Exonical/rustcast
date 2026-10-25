import assert from "node:assert/strict";
import test from "node:test";
import {
  initialLoginCard,
  reduceLoginMessage,
  reduceLoginSubmit,
} from "./login.js";

test("prompt sets input style and clears pending", () => {
  let s = initialLoginCard();
  assert.equal(s.pending, true);
  s = reduceLoginMessage(s, { state: "prompt", style: "echo_on", message: "Username" });
  assert.equal(s.prompt?.style, "echo_on");
  assert.equal(s.pending, false);
  s = reduceLoginSubmit(s);
  assert.equal(s.pending, true);
  s = reduceLoginMessage(s, { state: "prompt", style: "echo_off", message: "Password: " });
  assert.equal(s.prompt?.style, "echo_off");
});

test("info/error prompts accumulate as lines", () => {
  let s = initialLoginCard();
  s = reduceLoginMessage(s, { state: "prompt", style: "info", message: "Creating home" });
  s = reduceLoginMessage(s, { state: "prompt", style: "error", message: "nope" });
  assert.equal(s.prompt, null);
  assert.deepEqual(s.lines.map((l) => l.kind), ["info", "error"]);
});

test("failed keeps the session in login flow", () => {
  const s = reduceLoginMessage(initialLoginCard(), { state: "failed", message: "bad", user: "b" });
  assert.equal(s.prompt, null);
  assert.equal(s.pending, false);
  assert.equal(s.lines[s.lines.length - 1]?.kind, "error");
});

test("ok and busy produce status", () => {
  const ok = reduceLoginMessage(initialLoginCard(), { state: "ok", user: "devuser", uid: 2101, reattached: false });
  assert.match(ok.status!, /Signed in as devuser/);
  const re = reduceLoginMessage(initialLoginCard(), { state: "ok", user: "devuser", uid: 2101, reattached: true });
  assert.match(re.status!, /Resumed/);
  const busy = reduceLoginMessage(initialLoginCard(), { state: "busy", user: "bryce" });
  assert.match(busy.status!, /bryce/);
});
