import { test } from "node:test";
import assert from "node:assert/strict";
import { isLocalControlTarget } from "./local-controls";

type FakeNode = {
  control?: boolean;
  closest: () => FakeNode | null;
};

function node(control: boolean, parent?: FakeNode): FakeNode {
  return {
    control,
    closest: () => (control ? ({} as FakeNode) : parent?.closest() ?? null),
  };
}

test("recognizes a local control", () => {
  assert.equal(isLocalControlTarget(node(true) as unknown as EventTarget), true);
});

test("recognizes a child inside a local control", () => {
  assert.equal(isLocalControlTarget(node(false, node(true)) as unknown as EventTarget), true);
});

test("does not classify the video surface as local", () => {
  assert.equal(isLocalControlTarget(node(false) as unknown as EventTarget), false);
});

test("does not classify the viewer container as local", () => {
  assert.equal(isLocalControlTarget(null), false);
});
