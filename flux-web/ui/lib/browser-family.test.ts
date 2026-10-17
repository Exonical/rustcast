import assert from "node:assert/strict";
import test from "node:test";
import { browserFamily } from "./browser-family.js";

test("detects supported browser families without retaining user-agent strings", () => {
  assert.equal(browserFamily("Mozilla/5.0 Chrome/124.0 Safari/537.36"), "chrome");
  assert.equal(browserFamily("Mozilla/5.0 CriOS/124.0 Mobile Safari/537.36"), "chrome");
  assert.equal(browserFamily("Mozilla/5.0 Firefox/126.0"), "firefox");
  assert.equal(browserFamily("Mozilla/5.0 FxiOS/126.0 Mobile"), "firefox");
  assert.equal(browserFamily("Mozilla/5.0 Version/17.0 Safari/605.1"), "safari");
  assert.equal(browserFamily("Mozilla/5.0 Chrome/124.0 Edg/124.0"), "edge");
  assert.equal(browserFamily("Mozilla/5.0 EdgA/124.0"), "edge");
  assert.equal(browserFamily("Mozilla/5.0 OPR/100.0"), "other");
});
