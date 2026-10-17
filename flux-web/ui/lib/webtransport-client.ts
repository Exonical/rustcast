import { h264CodecFromSps, isH264Keyframe } from "./h264";
import { browserFamily } from "./browser-family";
import { FrameSequencer, type EncodedFrame, type SequenceResult } from "./frame-sequencer";
import { ResolutionPaintTracker } from "./resolution-paint";
import { BufferedByteReader, encodeControlMessage, parseVideoFrameHeader, readControlMessage } from "./wt-framing";

export type ConnectionState = "disconnected" | "connecting" | "connected" | "failed";

export interface StreamStats {
  fps: number;
  width: number;
  height: number;
  bytesReceived: number;
  packetsLost: number;
  bitrate: number;
  mediaRttMs: number | null;
  controlRttMs: number | null;
  receiveToPaintMs: number | null;
  decodeMs: number | null;
  framesDropped: number;
}

export interface CursorBitmap {
  width: number;
  height: number;
  stride: number;
  format: number;
  pixels: string;
}

export interface CursorMetadata {
  position: [number, number] | null;
  hotspot: [number, number];
  bitmap: CursorBitmap | null;
}

export interface ResolutionStatus {
  state: "transitioning" | "recovering" | "succeeded" | "failed";
  width?: number;
  height?: number;
  encode_width?: number;
  encode_height?: number;
  error?: string;
}

export type InputEvent =
  | { Mouse: MouseEvent }
  | { Keyboard: KeyboardEvent }
  | { Gamepad: GamepadEvent };

export interface MouseEvent {
  Move?: { dx: number; dy: number };
  MoveAbsolute?: { x: number; y: number };
  ButtonDown?: { button: MouseButton };
  ButtonUp?: { button: MouseButton };
  Scroll?: { dx: number; dy: number };
}

export type MouseButton = "Left" | "Right" | "Middle" | "Back" | "Forward";

export interface KeyboardEvent {
  KeyDown?: { scan_code: number; key_code?: number; modifiers: number };
  KeyUp?: { scan_code: number; key_code?: number; modifiers: number };
}

export type GamepadEvent = Record<string, never>;

interface ControlMessage {
  type: string;
  data?: unknown;
}

interface DiscoveryResponse {
  host: string;
  port: number;
  certHashes: string[];
}

interface FrameMetadata {
  receivedAt: number;
  decodeStarted: number;
  keyframe: boolean;
  arrivalOrdinal: number;
  decodeMs?: number;
}

export class WebTransportClient {
  private transport: WebTransport | null = null;
  private controlWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private decoder: VideoDecoder | null = null;
  private decoderConfig: VideoDecoderConfig | null = null;
  private canvas: HTMLCanvasElement | null = null;
  private canvasContext: CanvasRenderingContext2D | null = null;
  private pendingPaint: { frame: VideoFrame; metadata?: FrameMetadata } | null = null;
  private paintScheduled = false;
  private readonly frameMetadata = new Map<number, FrameMetadata>();
  private sequencer = new FrameSequencer();
  private readonly resolutionPaintTracker = new ResolutionPaintTracker();
  private decodeChain: Promise<void> = Promise.resolve();
  private generation = 0;
  private intentionalClose = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private statsInterval: ReturnType<typeof setInterval> | null = null;
  private pingInterval: ReturnType<typeof setInterval> | null = null;
  private sequenceInterval: ReturnType<typeof setInterval> | null = null;
  private keyframeNudgeInterval: ReturnType<typeof setInterval> | null = null;
  private clientStatsInterval: ReturnType<typeof setInterval> | null = null;
  private lastKeyframeRequest = Number.NEGATIVE_INFINITY;
  private controlRttMs: number | null = null;
  private firstDecodedFrame = false;
  private waitingForKeyframe = true;
  sessionId: string | null = null;
  private bytesReceived = 0;
  private bytesWindow = 0;
  private framesWindow = 0;
  private framesDropped = 0;
  private packetsLost = 0;
  private receiveToPaintTotal = 0;
  private receiveToPaintCount = 0;
  private decodeTotal = 0;
  private decodeCount = 0;
  private lastStatsAt = 0;
  private decoderType: "hardware" | "software" | "unknown" = "unknown";
  private clientWindowFrames = 0;
  private clientWindowBytes = 0;
  private clientWindowElapsed = 0;
  private clientWindowDropped = 0;
  private clientWindowLastDropped = 0;
  private clientWindowDecodeTotal = 0;
  private clientWindowDecodeCount = 0;
  private clientWindowReceivePaintTotal = 0;
  private clientWindowReceivePaintCount = 0;

