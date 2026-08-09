"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import * as Toggle from "@radix-ui/react-toggle";
import * as Tooltip from "@radix-ui/react-tooltip";
import * as Separator from "@radix-ui/react-separator";
import { WebRTCClient, type ConnectionState, type WebRTCStats } from "@/lib/webrtc-client";
import { scanCodeFor } from "@/lib/keycodes";

// ── Helper Functions ────────────────────────────────────────────────────────

function formatBitrate(kbps: number): string {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${kbps} kbps`;
}

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.round(bytes / 1024)} KB`;
}

function mapMouseButton(button: number): "Left" | "Right" | "Middle" | "Back" | "Forward" | undefined {
  switch (button) {
    case 0: return "Left";
    case 1: return "Middle";
    case 2: return "Right";
    case 3: return "Back";
    case 4: return "Forward";
    default: return undefined;
  }
}

function getModifiers(e: KeyboardEvent): number {
  let modifiers = 0;
  if (e.shiftKey) modifiers |= 0x0001; // SHIFT
  if (e.ctrlKey) modifiers |= 0x0002;  // CTRL
  if (e.altKey) modifiers |= 0x0004;   // ALT
  if (e.metaKey) modifiers |= 0x0008;  // META/WIN
  // CAPS_LOCK (0x0010) and NUM_LOCK (0x0020) are harder to detect reliably on keydown/up without getModifierState
  if (e.getModifierState("CapsLock")) modifiers |= 0x0010;
  if (e.getModifierState("NumLock")) modifiers |= 0x0020;
  return modifiers;
}

// ── Icons (inline SVG) ──────────────────────────────────────────────────────

const MonitorIcon = () => (
  <svg className="w-5 h-5" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <rect x="2" y="3" width="20" height="14" rx="2" /><line x1="8" y1="21" x2="16" y2="21" /><line x1="12" y1="17" x2="12" y2="21" />
  </svg>
);
const RefreshIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/>
  </svg>
);
const ActivityIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/>
  </svg>
);
const MaximizeIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="15 3 21 3 21 9"/><polyline points="9 21 3 21 3 15"/><line x1="21" y1="3" x2="14" y2="10"/><line x1="3" y1="21" x2="10" y2="14"/>
  </svg>
);
const MinimizeIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="4 14 10 14 10 20"/><polyline points="20 10 14 10 14 4"/><line x1="14" y1="10" x2="21" y2="3"/><line x1="3" y1="21" x2="10" y2="14"/>
  </svg>
);
const PointerLockIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="3" /><path d="M12 2v4M12 18v4M2 12h4M18 12h4" />
  </svg>
);
const HelpIcon = () => (
  <svg className="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="9" /><path d="M9.5 9a2.5 2.5 0 1 1 4.1 1.9c-.9.7-1.6 1.1-1.6 2.4" /><path d="M12 17h.01" />
  </svg>
);
const AlertIcon = () => (
  <svg className="w-10 h-10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
    <path d="M10.3 3.4 2.2 17.5A2 2 0 0 0 3.9 20.5h16.2a2 2 0 0 0 1.7-3L13.7 3.4a2 2 0 0 0-3.4 0Z" /><path d="M12 9v4" /><path d="M12 17h.01" />
  </svg>
);
const WifiIcon = () => (
  <svg className="w-10 h-10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M5 12.55a11 11 0 0 1 14.08 0"/><path d="M1.42 9a16 16 0 0 1 21.16 0"/><path d="M8.53 16.11a6 6 0 0 1 6.95 0"/><line x1="12" y1="20" x2="12.01" y2="20"/>
  </svg>
);
const WifiOffIcon = () => (
  <svg className="w-10 h-10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <line x1="1" y1="1" x2="23" y2="23"/><path d="M16.72 11.06A10.94 10.94 0 0 1 19 12.55"/><path d="M5 12.55a10.94 10.94 0 0 1 5.17-2.39"/><path d="M10.71 5.05A16 16 0 0 1 22.56 9"/><path d="M1.42 9a15.91 15.91 0 0 1 4.7-2.88"/><path d="M8.53 16.11a6 6 0 0 1 6.95 0"/><line x1="12" y1="20" x2="12.01" y2="20"/>
  </svg>
);

