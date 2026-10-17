import assert from "node:assert/strict";
import test from "node:test";
import { ResolutionPaintTracker } from "./resolution-paint.js";

test("requires a keyframe received after the succeeded status", () => {
  const tracker = new ResolutionPaintTracker();
  const earlierKeyframe = tracker.onFrameReceived();
  tracker.onResolutionStatus("succeeded");

  assert.equal(tracker.onKeyframePainted(earlierKeyframe, true), false);
  const laterDelta = tracker.onFrameReceived();
  assert.equal(tracker.onKeyframePainted(laterDelta, false), false);
  const laterKeyframe = tracker.onFrameReceived();
  assert.equal(tracker.onKeyframePainted(laterKeyframe, true), true);
  assert.equal(tracker.onKeyframePainted(laterKeyframe + 1, true), false);
});

test("a new non-success status cancels the pending keyframe", () => {
  const tracker = new ResolutionPaintTracker();
  tracker.onResolutionStatus("succeeded");
  const frame = tracker.onFrameReceived();
  tracker.onResolutionStatus("recovering");

  assert.equal(tracker.onKeyframePainted(frame, true), false);
});
