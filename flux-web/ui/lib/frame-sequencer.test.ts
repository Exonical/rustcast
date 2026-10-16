import assert from "node:assert/strict";
import test from "node:test";
import { FrameSequencer, type EncodedFrame } from "./frame-sequencer.js";

function frame(seq: number, keyframe = false): EncodedFrame {
  return { seq, keyframe, data: new Uint8Array([seq & 0xff]), receivedAt: seq };
}

test("emits frames in sequence and buffers out-of-order arrivals", () => {
  const sequencer = new FrameSequencer();
  assert.deepEqual(sequencer.push(frame(0, true), 0).ready.map((item) => item.seq), [0]);
  assert.equal(sequencer.push(frame(2), 2).ready.length, 0);
  assert.deepEqual(sequencer.push(frame(1), 3).ready.map((item) => item.seq), [1, 2]);
});

test("buffers startup deltas until a lower-sequence keyframe arrives", () => {
  const sequencer = new FrameSequencer();
  assert.deepEqual(sequencer.push(frame(1), 1).ready, []);
  assert.deepEqual(sequencer.push(frame(0, true), 2).ready.map((item) => item.seq), [0, 1]);
  assert.deepEqual(sequencer.push(frame(2), 3).ready.map((item) => item.seq), [2]);
});

test("drops startup deltas older than the first keyframe", () => {
  const sequencer = new FrameSequencer();
  sequencer.push(frame(3), 1);
  sequencer.push(frame(4), 2);
  const result = sequencer.push(frame(5, true), 3);
  assert.deepEqual(result.ready.map((item) => item.seq), [5]);
  assert.equal(result.dropped, 2);
});

test("pre-sync expiry drops stale deltas and requests a keyframe", () => {
  const sequencer = new FrameSequencer(100);
  sequencer.push(frame(1), 0);
  const result = sequencer.expire(102);
  assert.equal(result.dropped, 1);
  assert.equal(result.requestKeyframe, true);
  assert.deepEqual(result.ready, []);
  assert.deepEqual(sequencer.push(frame(0, true), 103).ready.map((item) => item.seq), [0]);
});

test("a keyframe resynchronizes after a missing sequence", () => {
  const sequencer = new FrameSequencer();
  sequencer.push(frame(0, true), 0);
  const result = sequencer.push(frame(3, true), 3);
  assert.deepEqual(result.ready.map((item) => item.seq), [3]);
  assert.equal(result.dropped, 2);
});

test("expired gaps discard deltas until a keyframe arrives", () => {
  const sequencer = new FrameSequencer(100);
  sequencer.push(frame(0, true), 0);
  sequencer.push(frame(2), 10);
  const result = sequencer.expire(111);
  assert.deepEqual(result.ready, []);
  assert.equal(result.dropped, 2);
  assert.equal(result.requestKeyframe, true);
  assert.deepEqual(sequencer.push(frame(3, true), 112).ready.map((item) => item.seq), [3]);
});

test("reset streams advance lost sequences and request a keyframe", () => {
  const sequencer = new FrameSequencer();
  sequencer.push(frame(0, true), 0);
  const result = sequencer.markLost(1, 1);
  assert.equal(result.dropped, 1);
  assert.equal(result.requestKeyframe, true);
  assert.deepEqual(sequencer.push(frame(2), 2).ready, []);
});
