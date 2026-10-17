import test from "node:test";
import assert from "node:assert/strict";
import { formatDuration, shortUserAgent } from "./format.js";

test("formats active session durations", () => {
  assert.equal(formatDuration(0), "0m 00s");
  assert.equal(formatDuration(185), "3m 05s");
  assert.equal(formatDuration(3725), "1h 02m");
});

test("summarizes common browser and operating system user agents", () => {
  assert.equal(shortUserAgent("Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/140.0.0.0 Safari/537.36"), "Chrome · Windows");
  assert.equal(shortUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15"), "Safari · macOS");
  assert.equal(shortUserAgent("Firefox/143.0 (X11; Linux x86_64)"), "Firefox · Linux");
});
