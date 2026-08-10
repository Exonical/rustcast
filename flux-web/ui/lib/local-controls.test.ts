import { test } from "node:test";
import assert from "node:assert/strict";
import { isLocalControlTarget, LOCAL_CONTROL_SELECTORS } from "./local-controls";

test("selector list covers the local controls", () => {
  for (const selector of ["input", "button", "select", "textarea", "[data-local-control]"] as const) {
    assert.ok(LOCAL_CONTROL_SELECTORS.includes(selector));
  }
});

test("non-element event targets are not local controls", () => {
  assert.equal(isLocalControlTarget(null), false);
  assert.equal(isLocalControlTarget({} as EventTarget), false);
});
