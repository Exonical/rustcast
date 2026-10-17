import { test } from "node:test";
import assert from "node:assert/strict";
import {
  isLocalControlTarget,
  isLocalKeyboardTarget,
  LOCAL_CONTROL_SELECTORS,
} from "./local-controls";

test("selector list covers the local controls", () => {
  for (const selector of ["input", "button", "select", "textarea", "[data-local-control]"] as const) {
    assert.ok(LOCAL_CONTROL_SELECTORS.includes(selector));
  }
});

test("non-element event targets are not local controls", () => {
  assert.equal(isLocalControlTarget(null), false);
  assert.equal(isLocalControlTarget({} as EventTarget), false);
  assert.equal(isLocalKeyboardTarget(null), false);
  assert.equal(isLocalKeyboardTarget({} as EventTarget), false);
});

function fake(tag: string): EventTarget {
  return {
    closest(selector: string) {
      return selector.split(",").includes(tag) ? this : null;
    },
  } as unknown as EventTarget;
}

test("isLocalKeyboardTarget only matches text-entry controls", () => {
  assert.equal(isLocalKeyboardTarget(fake("button")), false);
  assert.equal(isLocalKeyboardTarget(fake("textarea")), true);
  assert.equal(isLocalKeyboardTarget(fake("select")), true);
  assert.equal(isLocalKeyboardTarget(fake("[data-local-control]")), false);
  assert.equal(isLocalKeyboardTarget(fake("[data-local-keyboard]")), true);
  // The broad pointer list still treats buttons as local controls.
  assert.equal(isLocalControlTarget(fake("button")), true);
});
