"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { machineState } from "@/lib/machines";
import type { ClipboardPolicy, Machine } from "@/lib/api";
import { fetchPolicy, updatePolicy } from "@/lib/api";
import { useAuth } from "@/lib/auth";
import { policyLabel } from "@/lib/clipboard";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";

const POLICY_MODES: { value: ClipboardPolicy["clipboard_mode"]; label: string }[] = [
  { value: "both", label: "Both directions" },
  { value: "host_to_client", label: "Host → client" },
  { value: "client_to_host", label: "Client → host" },
  { value: "off", label: "Off" },
];

export function MachineCard({ machine, onConnect }: {
  machine: Machine;
  onConnect: (machine: Machine) => void;
}) {
  const { me } = useAuth();
  const admin = !!me?.admin;
  const state = machineState(machine);
  const online = state !== "offline";
  const display = machine.width && machine.height ? `${machine.width} × ${machine.height}` : "Native mode";

  const [policy, setPolicy] = useState<ClipboardPolicy | null>(null);
  const [maxDraft, setMaxDraft] = useState("");
  const [policyError, setPolicyError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!admin) return;
    let cancelled = false;
    fetchPolicy(machine.id)
      .then((next) => {
        if (cancelled) return;
        setPolicy(next);
        setMaxDraft(String(next.max_bytes));
      })
      .catch(() => { if (!cancelled) setPolicyError("policy unavailable"); });
    return () => { cancelled = true; };
  }, [admin, machine.id]);

  const savePolicy = (patch: Partial<ClipboardPolicy>) => {
    if (!policy || saving) return;
    const previous = policy;
    const previousDraft = maxDraft;
    const next = { ...policy, ...patch };
    setPolicy(next);
    setMaxDraft(String(next.max_bytes));
    setPolicyError(null);
    setSaving(true);
    updatePolicy(machine.id, {
      clipboard_mode: next.clipboard_mode,
      max_bytes: next.max_bytes,
      capture_content: next.capture_content,
    })
      .then((saved) => {
        setPolicy(saved);
        setMaxDraft(String(saved.max_bytes));
      })
      .catch(() => {
        setPolicy(previous);
        setMaxDraft(previousDraft);
        setPolicyError("save failed");
      })
      .finally(() => setSaving(false));
  };

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
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          <Badge tone={state === "available" ? "green" : state === "in-use" ? "amber" : "neutral"}>
            <span className={`h-1.5 w-1.5 rounded-full ${state === "available" ? "bg-emerald-400" : state === "in-use" ? "bg-amber-300" : "bg-zinc-500"}`} />
            {state === "available" ? "Online" : state === "in-use" ? "In use" : "Offline"}
          </Badge>
          {machine.logged_in_user && (
            <span title={`Host login: ${machine.logged_in_user}`}>
              <Badge tone="amber">In use by {machine.logged_in_user}</Badge>
            </span>
          )}
          {machine.clipboard_policy && (
            <span title={`Clipboard: ${policyLabel(machine.clipboard_policy)}`}>
              <Badge tone={machine.clipboard_policy === "both" ? "neutral" : "amber"}>
                ⧉ {policyLabel(machine.clipboard_policy)}
              </Badge>
            </span>
          )}
        </div>
      </div>

      <div className="my-5 grid grid-cols-2 gap-x-4 gap-y-3 border-y border-white/[0.055] py-4">
        <Spec label="Operating system" value={machine.os || "Unknown"} />
        <Spec label="Graphics" value={machine.gpu_vendor || "Unknown"} />
        <Spec label="Encoder" value={machine.encoder_backend || "Unknown"} />
        <Spec label="Display" value={`${display}${machine.target_fps ? ` · ${machine.target_fps} FPS` : ""}`} />
        <Spec label="Display mode" value={machine.virtual_display ? "Virtual" : "Physical"} />
        <Spec label="Viewers" value={String(machine.viewers)} />
      </div>

      {admin && policy && (
        <div className="mb-4 rounded-xl border border-white/[0.06] bg-white/[0.02] p-3">
          <div className="mb-2 flex items-center justify-between">
            <p className="text-[9px] font-bold uppercase tracking-[.13em] text-zinc-600">Clipboard policy</p>
            {policyError && <p className="text-[10px] text-red-300">{policyError}</p>}
          </div>
          <div className="flex items-center gap-2">
            <select
              aria-label="Clipboard policy"
              disabled={saving}
              value={policy.clipboard_mode}
              onChange={(e) => savePolicy({ clipboard_mode: e.target.value as ClipboardPolicy["clipboard_mode"] })}
              className="flex-1 rounded-lg border border-white/10 bg-[#0d0f13] px-2 py-1.5 text-xs text-zinc-200 outline-none focus:border-blue-400/40"
            >
              {POLICY_MODES.map((mode) => (
                <option key={mode.value} value={mode.value}>{mode.label}</option>
              ))}
            </select>
            <input
              aria-label="Max clipboard bytes"
              type="number"
              min={0}
              disabled={saving}
              value={maxDraft}
              onChange={(e) => setMaxDraft(e.target.value)}
              onBlur={() => {
                const value = Math.max(0, Math.trunc(Number(maxDraft) || 0));
                if (value !== policy.max_bytes) savePolicy({ max_bytes: value });
                else setMaxDraft(String(policy.max_bytes));
              }}
              className="w-24 rounded-lg border border-white/10 bg-[#0d0f13] px-2 py-1.5 text-xs text-zinc-200 outline-none focus:border-blue-400/40"
              title="Max clipboard size (bytes)"
            />
          </div>
        </div>
      )}

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
