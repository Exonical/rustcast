export interface ByteReader {
  read(): Promise<{ done: boolean; value?: Uint8Array }>;
}

export class BufferedByteReader {
  private chunks: Uint8Array[] = [];
  private bufferedBytes = 0;
  private ended = false;

  constructor(private readonly reader: ByteReader) {}

  async readExactly(length: number): Promise<Uint8Array> {
    if (!Number.isSafeInteger(length) || length < 0) throw new RangeError("invalid read length");
    while (this.bufferedBytes < length && !this.ended) {
      const next = await this.reader.read();
      if (next.done) {
        this.ended = true;
        break;
      }
      if (next.value?.byteLength) {
        this.chunks.push(next.value);
        this.bufferedBytes += next.value.byteLength;
      }
    }
    if (this.bufferedBytes < length) throw new RangeError("stream ended before frame completed");

    const result = new Uint8Array(length);
    let copied = 0;
    while (copied < length) {
      const first = this.chunks[0];
      const count = Math.min(first.byteLength, length - copied);
      result.set(first.subarray(0, count), copied);
      copied += count;
      this.bufferedBytes -= count;
      if (count === first.byteLength) this.chunks.shift();
      else this.chunks[0] = first.subarray(count);
    }
    return result;
  }
}

export interface VideoFrameHeader {
  seq: number;
  captureTimestampUs: bigint;
  keyframe: boolean;
  payloadLength: number;
}

export function parseVideoFrameHeader(header: Uint8Array): VideoFrameHeader {
  if (header.byteLength !== 17) throw new RangeError("invalid video frame header length");
  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  return {
    seq: view.getUint32(0, false),
    captureTimestampUs: view.getBigUint64(4, false),
    keyframe: (header[12] & 1) !== 0,
    payloadLength: view.getUint32(13, false),
  };
}

export function encodeControlMessage(type: string, data?: unknown): Uint8Array {
  const payload = new TextEncoder().encode(JSON.stringify({ type, ...(data === undefined ? {} : { data }) }));
  const framed = new Uint8Array(4 + payload.byteLength);
  new DataView(framed.buffer).setUint32(0, payload.byteLength, false);
  framed.set(payload, 4);
  return framed;
}

export async function readControlMessage(reader: BufferedByteReader): Promise<{ type: string; data?: unknown }> {
  const header = await reader.readExactly(4);
  const length = new DataView(header.buffer, header.byteOffset, header.byteLength).getUint32(0, false);
  if (length === 0 || length > 1024 * 1024) throw new RangeError("invalid control message length");
  const payload = await reader.readExactly(length);
  const parsed: unknown = JSON.parse(new TextDecoder().decode(payload));
  if (!parsed || typeof parsed !== "object" || typeof (parsed as { type?: unknown }).type !== "string") {
    throw new TypeError("invalid control message");
  }
  return parsed as { type: string; data?: unknown };
}
