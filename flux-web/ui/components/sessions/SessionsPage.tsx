"use client";

import { useEffect, useState } from "react";
import { AppShell } from "@/components/shell/AppShell";
import { PageHeader } from "@/components/shell/PageHeader";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { EmptyState } from "@/components/ui/empty-state";
import { Skeleton } from "@/components/ui/skeleton";
import { disconnectSession, useMachines, useSessions, type SessionInfo } from "@/lib/api";
import { useAuth } from "@/lib/auth";
import { formatDuration, shortUserAgent } from "@/lib/format";

export function SessionsPage() {
  const sessions = useSessions();
  const machines = useMachines();
  const [now, setNow] = useState(0);
  const [selected, setSelected] = useState<SessionInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { me } = useAuth();
  const canDisconnect = !me?.auth_enabled || me?.admin === true;

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  const disconnect = async () => {
    if (!selected) return;
    setBusy(true);
    setError(null);
    try {
      await disconnectSession(selected.id);
      await sessions.refresh();
      setSelected(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Could not disconnect session");
    } finally {
      setBusy(false);
    }
  };

  const content = sessions.loading && !sessions.data ? (
    <div className="space-y-3">{Array.from({ length: 4 }, (_, i) => <Skeleton key={i} className="h-20 rounded-2xl" />)}</div>
  ) : sessions.error && !sessions.data ? (
    <Card className="p-10 text-center">
      <h2 className="font-semibold">Could not load sessions</h2>
      <p className="mt-2 text-sm text-zinc-500">{sessions.error}</p>
      <Button variant="primary" onClick={() => void sessions.refresh()} className="mt-5">Retry</Button>
    </Card>
  ) : !sessions.data?.length ? (
    <EmptyState title="No active sessions" detail="When someone connects to a machine, the live session appears here." action={{ label: "Browse machines", href: "/" }} />
  ) : (
    <>
      <div className="hidden overflow-hidden rounded-2xl border border-white/[0.07] bg-[#111318] md:block">
        <table className="w-full border-collapse text-left">
          <thead className="border-b border-white/[0.06] bg-white/[0.02]">
            <tr>{["Machine", "Client", "Resolution", "Started", "Duration", ""].map((title) =>
              <th key={title} className="px-5 py-3 text-[10px] font-bold uppercase tracking-[.15em] text-zinc-600">{title}</th>)}</tr>
          </thead>
          <tbody className="divide-y divide-white/[0.045]">
            {sessions.data.map((session) => (
              <tr key={session.id} className="transition hover:bg-white/[0.02]">
                <td className="px-5 py-4"><p className="text-sm font-semibold text-zinc-200">{session.machine_name}</p><p className="mt-1 font-mono text-[10px] text-zinc-600">{session.machine_id}</p>{session.os_user && <p className="mt-1 text-[10px] text-zinc-500">OS: {session.os_user}{session.os_uid ? ` (${session.os_uid})` : ""}</p>}</td>
                <td className="px-5 py-4"><p className="text-sm text-zinc-300">{session.client_ip}</p><p className="mt-1 text-[11px] text-zinc-600">{shortUserAgent(session.user_agent)}</p></td>
                <td className="px-5 py-4"><span className="font-mono text-xs text-zinc-300">{session.width && session.height ? `${session.width}×${session.height}` : "Native"}</span></td>
                <td className="px-5 py-4 text-xs text-zinc-500">{new Date(session.started_at).toLocaleTimeString()}</td>
                <td className="px-5 py-4 font-mono text-xs text-zinc-300">{formatDuration((now - Date.parse(session.started_at)) / 1000)}</td>
                <td className="px-5 py-4 text-right">{canDisconnect && <Button variant="danger" className="px-3 py-2 text-xs" onClick={() => { setSelected(session); setError(null); }}>Disconnect</Button>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="space-y-3 md:hidden">
        {sessions.data.map((session) => (
          <Card key={session.id} className="p-4">
            <div className="flex items-start justify-between gap-3">
              <div><p className="font-semibold">{session.machine_name}</p><p className="mt-1 text-xs text-zinc-500">{session.client_ip} · {shortUserAgent(session.user_agent)}</p></div>
              <Badge tone="green">Active</Badge>
            </div>
            <div className="my-4 grid grid-cols-3 gap-2 border-y border-white/[0.06] py-3 text-[11px]">
              <MobileStat label="Resolution" value={session.width && session.height ? `${session.width}×${session.height}` : "Native"} />
              <MobileStat label="Started" value={new Date(session.started_at).toLocaleTimeString()} />
              <MobileStat label="Duration" value={formatDuration((now - Date.parse(session.started_at)) / 1000)} />
            </div>
            {canDisconnect && <Button variant="danger" className="w-full" onClick={() => { setSelected(session); setError(null); }}>Disconnect session</Button>}
          </Card>
        ))}
      </div>
    </>
  );

  return (
    <AppShell relayOnline={!machines.loading && !machines.error} activeSessions={sessions.data?.length ?? 0}>
      <PageHeader title="Active sessions" description="Monitor connected viewers and reclaim a workspace when needed."
        actions={<Badge tone="green">{sessions.data?.length ?? 0} live</Badge>} />
      {error && <p role="alert" className="mb-4 rounded-xl border border-red-400/20 bg-red-400/[0.06] px-4 py-3 text-sm text-red-200">{error}</p>}
      {content}
      <Dialog open={!!selected} onOpenChange={(open) => { if (!open && !busy) setSelected(null); }}>
        <DialogContent>
          <div className="mb-6 grid h-11 w-11 place-items-center rounded-xl bg-red-400/10 text-red-300">!</div>
          <DialogTitle className="text-xl font-semibold">Disconnect this session?</DialogTitle>
          <DialogDescription className="mt-2 text-sm leading-6 text-zinc-500">
            The viewer on <span className="text-zinc-200">{selected?.machine_name}</span> will be disconnected immediately.
          </DialogDescription>
          <div className="mt-7 flex justify-end gap-2">
            <Button disabled={busy} onClick={() => setSelected(null)}>Cancel</Button>
            <Button variant="danger" disabled={busy} onClick={() => void disconnect()}>{busy ? "Disconnecting…" : "Disconnect"}</Button>
          </div>
        </DialogContent>
      </Dialog>
    </AppShell>
  );
}

function MobileStat({ label, value }: { label: string; value: string }) {
  return <div><p className="text-[9px] uppercase tracking-wider text-zinc-600">{label}</p><p className="mt-1 truncate font-mono text-zinc-300">{value}</p></div>;
}
