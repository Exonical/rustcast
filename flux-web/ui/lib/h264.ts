export interface H264NalUnit {
  type: number;
  data: Uint8Array;
}

export function annexBNalUnits(data: Uint8Array): H264NalUnit[] {
  const units: H264NalUnit[] = [];
  let start = findStartCode(data, 0);
  if (!start) return data.byteLength ? [{ type: data[0] & 0x1f, data }] : [];
  while (start) {
    const payloadStart = start.index + start.length;
    const next = findStartCode(data, payloadStart);
    let payloadEnd = next?.index ?? data.byteLength;
    while (payloadEnd > payloadStart && data[payloadEnd - 1] === 0) payloadEnd--;
    if (payloadEnd > payloadStart) {
      const nal = data.subarray(payloadStart, payloadEnd);
      units.push({ type: nal[0] & 0x1f, data: nal });
    }
    start = next;
  }
  return units;
}

export function isH264Keyframe(data: Uint8Array): boolean {
  return annexBNalUnits(data).some((nal) => nal.type === 5);
}

export function h264CodecFromSps(data: Uint8Array): string | null {
  const sps = annexBNalUnits(data).find((nal) => nal.type === 7)?.data;
  if (!sps || sps.byteLength < 4) return null;
  const rbsp = removeEmulationPrevention(sps.subarray(1));
  if (rbsp.byteLength < 3) return null;
  return `avc1.${[rbsp[0], rbsp[1], rbsp[2]].map((value) => value.toString(16).padStart(2, "0")).join("").toUpperCase()}`;
}

function findStartCode(data: Uint8Array, from: number): { index: number; length: number } | null {
  for (let index = from; index + 3 <= data.byteLength; index++) {
    if (data[index] !== 0 || data[index + 1] !== 0) continue;
    if (data[index + 2] === 1) return { index, length: 3 };
    if (index + 4 <= data.byteLength && data[index + 2] === 0 && data[index + 3] === 1) {
      return { index, length: 4 };
    }
  }
  return null;
}

function removeEmulationPrevention(data: Uint8Array): Uint8Array {
  const result = new Uint8Array(data.byteLength);
  let length = 0;
  let zeros = 0;
  for (const byte of data) {
    if (zeros >= 2 && byte === 3) {
      zeros = 0;
      continue;
    }
    result[length++] = byte;
    zeros = byte === 0 ? zeros + 1 : 0;
  }
  return result.subarray(0, length);
}
