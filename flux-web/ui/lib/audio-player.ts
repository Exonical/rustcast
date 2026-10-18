export interface AudioPacket {
  seq: number;
  tsMicros: bigint | number;
  data: Uint8Array;
}

export interface AudioPlayerStats {
  buffered_ms: number;
  underruns: number;
  dropped: number;
}

const MAX_DECODE_QUEUE = 8;

/// Decodes Opus packets (WebCodecs) into a stereo ring buffer driven by an
/// AudioWorklet (`/audio-worklet.js`). Created lazily on connect; the
/// AudioContext stays suspended until `resume()` is called from a user
/// gesture.
export class AudioPlayer {
  static isSupported(): boolean {
    return typeof AudioDecoder !== "undefined" && typeof AudioContext !== "undefined";
  }

  private ctx: AudioContext | null = null;
  private decoder: AudioDecoder | null = null;
  private worklet: AudioWorkletNode | null = null;
  private lastSeq: number | null = null;
  private statsSnapshot: AudioPlayerStats = { buffered_ms: 0, underruns: 0, dropped: 0 };
  private errorLogged = false;

  async start(): Promise<void> {
    if (this.ctx) return;
    const ctx = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
    this.ctx = ctx;
    await ctx.audioWorklet.addModule("/audio-worklet.js");
    const worklet = new AudioWorkletNode(ctx, "flux-audio-sink", { outputChannelCount: [2] });
    worklet.port.onmessage = (event: MessageEvent) => {
      if (event.data?.type === "stats") {
        this.statsSnapshot = {
          buffered_ms: event.data.buffered_ms ?? 0,
          underruns: event.data.underruns ?? 0,
          dropped: event.data.dropped ?? 0,
        };
      }
    };
    worklet.connect(ctx.destination);
    this.worklet = worklet;
    this.decoder = this.createDecoder();
  }

  private createDecoder(): AudioDecoder {
    const decoder = new AudioDecoder({
      output: (frame) => this.handleDecoded(frame),
      error: (error) => this.handleDecoderError(error),
    });
    decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
    return decoder;
  }

  private handleDecoded(frame: AudioData): void {
    const channels = frame.numberOfChannels;
    const frames = frame.numberOfFrames;
    const planes: Float32Array[] = [];
    if (frame.format === "f32-planar" || frame.format === "f32") {
      for (let channel = 0; channel < Math.min(channels, 2); channel++) {
        const plane = new Float32Array(frames);
        frame.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
        planes.push(plane);
      }
    } else {
      // Interleaved output: deinterleave into per-channel planes.
      const interleaved = new Float32Array(frames * channels);
      frame.copyTo(interleaved, { planeIndex: 0, format: "f32" });
      const left = new Float32Array(frames);
      const right = new Float32Array(frames);
      for (let i = 0; i < frames; i++) {
        left[i] = interleaved[i * channels];
        right[i] = channels > 1 ? interleaved[i * channels + 1] : interleaved[i * channels];
      }
      planes.push(left, right);
    }
    frame.close();
    if (planes.length === 1) planes.push(planes[0].slice());
    if (planes.length >= 2 && this.worklet) {
      const buffers = planes.slice(0, 2).map((plane) => plane.buffer);
      this.worklet.port.postMessage({ type: "push", planes: planes.slice(0, 2) }, buffers);
    }
  }

  /// Sequence-gap policy shared by tests: gaps under 10 packets play through
  /// (the worklet underruns to silence); bigger jumps reset the decoder and
  /// worklet so stale audio can't smear.
  static needsReset(previous: number | null, seq: number): boolean {
    if (previous === null) return false;
    const gap = seq - previous;
    return gap >= 10 || gap < -100;
  }

  push(packet: AudioPacket): void {
    const decoder = this.decoder;
    const worklet = this.worklet;
    if (!decoder || !worklet || decoder.state === "closed") return;
    if (AudioPlayer.needsReset(this.lastSeq, packet.seq)) {
      if (decoder.state === "configured") decoder.reset();
      decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
      worklet.port.postMessage({ type: "reset" });
    }
    this.lastSeq = packet.seq;
    if (decoder.decodeQueueSize > MAX_DECODE_QUEUE) return;
    decoder.decode(new EncodedAudioChunk({
      type: "key",
      timestamp: Number(packet.tsMicros),
      data: packet.data,
    }));
  }

  setMuted(muted: boolean): void {
    this.worklet?.port.postMessage({ type: "mute", muted });
  }

  reset(): void {
    this.worklet?.port.postMessage({ type: "reset" });
  }

  async resume(): Promise<void> {
    await this.ctx?.resume();
  }

  stats(): AudioPlayerStats {
    return { ...this.statsSnapshot };
  }

  stop(): void {
    if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
    this.worklet?.disconnect();
    this.worklet = null;
    void this.ctx?.close();
    this.ctx = null;
    this.lastSeq = null;
  }

  private handleDecoderError(error: DOMException): void {
    if (!this.errorLogged) {
      console.warn("AudioDecoder failed", error);
      this.errorLogged = true;
    }
    if (!this.decoder || this.decoder.state === "closed") return;
    this.decoder.reset();
    this.decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
    this.worklet?.port.postMessage({ type: "reset" });
  }
}
