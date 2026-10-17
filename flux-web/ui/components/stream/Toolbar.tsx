"use client";

import { useState } from "react";
import type { Machine } from "@/lib/api";
import type { StreamStats } from "@/lib/webtransport-client";
import { RESOLUTION_OPTIONS } from "@/components/machines/LaunchDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Slider } from "@/components/ui/slider";
import { targetBitrateKbps } from "@/lib/protocol";

export function Toolbar({ machine, stats, visible, showStats, pointerLockEnabled, isPointerLocked, isFullscreen,
  qualityLevel, fpsCap, onBack, onReconnect, onToggleStats, onPointerLock, onFullscreen, onShortcuts,
  onQualityChange, onFpsChange, onCommit, onResolution }: {
  machine: Machine;
  stats: StreamStats | null;
  visible: boolean;
  showStats: boolean;
  pointerLockEnabled: boolean;
  isPointerLocked: boolean;
  isFullscreen: boolean;
  qualityLevel: number | null;
  fpsCap: number | null;
  onBack: () => void;
  onReconnect: () => void;
  onToggleStats: () => void;
  onPointerLock: () => void;
  onFullscreen: () => void;
  onShortcuts: () => void;
  onQualityChange: (value: number) => void;
  onFpsChange: (value: number) => void;
  onCommit: () => void;
  onResolution: (width: number, height: number) => void;
}) {
  const [settingsOpen, setSettingsOpen] = useState(false);
  const width = stats?.width || machine.width || 1920;
  const height = stats?.height || machine.height || 1080;
  const fps = fpsCap ?? machine.default_fps ?? 48;
  const estimate = (level: number) => `${(targetBitrateKbps(level, width, height, fps) / 1000).toFixed(1)} Mbps`;

  return (
    <div data-local-control className={`absolute left-1/2 top-4 z-30 -translate-x-1/2 transition-all duration-300 ${visible ? "translate-y-0 opacity-100" : "-translate-y-5 pointer-events-none opacity-0"}`}>
      <div data-local-control className="flex max-w-[calc(100vw-1.25rem)] items-center gap-1.5 rounded-2xl border border-white/10 bg-[#101318]/90 p-1.5 shadow-2xl shadow-black/40 backdrop-blur-2xl sm:gap-2 sm:p-2">
        <div className="hidden items-center gap-2 px-2 sm:flex">
          <span className={`h-2 w-2 rounded-full ${stats ? "bg-emerald-400 shadow-[0_0_9px_rgba(52,211,153,.8)]" : "animate-pulse bg-amber-300"}`} />
          <span className="max-w-40 truncate text-xs font-semibold text-zinc-100">{machine.display_name || machine.name}</span>
          <span className="font-mono text-[10px] text-zinc-500">{stats?.controlRttMs != null ? `${Math.round(stats.controlRttMs)}ms` : "—"}</span>
        </div>
        <span className="hidden h-5 w-px bg-white/10 sm:block" />
        <Button aria-label="Reconnect" title="Reconnect · Ctrl+Alt+Shift+R" onClick={onReconnect} className="h-9 w-9 rounded-xl p-0 text-base">↻</Button>
        <Button aria-label="Toggle stats" aria-pressed={showStats} title="Stats · Ctrl+Alt+Shift+S" onClick={onToggleStats}
          className={`h-9 w-9 rounded-xl p-0 ${showStats ? "bg-blue-400/15 text-blue-200" : ""}`}>⌁</Button>
        <Button aria-label={isPointerLocked ? "Exit pointer lock" : "Lock pointer"} aria-pressed={pointerLockEnabled}
          title={isPointerLocked ? "Exit pointer lock · Esc" : "Pointer lock"} onClick={onPointerLock}
          className={`h-9 w-9 rounded-xl p-0 ${pointerLockEnabled ? "bg-blue-400/15 text-blue-200" : ""}`}>◎</Button>
        <Popover open={settingsOpen} onOpenChange={setSettingsOpen}>
          <PopoverTrigger asChild>
            <Button aria-label="Stream settings" aria-expanded={settingsOpen} className={`h-9 w-9 rounded-xl p-0 ${settingsOpen ? "bg-white/10 text-white" : ""}`}>⚙</Button>
          </PopoverTrigger>
          <PopoverContent>
            <div className="mb-4">
              <p className="text-sm font-semibold text-white">Stream settings</p>
              <p className="mt-1 text-[11px] text-zinc-500">These controls affect all viewers on this machine.</p>
            </div>
            <div className="space-y-5">
              <Slider label="Quality" min={0} max={10} value={qualityLevel ?? 0} onChange={onQualityChange}
                valueLabel={qualityLevel === null ? `Auto · ${estimate(6)}` : `${qualityLevel} · ${estimate(qualityLevel)}`} />
              <Slider label="Frame rate" min={0} max={144} value={fpsCap ?? 0} onChange={onFpsChange}
                valueLabel={fpsCap === null ? `Auto · ${machine.default_fps || 48} FPS` : `${fpsCap} FPS`} />
              <Button variant="primary" className="w-full" onClick={onCommit}>Apply stream settings</Button>
              {machine.virtual_display && (
                <div className="border-t border-white/[0.06] pt-4">
                  <p className="mb-2 text-xs font-semibold text-zinc-300">Change resolution</p>
                  <div className="grid grid-cols-2 gap-1.5">
                    {RESOLUTION_OPTIONS.map((mode) => (
                      <button key={`${mode.width}x${mode.height}`} data-local-control onClick={() => {
                        setSettingsOpen(false);
                        onResolution(mode.width, mode.height);
                      }}
                        className="rounded-lg border border-white/[0.06] px-2 py-2 text-[10px] font-medium text-zinc-400 hover:border-blue-400/30 hover:bg-blue-400/[0.06] hover:text-blue-100">
                        {mode.width} × {mode.height}
                      </button>
                    ))}
                  </div>
                  <p className="mt-2 text-[10px] leading-4 text-zinc-600">Changes apply without reconnecting.</p>
                </div>
              )}
            </div>
          </PopoverContent>
        </Popover>
        <Button aria-label="Keyboard shortcuts" title="Keyboard shortcuts" onClick={onShortcuts} className="hidden h-9 w-9 rounded-xl p-0 sm:inline-flex">?</Button>
        <Button aria-label={isFullscreen ? "Exit fullscreen" : "Fullscreen"} title="Fullscreen · Ctrl+Alt+Shift+F" onClick={onFullscreen} className="hidden h-9 w-9 rounded-xl p-0 sm:inline-flex">{isFullscreen ? "↙" : "↗"}</Button>
        <Button variant="danger" onClick={onBack} className="h-9 px-3 text-xs sm:px-4">Disconnect</Button>
      </div>
      <div className="mt-2 flex justify-center sm:hidden">
        <Badge tone="neutral">{stats?.controlRttMs != null ? `${Math.round(stats.controlRttMs)} ms RTT` : "Connecting"}</Badge>
      </div>
    </div>
  );
}
