import assert from "node:assert/strict";
import test from "node:test";
import { connectFirst, orderCandidates, startDirectProbe, type Candidate } from "./candidates.js";

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

function fakeInterval() {
  let callback: (() => void) | null = null;
  let canceled = false;
  return {
    schedule: (next: () => void, delayMs: number) => {
      assert.equal(delayMs, 60_000);
      callback = next;
      return 1;
    },
    cancel: () => { canceled = true; },
    tick: () => callback?.(),
    get canceled() { return canceled; },
  };
}

const flushProbe = () => new Promise<void>((resolve) => setImmediate(resolve));

test("direct probing is not scheduled without direct candidates", () => {
  const timer = fakeInterval();
  startDirectProbe({
    hasDirectCandidates: false,
    fetchCandidates: async () => [],
    open: () => { throw new Error("must not open"); },
    isHidden: () => false,
    onAvailable: () => assert.fail("must not become available"),
    schedule: timer.schedule,
    cancel: timer.cancel,
  });
  assert.equal(timer.canceled, false);
});

test("direct probe success marks availability and stops the timer", async () => {
  const timer = fakeInterval();
  let available = false;
  let closed = false;
  startDirectProbe({
    hasDirectCandidates: true,
    fetchCandidates: async () => [direct],
    open: () => ({
      transport: { close: () => { closed = true; } },
      ready: Promise.resolve(),
    }),
    isHidden: () => false,
    onAvailable: () => { available = true; },
    schedule: timer.schedule,
    cancel: timer.cancel,
  });
  timer.tick();
  await flushProbe();
  assert.equal(available, true);
  assert.equal(closed, true);
  assert.equal(timer.canceled, true);
});

test("direct probe failures retry on the next timer tick", async () => {
  const timer = fakeInterval();
  let attempts = 0;
  startDirectProbe({
    hasDirectCandidates: true,
    fetchCandidates: async () => [direct],
    open: () => {
      attempts++;
      return { transport: { close: () => {} }, ready: Promise.reject(new Error("unreachable")) };
    },
    isHidden: () => false,
    onAvailable: () => assert.fail("failed probe became available"),
    schedule: timer.schedule,
    cancel: timer.cancel,
  });
  timer.tick();
  await flushProbe();
  timer.tick();
  await flushProbe();
  assert.equal(attempts, 2);
  assert.equal(timer.canceled, false);
});

test("hidden tabs skip direct probes", async () => {
  const timer = fakeInterval();
  let hidden = true;
  let attempts = 0;
  startDirectProbe({
    hasDirectCandidates: true,
    fetchCandidates: async () => { attempts++; return [direct]; },
    open: () => ({ transport: { close: () => {} }, ready: Promise.resolve() }),
    isHidden: () => hidden,
    onAvailable: () => {},
    schedule: timer.schedule,
    cancel: timer.cancel,
  });
  timer.tick();
  await flushProbe();
  assert.equal(attempts, 0);
  hidden = false;
  timer.tick();
  await flushProbe();
  assert.equal(attempts, 1);
});

test("late direct-probe rejection after timeout is handled", async () => {
  const timer = fakeInterval();
  const unhandled: unknown[] = [];
  const onUnhandled = (reason: unknown) => unhandled.push(reason);
  process.on("unhandledRejection", onUnhandled);
  try {
    startDirectProbe({
      hasDirectCandidates: true,
      fetchCandidates: async () => [direct],
      open: () => ({
        transport: { close: () => {} },
        ready: new Promise<void>((_, reject) => setTimeout(() => reject(new Error("late rejection")), 10)),
      }),
      isHidden: () => false,
      onAvailable: () => assert.fail("timed-out probe became available"),
      timeoutMs: 1,
      schedule: timer.schedule,
      cancel: timer.cancel,
    });
    timer.tick();
    await new Promise((resolve) => setTimeout(resolve, 25));
    await flushProbe();
    assert.deepEqual(unhandled, []);
  } finally {
    process.off("unhandledRejection", onUnhandled);
  }
});
