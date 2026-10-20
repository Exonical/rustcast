"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import type { ReactNode } from "react";
import { useAuth } from "@/lib/auth";
import { logout } from "@/lib/api";

function Brand() {
  return (
    <Link href="/" className="flex items-center gap-3 px-2 py-3">
      <span className="grid h-9 w-9 place-items-center rounded-xl bg-blue-500 text-white shadow-lg shadow-blue-950/40">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path d="M4 6.5 12 3l8 3.5v11L12 21l-8-3.5v-11Z" stroke="currentColor" strokeWidth="1.7" />
          <path d="m8 9 4-2 4 2v6l-4 2-4-2V9Z" fill="currentColor" opacity=".8" />
        </svg>
      </span>
      <span>
        <span className="block text-sm font-bold tracking-[.04em] text-white">FLUX</span>
        <span className="block text-[10px] font-medium tracking-[.12em] text-zinc-600">REMOTE WORKSPACE</span>
      </span>
    </Link>
  );
}

export function AppShell({ children, relayOnline, activeSessions }: {
  children: ReactNode;
  relayOnline: boolean;
  activeSessions: number;
}) {
  const pathname = usePathname();
  const { me } = useAuth();
  const nav = [
    { href: "/", label: "Machines", icon: "▦" },
    { href: "/sessions/", label: "Sessions", icon: "◷", count: activeSessions },
    ...(me?.admin ? [{ href: "/audit/", label: "Audit", icon: "⧉" }] : []),
  ];
  return (
    <div className="min-h-screen bg-[#090a0d] text-zinc-100">
      <aside data-local-control className="fixed inset-y-0 left-0 z-20 hidden w-[248px] flex-col border-r border-white/[0.06] bg-[#0d0f13] px-4 py-5 lg:flex">
        <Brand />
        <div className="mb-3 mt-9 px-3 text-[10px] font-bold uppercase tracking-[.18em] text-zinc-600">Workspace</div>
        <nav className="space-y-1">
          {nav.map((item) => {
            const active = item.href === "/" ? pathname === "/" : pathname.startsWith(item.href);
            return (
              <Link key={item.href} href={item.href} aria-current={active ? "page" : undefined}
                className={`flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm font-medium transition ${active ? "bg-blue-500/10 text-blue-200 ring-1 ring-inset ring-blue-400/10" : "text-zinc-500 hover:bg-white/[0.035] hover:text-zinc-200"}`}>
                <span className="grid w-5 place-items-center text-base">{item.icon}</span>
                <span className="flex-1">{item.label}</span>
                {item.count ? <span className="rounded-md bg-blue-400/10 px-1.5 py-0.5 text-[10px] font-bold text-blue-200">{item.count}</span> : null}
              </Link>
            );
          })}
        </nav>
        <div className="mt-auto space-y-2">
          {me?.auth_enabled === false && (
            <p className="rounded-lg border border-amber-400/20 bg-amber-400/[0.06] px-2.5 py-1.5 text-center text-[10px] font-medium text-amber-200">auth disabled</p>
          )}
          <div className="rounded-2xl border border-white/[0.06] bg-white/[0.025] p-3.5">
            {me && (
              <div className="mb-3 flex items-center justify-between gap-2 border-b border-white/[0.06] pb-3">
                <span className="truncate text-xs font-medium text-zinc-200" title={me.email || me.sub}>{me.name || me.email || me.sub}</span>
                {me.auth_enabled !== false && (
                  <button onClick={() => void logout()} className="shrink-0 rounded-md px-2 py-1 text-[10px] font-medium text-zinc-500 hover:bg-white/[0.06] hover:text-zinc-200">
                    Sign out
                  </button>
                )}
              </div>
            )}
            <div className="flex items-center gap-2">
              <span className={`h-2 w-2 rounded-full ${relayOnline ? "bg-emerald-400 shadow-[0_0_10px_rgba(52,211,153,.7)]" : "bg-red-400"}`} />
              <span className="text-xs font-medium text-zinc-300">Relay {relayOnline ? "connected" : "unreachable"}</span>
            </div>
            <p className="mt-2 text-[10px] leading-4 text-zinc-600">Self-hosted remote desktop</p>
          </div>
        </div>
      </aside>
      <div className="min-h-screen lg:pl-[248px]">
        <header data-local-control className="sticky top-0 z-10 flex h-14 items-center justify-between border-b border-white/[0.06] bg-[#090a0d]/90 px-4 backdrop-blur-xl lg:hidden">
          <Brand />
          <Link href="/sessions/" className="rounded-lg px-3 py-2 text-xs text-zinc-400">Sessions {activeSessions || ""}</Link>
        </header>
        <main className="mx-auto w-full max-w-[1440px] px-4 py-7 sm:px-7 sm:py-9 xl:px-10">{children}</main>
      </div>
    </div>
  );
}
