"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { WebTransportClient, type ConnectionState, type CursorMetadata, type ResolutionStatus, type StreamStats } from "@/lib/webtransport-client";
import { getContainedVideoGeometry, mapCursorToVideo } from "@/lib/cursor-overlay";
import { scanCodeFor } from "@/lib/keycodes";
import { isLocalControlTarget, isLocalKeyboardTarget } from "@/lib/local-controls";
import { modifierFlags } from "@/lib/protocol";
import type { Machine } from "@/lib/api";
import { buildConnectUrl } from "@/lib/launch";
import { Toolbar } from "@/components/stream/Toolbar";
import { StatsHud } from "@/components/stream/StatsHud";
import { ConnectionOverlay } from "@/components/stream/ConnectionOverlay";

// ── Helper Functions ────────────────────────────────────────────────────────

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

function CursorOverlay({
  cursor,
  layout,
}: {
  cursor: CursorMetadata;
  layout: { width: number; height: number; boxWidth: number; boxHeight: number } | null;
}) {
  const bitmap = cursor.bitmap;
  const source = useMemo(() => {
    if (!bitmap || typeof document === "undefined") return null;
    const canvas = document.createElement("canvas");
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    const context = canvas.getContext("2d");
    if (!context) return null;
    const encoded = atob(bitmap.pixels);
    if (encoded.length < bitmap.stride * bitmap.height) return null;
    const sourcePixels = Uint8Array.from(encoded, (character) => character.charCodeAt(0));
    const rgba = new Uint8ClampedArray(bitmap.width * bitmap.height * 4);
    for (let y = 0; y < bitmap.height; y++) {
      rgba.set(sourcePixels.subarray(y * bitmap.stride, y * bitmap.stride + bitmap.width * 4), y * bitmap.width * 4);
    }
    context.putImageData(new ImageData(rgba, bitmap.width, bitmap.height), 0, 0);
    return canvas.toDataURL();
  }, [bitmap]);

  if (!cursor.position || !bitmap || !source || !layout) return null;
  const geometry = getContainedVideoGeometry(layout.boxWidth, layout.boxHeight, layout.width, layout.height);
  if (!geometry) return null;
  const placement = mapCursorToVideo(
    cursor.position,
    geometry,
    layout.width,
    layout.height,
    bitmap.width,
    bitmap.height,
  );
  return (
    <img
      src={source}
      alt=""
      aria-hidden="true"
      className="pointer-events-none absolute"
      style={{
        left: placement.left,
        top: placement.top,
        width: placement.width,
        height: placement.height,
      }}
    />
  );
}

