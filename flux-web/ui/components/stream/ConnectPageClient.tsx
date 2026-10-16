"use client";

import { useRouter, useSearchParams } from "next/navigation";
import { useEffect, useMemo, useState } from "react";
import { StreamViewer } from "@/components/stream/StreamViewer";
import { useMachines, type Machine } from "@/lib/api";
import { parseConnectParams } from "@/lib/launch";
import { machineState } from "@/lib/machines";
import { Button } from "@/components/ui/button";

export function ConnectPageClient() {
  const params = useSearchParams();
  const router = useRouter();
  const search = params.toString();
  const parsed = useMemo(() => parseConnectParams(search), [search]);
  const machines = useMachines();
  const candidate = machines.data?.find((item) => item.id === parsed.machine);
  const [machine, setMachine] = useState<Machine | null>(null);

  useEffect(() => {
    if (machine || !candidate || machineState(candidate) === "offline") return;
    const timer = window.setTimeout(() => setMachine(candidate), 0);
    return () => window.clearTimeout(timer);
  }, [candidate, machine]);

  const resolution = useMemo(() => (
    machine?.virtual_display && parsed.width && parsed.height
      ? { width: parsed.width, height: parsed.height }
      : undefined
  ), [parsed.height, parsed.width, machine?.virtual_display]);

  if (machines.loading && !machines.data) {
    return <main className="grid min-h-screen place-items-center bg-black text-sm text-zinc-500">Connecting to the relay…</main>;
  }

  if (!machine && (machines.error || !candidate || machineState(candidate) === "offline")) {
    return (
      <main className="grid min-h-screen place-items-center bg-[#090a0d] px-5 text-zinc-100">
        <section className="max-w-md text-center">
          <div className="mx-auto grid h-14 w-14 place-items-center rounded-2xl border border-white/10 bg-white/[0.035] text-xl text-zinc-400">⌁</div>
          <h1 className="mt-5 text-2xl font-semibold">{machines.error ? "Relay unavailable" : candidate ? "Machine offline" : "Machine not found"}</h1>
          <p className="mt-2 text-sm leading-6 text-zinc-500">
            {machines.error || "This workspace is unavailable. It may have gone offline or the link may be out of date."}
          </p>
          <Button variant="primary" onClick={() => router.push("/")} className="mt-6">Back to machines</Button>
        </section>
      </main>
    );
  }

  return (
    machine ? (
      <StreamViewer
        machine={machine}
        resolution={resolution}
        initialFps={parsed.fps}
        initialQuality={parsed.quality}
        onBack={() => router.push("/")}
      />
    ) : (
      <main className="grid min-h-screen place-items-center bg-black text-sm text-zinc-500">Preparing your workspace…</main>
    )
  );
}
