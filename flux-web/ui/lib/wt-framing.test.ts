import assert from "node:assert/strict";
import test from "node:test";
import {
  BufferedByteReader,
  encodeControlMessage,
  parseVideoFrameHeader,
  readControlMessage,
  type ByteReader,
} from "./wt-framing.js";

class ChunkReader implements ByteReader {
  private index = 0;

  constructor(private readonly chunks: Uint8Array[]) {}

  async read(): Promise<{ done: boolean; value?: Uint8Array }> {
    const value = this.chunks[this.index++];
    return value ? { done: false, value } : { done: true };
  }
}

test("buffered reader joins partial reads and preserves bytes", async () => {
  const reader = new BufferedByteReader(new ChunkReader([new Uint8Array([1, 2]), new Uint8Array([3, 4, 5])]));
  assert.deepEqual(await reader.readExactly(3), new Uint8Array([1, 2, 3]));
  assert.deepEqual(await reader.readExactly(2), new Uint8Array([4, 5]));
});

test("control framing round trips messages split across reads", async () => {
  const encoded = encodeControlMessage("input", { key: "value" });
  const reader = new BufferedByteReader(new ChunkReader([
    encoded.subarray(0, 2),
    encoded.subarray(2, 7),
    encoded.subarray(7),
  ]));
  assert.deepEqual(await readControlMessage(reader), { type: "input", data: { key: "value" } });
});

test("truncated control frames are rejected", async () => {
  const reader = new BufferedByteReader(new ChunkReader([new Uint8Array([0, 0, 0, 3, 1])]));
  await assert.rejects(readControlMessage(reader), /stream ended before frame completed/);
});

test("video frame headers parse their fields", () => {
  const header = new Uint8Array(17);
  const view = new DataView(header.buffer);
  view.setUint32(0, 0x12345678, false);
  view.setBigUint64(4, BigInt("0x0102030405060708"), false);
  header[12] = 1;
  view.setUint32(13, 4097, false);
  assert.deepEqual(parseVideoFrameHeader(header), {
    seq: 0x12345678,
    captureTimestampUs: BigInt("0x0102030405060708"),
    keyframe: true,
    payloadLength: 4097,
  });
});

test("video frame header parser validates its length", () => {
  assert.throws(() => parseVideoFrameHeader(new Uint8Array(16)), RangeError);
});
