import assert from "node:assert/strict";
import test from "node:test";
import { connectFirst, orderCandidates, type Candidate } from "./candidates.js";

const direct: Candidate = { path: "direct", url: "https://host.example.test/wt", ticket: "d" };
const relay: Candidate = { path: "relay", host: "relay.example.test", port: 8443, ticket: "r" };

test("candidate ordering preserves direct-first discovery and prefers relay after a recent direct failure", () => {
  const now = 10_000_000;
  assert.deepEqual(orderCandidates({ candidates: [direct, relay] }, undefined, now), [direct, relay]);
  assert.deepEqual(orderCandidates({ candidates: [direct, relay] }, now - 1000, now), [relay, direct]);
  assert.deepEqual(orderCandidates({ candidates: [direct, relay] }, now - 5 * 60 * 1000, now), [direct, relay]);
});

test("a relay success does not renew the direct-failure preference", async () => {
  const now = 20_000_000;
  const savedAt = now - 1000;
  let directFailedAt = savedAt;
  await connectFirst(
    orderCandidates({ candidates: [direct, relay] }, directFailedAt, now),
    () => ({ transport: { close: () => {} }, ready: Promise.resolve() }),
    { direct: 5, relay: 50 },
    (candidate) => {
      if (candidate.path === "direct") directFailedAt = now;
    },
  );
  assert.equal(directFailedAt, savedAt);
  assert.equal(orderCandidates({ candidates: [direct, relay] }, directFailedAt, now + 4 * 60 * 1000)[0].path, "relay");
  assert.equal(orderCandidates({ candidates: [direct, relay] }, directFailedAt, now + 6 * 60 * 1000)[0].path, "direct");
});

test("old discovery shape becomes one relay candidate", () => {
  assert.deepEqual(
    orderCandidates({ host: "relay.example.test", port: 8443, certHashes: ["hash"], ticket: "ticket" }),
    [{ path: "relay", host: "relay.example.test", port: 8443, certHashes: ["hash"], ticket: "ticket" }],
  );
});

test("a timed-out direct candidate is closed before relay is used", async () => {
  let directClosed = false;
  const result = await connectFirst(
    [direct, relay],
    (candidate) => candidate.path === "direct"
      ? { transport: { close: () => { directClosed = true; } }, ready: new Promise<void>(() => {}) }
      : { transport: { close: () => {} }, ready: Promise.resolve() },
    { direct: 5, relay: 50 },
  );
  assert.equal(result.candidate.path, "relay");
  assert.equal(directClosed, true);
});

test("a late timeout rejection does not become unhandled", async () => {
  const unhandled: unknown[] = [];
  const onUnhandled = (reason: unknown) => unhandled.push(reason);
  process.on("unhandledRejection", onUnhandled);
  try {
    const result = await connectFirst(
      [direct, relay],
      (candidate) => candidate.path === "direct"
        ? {
            transport: { close: () => {} },
            ready: new Promise<void>((_, reject) => setTimeout(() => reject(new Error("late rejection")), 25)),
          }
        : { transport: { close: () => {} }, ready: Promise.resolve() },
      { direct: 5, relay: 50 },
    );
    assert.equal(result.candidate.path, "relay");
    await new Promise((resolve) => setTimeout(resolve, 40));
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.deepEqual(unhandled, []);
  } finally {
    process.off("unhandledRejection", onUnhandled);
  }
});

test("a rejected direct candidate falls back to relay", async () => {
  const result = await connectFirst(
    [direct, relay],
    (candidate) => candidate.path === "direct"
      ? { transport: { close: () => {} }, ready: Promise.reject(new Error("direct rejected")) }
      : { transport: { close: () => {} }, ready: Promise.resolve() },
    { direct: 50, relay: 50 },
  );
  assert.equal(result.candidate.path, "relay");
});

test("all candidate failures throw the last error", async () => {
  const lastError = new Error("relay rejected");
  await assert.rejects(
    connectFirst(
      [direct, relay],
      (candidate) => ({
        transport: { close: () => {} },
        ready: Promise.reject(candidate.path === "relay" ? lastError : new Error("direct rejected")),
      }),
      { direct: 50, relay: 50 },
    ),
    (error) => error === lastError,
  );
});
