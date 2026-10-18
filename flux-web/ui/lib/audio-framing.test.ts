import assert from "node:assert/strict";
import test from "node:test";
import { MAX_AUDIO_PAYLOAD, parseAudioHeader } from "./audio-framing.js";

test("audio headers parse their fields", () => {
  const header = new Uint8Array(16);
  const view = new DataView(header.buffer);
  view.setBigUint64(0, BigInt("0x0102030405060708"), false);
  view.setUint32(8, 0x12345678, false);
  view.setUint32(12, 512, false);
  assert.deepEqual(parseAudioHeader(header), {
    tsMicros: BigInt("0x0102030405060708"),
    seq: 0x12345678,
    payloadLength: 512,
  });
});

test("audio header parser validates its length", () => {
  assert.throws(() => parseAudioHeader(new Uint8Array(15)), RangeError);
  assert.throws(() => parseAudioHeader(new Uint8Array(17)), RangeError);
});

test("audio header parser rejects oversized payloads", () => {
  const header = new Uint8Array(16);
  new DataView(header.buffer).setUint32(12, MAX_AUDIO_PAYLOAD + 1, false);
  assert.throws(() => parseAudioHeader(header), RangeError);
});
