"use client";

import { useMemo, useState } from "react";
import { AppShell } from "@/components/shell/AppShell";
import { PageHeader } from "@/components/shell/PageHeader";
import { MachineCard } from "@/components/machines/MachineCard";
import { LaunchDialog } from "@/components/machines/LaunchDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { EmptyState } from "@/components/ui/empty-state";
import { Skeleton } from "@/components/ui/skeleton";
import { useMachines, useSessions, type Machine } from "@/lib/api";
import { dedupeDefaultMachine, filterMachines, machineState, type MachineFilter } from "@/lib/machines";

const filters: { value: MachineFilter; label: string }[] = [
  { value: "all", label: "All machines" },
  { value: "available", label: "Available" },
  { value: "in-use", label: "In use" },
  { value: "offline", label: "Offline" },
];

export function MachinesPage() {
  const machines = useMachines();
  const sessions = useSessions();
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<MachineFilter>("all");
  const [launchMachine, setLaunchMachine] = useState<Machine | null>(null);
  const items = useMemo(() => machines.data ?? [], [machines.data]);
  const uniqueItems = useMemo(() => dedupeDefaultMachine(items), [items]);
  const visible = useMemo(() => filterMachines(items, query, filter), [items, query, filter]);
  const online = uniqueItems.filter((machine) => machine.status === "online").length;
  const available = uniqueItems.filter((machine) => machineState(machine) === "available").length;

  return (
    <AppShell relayOnline={!machines.loading && !machines.error} activeSessions={sessions.data?.length ?? 0}>
      <PageHeader
        title="Machines"
        description="Connect to your remote workspaces."
        actions={<Button onClick={() => void machines.refresh()}><span aria-hidden="true">↻</span> Refresh</Button>}
      />
      <section className="mb-7 grid gap-3 sm:grid-cols-3">
        <Metric label="Machines online" value={machines.loading ? "—" : String(online)} caption="Heartbeat status" tone="blue" />
        <Metric label="Active sessions" value={sessions.loading ? "—" : String(sessions.data?.length ?? 0)} caption="Live viewers" tone="purple" />
        <Metric label="Available" value={machines.loading ? "—" : String(available)} caption="Ready to connect" tone="green" />
      </section>

      <div data-local-control className="mb-5 flex flex-col gap-3 xl:flex-row xl:items-center xl:justify-between">
        <label className="relative block w-full max-w-md">
          <span className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-zinc-600">⌕</span>
          <input
            data-local-control
            value={query}
            onChange={(event) => setQuery(event.currentTarget.value)}
            placeholder="Search machines, GPU, encoder…"
            className="w-full rounded-xl border border-white/[0.07] bg-[#111318] py-2.5 pl-9 pr-3 text-sm text-zinc-100 outline-none placeholder:text-zinc-600 focus:border-blue-400/30"
          />
        </label>
        <div className="flex max-w-full gap-1 overflow-x-auto rounded-xl border border-white/[0.055] bg-white/[0.025] p-1">
          {filters.map((option) => (
            <button key={option.value} data-local-control aria-pressed={filter === option.value}
              onClick={() => setFilter(option.value)}
              className={`whitespace-nowrap rounded-lg px-3 py-2 text-xs font-medium transition ${filter === option.value ? "bg-white/[0.09] text-white shadow-sm" : "text-zinc-500 hover:text-zinc-200"}`}>
              {option.label}
            </button>
          ))}
        </div>
      </div>

      {machines.loading && !machines.data ? (
        <div className="grid gap-4 md:grid-cols-2 2xl:grid-cols-3">
          {Array.from({ length: 6 }, (_, i) => <Skeleton key={i} className="h-[260px] rounded-2xl" />)}
        </div>
      ) : machines.error && !machines.data ? (
        <Card className="p-10 text-center">
          <div className="text-2xl text-red-300">!</div>
          <h2 className="mt-3 font-semibold">Relay unavailable</h2>
          <p className="mt-2 text-sm text-zinc-500">{machines.error}</p>
          <Button variant="primary" onClick={() => void machines.refresh()} className="mt-5">Retry</Button>
        </Card>
      ) : visible.length === 0 ? (
        <EmptyState
          title={items.length ? "No matching machines" : "No machines registered"}
          detail={items.length ? "Try another search or status filter." : "Start Flux on a host to make its workspace available here."}
        />
      ) : (
        <div className="grid auto-rows-fr gap-4 md:grid-cols-2 2xl:grid-cols-3">
          {visible.map((machine) => (
            <MachineCard key={machine.id} machine={machine} onConnect={setLaunchMachine} />
          ))}
        </div>
      )}
      <p className="mt-6 text-[11px] text-zinc-700">Machine status refreshes every 5 seconds.</p>
      <LaunchDialog key={launchMachine?.id ?? "none"} machine={launchMachine} open={!!launchMachine} onOpenChange={(open) => { if (!open) setLaunchMachine(null); }} />
    </AppShell>
  );
}

function Metric({ label, value, caption, tone }: { label: string; value: string; caption: string; tone: "blue" | "purple" | "green" }) {
  const colors = {
    blue: "text-blue-200 bg-blue-400/10",
    purple: "text-violet-200 bg-violet-400/10",
    green: "text-emerald-200 bg-emerald-400/10",
  };
  return (
    <Card className="flex items-center gap-4 p-4 sm:p-5">
      <span className={`grid h-10 w-10 place-items-center rounded-xl text-lg ${colors[tone]}`}>◌</span>
      <div className="min-w-0 flex-1">
        <p className="text-[10px] font-bold uppercase tracking-[.14em] text-zinc-600">{label}</p>
        <div className="mt-1 flex items-baseline justify-between gap-2">
          <p className="text-2xl font-semibold tracking-tight text-zinc-100">{value}</p>
          <Badge tone={tone === "green" ? "green" : tone === "purple" ? "blue" : "neutral"} className="hidden sm:inline-flex">{caption}</Badge>
        </div>
      </div>
    </Card>
  );
}
