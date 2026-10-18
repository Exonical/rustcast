export interface AudioHeader {
  tsMicros: bigint;
  seq: number;
  payloadLength: number;
}

export const MAX_AUDIO_PAYLOAD = 16 * 1024;

/// Parses the 16 bytes after the 0x04 kind byte on a WebTransport audio
/// uni-stream: [ts u64 BE][seq u32 BE][len u32 BE].
export function parseAudioHeader(header: Uint8Array): AudioHeader {
  if (header.byteLength !== 16) throw new RangeError("invalid audio header length");
  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  const payloadLength = view.getUint32(12, false);
  if (payloadLength > MAX_AUDIO_PAYLOAD) throw new RangeError("audio payload exceeds maximum size");
  return {
    tsMicros: view.getBigUint64(0, false),
    seq: view.getUint32(8, false),
    payloadLength,
  };
}
