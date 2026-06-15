"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import * as Toggle from "@radix-ui/react-toggle";
import * as Tooltip from "@radix-ui/react-tooltip";
import * as Separator from "@radix-ui/react-separator";
import { WebRTCClient, type ConnectionState, type WebRTCStats } from "@/lib/webrtc-client";

// ── Helper Functions ────────────────────────────────────────────────────────

function formatBitrate(kbps: number): string {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${kbps} kbps`;
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

export default function StreamViewer() {
  const videoRef = useRef<HTMLVideoElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const clientRef = useRef<WebRTCClient | null>(null);

  const [connectionState, setConnectionState] = useState<ConnectionState>("disconnected");
  const [stats, setStats] = useState<WebRTCStats | null>(null);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [showStats, setShowStats] = useState(true);
  const [showControls, setShowControls] = useState(true);
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // WebRTC client
  useEffect(() => {
    const client = new WebRTCClient();
    clientRef.current = client;
    client.onStateChange = setConnectionState;
    client.onStream = (stream) => {
      const video = videoRef.current;
      if (video) {
        video.srcObject = stream;
        video.play().catch(() => {});
      }
    };
    client.onStats = setStats;
    client.connect().catch(console.error);
    return () => { client.disconnect(); clientRef.current = null; };
  }, []);

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

  const handleMouseMove = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    // Show controls logic
    setShowControls(true);
    if (hideTimer.current) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => setShowControls(false), 3000);

    // Input capture
    if (!clientRef.current || !videoRef.current) return;
    
    // Only send move if we have valid video dimensions
    if (videoRef.current.videoWidth === 0) return;

    const coords = getNormalizedCoords(e);
    if (!coords) return;
    
    clientRef.current.sendInput({
      Mouse: {
        MoveAbsolute: { x: coords.x, y: coords.y }
      }
    });
  }, [getNormalizedCoords]);

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
    }
  }, []);

  const handleMouseUp = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (!clientRef.current) return;
    e.preventDefault();
    const button = mapMouseButton(e.button);
    if (button) {
      clientRef.current.sendInput({
        Mouse: {
          ButtonUp: { button }
        }
      });
    }
  }, []);

  const handleWheel = useCallback((e: React.WheelEvent<HTMLDivElement>) => {
    if (!clientRef.current) return;
    // e.deltaY is usually 100 or -100. Flux expects windows WHEEL_DELTA (120).
    // But we are sending raw delta. The server injects it as mouseData.
    // Standard mouse wheel is 120 per notch.
    // e.deltaY is positive for scrolling down (towards user).
    // Windows WHEEL_DELTA is negative for scrolling down? No, SendInput:
    // "If dwFlags contains MOUSEEVENTF_WHEEL, then mouseData specifies the amount of wheel movement. A positive value indicates that the wheel was rotated forward, away from the user; a negative value indicates that the wheel was rotated backward, toward the user."
    // e.deltaY > 0 is scroll down (toward user) -> should be negative for Windows.
    const delta = Math.round(-e.deltaY); 
    clientRef.current.sendInput({
      Mouse: {
        Scroll: { dx: 0, dy: delta }
      }
    });
  }, []);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Viewer shortcuts (only if modifiers aren't held, to allow sending Ctrl+R etc)
      if (!e.ctrlKey && !e.metaKey && !e.altKey) {
        if (e.key === "F11") { e.preventDefault(); toggleFullscreen(); return; }
      }

      if (clientRef.current) {
        // e.preventDefault(); // Aggressive prevention might block F12/F5
        // Only prevent if we successfully sent?
        // Let's send everything.
        clientRef.current.sendInput({
          Keyboard: {
            KeyDown: {
              scan_code: 0,
              key_code: e.keyCode,
              modifiers: getModifiers(e)
            }
          }
        });
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      if (clientRef.current) {
        clientRef.current.sendInput({
          Keyboard: {
            KeyUp: {
              scan_code: 0,
              key_code: e.keyCode,
              modifiers: getModifiers(e)
            }
          }
        });
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    window.addEventListener("keyup", handleKeyUp);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("keyup", handleKeyUp);
    };
  }, [toggleFullscreen]);

  const dotColor = connectionState === "connected" ? "bg-emerald-400"
    : connectionState === "connecting" ? "bg-amber-400" : "bg-red-400";
  const textColor = connectionState === "connected" ? "text-emerald-400"
    : connectionState === "connecting" ? "text-amber-400" : "text-red-400";

  return (
    <Tooltip.Provider delayDuration={300}>
      <div
        ref={containerRef}
        className="relative w-screen h-screen bg-black overflow-hidden select-none"
        onMouseMove={handleMouseMove}
        onMouseDown={handleMouseDown}
        onMouseUp={handleMouseUp}
        onWheel={handleWheel}
        onContextMenu={(e) => e.preventDefault()}
        style={{ cursor: showControls ? "default" : "none" }}
      >
        <video
          ref={videoRef}
          autoPlay
          playsInline
          muted
          className="absolute inset-0 w-full h-full object-contain bg-black pointer-events-none"
        />

        {/* Top bar */}
        <div className={`absolute top-0 left-0 right-0 p-4 flex items-center justify-between transition-opacity duration-300 ${showControls ? "opacity-100" : "opacity-0 pointer-events-none"}`}>
          {/* Logo + status */}
          <div className="glass rounded-xl px-4 py-2.5 flex items-center gap-3 animate-fade-in">
            <span className="text-[var(--color-accent)]"><MonitorIcon /></span>
            <span className="text-sm font-semibold tracking-tight">Flux Stream</span>
            <Separator.Root className="w-px h-4 bg-zinc-700" orientation="vertical" />
            <div className="flex items-center gap-2">
              <div className={`w-2 h-2 rounded-full pulse-dot ${dotColor}`} />
              <span className={`text-xs font-medium capitalize ${textColor}`}>{connectionState}</span>
            </div>
          </div>

          {/* Controls */}
          <div className="glass rounded-xl px-2 py-1.5 flex items-center gap-1 animate-fade-in">
            <CtrlTooltip label="Reconnect (R)">
              <button onClick={reconnect} className="p-2 rounded-lg text-zinc-400 hover:text-white hover:bg-zinc-800 transition-colors">
                <RefreshIcon />
              </button>
            </CtrlTooltip>

            <CtrlTooltip label="Stats (S)">
              <Toggle.Root
                pressed={showStats}
                onPressedChange={setShowStats}
                className="p-2 rounded-lg transition-colors data-[state=on]:bg-zinc-700 data-[state=on]:text-white data-[state=off]:text-zinc-400 hover:text-white hover:bg-zinc-800"
              >
                <ActivityIcon />
              </Toggle.Root>
            </CtrlTooltip>

            <Separator.Root className="w-px h-5 bg-zinc-700 mx-1" orientation="vertical" />

            <CtrlTooltip label="Fullscreen (F)">
              <button onClick={toggleFullscreen} className="p-2 rounded-lg text-zinc-400 hover:text-white hover:bg-zinc-800 transition-colors">
                {isFullscreen ? <MinimizeIcon /> : <MaximizeIcon />}
              </button>
            </CtrlTooltip>
          </div>
        </div>

        {/* Stats overlay */}
        {showStats && stats && (
          <div className="absolute bottom-4 left-4 glass rounded-xl px-4 py-3 animate-fade-in">
            <div className="grid grid-cols-2 gap-x-6 gap-y-1.5 text-xs font-mono">
              <span className="text-zinc-500">Resolution</span><span className="text-zinc-200 text-right">{stats.width}x{stats.height}</span>
              <span className="text-zinc-500">FPS</span><span className="text-zinc-200 text-right">{stats.fps}</span>
              <span className="text-zinc-500">Bitrate</span><span className="text-zinc-200 text-right">{formatBitrate(stats.bitrate)}</span>
              <span className="text-zinc-500">Packets Lost</span><span className="text-zinc-200 text-right">{stats.packetsLost}</span>
              <span className="text-zinc-500">Jitter</span><span className="text-zinc-200 text-right">{(stats.jitter * 1000).toFixed(1)}ms</span>
            </div>
          </div>
        )}

        {/* Connection overlay */}
        {connectionState !== "connected" && (
          <div className="absolute inset-0 flex items-center justify-center">
            <div className="glass rounded-2xl px-8 py-6 flex flex-col items-center gap-4 animate-fade-in">
              {connectionState === "connecting" ? (
                <>
                  <span className="text-amber-400 animate-pulse"><WifiIcon /></span>
                  <p className="text-sm text-zinc-300">Connecting to stream...</p>
                </>
              ) : connectionState === "failed" ? (
                <>
                  <span className="text-red-400"><WifiOffIcon /></span>
                  <p className="text-sm text-zinc-300">Connection failed</p>
                  <button onClick={reconnect} className="px-4 py-2 rounded-lg bg-[var(--color-accent)] hover:brightness-110 text-white text-sm font-medium transition">Retry</button>
                </>
              ) : (
                <>
                  <span className="text-zinc-500"><WifiOffIcon /></span>
                  <p className="text-sm text-zinc-300">Disconnected</p>
                  <button onClick={reconnect} className="px-4 py-2 rounded-lg bg-[var(--color-accent)] hover:brightness-110 text-white text-sm font-medium transition">Connect</button>
                </>
              )}
            </div>
          </div>
        )}
      </div>
    </Tooltip.Provider>
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