  onStateChange: ((state: ConnectionState) => void) | null = null;
  onStats: ((stats: StreamStats) => void) | null = null;
  onCursor: ((cursor: CursorMetadata) => void) | null = null;
  onResolutionStatus: ((status: ResolutionStatus) => void) | null = null;
  onKeyframePainted: (() => void) | null = null;
  onSessionId: ((id: string) => void) | null = null;
  onError: ((message: string) => void) | null = null;
  onSessionEnded: ((reason: string) => void) | null = null;

  constructor(
    private readonly machineId?: string,
    private resolution?: { width: number; height: number },
  ) {}

  get requestedResolution(): { width: number; height: number } | undefined {
    return this.resolution ? { ...this.resolution } : undefined;
  }

  attachCanvas(canvas: HTMLCanvasElement): void {
    this.canvas = canvas;
    this.canvasContext = canvas.getContext("2d", { alpha: false });
  }

  async connect(): Promise<void> {
    this.intentionalClose = false;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.closeCurrent();
    this.generation++;
    const generation = this.generation;
    this.sessionId = null;
    this.decodeChain = Promise.resolve();
    this.setState("connecting");
    this.sequencer = new FrameSequencer();
    this.resolutionPaintTracker.reset();
    this.firstDecodedFrame = false;
    this.waitingForKeyframe = true;
    this.resetStats();
    try {
      const response = await fetch("/api/webtransport");
      if (!response.ok) throw new Error(`WebTransport discovery failed: ${response.status}`);
      const discovery = await response.json() as DiscoveryResponse;
      const host = discovery.host || location.hostname;
      const urlHost = host.includes(":") && !host.startsWith("[") ? `[${host}]` : host;
      const url = new URL(`https://${urlHost}:${discovery.port}/wt`);
      if (this.machineId) url.searchParams.set("machine", this.machineId);
      if (typeof navigator !== "undefined" && navigator.userAgent) url.searchParams.set("user_agent", navigator.userAgent);
      if (this.resolution) {
        url.searchParams.set("width", String(this.resolution.width));
        url.searchParams.set("height", String(this.resolution.height));
      }
      const hashes = (discovery.certHashes ?? []).map((hash) => ({
        algorithm: "sha-256",
        value: Uint8Array.from(atob(hash), (character) => character.charCodeAt(0)),
      }));
      const transport = new WebTransport(url, hashes.length ? { serverCertificateHashes: hashes } : undefined);
      this.transport = transport;
      await transport.ready;
      if (generation !== this.generation) {
        transport.close({ closeCode: 0, reason: "superseded" });
        return;
      }
      const control = await transport.createBidirectionalStream();
      this.controlWriter = control.writable.getWriter();
      void this.readControl(control.readable, generation);
      this.sendControl("latency-ping", { t: performance.now() });
      void this.readIncomingStreams(transport, generation);
      this.startIntervals(transport, generation);
      this.setState("connected");
      void transport.closed.then(
        () => this.handleClosed(transport, generation),
        () => this.handleClosed(transport, generation),
      );
    } catch (error) {
      if (generation !== this.generation) return;
      console.error("WebTransport connection failed", error);
      this.closeCurrent();
      this.setState("failed");
      this.scheduleReconnect();
    }
  }

  sendInput(event: InputEvent): void {
    this.sendControl("input", event);
  }

  sendQuality(level: number, fps: number): void {
    this.sendControl("quality", { level: Math.round(level), fps: Math.round(fps) });
  }

  requestResolution(width: number, height: number): void {
    this.resolution = { width, height };
    this.sendControl("resolution", { width, height });
  }

  requestKeyframe(): void {
    this.requestKeyframeIfAllowed();
  }

  disconnect(): void {
    this.intentionalClose = true;
    this.generation++;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.closeCurrent();
    this.setState("disconnected");
  }

