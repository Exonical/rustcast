"use client";

import Link from "next/link";
import { machineState } from "@/lib/machines";
import type { Machine } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";

export function MachineCard({ machine, onConnect }: {
  machine: Machine;
  onConnect: (machine: Machine) => void;
}) {
  const state = machineState(machine);
  const online = state !== "offline";
  const display = machine.width && machine.height ? `${machine.width} × ${machine.height}` : "Native mode";
  return (
    <Card className="group flex min-h-[250px] flex-col p-5 transition hover:border-white/[0.14] hover:bg-[#14171d]">
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-center gap-3">
          <div className="grid h-11 w-11 shrink-0 place-items-center rounded-xl border border-white/[0.07] bg-white/[0.035] text-lg text-blue-200">
            {machine.os?.toLowerCase().includes("windows") ? "⊞" : machine.os?.toLowerCase().includes("mac") ? "⌘" : "◈"}
          </div>
          <div className="min-w-0">
            <h2 className="truncate text-base font-semibold text-zinc-100">{machine.display_name || machine.name}</h2>
            <p className="mt-0.5 truncate text-xs text-zinc-500">{machine.name}</p>
          </div>
        </div>
        <Badge tone={state === "available" ? "green" : state === "in-use" ? "amber" : "neutral"}>
          <span className={`h-1.5 w-1.5 rounded-full ${state === "available" ? "bg-emerald-400" : state === "in-use" ? "bg-amber-300" : "bg-zinc-500"}`} />
          {state === "available" ? "Online" : state === "in-use" ? "In use" : "Offline"}
        </Badge>
      </div>

      <div className="my-5 grid grid-cols-2 gap-x-4 gap-y-3 border-y border-white/[0.055] py-4">
        <Spec label="Operating system" value={machine.os || "Unknown"} />
        <Spec label="Graphics" value={machine.gpu_vendor || "Unknown"} />
        <Spec label="Encoder" value={machine.encoder_backend || "Unknown"} />
        <Spec label="Display" value={`${display}${machine.target_fps ? ` · ${machine.target_fps} FPS` : ""}`} />
        <Spec label="Display mode" value={machine.virtual_display ? "Virtual" : "Physical"} />
        <Spec label="Viewers" value={String(machine.viewers)} />
      </div>

      <div className="mt-auto flex items-center gap-2">
        <span title={state === "in-use" ? "In use by another viewer" : undefined} className="flex-1">
          <Button
            variant="primary"
            disabled={!online || state === "in-use"}
            onClick={() => onConnect(machine)}
            className="w-full"
          >
            <span>{state === "offline" ? "Offline" : state === "in-use" ? "In use" : "Connect"}</span>
            {state === "available" && <span aria-hidden="true">↗</span>}
          </Button>
        </span>
        {state === "in-use" && (
          <Link href="/sessions/" className="shrink-0 rounded-xl border border-white/10 bg-white/[0.04] px-3 py-2.5 text-xs font-semibold text-zinc-200 hover:bg-white/[0.08]">View session</Link>
        )}
      </div>
    </Card>
  );
}

function Spec({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <p className="text-[9px] font-bold uppercase tracking-[.13em] text-zinc-600">{label}</p>
      <p className="mt-1 truncate text-xs font-medium text-zinc-300">{value}</p>
    </div>
  );
}
