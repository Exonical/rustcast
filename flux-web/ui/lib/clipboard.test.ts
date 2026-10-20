import test from "node:test";
import assert from "node:assert/strict";
import {
  ClipboardDedup,
  PendingRemoteClipboard,
  CLIPBOARD_MAX_BYTES,
  isPasteChord,
  policyAllowsClientSend,
  policyAllowsRemoteReceive,
  policyLabel,
  withinSizeCap,
} from "./clipboard.js";

const chord = (over: Partial<Parameters<typeof isPasteChord>[0]>) =>
  isPasteChord({ ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, code: "", ...over });

test("isPasteChord detects paste gestures", () => {
  assert.equal(chord({ ctrlKey: true, code: "KeyV" }), true);
  assert.equal(chord({ metaKey: true, code: "KeyV" }), true);
  assert.equal(chord({ shiftKey: true, code: "Insert" }), true);
  assert.equal(chord({ shiftKey: true, key: "Insert" }), true);
});

test("isPasteChord rejects non-paste keys", () => {
  assert.equal(chord({ ctrlKey: true, code: "KeyC" }), false);
  assert.equal(chord({ code: "KeyV" }), false);
  assert.equal(chord({ ctrlKey: true, altKey: true, code: "KeyV" }), false);
  assert.equal(chord({ code: "Insert" }), false);
});

test("policy gating per mode", () => {
  assert.equal(policyAllowsClientSend("both"), true);
  assert.equal(policyAllowsClientSend("client_to_host"), true);
  assert.equal(policyAllowsClientSend("host_to_client"), false);
  assert.equal(policyAllowsClientSend("off"), false);
  assert.equal(policyAllowsRemoteReceive("both"), true);
  assert.equal(policyAllowsRemoteReceive("host_to_client"), true);
  assert.equal(policyAllowsRemoteReceive("client_to_host"), false);
  assert.equal(policyAllowsRemoteReceive("off"), false);
});

test("policyLabel humanizes modes", () => {
  assert.equal(policyLabel("both"), "both directions");
  assert.equal(policyLabel("off"), "disabled by policy");
});

test("withinSizeCap enforces the 1 MiB limit", () => {
  assert.equal(withinSizeCap("x".repeat(1024)), true);
  assert.equal(withinSizeCap("x".repeat(CLIPBOARD_MAX_BYTES + 1)), false);
  // multibyte characters count as UTF-8 bytes, not chars
  assert.equal(withinSizeCap("界".repeat(CLIPBOARD_MAX_BYTES / 2)), false);
});

test("dedup suppresses identical text in both directions", () => {
  const d = new ClipboardDedup();
  assert.equal(d.shouldSendLocal("a"), true);
  // Host echoes "a" back through SelectionOwnerChanged → suppressed.
  assert.equal(d.shouldApplyRemote("a"), false);
  // Real remote change applies once, and reading it locally doesn't resend.
  assert.equal(d.shouldApplyRemote("b"), true);
  assert.equal(d.shouldSendLocal("b"), false);
  assert.equal(d.shouldSendLocal("c"), true);
  d.noteSent("d");
  assert.equal(d.shouldApplyRemote("d"), false);
});

test("pending remote flush retries on failure and clears on success", async () => {
  const p = new PendingRemoteClipboard();
  assert.equal(await p.flushWith(async () => {}), false); // nothing pending

  p.store("remote-text");
  let attempts = 0;
  const failing = async (): Promise<void> => { attempts++; throw new Error("no gesture"); };
  assert.equal(await p.flushWith(failing), false);
  assert.equal(p.current, "remote-text"); // retained for next gesture

  const written: string[] = [];
  assert.equal(await p.flushWith(async (t) => { void written.push(t); }), true);
  assert.deepEqual(written, ["remote-text"]);
  assert.equal(p.current, null);
  assert.equal(attempts, 1);

  // A newer store wins over an older pending value.
  p.store("old");
  p.store("new");
  await p.flushWith(async (t) => { written.push(t); });
  assert.equal(written[1], "new");
});
