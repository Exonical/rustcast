import assert from "node:assert/strict";
import test from "node:test";
import { DropCounter, DropVisibilityPolicy } from "./frame-drops.js";

test("counts drops cumulatively and within the current window", () => {
  const drops = new DropCounter();
  drops.add("lost");
  drops.add("lost", 2);
  drops.add("awaiting_keyframe", 2);
  drops.add("decoder_error", 3);
  drops.add("decode_backlog", 0);
  drops.add("not_painted", -1);

  assert.equal(drops.total, 8);
  assert.deepEqual(drops.takeWindow(), {
    not_painted: 0,
    awaiting_keyframe: 2,
    decode_backlog: 0,
    lost: 3,
    decoder_error: 3,
  });
  assert.equal(drops.total, 8);
  assert.deepEqual(drops.takeWindow(), {
    not_painted: 0,
    awaiting_keyframe: 0,
    decode_backlog: 0,
    lost: 0,
    decoder_error: 0,
  });
});

test("reset clears cumulative and window drop counts", () => {
  const drops = new DropCounter();
  drops.add("awaiting_keyframe", 4);
  drops.reset();
  assert.equal(drops.total, 0);
  assert.deepEqual(drops.takeWindow(), {
    not_painted: 0,
    awaiting_keyframe: 0,
    decode_backlog: 0,
    lost: 0,
    decoder_error: 0,
  });
});

test("drop visibility policy suppresses hidden drops and resume keyframe waits", () => {
  const visibility = new DropVisibilityPolicy();
  assert.equal(visibility.suppresses("awaiting_keyframe"), false);
  assert.equal(visibility.setHidden(true), false);
  assert.equal(visibility.isHidden, true);
  assert.equal(visibility.suppresses("lost"), true);
  assert.equal(visibility.setHidden(false), true);
  assert.equal(visibility.isHidden, false);
  assert.equal(visibility.suppresses("lost"), false);
  assert.equal(visibility.suppresses("awaiting_keyframe"), true);
  visibility.acceptKeyframe();
  assert.equal(visibility.suppresses("awaiting_keyframe"), false);
});
