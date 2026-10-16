import assert from "node:assert/strict";
import test from "node:test";
import { annexBNalUnits, h264CodecFromSps, isH264Keyframe } from "./h264.js";

test("Annex-B parser handles three- and four-byte start codes", () => {
  const data = new Uint8Array([0, 0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 1, 0x65, 1, 2]);
  assert.deepEqual(annexBNalUnits(data).map((nal) => nal.type), [7, 5]);
});

test("SPS codec parsing removes emulation-prevention bytes", () => {
  const sps = new Uint8Array([0, 0, 0, 1, 0x67, 0x64, 0, 0, 3, 0x1f]);
  assert.equal(h264CodecFromSps(sps), "avc1.640000");
});

test("keyframe detection requires an IDR NAL unit", () => {
  assert.equal(isH264Keyframe(new Uint8Array([0, 0, 1, 0x41, 1])), false);
  assert.equal(isH264Keyframe(new Uint8Array([0, 0, 1, 0x65, 1])), true);
});