  private async readControl(stream: ReadableStream<Uint8Array>, generation: number): Promise<void> {
    const reader = new BufferedByteReader(stream.getReader());
    try {
      while (generation === this.generation) {
        const message = await readControlMessage(reader);
        this.handleControlMessage(message);
      }
    } catch (error) {
      if (generation === this.generation && !this.intentionalClose) {
        console.warn("WebTransport control stream ended", error);
        this.transport?.close({ closeCode: 1, reason: "control stream ended" });
      }
    }
  }

  private handleControlMessage(message: ControlMessage): void {
    if (message.type === "session") {
      const id = (message.data as { id?: unknown } | undefined)?.id;
      if (typeof id === "string") {
        this.sessionId = id;
        this.onSessionId?.(id);
      }
    } else if (message.type === "error") {
      const data = message.data as { error?: unknown } | string | undefined;
      this.intentionalClose = true;
      this.onError?.(typeof data === "string"
        ? data
        : typeof data?.error === "string" ? data.error : "The relay rejected the connection.");
      this.closeCurrent();
      this.setState("failed");
    } else if (message.type === "session-ended") {
      const data = message.data as { reason?: unknown } | string | undefined;
      const reason = typeof data === "string"
        ? data
        : typeof data?.reason === "string" ? data.reason : "Disconnected by the relay";
      this.intentionalClose = true;
      this.closeCurrent();
      this.setState("disconnected");
      this.onSessionEnded?.(reason);
    } else if (message.type === "cursor") this.onCursor?.(message.data as CursorMetadata);
    else if (message.type === "resolution-status") this.handleResolutionStatus(message.data as ResolutionStatus);
    else if (message.type === "latency-pong") {
      const sentAt = (message.data as { t?: unknown } | undefined)?.t;
      if (typeof sentAt === "number") this.controlRttMs = performance.now() - sentAt;
    }
  }

  private async readIncomingStreams(transport: WebTransport, generation: number): Promise<void> {
    const reader = transport.incomingUnidirectionalStreams.getReader();
    try {
      while (generation === this.generation) {
        const result = await reader.read();
        if (result.done) return;
        void this.readMetadataStream(result.value, generation);
      }
    } catch (error) {
      if (generation === this.generation && !this.intentionalClose) {
        console.warn("WebTransport incoming streams ended", error);
      }
    }
  }

  private async readMetadataStream(stream: ReadableStream<Uint8Array>, generation: number): Promise<void> {
    const reader = new BufferedByteReader(stream.getReader());
    let seq: number | null = null;
    try {
      const kind = (await reader.readExactly(1))[0];
      if (kind === 0x01) {
        const header = await reader.readExactly(17);
        const parsed = parseVideoFrameHeader(header);
        seq = parsed.seq;
        if (parsed.payloadLength > 64 * 1024 * 1024) throw new RangeError("video frame exceeds maximum size");
        const payload = await reader.readExactly(parsed.payloadLength);
        if (generation !== this.generation) return;
        this.bytesReceived += payload.byteLength;
        this.bytesWindow += payload.byteLength;
        const receiveTime = performance.now();
        const arrivalOrdinal = this.resolutionPaintTracker.onFrameReceived();
        const frame: EncodedFrame = {
          seq,
          keyframe: parsed.keyframe || isH264Keyframe(payload),
          data: payload,
          receivedAt: receiveTime,
          captureTimestampUs: parsed.captureTimestampUs,
          arrivalOrdinal,
        };
        this.sendControl("frame-ack", { seq, doneMs: receiveTime });
        this.acceptSequence(this.sequencer.push(frame, receiveTime));
      } else if (kind === 0x02 || kind === 0x03) {
        const lengthHeader = await reader.readExactly(4);
        const length = new DataView(lengthHeader.buffer, lengthHeader.byteOffset, 4).getUint32(0, false);
        if (length > 1024 * 1024) throw new RangeError("metadata exceeds maximum size");
        const payload = await reader.readExactly(length);
        const data: unknown = JSON.parse(new TextDecoder().decode(payload));
        if (generation !== this.generation) return;
        if (kind === 0x02) this.onCursor?.(data as CursorMetadata);
        else this.handleResolutionStatus(data as ResolutionStatus);
      }
    } catch (error) {
      if (seq !== null && generation === this.generation) {
        this.acceptSequence(this.sequencer.markLost(seq, performance.now()));
      } else {
        this.framesDropped++;
      }
      if (generation === this.generation && !this.intentionalClose) {
        console.debug("WebTransport stream was incomplete", error);
      }
    } finally {
      await stream.cancel().catch(() => {});
    }
  }

