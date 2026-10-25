"use client";

import { useCallback, useEffect, useState } from "react";
import { AppShell } from "@/components/shell/AppShell";
import { PageHeader } from "@/components/shell/PageHeader";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { EmptyState } from "@/components/ui/empty-state";
import { Skeleton } from "@/components/ui/skeleton";
import {
  fetchClipboardAudit,
  fetchClipboardEvent,
  fetchLogins,
  useMachines,
  useSessions,
  type ClipboardAuditDetail,
  type ClipboardAuditRow,
  type LoginAuditRow,
} from "@/lib/api";
import { useAuth } from "@/lib/auth";

const PAGE_SIZE = 100;

function directionLabel(direction: string): string {
  return direction === "client_to_host" ? "you→host" : direction === "host_to_client" ? "host→you" : direction || "—";
}

function resultTone(result: string): "green" | "amber" | "red" | "neutral" {
  if (result === "ok") return "green";
  if (result === "blocked") return "amber";
  if (result === "too_large" || result === "invalid") return "red";
  return "neutral";
}

export function AuditPage() {
  const machines = useMachines();
  const sessions = useSessions();
  const { me, loading: authLoading } = useAuth();
  const admin = !me?.auth_enabled || me?.admin === true;

  const [tab, setTab] = useState<"clipboard" | "logins">("clipboard");
  const [machine, setMachine] = useState("");
  const [rows, setRows] = useState<ClipboardAuditRow[]>([]);
  const [loginRows, setLoginRows] = useState<LoginAuditRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [detail, setDetail] = useState<ClipboardAuditDetail | null>(null);
  const [detailOpen, setDetailOpen] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const loadLogins = useCallback(async (machineId: string, before?: number) => {
    if (before === undefined) setLoading(true);
    else setLoadingMore(true);
    setError(null);
    try {
      const page = await fetchLogins({ machine: machineId || undefined, limit: PAGE_SIZE, before });
      setLoginRows((prev) => (before === undefined ? page : [...prev, ...page]));
      setHasMore(page.length === PAGE_SIZE);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Unable to load login audit");
    } finally {
      setLoading(false);
      setLoadingMore(false);
    }
  }, []);

  const load = useCallback(async (machineId: string) => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchClipboardAudit({ machine: machineId || undefined, limit: PAGE_SIZE });
      setRows(page);
      setHasMore(page.length === PAGE_SIZE);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Unable to load audit log");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!admin || authLoading) return;
    const initial = window.setTimeout(() => {
      if (tab === "logins") void loadLogins(machine);
      else void load(machine);
    }, 0);
    return () => window.clearTimeout(initial);
  }, [admin, authLoading, machine, tab, load, loadLogins]);

  const loadOlder = async () => {
    if (tab === "logins") {
      const oldest = loginRows[loginRows.length - 1];
      if (oldest) await loadLogins(machine, oldest.id);
      return;
    }
    const oldest = rows[rows.length - 1];
    if (!oldest) return;
    setLoadingMore(true);
    try {
      const page = await fetchClipboardAudit({ machine: machine || undefined, limit: PAGE_SIZE, before: oldest.id });
      setRows((prev) => [...prev, ...page]);
      setHasMore(page.length === PAGE_SIZE);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Unable to load more events");
    } finally {
      setLoadingMore(false);
    }
  };

  const openDetail = async (row: ClipboardAuditRow) => {
    setDetail(null);
    setDetailError(null);
    setCopied(false);
    setDetailOpen(true);
    try {
      setDetail(await fetchClipboardEvent(row.id));
    } catch (cause) {
      setDetailError(cause instanceof Error ? cause.message : "Unable to load event");
    }
  };

  const copyContent = async () => {
    if (!detail?.content || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(detail.content);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {}
  };

  return (
    <AppShell relayOnline={!machines.error} activeSessions={sessions.data?.length ?? 0}>
      <PageHeader title="Clipboard audit" description="Every clipboard event enforced by the relay, with identity and content hashes." />

      {!authLoading && !admin && (
        <EmptyState
          title="Access denied"
          detail="Clipboard audit is restricted to administrators. Ask an admin to grant your account the flux-admins group."
          action={{ label: "Back to machines", href: "/" }}
        />
      )}

      {admin && (
        <>
          <div className="mb-5 flex items-center gap-3">
            <div className="flex overflow-hidden rounded-lg border border-white/10 text-xs">
              {(["clipboard", "logins"] as const).map((t) => (
                <button
                  key={t}
                  onClick={() => setTab(t)}
                  className={`px-3 py-2 capitalize ${tab === t ? "bg-blue-400/15 text-blue-200" : "text-zinc-500 hover:bg-white/5"}`}
                >{t === "clipboard" ? "Clipboard" : "Logins"}</button>
              ))}
            </div>
            <label className="text-xs font-medium text-zinc-500" htmlFor="audit-machine">Machine</label>
            <select
              id="audit-machine"
              value={machine}
              onChange={(e) => setMachine(e.target.value)}
              className="rounded-lg border border-white/10 bg-[#0d0f13] px-3 py-2 text-xs text-zinc-200 outline-none focus:border-blue-400/40"
            >
              <option value="">All machines</option>
              {(machines.data ?? []).map((m) => (
                <option key={m.id} value={m.id}>{m.display_name || m.name}</option>
              ))}
            </select>
            {error && <span className="text-xs text-red-300">{error}</span>}
          </div>

          <Card className="overflow-hidden p-0">
            {tab === "logins" ? (
              loading ? (
                <div className="space-y-2 p-6">
                  {[0, 1, 2, 3].map((i) => <Skeleton key={i} className="h-9 w-full" />)}
                </div>
              ) : loginRows.length === 0 ? (
                <div className="p-6">
                  <EmptyState title="No host logins" detail="OS logins on the host are recorded here once a viewer signs in." />
                </div>
              ) : (
                <table className="w-full text-left text-xs">
                  <thead>
                    <tr className="border-b border-white/[0.07] text-[10px] uppercase tracking-[.13em] text-zinc-600">
                      <th className="px-4 py-3 font-bold">Time</th>
                      <th className="px-4 py-3 font-bold">Machine</th>
                      <th className="hidden px-4 py-3 font-bold sm:table-cell">Relay user</th>
                      <th className="px-4 py-3 font-bold">OS user</th>
                      <th className="px-4 py-3 font-bold">Result</th>
                      <th className="hidden px-4 py-3 font-bold md:table-cell">Client IP</th>
                    </tr>
                  </thead>
                  <tbody>
                    {loginRows.map((row) => (
                      <tr key={row.id} className="border-b border-white/[0.04]">
                        <td className="px-4 py-2.5 text-zinc-400">{new Date(row.ts).toLocaleString()}</td>
                        <td className="px-4 py-2.5 text-zinc-300">{row.machine_name || row.machine_id}</td>
                        <td className="hidden px-4 py-2.5 text-zinc-400 sm:table-cell">{row.user_name || row.user_sub || "—"}</td>
                        <td className="px-4 py-2.5 font-mono text-[11px] text-zinc-300">{row.os_user || "—"}</td>
                        <td className="px-4 py-2.5"><Badge tone={resultTone(row.result)}>{row.result}</Badge></td>
                        <td className="hidden px-4 py-2.5 text-zinc-500 md:table-cell">{row.client_ip || "—"}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )
            ) : loading ? (
              <div className="space-y-2 p-6">
                {[0, 1, 2, 3].map((i) => <Skeleton key={i} className="h-9 w-full" />)}
              </div>
            ) : rows.length === 0 ? (
              <div className="p-6">
                <EmptyState title="No clipboard events" detail="Nothing has been recorded yet — copy or paste inside a session to generate audit rows." />
              </div>
            ) : (
              <table className="w-full text-left text-xs">
                <thead>
                  <tr className="border-b border-white/[0.07] text-[10px] uppercase tracking-[.13em] text-zinc-600">
                    <th className="px-4 py-3 font-bold">Time</th>
                    <th className="px-4 py-3 font-bold">Machine</th>
                    <th className="hidden px-4 py-3 font-bold sm:table-cell">User</th>
                    <th className="px-4 py-3 font-bold">Direction</th>
                    <th className="px-4 py-3 font-bold">Result</th>
                    <th className="px-4 py-3 text-right font-bold">Size</th>
                    <th className="hidden px-4 py-3 font-bold md:table-cell">SHA-256</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((row) => (
                    <tr
                      key={row.id}
                      onClick={() => void openDetail(row)}
                      className="cursor-pointer border-b border-white/[0.04] text-zinc-300 transition hover:bg-white/[0.03]"
                    >
                      <td className="whitespace-nowrap px-4 py-2.5 font-mono text-[11px] text-zinc-400">
                        {new Date(row.ts).toLocaleString()}
                      </td>
                      <td className="px-4 py-2.5">{row.machine_name || row.machine_id}</td>
                      <td className="hidden px-4 py-2.5 text-zinc-400 sm:table-cell">{row.user_name || row.user_sub || "—"}</td>
                      <td className="px-4 py-2.5">{directionLabel(row.direction)}</td>
                      <td className="px-4 py-2.5"><Badge tone={resultTone(row.result)}>{row.result}</Badge></td>
                      <td className="px-4 py-2.5 text-right font-mono text-[11px] text-zinc-400">{row.size}</td>
                      <td className="hidden px-4 py-2.5 font-mono text-[11px] text-zinc-500 md:table-cell" title={row.sha256}>
                        {row.sha256.slice(0, 12)}…
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </Card>

          {hasMore && !loading && (
            <div className="mt-4 flex justify-center">
              <Button onClick={() => void loadOlder()} disabled={loadingMore}>
                {loadingMore ? "Loading…" : "Load older"}
              </Button>
            </div>
          )}
        </>
      )}

      <Dialog open={detailOpen} onOpenChange={setDetailOpen}>
        <DialogContent className="w-[min(46rem,calc(100vw-2rem))]">
          <DialogTitle className="text-base font-semibold text-zinc-100">Clipboard event</DialogTitle>
          <DialogDescription className="mt-1 text-xs text-zinc-500">
            {detail ? `${detail.row.machine_name || detail.row.machine_id} · ${new Date(detail.row.ts).toLocaleString()}` : "Loading…"}
          </DialogDescription>
          {detailError && <p className="mt-4 text-sm text-red-300">{detailError}</p>}
          {!detail && !detailError && <Skeleton className="mt-4 h-40 w-full" />}
          {detail && (
            <div className="mt-4 space-y-4">
              <div className="grid grid-cols-2 gap-x-6 gap-y-2 text-xs sm:grid-cols-3">
                <Meta label="User" value={detail.row.user_name || detail.row.user_sub || "—"} />
                <Meta label="Direction" value={directionLabel(detail.row.direction)} />
                <Meta label="Result" value={detail.row.result} />
                <Meta label="Size" value={`${detail.row.size} B`} />
                <Meta label="Client IP" value={detail.row.client_ip || "—"} />
                <Meta label="SHA-256" value={detail.row.sha256.slice(0, 16) + "…"} title={detail.row.sha256} />
              </div>
              {detail.encoding === "base64" && (
                <p className="rounded-lg border border-amber-400/20 bg-amber-400/[0.06] px-3 py-2 text-[11px] text-amber-200">
                  Content was not valid UTF-8 — shown base64-encoded.
                </p>
              )}
              {detail.content != null ? (
                <pre className="max-h-72 overflow-auto rounded-xl border border-white/[0.07] bg-black/40 p-4 font-mono text-[11px] leading-5 text-zinc-300 whitespace-pre-wrap break-all">
                  {detail.content}
                </pre>
              ) : (
                <p className="rounded-xl border border-white/[0.07] bg-white/[0.02] px-4 py-3 text-xs text-zinc-500">
                  No content retained for this event (policy may have capture disabled).
                </p>
              )}
              <div className="flex justify-end gap-2">
                {detail.content != null && (
                  <Button onClick={() => void copyContent()}>{copied ? "Copied" : "Copy content"}</Button>
                )}
              </div>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </AppShell>
  );
}

function Meta({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <div className="min-w-0">
      <p className="text-[9px] font-bold uppercase tracking-[.13em] text-zinc-600">{label}</p>
      <p className="mt-0.5 truncate text-zinc-300" title={title}>{value}</p>
    </div>
  );
}