// ── Main Component ──────────────────────────────────────────────────────────

type Machine = {
  id: string;
  name: string;
  display_name?: string;
  status: "online" | "offline";
  os?: string;
  gpu_vendor?: string;
  encoder_backend?: string;
  virtual_display: boolean;
  width?: number;
  height?: number;
  target_fps?: number;
};

export default function App() {
  const [selectedMachine, setSelectedMachine] = useState<Machine | null>(null);
  if (selectedMachine) {
    return <StreamViewer machine={selectedMachine} onBack={() => setSelectedMachine(null)} />;
  }
  return <MachinePicker onSelect={setSelectedMachine} />;
}

function StreamViewer({ machine, onBack }: { machine: Machine; onBack: () => void }) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const clientRef = useRef<WebRTCClient | null>(null);

  const [connectionState, setConnectionState] = useState<ConnectionState>("disconnected");
  const [stats, setStats] = useState<WebRTCStats | null>(null);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [showStats, setShowStats] = useState(true);
  const [showControls, setShowControls] = useState(true);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [pointerLockEnabled, setPointerLockEnabled] = useState(false);
  const [isPointerLocked, setIsPointerLocked] = useState(false);
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const heldKeys = useRef(new Map<string, { scan_code: number; key_code: number }>());
  const heldButtons = useRef(new Set<"Left" | "Right" | "Middle" | "Back" | "Forward">());
  const pendingMove = useRef<{ absolute: true; x: number; y: number } | { absolute: false; dx: number; dy: number } | null>(null);
  const moveFrame = useRef<number | null>(null);

  const flushMove = useCallback(() => {
    moveFrame.current = null;
    const move = pendingMove.current;
    pendingMove.current = null;
    if (!move || !clientRef.current) return;
    clientRef.current.sendInput({
      Mouse: move.absolute
        ? { MoveAbsolute: { x: move.x, y: move.y } }
        : { Move: { dx: move.dx, dy: move.dy } },
    });
  }, []);

  const queueMove = useCallback((move: NonNullable<typeof pendingMove.current>) => {
    const pending = pendingMove.current;
    if (!pending || pending.absolute !== move.absolute) {
      pendingMove.current = move;
    } else if (move.absolute) {
      pendingMove.current = move;
    } else if (!pending.absolute) {
      pendingMove.current = {
        absolute: false,
        dx: pending.dx + move.dx,
        dy: pending.dy + move.dy,
      };
    }
    if (moveFrame.current === null) {
      moveFrame.current = requestAnimationFrame(flushMove);
    }
  }, [flushMove]);

  const cancelPendingMove = useCallback(() => {
    pendingMove.current = null;
    if (moveFrame.current !== null) {
      cancelAnimationFrame(moveFrame.current);
      moveFrame.current = null;
    }
  }, []);

  const releaseAllInput = useCallback(() => {
    cancelPendingMove();
    const client = clientRef.current;
    if (client) {
      for (const { scan_code, key_code } of heldKeys.current.values()) {
        client.sendInput({
          Keyboard: { KeyUp: { scan_code, key_code, modifiers: 0 } },
        });
      }
      for (const button of heldButtons.current) {
        client.sendInput({ Mouse: { ButtonUp: { button } } });
      }
    }
    heldKeys.current.clear();
    heldButtons.current.clear();
  }, [cancelPendingMove]);

  // WebRTC client
  useEffect(() => {
    const client = new WebRTCClient(undefined, machine.id);
    clientRef.current = client;
    client.onStateChange = (state) => {
      setConnectionState(state);
      if (state !== "connected") releaseAllInput();
    };
    client.onStream = (stream) => {
      const video = videoRef.current;
      if (video) {
        video.srcObject = stream;
        video.play().catch(() => {});
      }
    };
    client.onStats = setStats;
    client.connect().catch(console.error);
    const container = containerRef.current;
    return () => {
      releaseAllInput();
      if (document.pointerLockElement === container) document.exitPointerLock();
      client.disconnect();
      clientRef.current = null;
    };
  }, [machine.id, releaseAllInput]);

  useEffect(() => {
    const h = () => setIsFullscreen(!!document.fullscreenElement);
    document.addEventListener("fullscreenchange", h);
    return () => document.removeEventListener("fullscreenchange", h);
  }, []);

  const toggleFullscreen = useCallback(() => {
    if (!document.fullscreenElement) containerRef.current?.requestFullscreen();
    else document.exitFullscreen();
  }, []);

  const reconnect = useCallback(() => {
    clientRef.current?.connect().catch(console.error);
  }, []);

  const requestPointerLock = useCallback(() => {
    containerRef.current?.requestPointerLock();
  }, []);

  const togglePointerLock = useCallback(() => {
    const locked = document.pointerLockElement === containerRef.current;
    if (locked || pointerLockEnabled) {
      setPointerLockEnabled(false);
      if (locked) {
        document.exitPointerLock();
      }
    } else {
      setPointerLockEnabled(true);
      requestPointerLock();
    }
  }, [pointerLockEnabled, requestPointerLock]);

  const getNormalizedCoords = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (!videoRef.current) return null;
    const video = videoRef.current;
    const rect = video.getBoundingClientRect();
    
    // Calculate the actual rendered video rectangle (accounting for object-contain)
    const videoRatio = video.videoWidth / video.videoHeight;
    const elementRatio = rect.width / rect.height;
    
    let renderWidth = rect.width;
    let renderHeight = rect.height;
    let offsetX = 0;
    let offsetY = 0;

    if (elementRatio > videoRatio) {
      // Element is wider than video - pillars (black bars on sides)
      renderWidth = rect.height * videoRatio;
      offsetX = (rect.width - renderWidth) / 2;
    } else {
      // Element is taller than video - letterbox (black bars on top/bottom)
      renderHeight = rect.width / videoRatio;
      offsetY = (rect.height - renderHeight) / 2;
    }

    const relativeX = e.clientX - rect.left - offsetX;
    const relativeY = e.clientY - rect.top - offsetY;

    // Normalize and clamp to 0.0 - 1.0
    const x = Math.max(0, Math.min(1, relativeX / renderWidth));
    const y = Math.max(0, Math.min(1, relativeY / renderHeight));

    // If click is outside the video content (in the black bars), we might want to ignore it?
    // Or just clamping is enough. Clamping is safer.
    return { x, y };
  }, []);

  const revealControls = useCallback(() => {
    setShowControls(true);
    if (hideTimer.current) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => setShowControls(false), 3000);
  }, []);

  const handleMouseMove = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    revealControls();

    // Input capture
    if (!clientRef.current || !videoRef.current) return;
    
    // Only send move if we have valid video dimensions
    if (videoRef.current.videoWidth === 0) return;

    const coords = getNormalizedCoords(e);
    if (!coords) return;
    
    if (isPointerLocked) {
      if (e.movementX !== 0 || e.movementY !== 0) {
        queueMove({ absolute: false, dx: e.movementX, dy: e.movementY });
      }
    } else {
      queueMove({ absolute: true, x: coords.x, y: coords.y });
    }
  }, [getNormalizedCoords, isPointerLocked, queueMove, revealControls]);

  const handleMouseDown = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (!clientRef.current) return;
    e.preventDefault();
    const button = mapMouseButton(e.button);
    if (button) {
      clientRef.current.sendInput({
        Mouse: {
          ButtonDown: { button }
        }
      });
      heldButtons.current.add(button);
    }
  }, []);

  const handleMouseUp = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (!clientRef.current) return;
    e.preventDefault();
    const button = mapMouseButton(e.button);
    if (button) {
      if (!heldButtons.current.has(button)) return;
      clientRef.current.sendInput({
        Mouse: {
          ButtonUp: { button }
        }
      });
      heldButtons.current.delete(button);
    }
  }, []);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const handleWheel = (e: WheelEvent) => {
      if (!clientRef.current) return;
      e.preventDefault();
      // e.deltaY is usually 100 or -100. Flux expects windows WHEEL_DELTA (120).
      // But we are sending raw delta. The server injects it as mouseData.
      // Standard mouse wheel is 120 per notch.
      // e.deltaY is positive for scrolling down (towards user).
      // Windows WHEEL_DELTA is negative for scrolling down? No, SendInput:
      // "If dwFlags contains MOUSEEVENTF_WHEEL, then mouseData specifies the amount of wheel movement. A positive value indicates that the wheel was rotated forward, away from the user; a negative value indicates that the wheel was rotated backward, toward the user."
      // e.deltaY > 0 is scroll down (toward user) -> should be negative for Windows.
      // Browser deltaX and MOUSEEVENTF_HWHEEL both use positive for rightward scrolling.
      const deltaX = Math.round(e.deltaX);
      const deltaY = Math.round(-e.deltaY);
      clientRef.current.sendInput({
        Mouse: {
          Scroll: { dx: deltaX, dy: deltaY }
        }
      });
    };
    container.addEventListener("wheel", handleWheel, { passive: false });
    return () => container.removeEventListener("wheel", handleWheel);
  }, []);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const handleTouchStart = (e: TouchEvent) => {
      revealControls();
      if (!(e.target instanceof Element) || e.target.closest("button")) return;
      e.preventDefault();
    };
    container.addEventListener("touchstart", handleTouchStart, { passive: false });
    return () => container.removeEventListener("touchstart", handleTouchStart);
  }, [revealControls]);

  const handleSurfaceClick = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (
      pointerLockEnabled &&
      !isPointerLocked &&
      (e.target === containerRef.current || e.target === videoRef.current)
    ) {
      requestPointerLock();
    }
  }, [isPointerLocked, pointerLockEnabled, requestPointerLock]);

  useEffect(() => {
    const handlePointerLockChange = () => {
      const locked = document.pointerLockElement === containerRef.current;
      setIsPointerLocked(locked);
      if (!locked) {
        setPointerLockEnabled(false);
        releaseAllInput();
      }
    };
    document.addEventListener("pointerlockchange", handlePointerLockChange);
    return () => document.removeEventListener("pointerlockchange", handlePointerLockChange);
  }, [releaseAllInput]);

  useEffect(() => {
    const isViewerChord = (e: KeyboardEvent) =>
      e.ctrlKey && e.altKey && e.shiftKey && !e.metaKey;
    const isDevtoolsShortcut = (e: KeyboardEvent) =>
      e.code === "F12" ||
      (e.ctrlKey && e.shiftKey && ["KeyC", "KeyI", "KeyJ"].includes(e.code));
    const isBrowserReservedChord = (e: KeyboardEvent) =>
      (e.ctrlKey || e.metaKey) && ["KeyW", "KeyT", "KeyN", "KeyL"].includes(e.code);

    const handleKeyDown = (e: KeyboardEvent) => {
      if (isViewerChord(e)) {
        if (e.code === "KeyR") { e.preventDefault(); reconnect(); return; }
        if (e.code === "KeyS") { e.preventDefault(); setShowStats((visible) => !visible); return; }
        if (e.code === "KeyF") { e.preventDefault(); toggleFullscreen(); return; }
      }
      if (e.code === "F11") { e.preventDefault(); toggleFullscreen(); return; }

      if (clientRef.current) {
        if (
          connectionState === "connected" &&
          !isDevtoolsShortcut(e) &&
          !isBrowserReservedChord(e)
        ) {
          e.preventDefault();
        }
        const keyId = e.code || e.key;
        const scanCode = scanCodeFor(e.code) ?? 0;
        heldKeys.current.set(keyId, { scan_code: scanCode, key_code: e.keyCode });
        clientRef.current.sendInput({
          Keyboard: {
            KeyDown: {
              scan_code: scanCode,
              key_code: e.keyCode,
              modifiers: getModifiers(e)
            }
          }
        });
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      const keyId = e.code || e.key;
      const held = heldKeys.current.get(keyId);
      if (clientRef.current && held) {
        if (
          connectionState === "connected" &&
          !isDevtoolsShortcut(e) &&
          !isBrowserReservedChord(e)
        ) {
          e.preventDefault();
        }
        clientRef.current.sendInput({
          Keyboard: {
            KeyUp: {
              scan_code: held.scan_code,
              key_code: held.key_code,
              modifiers: getModifiers(e)
            }
          }
        });
        heldKeys.current.delete(keyId);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    window.addEventListener("keyup", handleKeyUp);
    window.addEventListener("blur", releaseAllInput);
    window.addEventListener("pagehide", releaseAllInput);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("keyup", handleKeyUp);
      window.removeEventListener("blur", releaseAllInput);
      window.removeEventListener("pagehide", releaseAllInput);
    };
  }, [connectionState, reconnect, releaseAllInput, toggleFullscreen]);

  const dotColor = connectionState === "connected" ? "bg-emerald-400"
    : connectionState === "connecting" ? "bg-amber-400" : "bg-red-400";
  const textColor = connectionState === "connected" ? "text-emerald-400"
    : connectionState === "connecting" ? "text-amber-400" : "text-red-400";
  const connectionCopy: Record<ConnectionState, { label: string; title: string; detail: string }> = {
    connecting: {
      label: "Connecting",
      title: "Connecting to stream",
      detail: `Reaching ${machine.name}. This can take a moment.`,
    },
    connected: {
      label: "Connected",
      title: "Live stream",
      detail: "Your remote desktop is ready.",
    },
    disconnected: {
      label: "Disconnected",
      title: "Stream disconnected",
      detail: "The connection was interrupted. Flux will try again automatically.",
    },
    failed: {
      label: "Failed",
      title: "Couldn’t connect to stream",
      detail: "Check that the sender is online, then try again.",
    },
  };
  const currentConnection = connectionCopy[connectionState];

  return (
    <Tooltip.Provider delayDuration={300}>
      <div
        ref={containerRef}
        className="relative min-h-screen w-full bg-[var(--color-background)] overflow-hidden select-none"
        onMouseMove={handleMouseMove}
        onMouseDown={handleMouseDown}
        onMouseUp={handleMouseUp}
        onClick={handleSurfaceClick}
        onContextMenu={(e) => e.preventDefault()}
        style={{ cursor: isPointerLocked || !showControls ? "none" : "default" }}
      >
        <div className="absolute inset-0 bg-black">
          <video
            ref={videoRef}
            autoPlay
            playsInline
            muted
            className="absolute inset-0 h-full w-full object-contain bg-black pointer-events-none"
          />
        </div>

        {/* Top bar */}
        <div className={`absolute top-0 left-0 right-0 z-10 p-3 sm:p-5 flex flex-wrap items-start justify-between gap-3 transition-opacity duration-300 ${showControls ? "opacity-100" : "opacity-0 pointer-events-none"}`}>
          {/* Logo + status */}
          <div className="glass rounded-2xl px-3 py-2.5 sm:px-4 flex min-w-0 items-center gap-2.5 sm:gap-3 animate-fade-in">
            <span className="text-[var(--color-accent)]"><MonitorIcon /></span>
            <div className="min-w-0">
              <div className="text-sm font-semibold tracking-tight">Flux Stream</div>
              <div className="max-w-36 truncate text-[11px] text-zinc-500 sm:max-w-52">{machine.name}</div>
            </div>
            <Separator.Root className="hidden sm:block w-px h-7 bg-zinc-700" orientation="vertical" />
            <div className="flex items-center gap-2 whitespace-nowrap">
              <div className={`w-2 h-2 rounded-full pulse-dot ${dotColor}`} />
              <span className={`text-xs font-semibold ${textColor}`}>{currentConnection.label}</span>
            </div>
            <button onClick={onBack} className="ml-1 rounded-lg px-2 py-1 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-white">
              Machines
            </button>
          </div>

          {/* Controls */}
          <div className="glass rounded-2xl px-2 py-1.5 flex items-center gap-1 animate-fade-in">
            <CtrlTooltip label="Reconnect (Ctrl+Alt+Shift+R)">
              <button onClick={reconnect} className="p-2 rounded-lg text-zinc-400 hover:text-white hover:bg-zinc-800 transition-colors">
                <RefreshIcon />
              </button>
            </CtrlTooltip>

            <CtrlTooltip label="Stats (Ctrl+Alt+Shift+S)">
              <Toggle.Root
                pressed={showStats}
                onPressedChange={setShowStats}
                className="p-2 rounded-lg transition-colors data-[state=on]:bg-zinc-700 data-[state=on]:text-white data-[state=off]:text-zinc-400 hover:text-white hover:bg-zinc-800"
              >
                <ActivityIcon />
              </Toggle.Root>
            </CtrlTooltip>

            <CtrlTooltip label={isPointerLocked ? "Exit pointer lock (Esc)" : "Pointer lock"}>
              <Toggle.Root
                pressed={pointerLockEnabled}
                onPressedChange={togglePointerLock}
                className="p-2 rounded-lg transition-colors data-[state=on]:bg-zinc-700 data-[state=on]:text-white data-[state=off]:text-zinc-400 hover:text-white hover:bg-zinc-800"
              >
                <PointerLockIcon />
              </Toggle.Root>
            </CtrlTooltip>

            <Separator.Root className="w-px h-5 bg-zinc-700 mx-1" orientation="vertical" />

            <CtrlTooltip label="Fullscreen (Ctrl+Alt+Shift+F)">
              <button onClick={toggleFullscreen} className="p-2 rounded-lg text-zinc-400 hover:text-white hover:bg-zinc-800 transition-colors">
                {isFullscreen ? <MinimizeIcon /> : <MaximizeIcon />}
              </button>
            </CtrlTooltip>
            <CtrlTooltip label="Keyboard shortcuts">
              <button
                aria-label="Show keyboard shortcuts"
                aria-expanded={showShortcuts}
                onClick={() => setShowShortcuts((visible) => !visible)}
                className={`p-2 rounded-lg transition-colors ${showShortcuts ? "bg-zinc-700 text-white" : "text-zinc-400 hover:text-white hover:bg-zinc-800"}`}
              >
                <HelpIcon />
              </button>
            </CtrlTooltip>
          </div>
        </div>

        {showShortcuts && (
          <div className="absolute right-3 top-20 z-20 w-[min(19rem,calc(100vw-1.5rem))] glass rounded-2xl p-4 shadow-2xl animate-fade-in sm:right-5 sm:top-24">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-sm font-semibold text-zinc-100">Shortcuts</h2>
              <button onClick={() => setShowShortcuts(false)} className="text-xs text-zinc-500 hover:text-white">Close</button>
            </div>
            <div className="space-y-2 text-xs text-zinc-400">
              <ShortcutRow keys="Ctrl + Alt + Shift + R" label="Reconnect" />
              <ShortcutRow keys="Ctrl + Alt + Shift + S" label="Toggle stats" />
              <ShortcutRow keys="Ctrl + Alt + Shift + F" label="Toggle fullscreen" />
              <ShortcutRow keys="Esc" label="Exit pointer lock" />
              <p className="pt-1 text-[11px] text-zinc-600">Pointer lock also enables relative mouse movement.</p>
            </div>
          </div>
        )}

        {/* Stats overlay */}
        {showStats && connectionState === "connected" && (
          <div className="absolute bottom-3 left-3 z-10 w-[min(22rem,calc(100vw-1.5rem))] glass rounded-2xl p-3 sm:bottom-5 sm:left-5 sm:p-4 animate-fade-in">
            <div className="mb-3 flex items-center justify-between">
              <div>
                <p className="text-xs font-semibold text-zinc-200">Stream health</p>
                <p className="text-[11px] text-zinc-500">Live WebRTC telemetry</p>
              </div>
              <span className={`rounded-full px-2 py-1 text-[10px] font-medium ${stats ? "bg-emerald-400/10 text-emerald-300" : "bg-amber-400/10 text-amber-300"}`}>
                {stats ? "LIVE" : "WAITING"}
              </span>
            </div>
            <div className="grid grid-cols-2 gap-2">
              <StatCard label="Resolution" value={stats ? `${stats.width} × ${stats.height}` : "—"} />
              <StatCard label="Frame rate" value={stats ? `${stats.fps.toFixed(1)} fps` : "—"} />
              <StatCard label="Bitrate" value={stats ? formatBitrate(stats.bitrate) : "—"} accent />
              <StatCard label="Packets lost" value={stats ? stats.packetsLost.toLocaleString() : "—"} accent={!!stats && stats.packetsLost > 0} />
              <StatCard label="Jitter" value={stats ? `${(stats.jitter * 1000).toFixed(1)} ms` : "—"} />
              <StatCard label="Received" value={stats ? formatBytes(stats.bytesReceived) : "—"} />
            </div>
          </div>
        )}

        {/* Connection overlay */}
        {connectionState !== "connected" && (
          <div className="absolute inset-0 z-[1] flex items-center justify-center px-4">
            <div className="glass w-full max-w-md rounded-3xl px-6 py-7 text-center shadow-2xl animate-fade-in sm:px-10">
              <div className={`mx-auto mb-4 flex h-16 w-16 items-center justify-center rounded-2xl ${connectionState === "connecting" ? "bg-amber-400/10 text-amber-300" : "bg-red-400/10 text-red-300"}`}>
                {connectionState === "connecting" ? <span className="animate-pulse"><WifiIcon /></span> : connectionState === "failed" ? <AlertIcon /> : <WifiOffIcon />}
              </div>
              <p className="mb-2 text-lg font-semibold text-zinc-100">{currentConnection.title}</p>
              <p className="mx-auto max-w-sm text-sm leading-6 text-zinc-400">{currentConnection.detail}</p>
              {connectionState !== "connecting" && (
                <div className="mt-6 flex flex-col justify-center gap-2 sm:flex-row">
                  <button onClick={reconnect} className="rounded-xl bg-[var(--color-accent)] px-4 py-2.5 text-sm font-semibold text-white transition hover:brightness-110">
                    {connectionState === "failed" ? "Retry connection" : "Reconnect"}
                  </button>
                  <button onClick={onBack} className="rounded-xl border border-zinc-700 px-4 py-2.5 text-sm font-semibold text-zinc-300 transition hover:border-zinc-500 hover:text-white">
                    Back to machines
                  </button>
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </Tooltip.Provider>
  );
}

function MachinePicker({ onSelect }: { onSelect: (machine: Machine) => void }) {
  const [machines, setMachines] = useState<Machine[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const response = await fetch("/api/machines", { cache: "no-store" });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      setMachines((await response.json()) as Machine[]);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unable to load machines");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    const initial = window.setTimeout(() => void refresh(), 0);
    const timer = window.setInterval(() => void refresh(), 5000);
    return () => {
      window.clearTimeout(initial);
      window.clearInterval(timer);
    };
  }, [refresh]);

  return (
    <main className="h-screen overflow-y-auto bg-[var(--color-background)] px-4 py-6 text-zinc-100 sm:px-6 sm:py-12">
      <div className="mx-auto max-w-5xl">
        <div className="mb-8 flex flex-wrap items-center justify-between gap-4">
          <div className="flex min-w-0 items-center gap-3">
            <span className="text-[var(--color-accent)]"><MonitorIcon /></span>
            <div>
              <h1 className="text-xl font-semibold tracking-tight">Flux Stream</h1>
              <p className="text-sm text-zinc-500">Choose a machine to view</p>
            </div>
          </div>
          <button onClick={() => void refresh()} className="flex items-center gap-2 rounded-xl border border-zinc-800 px-3 py-2 text-xs font-medium text-zinc-400 transition hover:border-zinc-700 hover:bg-zinc-900 hover:text-white">
            <RefreshIcon /><span className="hidden sm:inline">Refresh</span>
          </button>
        </div>
        {loading ? (
          <div className="glass rounded-2xl p-8 text-center text-zinc-400">Loading machines...</div>
        ) : error ? (
          <div className="glass rounded-2xl p-8 text-center">
            <p className="text-red-400 mb-4">Could not reach the machine registry.</p>
            <p className="text-sm text-zinc-500 mb-4">{error}</p>
            <button onClick={() => void refresh()} className="px-4 py-2 rounded-lg bg-[var(--color-accent)] text-sm">Retry</button>
          </div>
        ) : machines.length === 0 ? (
          <div className="glass rounded-2xl p-8 text-center text-zinc-400">No machines have registered yet.</div>
        ) : (
          <div className="grid auto-rows-fr gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {machines.map((machine) => {
              const online = machine.status === "online";
              return (
                <button
                  key={machine.id}
                  disabled={!online}
                  onClick={() => onSelect(machine)}
                  className={`glass h-full w-full rounded-2xl p-5 text-left transition focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-accent)] ${online ? "hover:-translate-y-0.5 hover:border-zinc-700 hover:bg-zinc-800/80" : "cursor-not-allowed opacity-50"}`}
                >
                  <div className="mb-4 flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <h2 className="truncate font-semibold">{machine.name}</h2>
                      {machine.display_name && <p className="mt-1 truncate text-xs text-zinc-500">{machine.display_name}</p>}
                    </div>
                    <span className={`shrink-0 rounded-full px-2 py-1 text-[10px] font-semibold uppercase tracking-wide ${online ? "bg-emerald-400/10 text-emerald-300" : "bg-zinc-800 text-zinc-500"}`}>
                      {online ? "Online" : "Offline"}
                    </span>
                  </div>
                  <div className="space-y-1 text-xs text-zinc-400">
                    {machine.os && <p>OS: {machine.os}</p>}
                    {machine.gpu_vendor && <p>GPU: {machine.gpu_vendor}</p>}
                    {machine.encoder_backend && <p>Encoder: {machine.encoder_backend}</p>}
                    {(machine.width && machine.height) && <p>Display: {machine.width}×{machine.height}{machine.target_fps ? ` @ ${machine.target_fps} FPS` : ""}</p>}
                    <p>{machine.virtual_display ? "Virtual display" : "Physical display"}</p>
                  </div>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </main>
  );
}

function StatCard({ label, value, accent = false }: { label: string; value: string; accent?: boolean }) {
  return (
    <div className="rounded-xl border border-white/5 bg-black/20 px-3 py-2">
      <p className="text-[10px] uppercase tracking-wide text-zinc-600">{label}</p>
      <p className={`mt-1 truncate font-mono text-sm font-semibold ${accent ? "text-[var(--color-accent)]" : "text-zinc-200"}`}>{value}</p>
    </div>
  );
}

function ShortcutRow({ keys, label }: { keys: string; label: string }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span>{label}</span>
      <kbd className="rounded-md border border-zinc-700 bg-black/30 px-2 py-1 font-mono text-[10px] text-zinc-300">{keys}</kbd>
    </div>
  );
}

function CtrlTooltip({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger asChild>{children}</Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content
          side="bottom"
          sideOffset={6}
          className="glass rounded-lg px-3 py-1.5 text-xs text-zinc-200 animate-fade-in z-50"
        >
          {label}
          <Tooltip.Arrow className="fill-zinc-800" />
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}
