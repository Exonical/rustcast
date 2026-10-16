export interface EncodedFrame {
  seq: number;
  keyframe: boolean;
  data: Uint8Array;
  receivedAt: number;
  captureTimestampUs?: bigint;
}

export interface SequenceResult {
  ready: EncodedFrame[];
  dropped: number;
  requestKeyframe: boolean;
}

export class FrameSequencer {
  private expected: number | null = null;
  private waitingForKeyframe = true;
  private gapSince: number | null = null;
  private readonly buffered = new Map<number, EncodedFrame>();

  constructor(private readonly gapTimeoutMs = 100, private readonly maxBufferedFrames = 256) {}

  push(frame: EncodedFrame, now: number): SequenceResult {
    const result: SequenceResult = { ready: [], dropped: 0, requestKeyframe: false };
    if (this.expected === null) this.expected = frame.seq;
    const distance = sequenceDistance(frame.seq, this.expected);
    if (distance >= 0x80000000) {
      result.dropped++;
      return result;
    }
    if (frame.keyframe) {
      for (const seq of this.buffered.keys()) {
        if (seq === frame.seq || sequenceDistance(seq, frame.seq) >= 0x80000000) {
          this.buffered.delete(seq);
          result.dropped++;
        }
      }
      if (distance > 0) result.dropped += distance;
      this.expected = frame.seq;
      this.gapSince = null;
      this.waitingForKeyframe = false;
    } else if (distance > 0) {
      if (this.buffered.size >= this.maxBufferedFrames) {
        result.dropped++;
        result.requestKeyframe = true;
        return result;
      }
      this.buffered.set(frame.seq, frame);
      this.gapSince ??= now;
      return result;
    }
    this.consume(frame, result);
    this.drain(result, now);
    return result;
  }

  markLost(seq: number, now: number): SequenceResult {
    const result: SequenceResult = { ready: [], dropped: 0, requestKeyframe: false };
    if (this.expected === null) {
      this.expected = (seq + 1) >>> 0;
      this.waitingForKeyframe = true;
      result.dropped = 1;
      result.requestKeyframe = true;
      return result;
    }
    const distance = sequenceDistance(seq, this.expected);
    if (distance >= 0x80000000) return result;
    if (distance > 0) {
      this.gapSince ??= now;
      return result;
    }
    this.expected = (this.expected + 1) >>> 0;
    result.dropped++;
    result.requestKeyframe = true;
    this.waitingForKeyframe = true;
    this.drain(result, now);
    return result;
  }

  expire(now: number): SequenceResult {
    const result: SequenceResult = { ready: [], dropped: 0, requestKeyframe: false };
    if (this.gapSince === null || now - this.gapSince < this.gapTimeoutMs) return result;
    while (this.buffered.size) {
      const next = this.nextBuffered();
      if (next === null) break;
      const distance = this.expected === null ? 0 : sequenceDistance(next.seq, this.expected);
      if (distance < 0x80000000 && distance > 0) {
        result.dropped += distance;
        this.expected = next.seq;
        this.waitingForKeyframe = true;
        result.requestKeyframe = true;
      }
      this.buffered.delete(next.seq);
      if (this.waitingForKeyframe && !next.keyframe) {
        result.dropped++;
        this.expected = (next.seq + 1) >>> 0;
        result.requestKeyframe = true;
        continue;
      }
      this.consume(next, result);
      this.drain(result, now);
    }
    this.gapSince = this.buffered.size ? now : null;
    if (result.dropped) result.requestKeyframe = true;
    return result;
  }

  private consume(frame: EncodedFrame, result: SequenceResult): void {
    if (this.waitingForKeyframe && !frame.keyframe) {
      result.dropped++;
      result.requestKeyframe = true;
    } else {
      if (frame.keyframe) this.waitingForKeyframe = false;
      result.ready.push(frame);
    }
    this.expected = (frame.seq + 1) >>> 0;
  }

  private drain(result: SequenceResult, now: number): void {
    while (this.expected !== null) {
      const next = this.buffered.get(this.expected);
      if (!next) break;
      this.buffered.delete(this.expected);
      this.consume(next, result);
    }
    this.gapSince = this.buffered.size ? (this.gapSince ?? now) : null;
  }

  private nextBuffered(): EncodedFrame | null {
    if (this.expected === null) return null;
    let next: EncodedFrame | null = null;
    let nearest = 0xffffffff;
    for (const frame of this.buffered.values()) {
      const distance = sequenceDistance(frame.seq, this.expected);
      if (distance < nearest) {
        nearest = distance;
        next = frame;
      }
    }
    return next;
  }
}

function sequenceDistance(seq: number, expected: number): number {
  return (seq - expected) >>> 0;
}
