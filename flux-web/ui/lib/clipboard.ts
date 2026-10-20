// Clipboard sync helpers: paste-chord detection, per-machine policy gating,
// and send/receive deduplication shared between the WebTransport client and
// the stream viewer.

export const CLIPBOARD_MIME = "text/plain;charset=utf-8";
export const CLIPBOARD_MAX_BYTES = 1024 * 1024;

export type ClipboardPolicyMode = "both" | "off" | "host_to_client" | "client_to_host";

export interface ClipboardStatus {
  direction: string;
  result: "ok" | "blocked" | "too_large" | "invalid" | string;
  policy: ClipboardPolicyMode | string;
}

export interface ClipboardPayload {
  mime: string;
  data: string;
}

export interface PasteChordEvent {
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  code: string;
  key?: string;
}

/** Ctrl/Cmd+V or Shift+Insert — the gestures where browsers allow readText. */
export function isPasteChord(e: PasteChordEvent): boolean {
  if ((e.ctrlKey || e.metaKey) && !e.altKey && e.code === "KeyV") return true;
  if (e.shiftKey && (e.code === "Insert" || e.key === "Insert")) return true;
  return false;
}

export function policyAllowsClientSend(mode: string): boolean {
  return mode === "both" || mode === "client_to_host";
}

export function policyAllowsRemoteReceive(mode: string): boolean {
  return mode === "both" || mode === "host_to_client";
}

export function policyLabel(mode: string): string {
  switch (mode) {
    case "both": return "both directions";
    case "host_to_client": return "host→you only";
    case "client_to_host": return "you→host only";
    case "off": return "disabled by policy";
    default: return "unknown";
  }
}

export function withinSizeCap(text: string): boolean {
  // UTF-8 worst case is 4 bytes per code unit; TextEncoder is exact but
  // avoiding an allocation per call matters little — use the encoder anyway.
  return new TextEncoder().encode(text).byteLength <= CLIPBOARD_MAX_BYTES;
}

/**
 * Buffer for host→client text that could not be written to the OS clipboard
 * yet (browser needs a gesture). `flushWith` attempts the write; on success
 * the buffer clears, on rejection it stays pending for the next gesture.
 */
export class PendingRemoteClipboard {
  private pending: string | null = null;

  store(text: string): void {
    this.pending = text;
  }

  get current(): string | null {
    return this.pending;
  }

  /** Attempts the write. Returns a promise resolving true when flushed. */
  async flushWith(write: (text: string) => Promise<void>): Promise<boolean> {
    const text = this.pending;
    if (text === null) return false;
    try {
      await write(text);
      if (this.pending === text) this.pending = null;
      return true;
    } catch {
      return false;
    }
  }
}

/**
 * Tracks the last text seen in either direction so identical round-trips
 * (host announcing back what the client just pasted) do not echo.
 */
export class ClipboardDedup {
  private last: string | null = null;

  /** True when local text should be sent; records it either way. */
  shouldSendLocal(text: string): boolean {
    if (text === this.last) return false;
    this.last = text;
    return true;
  }

  /** Record text just sent by another path so later reads don't re-send it. */
  noteSent(text: string): void {
    this.last = text;
  }

  /** True when a remote payload should be applied; records it either way. */
  shouldApplyRemote(text: string): boolean {
    if (text === this.last) return false;
    this.last = text;
    return true;
  }
}
