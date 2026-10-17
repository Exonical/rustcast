"use client";

import type { ConnectionState, ResolutionStatus } from "@/lib/webtransport-client";
import { Button } from "@/components/ui/button";

export function ConnectionOverlay({ state, machineName, error, endedReason, resolutionStatus, onReconnect, onBack }: {
  state: ConnectionState;
  machineName: string;
  error: string | null;
  endedReason: string | null;
  resolutionStatus: ResolutionStatus | null;
  onReconnect: () => void;
  onBack: () => void;
}) {
  return (
    <>
      {resolutionStatus?.state === "transitioning" && (
        <div data-local-control className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-amber-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-amber-100 shadow-xl backdrop-blur-xl">
          Applying display mode · the stream will reconnect briefly
        </div>
      )}
      {resolutionStatus?.state === "failed" && (
        <div data-local-control role="alert" className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-red-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-red-100 shadow-xl backdrop-blur-xl">
          Resolution change failed{resolutionStatus.error ? `: ${resolutionStatus.error}` : "."}
        </div>
      )}
      {(state !== "connected" || endedReason) && (
        <div data-local-control className="absolute inset-0 z-10 grid place-items-center bg-black/20 px-4 backdrop-blur-[2px]">
          <section className="w-full max-w-md rounded-3xl border border-white/10 bg-[#111318]/95 p-7 text-center shadow-2xl sm:p-9">
            <div className={`mx-auto grid h-14 w-14 place-items-center rounded-2xl ${endedReason ? "bg-amber-300/10 text-amber-200" : state === "connected" ? "bg-emerald-300/10 text-emerald-200" : "bg-blue-300/10 text-blue-200"}`}>
              {endedReason ? "Ⅱ" : state === "connected" ? "✓" : state === "failed" ? "!" : "◌"}
            </div>
            <p className="mt-5 text-xl font-semibold text-white">
              {endedReason ? "Session ended" : state === "connecting" ? "Opening your workspace" : state === "failed" ? "Couldn’t connect" : "Stream disconnected"}
            </p>
            <p className="mx-auto mt-2 max-w-sm text-sm leading-6 text-zinc-500">
              {endedReason ? `${endedReason} · ${machineName}` : error || (state === "connecting"
                ? `Reaching ${machineName}. This usually takes a few seconds.`
                : "The connection was interrupted. Flux will try again automatically.")}
            </p>
            {(state !== "connecting" || endedReason) && (
              <div className="mt-6 flex flex-col justify-center gap-2 sm:flex-row">
                <Button variant="primary" onClick={onReconnect}>{endedReason ? "Reconnect" : "Try again"}</Button>
                <Button onClick={onBack}>Back to machines</Button>
              </div>
            )}
          </section>
        </div>
      )}
    </>
  );
}