  private acceptSequence(result: SequenceResult): void {
    this.framesDropped += result.dropped;
    if (result.requestKeyframe) {
      this.waitingForKeyframe = true;
      this.requestKeyframeIfAllowed();
    }
    for (const frame of result.ready) {
      if (this.waitingForKeyframe && !frame.keyframe) {
        this.framesDropped++;
        this.requestKeyframeIfAllowed();
        continue;
      }
      if (this.decoder && this.decoder.decodeQueueSize > 2 && !frame.keyframe) {
        this.framesDropped++;
        this.waitingForKeyframe = true;
        this.requestKeyframeIfAllowed();
        continue;
      }
      if (frame.keyframe) this.waitingForKeyframe = false;
      const generation = this.generation;
      this.decodeChain = this.decodeChain.then(async () => {
        if (generation === this.generation) await this.decodeFrame(frame, generation);
      }).catch((error: unknown) => {
        if (generation === this.generation) this.handleDecoderError(error);
      });
    }
  }

  private async decodeFrame(frame: EncodedFrame, generation: number): Promise<void> {
    if (generation !== this.generation) return;
    if (!this.decoder && !frame.keyframe) {
      this.requestKeyframeIfAllowed();
      return;
    }
    if (frame.keyframe) {
      const codec = h264CodecFromSps(frame.data);
      if (codec && (!this.decoderConfig || this.decoderConfig.codec !== codec)) {
        try {
          await this.configureDecoder(codec);
          if (generation !== this.generation) return;
        } catch {
          this.waitingForKeyframe = true;
          this.requestKeyframeIfAllowed();
          return;
        }
      }
    }
    if (!this.decoder || this.decoder.state !== "configured") {
      this.requestKeyframeIfAllowed();
      return;
    }
    const timestamp = frame.seq;
    const started = performance.now();
    this.frameMetadata.set(timestamp, {
      receivedAt: frame.receivedAt,
      decodeStarted: started,
      keyframe: frame.keyframe,
      arrivalOrdinal: frame.arrivalOrdinal ?? 0,
    });
    try {
      this.decoder.decode(new EncodedVideoChunk({
        type: frame.keyframe ? "key" : "delta",
        timestamp,
        data: frame.data,
      }));
    } catch (error) {
      this.frameMetadata.delete(timestamp);
      this.handleDecoderError(error);
    }
  }

