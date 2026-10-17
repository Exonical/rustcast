"use client";

import { useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Slider } from "@/components/ui/slider";
import type { Machine } from "@/lib/api";
import { buildConnectUrl } from "@/lib/launch";
import { targetBitrateKbps } from "@/lib/protocol";

export const RESOLUTION_OPTIONS = [
  { width: 2560, height: 1440, ratio: "QHD" },
  { width: 1920, height: 1200, ratio: "16:10" },
  { width: 1920, height: 1080, ratio: "Full HD" },
  { width: 1600, height: 900, ratio: "HD+" },
  { width: 1366, height: 768, ratio: "Wide" },
  { width: 1280, height: 720, ratio: "HD" },
  { width: 2560, height: 1080, ratio: "Ultrawide" },
];

interface Choices {
  width: number;
  height: number;
  fps: number;
  quality: number;
}

function initialChoices(machine: Machine): Choices {
  const native = machine.width && machine.height
    && RESOLUTION_OPTIONS.some((option) => option.width === machine.width && option.height === machine.height)
    ? { width: machine.width, height: machine.height }
    : { width: 1920, height: 1080 };
  return {
    width: native.width,
    height: native.height,
    fps: 0,
    quality: 0,
  };
}

function readChoices(machine: Machine | null): Choices {
  if (!machine || typeof window === "undefined") return { width: 1920, height: 1080, fps: 0, quality: 0 };
  const fallback = initialChoices(machine);
  try {
    const saved = localStorage.getItem(`flux.launch.${machine.id}`);
    const parsed = saved ? JSON.parse(saved) as Partial<Choices> : null;
    return {
      width: parsed?.width ?? fallback.width,
      height: parsed?.height ?? fallback.height,
      fps: parsed?.fps ?? 0,
      quality: parsed?.quality ?? 0,
    };
  } catch {
    return fallback;
  }
}

export function LaunchDialog({ machine, open, onOpenChange }: {
  machine: Machine | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const router = useRouter();
  const [choices, setChoices] = useState<Choices>(() => readChoices(machine));
  const native = useMemo(() => machine && machine.width && machine.height
    ? { width: machine.width, height: machine.height }
    : undefined, [machine]);

  if (!machine) return null;
  const width = machine.virtual_display ? choices.width : native?.width || 1920;
  const height = machine.virtual_display ? choices.height : native?.height || 1080;
  const estimated = choices.quality
    ? (targetBitrateKbps(choices.quality, width, height, choices.fps || machine.default_fps || 48) / 1000).toFixed(1)
    : null;

  const start = () => {
    localStorage.setItem(`flux.launch.${machine.id}`, JSON.stringify(choices));
    router.push(buildConnectUrl(machine.id, native, {
      width: machine.virtual_display ? choices.width : native?.width,
      height: machine.virtual_display ? choices.height : native?.height,
      fps: choices.fps,
      quality: choices.quality,
    }));
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <div className="mb-7 flex items-start justify-between gap-4">
          <div>
            <p className="text-[10px] font-bold uppercase tracking-[.2em] text-blue-300">New stream</p>
            <DialogTitle className="mt-2 text-2xl font-semibold tracking-tight">Launch workspace</DialogTitle>
            <p className="mt-1 text-sm text-zinc-500">{machine.display_name || machine.name}</p>
            <DialogDescription className="sr-only">Choose a mode and stream quality for this Flux workspace.</DialogDescription>
          </div>
          <button data-local-control aria-label="Close launch dialog" onClick={() => onOpenChange(false)} className="rounded-lg px-2 py-1 text-zinc-500 hover:bg-white/5 hover:text-white">✕</button>
        </div>

        {machine.virtual_display ? (
          <section data-local-control>
            <h3 className="mb-3 text-xs font-semibold text-zinc-300">Stream resolution</h3>
            <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
              {RESOLUTION_OPTIONS.map((mode) => {
                const selected = mode.width === choices.width && mode.height === choices.height;
                return (
                  <button key={`${mode.width}x${mode.height}`} data-local-control
                    onClick={() => setChoices((current) => ({ ...current, width: mode.width, height: mode.height }))}
                    className={`rounded-xl border p-3 text-left transition ${selected ? "border-blue-400/50 bg-blue-400/10 text-white" : "border-white/[0.07] bg-white/[0.02] text-zinc-400 hover:border-white/15"}`}>
                    <span className="block text-sm font-semibold">{mode.width} × {mode.height}</span>
                    <span className="mt-1 block text-[10px] text-zinc-600">{mode.ratio}</span>
                  </button>
                );
              })}
            </div>
          </section>
        ) : (
          <section className="rounded-xl border border-white/[0.07] bg-white/[0.025] p-4">
            <p className="text-[10px] font-bold uppercase tracking-[.15em] text-zinc-600">Native display mode</p>
            <p className="mt-2 font-mono text-sm text-zinc-200">{native ? `${native.width} × ${native.height}` : "Resolution managed by host"}</p>
          </section>
        )}

        <section className="mt-7 grid gap-5 sm:grid-cols-2">
          <label data-local-control className="block space-y-2">
            <span className="text-xs font-semibold text-zinc-300">Frame rate</span>
            <select data-local-control value={choices.fps} onChange={(event) => {
              const fps = Number(event.currentTarget.value);
              setChoices((current) => ({ ...current, fps }));
            }}
              className="w-full rounded-xl border border-white/10 bg-[#171a20] px-3 py-2.5 text-sm text-zinc-100 outline-none focus:border-blue-400/50">
              {[0, 30, 60, 120, 144].map((fps) => <option key={fps} value={fps}>{fps === 0 ? `Auto (${machine.default_fps || 48} FPS)` : `${fps} FPS`}</option>)}
            </select>
          </label>
          <div data-local-control className="rounded-xl border border-white/[0.06] bg-white/[0.02] px-3 py-2.5">
            <Slider
              label="Quality"
              min={0}
              max={10}
              value={choices.quality}
              onChange={(quality) => setChoices((current) => ({ ...current, quality }))}
              valueLabel={choices.quality === 0
                ? `Auto · ${((targetBitrateKbps(6, width, height, choices.fps || machine.default_fps || 48)) / 1000).toFixed(1)} Mbps`
                : `Level ${choices.quality} · ${(targetBitrateKbps(choices.quality, width, height, choices.fps || machine.default_fps || 48) / 1000).toFixed(1)} Mbps`}
            />
          </div>
        </section>
        <p className="mt-4 rounded-xl border border-blue-400/10 bg-blue-400/[0.04] px-3.5 py-3 text-xs leading-5 text-zinc-500">
          Quality and frame rate are machine-wide and affect every active viewer.
          {estimated && <span className="ml-1 text-blue-200">Estimated target: {estimated} Mbps.</span>}
        </p>
        <div className="mt-7 flex justify-end gap-2">
          <Button onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button variant="primary" onClick={start}>Start session <span aria-hidden="true">→</span></Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
