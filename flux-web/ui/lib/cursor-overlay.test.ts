import assert from "node:assert/strict";
import test from "node:test";
import { getContainedVideoGeometry, mapCursorToVideo } from "./cursor-overlay";

test("maps a cursor through horizontal object-contain bars", () => {
  const geometry = getContainedVideoGeometry(1600, 900, 1920, 1080);
  assert.deepEqual(geometry, {
    boxWidth: 1600,
    boxHeight: 900,
    renderWidth: 1600,
    renderHeight: 900,
    offsetX: 0,
    offsetY: 0,
  });
  assert.deepEqual(mapCursorToVideo([960, 540], geometry!, 1920, 1080, 32, 32), {
    left: 800,
    top: 450,
    width: 26.666666666666668,
    height: 26.666666666666668,
  });
});

test("maps a cursor through vertical object-contain bars", () => {
  const geometry = getContainedVideoGeometry(1200, 900, 1920, 1080);
  assert.equal(geometry?.offsetY, 112.5);
  assert.deepEqual(mapCursorToVideo([0, 0], geometry!, 1920, 1080, 16, 16), {
    left: 0,
    top: 112.5,
    width: 10,
    height: 10,
  });
});
