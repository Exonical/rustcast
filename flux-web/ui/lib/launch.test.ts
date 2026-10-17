import test from "node:test";
import assert from "node:assert/strict";
import { buildConnectUrl, parseConnectParams, validResolution } from "./launch.js";

test("launch URL omits native mode and automatic controls", () => {
  assert.equal(
    buildConnectUrl("machine-1", { width: 1920, height: 1080 }, {
      width: 1920, height: 1080, fps: 0, quality: 0,
    }),
    "/connect/?machine=machine-1",
  );
});

test("launch URL encodes selected mode and fixed controls", () => {
  assert.equal(
    buildConnectUrl("machine 1", { width: 1920, height: 1080 }, {
      width: 1280, height: 720, fps: 60, quality: 7,
    }),
    "/connect/?machine=machine+1&width=1280&height=720&fps=60&quality=7",
  );
});

test("connect params accept only valid even modes and bounded controls", () => {
  assert.deepEqual(parseConnectParams("?machine=host&width=1280&height=720&fps=144&quality=10"), {
    machine: "host", width: 1280, height: 720, fps: 144, quality: 10,
  });
  assert.deepEqual(parseConnectParams("machine=host&width=1279&height=720&fps=145&quality=11"), {
    machine: "host",
  });
  assert.deepEqual(parseConnectParams("machine=host&width=1280&fps=30"), { machine: "host", fps: 30 });
});

test("mode validation uses the host dimensions and even bounds", () => {
  assert.equal(validResolution(640, 480), true);
  assert.equal(validResolution(2560, 1440), true);
  assert.equal(validResolution(1920, 1080), true);
  assert.equal(validResolution(641, 480), false);
  assert.equal(validResolution(2562, 1440), false);
});
