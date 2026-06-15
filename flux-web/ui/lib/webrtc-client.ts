/**
 * WebRTC Signaling Client
 *
 * Connects to the Go relay via WebSocket, establishes a WebRTC
 * peer connection, and provides the incoming video MediaStream.
 */

export type ConnectionState = "disconnected" | "connecting" | "connected" | "failed";

export interface WebRTCStats {
  fps: number;
  width: number;
  height: number;
  bytesReceived: number;
  packetsLost: number;
  jitter: number;
  bitrate: number;
}

interface WSMessage {
  type: string;
  data: unknown;
}

// Input event types matching Rust flux-input
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

export interface GamepadEvent {
  // TODO: Define gamepad event structure
}

export class WebRTCClient {
  private ws: WebSocket | null = null;
  private pc: RTCPeerConnection | null = null;
  private pendingCandidates: RTCIceCandidateInit[] = [];
  private remoteDescSet = false;
  private lastBytesReceived = 0;
  private lastStatsTimestamp = 0;
  private statsInterval: ReturnType<typeof setInterval> | null = null;

  onStateChange: ((state: ConnectionState) => void) | null = null;
  onStream: ((stream: MediaStream) => void) | null = null;
  onStats: ((stats: WebRTCStats) => void) | null = null;

  private signalingUrl: string;

  constructor(signalingUrl?: string) {
    if (signalingUrl) {
      this.signalingUrl = signalingUrl;
    } else {
      const proto = typeof window !== "undefined" && window.location.protocol === "https:" ? "wss:" : "ws:";
      const host = typeof window !== "undefined" ? window.location.hostname : "localhost";
      this.signalingUrl = `${proto}//${host}:8080/ws/signaling`;
    }
  }

  async connect(): Promise<void> {
    this.cleanup();
    this.setState("connecting");
    this.pendingCandidates = [];
    this.remoteDescSet = false;

    this.pc = new RTCPeerConnection({
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
    });

    this.pc.addTransceiver("video", { direction: "recvonly" });

    this.pc.ontrack = (e) => {
      if (e.streams?.[0]) this.onStream?.(e.streams[0]);
    };

    this.pc.oniceconnectionstatechange = () => {
      if (!this.pc) return;
      const state = this.pc.iceConnectionState;
      if (state === "connected" || state === "completed") {
        this.setState("connected");
        this.startStatsPolling();
      } else if (state === "failed") {
        this.setState("failed");
      } else if (state === "disconnected") {
        this.setState("disconnected");
        setTimeout(() => this.connect(), 2000);
      }
    };

    this.pc.onicecandidate = (e) => {
      if (e.candidate && this.ws?.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: "new-ice-candidate", data: e.candidate.toJSON() }));
      }
    };

    return new Promise<void>((resolve, reject) => {
      this.ws = new WebSocket(this.signalingUrl);

      this.ws.onopen = async () => {
        try {
          const offer = await this.pc!.createOffer();
          await this.pc!.setLocalDescription(offer);
          this.ws!.send(JSON.stringify({ type: "offer", data: { sd: offer.sdp } }));
          resolve();
        } catch (err) {
          reject(err);
        }
      };

      this.ws.onmessage = async (evt) => {
        const msg: WSMessage = JSON.parse(evt.data);

        if (msg.type === "answer") {
          const answer = typeof msg.data === "string" ? JSON.parse(msg.data) : msg.data;
          await this.pc!.setRemoteDescription({ type: "answer", sdp: (answer as { sd: string }).sd });
          this.remoteDescSet = true;
          for (const c of this.pendingCandidates) await this.pc!.addIceCandidate(c);
          this.pendingCandidates = [];
        } else if (msg.type === "new-ice-candidate") {
          const candidate = (typeof msg.data === "string" ? JSON.parse(msg.data) : msg.data) as RTCIceCandidateInit;
          if (candidate.candidate) {
            if (this.remoteDescSet) await this.pc!.addIceCandidate(candidate);
            else this.pendingCandidates.push(candidate);
          }
        }
      };

      this.ws.onerror = () => {
        this.setState("failed");
        reject(new Error("WebSocket connection failed"));
      };
    });
  }

  sendInput(event: InputEvent): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: "input", data: event }));
    }
  }

  disconnect(): void {
    this.cleanup();
    this.setState("disconnected");
  }

  private cleanup(): void {
    if (this.statsInterval) { clearInterval(this.statsInterval); this.statsInterval = null; }
    if (this.pc) { this.pc.close(); this.pc = null; }
    if (this.ws) { this.ws.close(); this.ws = null; }
  }

  private setState(state: ConnectionState): void {
    this.onStateChange?.(state);
  }

  private startStatsPolling(): void {
    if (this.statsInterval) clearInterval(this.statsInterval);
    this.statsInterval = setInterval(async () => {
      if (!this.pc) return;
      try {
        const stats = await this.pc.getStats();
        stats.forEach((report) => {
          if (report.type === "inbound-rtp" && report.kind === "video") {
            const now = report.timestamp;
            const bytesDelta = (report.bytesReceived || 0) - this.lastBytesReceived;
            const timeDelta = now - this.lastStatsTimestamp;
            const bitrate = timeDelta > 0 ? Math.round((bytesDelta * 8) / (timeDelta / 1000) / 1000) : 0;
            this.lastBytesReceived = report.bytesReceived || 0;
            this.lastStatsTimestamp = now;
            this.onStats?.({
              fps: report.framesPerSecond || 0,
              width: report.frameWidth || 0,
              height: report.frameHeight || 0,
              bytesReceived: report.bytesReceived || 0,
              packetsLost: report.packetsLost || 0,
              jitter: report.jitter || 0,
              bitrate,
            });
          }
        });
      } catch { /* stats may fail if PC closing */ }
    }, 1000);
  }
}
