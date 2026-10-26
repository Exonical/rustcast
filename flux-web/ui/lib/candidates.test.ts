import assert from "node:assert/strict";
import test from "node:test";
import { connectFirst, orderCandidates, type Candidate } from "./candidates.js";

const direct: Candidate = { path: "direct", url: "https://host.example.test/wt", ticket: "d" };
const relay: Candidate = { path: "relay", host: "relay.example.test", port: 8443, ticket: "r" };

test("candidate ordering preserves direct-first discovery and prefers recent relay", () => {
  assert.deepEqual(orderCandidates({ candidates: [direct, relay] }), [direct, relay]);
  assert.deepEqual(orderCandidates({ candidates: [direct, relay] }, "relay"), [relay, direct]);
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