  private async configureDecoder(codec: string): Promise<void> {
    this.decoder?.close();
    this.frameMetadata.clear();
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (this.decoder === decoder) this.handleDecodedFrame(frame);
        else frame.close();
      },
      error: (error) => {
        if (this.decoder === decoder) this.handleDecoderError(error);
      },
    });
    this.decoder = decoder;
    const base: VideoDecoderConfig = { codec, optimizeForLatency: true };
    const candidate: VideoDecoderConfig = { ...base, hardwareAcceleration: "prefer-hardware" };
    try {
      const preferred = await VideoDecoder.isConfigSupported(candidate);
      if (this.decoder !== decoder) return;
      const supported = preferred.supported ? preferred : await VideoDecoder.isConfigSupported(base);
      if (this.decoder !== decoder) return;
      if (!supported.supported || !supported.config) throw new Error(`Unsupported H.264 configuration ${codec}`);
      decoder.configure(supported.config);
      this.decoderConfig = supported.config;
      this.decoderType = preferred.supported ? "hardware" : "software";
    } catch (error) {
      if (this.decoder === decoder) this.handleDecoderError(error);
      throw error;
    }
  }

  private handleDecodedFrame(frame: VideoFrame): void {
    const metadata = this.frameMetadata.get(frame.timestamp);
    this.frameMetadata.delete(frame.timestamp);
    if (metadata) metadata.decodeMs = performance.now() - metadata.decodeStarted;
    if (this.pendingPaint) {
      this.pendingPaint.frame.close();
      this.framesDropped++;
    }
    this.pendingPaint = { frame, metadata };
    if (!this.paintScheduled) {
      this.paintScheduled = true;
      requestAnimationFrame(() => this.paintPendingFrame());
    }
  }

  private paintPendingFrame(): void {
    this.paintScheduled = false;
    const pending = this.pendingPaint;
    this.pendingPaint = null;
    if (!pending) return;
    const { frame, metadata } = pending;
    const canvas = this.canvas;
    const context = this.canvasContext;
    if (canvas && context) {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      context.drawImage(frame, 0, 0, canvas.width, canvas.height);
      if (metadata && this.resolutionPaintTracker.onKeyframePainted(metadata.arrivalOrdinal, metadata.keyframe)) {
        this.onKeyframePainted?.();
      }
      if (metadata) {
        const now = performance.now();
        this.receiveToPaintTotal += now - metadata.receivedAt;
        this.receiveToPaintCount++;
        this.decodeTotal += metadata.decodeMs ?? 0;
        this.decodeCount++;
      }
      this.framesWindow++;
      this.firstDecodedFrame = true;
    }
    frame.close();
  }

  private handleDecoderError(error: unknown): void {
    console.warn("WebCodecs decoder failed", error);
    if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
    this.decoderConfig = null;
    this.decoderType = "unknown";
    this.frameMetadata.clear();
    this.waitingForKeyframe = true;
    this.framesDropped++;
    this.requestKeyframeIfAllowed();
  }

  private handleResolutionStatus(status: ResolutionStatus): void {
    this.resolutionPaintTracker.onResolutionStatus(status.state);
    this.onResolutionStatus?.(status);
  }

  private startIntervals(transport: WebTransport, generation: number): void {
    this.lastStatsAt = performance.now();
    this.statsInterval = setInterval(() => void this.publishStats(transport, generation), 1000);
    this.pingInterval = setInterval(() => this.sendControl("latency-ping", { t: performance.now() }), 1000);
    this.sequenceInterval = setInterval(() => this.acceptSequence(this.sequencer.expire(performance.now())), 25);
    this.keyframeNudgeInterval = setInterval(() => {
      if (!this.firstDecodedFrame) this.requestKeyframeIfAllowed();
    }, 1000);
    this.clientStatsInterval = setInterval(() => this.publishClientStats(), 5000);
  }

  private async publishStats(transport: WebTransport, generation: number): Promise<void> {
    const now = performance.now();
    const elapsed = Math.max(1, now - this.lastStatsAt);
    let mediaRttMs: number | null = null;
    try {
      const getStats = (transport as WebTransport & {
        getStats?: () => Promise<{ smoothedRtt?: number; packetsLost?: number }>;
      }).getStats;
      const stats = await getStats?.call(transport);
      if (stats && typeof stats.smoothedRtt === "number") mediaRttMs = stats.smoothedRtt * 1000;
      if (stats && typeof stats.packetsLost === "number") this.packetsLost = stats.packetsLost;
    } catch {}
    if (generation !== this.generation) return;
    const result: StreamStats = {
      fps: (this.framesWindow * 1000) / elapsed,
      width: this.canvas?.width ?? 0,
      height: this.canvas?.height ?? 0,
      bytesReceived: this.bytesReceived,
      packetsLost: this.packetsLost,
      bitrate: Math.round((this.bytesWindow * 8 * 1000) / elapsed / 1000),
      mediaRttMs,
      controlRttMs: this.controlRttMs,
      receiveToPaintMs: this.receiveToPaintCount ? this.receiveToPaintTotal / this.receiveToPaintCount : null,
      decodeMs: this.decodeCount ? this.decodeTotal / this.decodeCount : null,
      framesDropped: this.framesDropped,
    };
    this.onStats?.(result);
    this.clientWindowFrames += this.framesWindow;
    this.clientWindowBytes += this.bytesWindow;
    this.clientWindowElapsed += elapsed;
    this.clientWindowDropped += Math.max(0, this.framesDropped - this.clientWindowLastDropped);
    this.clientWindowLastDropped = this.framesDropped;
    this.clientWindowDecodeTotal += this.decodeTotal;
    this.clientWindowDecodeCount += this.decodeCount;
    this.clientWindowReceivePaintTotal += this.receiveToPaintTotal;
    this.clientWindowReceivePaintCount += this.receiveToPaintCount;
    this.framesWindow = 0;
    this.bytesWindow = 0;
    this.receiveToPaintTotal = 0;
    this.receiveToPaintCount = 0;
    this.decodeTotal = 0;
    this.decodeCount = 0;
    this.lastStatsAt = now;
  }

  private publishClientStats(): void {
    const elapsed = Math.max(1, this.clientWindowElapsed);
    this.sendControl("client-stats", {
      fps: this.clientWindowFrames * 1000 / elapsed,
      decodeMs: this.clientWindowDecodeCount ? this.clientWindowDecodeTotal / this.clientWindowDecodeCount : 0,
      receiveToPaintMs: this.clientWindowReceivePaintCount
        ? this.clientWindowReceivePaintTotal / this.clientWindowReceivePaintCount
        : 0,
      bitrateKbps: this.clientWindowBytes * 8 / elapsed,
      framesDropped: this.clientWindowDropped,
      decoder: this.decoderType,
      browser: browserFamily(navigator.userAgent),
    });
    this.clientWindowFrames = 0;
    this.clientWindowBytes = 0;
    this.clientWindowElapsed = 0;
    this.clientWindowDropped = 0;
    this.clientWindowDecodeTotal = 0;
    this.clientWindowDecodeCount = 0;
    this.clientWindowReceivePaintTotal = 0;
    this.clientWindowReceivePaintCount = 0;
  }

  private sendControl(type: string, data?: unknown): void {
    const writer = this.controlWriter;
    if (!writer) return;
    void writer.write(encodeControlMessage(type, data)).catch((error: unknown) => {
      console.warn("WebTransport control write failed", error);
      this.transport?.close({ closeCode: 1, reason: "control write failed" });
    });
  }

  private requestKeyframeIfAllowed(): void {
    const now = performance.now();
    if (now - this.lastKeyframeRequest < 500) return;
    this.lastKeyframeRequest = now;
    this.sendControl("keyframe-request");
  }

  private handleClosed(transport: WebTransport, generation: number): void {
    if (generation !== this.generation || transport !== this.transport || this.intentionalClose) return;
    this.closeCurrent();
    this.setState("disconnected");
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.intentionalClose || this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      void this.connect();
    }, 2000);
  }

  private closeCurrent(): void {
    if (this.statsInterval) clearInterval(this.statsInterval);
    if (this.pingInterval) clearInterval(this.pingInterval);
    if (this.sequenceInterval) clearInterval(this.sequenceInterval);
    if (this.keyframeNudgeInterval) clearInterval(this.keyframeNudgeInterval);
    if (this.clientStatsInterval) clearInterval(this.clientStatsInterval);
    this.statsInterval = null;
    this.pingInterval = null;
    this.sequenceInterval = null;
    this.keyframeNudgeInterval = null;
    this.clientStatsInterval = null;
    this.controlWriter?.close().catch(() => {});
    this.controlWriter = null;
    if (this.pendingPaint) this.pendingPaint.frame.close();
    this.pendingPaint = null;
    if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
    this.decoderConfig = null;
    this.frameMetadata.clear();
    this.transport?.close({ closeCode: 0, reason: "client closing" });
    this.transport = null;
  }

  private resetStats(): void {
    this.bytesReceived = 0;
    this.bytesWindow = 0;
    this.framesWindow = 0;
    this.framesDropped = 0;
    this.decoderType = "unknown";
    this.clientWindowFrames = 0;
    this.clientWindowBytes = 0;
    this.clientWindowElapsed = 0;
    this.clientWindowDropped = 0;
    this.clientWindowLastDropped = 0;
    this.clientWindowDecodeTotal = 0;
    this.clientWindowDecodeCount = 0;
    this.clientWindowReceivePaintTotal = 0;
    this.clientWindowReceivePaintCount = 0;
    this.packetsLost = 0;
    this.receiveToPaintTotal = 0;
    this.receiveToPaintCount = 0;
    this.decodeTotal = 0;
    this.decodeCount = 0;
  }

  private setState(state: ConnectionState): void {
    this.onStateChange?.(state);
  }
}