export function StreamViewer({ machine, resolution, initialFps, initialQuality, onBack }: {
  machine: Machine;
  resolution?: { width: number; height: number };
  initialFps?: number;
  initialQuality?: number;
  onBack: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const clientRef = useRef<WebTransportClient | null>(null);

  const [connectionState, setConnectionState] = useState<ConnectionState>("disconnected");
  const [stats, setStats] = useState<StreamStats | null>(null);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [showStats, setShowStats] = useState(true);
  const [showControls, setShowControls] = useState(true);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [pointerLockEnabled, setPointerLockEnabled] = useState(false);
  const [isPointerLocked, setIsPointerLocked] = useState(false);
  const [qualityLevel, setQualityLevel] = useState<number | null>(null);
  const [fpsCap, setFpsCap] = useState<number | null>(null);
  const [resolutionStatus, setResolutionStatus] = useState<ResolutionStatus | null>(null);
  const [clientError, setClientError] = useState<string | null>(null);
  const [endedReason, setEndedReason] = useState<string | null>(null);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const resolutionStatusTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const committedQuality = useRef<number | null>(null);
  const committedFps = useRef<number | null>(null);
  const initialControlsApplied = useRef(false);
  const connectOptions = useRef({ resolution, initialFps, initialQuality });
  const [cursor, setCursor] = useState<CursorMetadata>({
    position: null,
    hotspot: [0, 0],
    bitmap: null,
  });
  const [videoHovered, setVideoHovered] = useState(false);
  const [videoLayout, setVideoLayout] = useState<{
    width: number;
    height: number;
    boxWidth: number;
    boxHeight: number;
  } | null>(null);
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

  const clearResolutionStatus = useCallback(() => {
    if (resolutionStatusTimer.current) clearTimeout(resolutionStatusTimer.current);
    resolutionStatusTimer.current = null;
    setResolutionStatus(null);
  }, []);

  useEffect(() => {
    const { resolution: connectResolution, initialFps: connectInitialFps, initialQuality: connectInitialQuality } = connectOptions.current;
    const client = new WebTransportClient(machine.id, connectResolution);
    if (canvasRef.current) client.attachCanvas(canvasRef.current);
    clientRef.current = client;
    client.onStateChange = (state) => {
      setConnectionState(state);
      if (state !== "connected") releaseAllInput();
      if (state === "connected" && !initialControlsApplied.current && (connectInitialFps !== undefined || connectInitialQuality !== undefined)) {
        initialControlsApplied.current = true;
        setFpsCap(connectInitialFps ?? null);
        setQualityLevel(connectInitialQuality ?? null);
        committedFps.current = connectInitialFps ?? null;
        committedQuality.current = connectInitialQuality ?? null;
        client.sendQuality(connectInitialQuality ?? 0, connectInitialFps ?? 0);
      }
    };
    client.onStats = setStats;
    client.onCursor = (next) => {
      setCursor((previous) => ({
        position: next.position,
        hotspot: next.hotspot,
        bitmap: next.bitmap ?? previous.bitmap,
      }));
    };
    client.onResolutionStatus = (status) => {
      if (resolutionStatusTimer.current) clearTimeout(resolutionStatusTimer.current);
      resolutionStatusTimer.current = null;
      setResolutionStatus(status);
      if (status.state === "succeeded") {
        resolutionStatusTimer.current = setTimeout(() => {
          resolutionStatusTimer.current = null;
          setResolutionStatus(null);
        }, 5000);
      }
    };
    client.onKeyframePainted = clearResolutionStatus;
    client.onError = (message) => {
      setClientError(message);
      setConnectionState("failed");
    };
    client.onSessionId = setSessionId;
    client.onSessionEnded = (reason) => {
      releaseAllInput();
      setEndedReason(reason);
    };
    client.connect().catch(console.error);
    const canvas = canvasRef.current;
    const closeOnPageHide = () => client.disconnect();
    window.addEventListener("pagehide", closeOnPageHide);
    return () => {
      window.removeEventListener("pagehide", closeOnPageHide);
      releaseAllInput();
      if (resolutionStatusTimer.current) clearTimeout(resolutionStatusTimer.current);
      resolutionStatusTimer.current = null;
      if (document.pointerLockElement === canvas) document.exitPointerLock();
      client.disconnect();
      clientRef.current = null;
    };
  }, [machine.id, releaseAllInput, clearResolutionStatus]);

  useEffect(() => {
    const client = clientRef.current;
    if (!client) return;
    if (connectionState === "connected" && resolution) {
      const currentResolution = client.requestedResolution;
      if (!currentResolution || currentResolution.width !== resolution.width || currentResolution.height !== resolution.height) {
        client.requestResolution(resolution.width, resolution.height);
      }
    }
    const nextFps = initialFps ?? null;
    const nextQuality = initialQuality ?? null;
    if (committedFps.current === nextFps && committedQuality.current === nextQuality) return;
    setFpsCap(nextFps);
    setQualityLevel(nextQuality);
    if (connectionState !== "connected") return;
    committedFps.current = nextFps;
    committedQuality.current = nextQuality;
    client.sendQuality(nextQuality ?? 0, nextFps ?? 0);
  }, [machine.id, resolution, initialFps, initialQuality, connectionState]);

  useEffect(() => {
    const updateLayout = () => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const rect = canvas.getBoundingClientRect();
      setVideoLayout({
        width: canvas.width,
        height: canvas.height,
        boxWidth: rect.width,
        boxHeight: rect.height,
      });
    };
    updateLayout();
    const canvas = canvasRef.current;
    const observer = canvas ? new ResizeObserver(updateLayout) : null;
    if (canvas && observer) observer.observe(canvas);
    window.addEventListener("resize", updateLayout);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", updateLayout);
    };
  }, [isFullscreen, stats?.width, stats?.height]);

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
    if (endedReason) {
      window.location.reload();
      return;
    }
    setClientError(null);
    clientRef.current?.connect().catch(console.error);
  }, [endedReason]);

  const changeQuality = useCallback((value: number) => {
    setQualityLevel(value === 0 ? null : value);
  }, []);

  const changeFpsCap = useCallback((value: number) => {
    setFpsCap(value === 0 ? null : value);
  }, []);

  const commitControls = useCallback(() => {
    if (committedQuality.current === qualityLevel && committedFps.current === fpsCap) return;
    committedQuality.current = qualityLevel;
    committedFps.current = fpsCap;
    clientRef.current?.sendQuality(qualityLevel ?? 0, fpsCap ?? 0);
  }, [fpsCap, qualityLevel]);
  const changeResolution = useCallback((width: number, height: number) => {
    clientRef.current?.requestResolution(width, height);
    window.history.replaceState(null, "", buildConnectUrl(
      machine.id,
      undefined,
      { width, height, fps: fpsCap ?? undefined, quality: qualityLevel ?? undefined },
    ));
  }, [fpsCap, machine.id, qualityLevel]);

  const requestPointerLock = useCallback(() => {
    canvasRef.current?.requestPointerLock();
  }, []);

  const togglePointerLock = useCallback(() => {
    const locked = document.pointerLockElement === canvasRef.current;
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
    if (!canvasRef.current) return null;
    const canvas = canvasRef.current;
    const rect = canvas.getBoundingClientRect();

    const geometry = getContainedVideoGeometry(
      rect.width,
      rect.height,
      canvas.width,
      canvas.height,
    );
    if (!geometry) return null;

    const relativeX = e.clientX - rect.left - geometry.offsetX;
    const relativeY = e.clientY - rect.top - geometry.offsetY;

    // Normalize and clamp to 0.0 - 1.0
    const x = Math.max(0, Math.min(1, relativeX / geometry.renderWidth));
    const y = Math.max(0, Math.min(1, relativeY / geometry.renderHeight));

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

    if (isLocalControlTarget(e.target)) return;

    // Input capture
    if (!clientRef.current || !canvasRef.current) return;

    // Only send move if we have valid video dimensions
    if (canvasRef.current.width === 0) return;

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
    if (isLocalControlTarget(e.target)) return;
    if (!clientRef.current) return;
    containerRef.current?.focus({ preventScroll: true });
    e.preventDefault();
    const button = mapMouseButton(e.button);
    if (button) {
      flushMove();
      clientRef.current.sendInput({
        Mouse: {
          ButtonDown: { button }
        }
      });
      heldButtons.current.add(button);
    }
  }, [flushMove]);

  const handleMouseUp = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (isLocalControlTarget(e.target)) return;
    if (!clientRef.current) return;
    e.preventDefault();
    const button = mapMouseButton(e.button);
    if (button) {
      if (!heldButtons.current.has(button)) return;
      flushMove();
      clientRef.current.sendInput({
        Mouse: {
          ButtonUp: { button }
        }
      });
      heldButtons.current.delete(button);
    }
  }, [flushMove]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const handleWheel = (e: WheelEvent) => {
      if (isLocalControlTarget(e.target)) return;
      if (!clientRef.current) return;
      e.preventDefault();
      flushMove();
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
  }, [flushMove]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const handleTouchStart = (e: TouchEvent) => {
      revealControls();
      if (isLocalControlTarget(e.target)) return;
      e.preventDefault();
    };
    container.addEventListener("touchstart", handleTouchStart, { passive: false });
    return () => container.removeEventListener("touchstart", handleTouchStart);
  }, [revealControls]);

  const handleSurfaceClick = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (
      pointerLockEnabled &&
      !isPointerLocked &&
      (e.target === containerRef.current || e.target === canvasRef.current)
    ) {
      requestPointerLock();
    }
  }, [isPointerLocked, pointerLockEnabled, requestPointerLock]);

  useEffect(() => {
    const handlePointerLockChange = () => {
      const locked = document.pointerLockElement === canvasRef.current;
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
      if (isLocalKeyboardTarget(e.target)) return;

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
              modifiers: modifierFlags(e)
            }
          }
        });
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      if (isLocalKeyboardTarget(e.target)) return;

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
              modifiers: modifierFlags(e)
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

  const visibleControls = showControls || connectionState !== "connected" || !!endedReason;

  return (
    <div
      ref={containerRef}
      data-session-id={sessionId ?? undefined}
      tabIndex={-1}
      className="relative h-screen min-h-screen w-screen overflow-hidden bg-black select-none outline-none"
      onMouseMove={handleMouseMove}
      onMouseDown={handleMouseDown}
      onMouseUp={handleMouseUp}
      onClick={handleSurfaceClick}
      onContextMenu={(event) => event.preventDefault()}
      style={{ cursor: isPointerLocked || !showControls || (connectionState === "connected" && videoHovered) ? "none" : "default" }}
    >
      <div className="absolute inset-0 bg-black" onMouseEnter={() => setVideoHovered(true)} onMouseLeave={() => setVideoHovered(false)}>
        <canvas ref={canvasRef} className="stream-canvas absolute inset-0 h-full w-full bg-black object-contain pointer-events-none" />
        <CursorOverlay cursor={cursor} layout={videoLayout} />
      </div>

      <Toolbar
        machine={machine}
        stats={stats}
        visible={visibleControls}
        showStats={showStats}
        pointerLockEnabled={pointerLockEnabled}
        isPointerLocked={isPointerLocked}
        isFullscreen={isFullscreen}
        qualityLevel={qualityLevel}
        fpsCap={fpsCap}
        onBack={onBack}
        onReconnect={reconnect}
        onToggleStats={() => setShowStats((value) => !value)}
        onPointerLock={togglePointerLock}
        onFullscreen={toggleFullscreen}
        onShortcuts={() => setShowShortcuts((value) => !value)}
        onQualityChange={changeQuality}
        onFpsChange={changeFpsCap}
        onCommit={commitControls}
        onResolution={changeResolution}
      />

      {showShortcuts && (
        <div data-local-control className="absolute right-4 top-[5.5rem] z-40 w-[min(20rem,calc(100vw-2rem))] rounded-2xl border border-white/10 bg-[#101318]/95 p-4 shadow-2xl backdrop-blur-2xl">
          <div className="mb-3 flex items-center justify-between">
            <div><p className="text-sm font-semibold text-zinc-100">Keyboard shortcuts</p><p className="mt-1 text-[10px] text-zinc-600">Local viewer controls</p></div>
            <button data-local-control aria-label="Close shortcuts" onClick={() => setShowShortcuts(false)} className="rounded-lg px-2 py-1 text-zinc-500 hover:bg-white/5 hover:text-white">✕</button>
          </div>
          <div className="space-y-2 text-xs text-zinc-400">
            <ShortcutRow keys="Ctrl + Alt + Shift + R" label="Reconnect" />
            <ShortcutRow keys="Ctrl + Alt + Shift + S" label="Toggle stats" />
            <ShortcutRow keys="Ctrl + Alt + Shift + F" label="Toggle fullscreen" />
            <ShortcutRow keys="Esc" label="Exit pointer lock" />
          </div>
        </div>
      )}

      <StatsHud stats={stats} enabled={showStats && connectionState === "connected" && !endedReason} />
      <ConnectionOverlay
        state={connectionState}
        machineName={machine.display_name || machine.name}
        error={clientError}
        endedReason={endedReason}
        resolutionStatus={resolutionStatus}
        onReconnect={reconnect}
        onBack={onBack}
      />
    </div>
  );
}

function ShortcutRow({ keys, label }: { keys: string; label: string }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span>{label}</span>
      <kbd className="rounded-md border border-white/10 bg-black/30 px-2 py-1 font-mono text-[10px] text-zinc-300">{keys}</kbd>
    </div>
  );
}
