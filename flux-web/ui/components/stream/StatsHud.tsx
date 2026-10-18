"use client";

import { useState } from "react";
import type { StreamStats } from "@/lib/webtransport-client";

function bitrate(kbps: number): string {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${kbps} kbps`;
}

function Stat({ label, value, hot = false }: { label: string; value: string; hot?: boolean }) {
  return (
    <div className="rounded-lg border border-white/[0.05] bg-black/20 px-2.5 py-2">
      <p className="text-[9px] font-bold uppercase tracking-[.13em] text-zinc-600">{label}</p>
      <p className={`mt-1 truncate font-mono text-xs font-semibold ${hot ? "text-amber-200" : "text-zinc-200"}`}>{value}</p>
    </div>
  );
}

export function StatsHud({ stats, enabled }: { stats: StreamStats | null; enabled: boolean }) {
  const [expanded, setExpanded] = useState(false);
  if (!enabled) return null;
  return (
    <section data-local-control className="absolute bottom-4 left-4 z-20 w-[min(21rem,calc(100vw-2rem))] select-text rounded-2xl border border-white/10 bg-[#101318]/85 p-3 shadow-xl backdrop-blur-2xl sm:bottom-6 sm:left-6">
      <div className="flex items-center gap-4">
        <Mini label="FPS" value={stats ? stats.fps.toFixed(0) : "—"} />
        <Mini label="Bitrate" value={stats ? bitrate(stats.bitrate) : "—"} />
        <Mini label="RTT" value={stats?.mediaRttMs != null ? `${stats.mediaRttMs.toFixed(0)}ms` : "—"} />
        <Mini label="Loss" value={stats ? `${stats.packetsLost}` : "—"} hot={!!stats?.packetsLost} />
        <button data-local-control aria-expanded={expanded} aria-label={expanded ? "Collapse stats" : "Expand stats"} onClick={() => setExpanded((value) => !value)}
          className="ml-auto grid h-7 w-7 shrink-0 place-items-center rounded-lg text-zinc-500 hover:bg-white/[0.06] hover:text-white">
          {expanded ? "−" : "+"}
        </button>
      </div>
      <p className="mt-2 text-[9px] font-bold uppercase tracking-[.15em] text-zinc-600">WebTransport</p>
      {expanded && (
        <div className="mt-3 grid grid-cols-2 gap-1.5 border-t border-white/[0.06] pt-3">
          <Stat label="Resolution" value={stats ? `${stats.width} × ${stats.height}` : "—"} />
          <Stat label="Received" value={stats ? `${(stats.bytesReceived / 1048576).toFixed(1)} MB` : "—"} />
          <Stat label="Media RTT" value={stats?.mediaRttMs != null ? `${stats.mediaRttMs.toFixed(0)} ms` : "—"} />
          <Stat label="Input RTT" value={stats?.controlRttMs != null ? `${stats.controlRttMs.toFixed(0)} ms` : "—"} />
          <Stat label="Receive → paint" value={stats?.receiveToPaintMs != null ? `${stats.receiveToPaintMs.toFixed(1)} ms` : "—"} />
          <Stat label="Decode" value={stats?.decodeMs != null ? `${stats.decodeMs.toFixed(1)} ms` : "—"} />
          <Stat label="Frames dropped" value={stats ? `${stats.framesDropped}` : "—"} hot={!!stats?.framesDropped} />
          {stats?.audioBufferedMs !== undefined && (
            <Stat label="Audio" value={`${Math.round(stats.audioBufferedMs)} ms buf · ${stats.audioUnderruns ?? 0} underruns`} />
          )}
        </div>
      )}
    </section>
  );
}

function Mini({ label, value, hot = false }: { label: string; value: string; hot?: boolean }) {
  return (
    <div className="min-w-0">
      <p className="text-[8px] font-bold uppercase tracking-[.12em] text-zinc-600">{label}</p>
      <p className={`mt-0.5 truncate font-mono text-[11px] font-semibold ${hot ? "text-amber-200" : "text-zinc-200"}`}>{value}</p>
    </div>
  );
}
