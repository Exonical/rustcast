import test from "node:test";
import assert from "node:assert/strict";
import { webtransportURL } from "./webtransport-client.js";

test("webtransportURL includes the session ticket", () => {
  const url = webtransportURL(
    { host: "relay", port: 8443, certHashes: [], ticket: "one-time-ticket" },
    { hostname: "localhost", machineId: "machine-1" },
  );
  assert.equal(url.searchParams.get("ticket"), "one-time-ticket");
  assert.equal(url.searchParams.get("machine"), "machine-1");
  assert.equal(url.origin, "https://relay:8443");
});

test("webtransportURL omits the ticket when discovery has none", () => {
  const url = webtransportURL(
    { host: "", port: 8443, certHashes: [] },
    { hostname: "localhost" },
  );
  assert.equal(url.searchParams.has("ticket"), false);
  assert.equal(url.hostname, "localhost");
});

test("fetchJSON redirects to login on 401", async () => {
  const assigned: string[] = [];
  const originalFetch = globalThis.fetch;
  const originalWindow = (globalThis as Record<string, unknown>).window;
  (globalThis as Record<string, unknown>).window = {
    location: {
      pathname: "/sessions/",
      search: "?x=1",
      assign: (url: string) => assigned.push(url),
    },
  };
  globalThis.fetch = async () => new Response(JSON.stringify({ error: "unauthenticated" }), { status: 401 });
  try {
    const { fetchMachines } = await import("./api.js");
    await assert.rejects(fetchMachines(), /unauthenticated/);
    assert.deepEqual(assigned, ["/auth/login?next=%2Fsessions%2F%3Fx%3D1"]);
  } finally {
    globalThis.fetch = originalFetch;
    (globalThis as Record<string, unknown>).window = originalWindow;
  }
});

function withFetch(responder: (url: string, init?: RequestInit) => Response, run: () => Promise<void>) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) =>
    responder(String(input), init);
  return run().finally(() => {
    globalThis.fetch = originalFetch;
  });
}

const okJSON = (body: unknown) =>
  new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });

test("fetchPolicy and updatePolicy hit the machine policy routes", async () => {
  const seen: string[] = [];
  const api = await import("./api.js");
  await withFetch((url, init) => {
    seen.push(`${init?.method ?? "GET"} ${url}`);
    return okJSON({ clipboard_mode: "both", max_bytes: 1048576, capture_content: true });
  }, async () => {
    await api.fetchPolicy("host-1");
    await api.updatePolicy("host-1", { clipboard_mode: "off", max_bytes: 64, capture_content: false });
  });
  assert.equal(seen[0], "GET /api/machines/host-1/policy");
  assert.equal(seen[1], "PUT /api/machines/host-1/policy");
});

test("updatePolicy sends the validated policy body", async () => {
  let body = "";
  const api = await import("./api.js");
  await withFetch((_url, init) => {
    body = String(init?.body ?? "");
    return okJSON({ clipboard_mode: "off", max_bytes: 64, capture_content: false });
  }, async () => {
    const policy = await api.updatePolicy("m", { clipboard_mode: "off", max_bytes: 64, capture_content: false });
    assert.equal(policy.clipboard_mode, "off");
  });
  assert.deepEqual(JSON.parse(body), { clipboard_mode: "off", max_bytes: 64, capture_content: false });
});

test("fetchClipboardAudit builds the query string", async () => {
  const urls: string[] = [];
  const api = await import("./api.js");
  await withFetch((url) => {
    urls.push(url);
    return okJSON([]);
  }, async () => {
    await api.fetchClipboardAudit({ machine: "m1", limit: 50, before: 99 });
    await api.fetchClipboardAudit();
  });
  assert.equal(urls[0], "/api/audit/clipboard?machine=m1&limit=50&before=99");
  assert.equal(urls[1], "/api/audit/clipboard");
});

test("fetchClipboardEvent returns row + content", async () => {
  const api = await import("./api.js");
  let url = "";
  let detail!: Awaited<ReturnType<typeof api.fetchClipboardEvent>>;
  await withFetch((u) => {
    url = u;
    return okJSON({ row: { id: 7, direction: "client_to_host" }, content: "hi", encoding: "utf-8" });
  }, async () => {
    detail = await api.fetchClipboardEvent(7);
  });
  assert.equal(url, "/api/audit/clipboard/7");
  assert.equal(detail.row.id, 7);
  assert.equal(detail.encoding, "utf-8");
});

test("policy endpoints redirect on 401 too", async () => {
  const assigned: string[] = [];
  const originalWindow = (globalThis as Record<string, unknown>).window;
  (globalThis as Record<string, unknown>).window = {
    location: { pathname: "/", search: "", assign: (u: string) => assigned.push(u) },
  };
  const api = await import("./api.js");
  await withFetch(() => new Response("{}", { status: 401 }), async () => {
    await assert.rejects(api.fetchPolicy("m"), /unauthenticated/);
    await assert.rejects(api.fetchClipboardAudit(), /unauthenticated/);
  });
  (globalThis as Record<string, unknown>).window = originalWindow;
  assert.equal(assigned.length, 2);
});
