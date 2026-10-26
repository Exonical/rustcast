"use client";

import { useCallback, useEffect, useState } from "react";

export interface Machine {
  id: string;
  name: string;
  display_name?: string;
  frame_endpoint: string;
  os?: string;
  gpu_vendor?: string;
  encoder_backend?: string;
  virtual_display: boolean;
  width?: number;
  height?: number;
  target_fps?: number;
  default_fps?: number;
  target_bitrate_kbps?: number;
  viewers: number;
  last_seen: string;
  status: "online" | "offline";
  upstream_status: string;
  clipboard_policy?: string;
  logged_in_user?: string;
  login_state?: "idle" | "authenticating" | "active" | string;
}

export interface ClipboardPolicy {
  clipboard_mode: "both" | "off" | "host_to_client" | "client_to_host";
  max_bytes: number;
  capture_content: boolean;
  connection_path: "auto" | "direct_only" | "relay_only";
  updated_at?: string;
  updated_by?: string;
}

export interface ClipboardAuditRow {
  id: number;
  ts: string;
  machine_id: string;
  machine_name?: string;
  session_id?: string;
  user_sub?: string;
  user_name?: string;
  client_ip?: string;
  direction: string;
  mime?: string;
  size: number;
  sha256: string;
  result: string;
  has_content?: boolean;
}

export interface LoginAuditRow {
  id: number;
  ts: string;
  machine_id: string;
  machine_name: string;
  session_id: string;
  user_sub: string;
  user_name: string;
  client_ip: string;
  os_user: string;
  result: string;
}

export interface ClipboardAuditDetail {
  row: ClipboardAuditRow;
  content: string | null;
  encoding?: string;
}

export interface SessionInfo {
  id: string;
  machine_id: string;
  machine_name: string;
  client_ip: string;
  user_agent: string;
  user?: string;
  user_sub?: string;
  os_user?: string;
  os_uid?: number;
  started_at: string;
  width: number;
  height: number;
}

export interface PollingState<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  refresh: () => Promise<void>;
}

export interface Me {
  sub: string;
  email?: string;
  name?: string;
  /** OIDC preferred_username — must equal a locked/busy host's logged_in_user to attach. */
  username?: string;
  admin: boolean;
  auth_enabled: boolean;
}

export function redirectToLogin(): void {
  const next = encodeURIComponent(window.location.pathname + window.location.search);
  // eslint-disable-next-line @next/next/no-location-assign-relative-destination -- /auth/login is a relay route, not a Next page
  window.location.assign(`/auth/login?next=${next}`);
}

function requireAuth(response: Response): void {
  if (response.status === 401) {
    redirectToLogin();
    throw new Error("unauthenticated");
  }
}

async function fetchJSON<T>(path: string): Promise<T> {
  const response = await fetch(path, { cache: "no-store" });
  requireAuth(response);
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json() as Promise<T>;
}

export function fetchMe(): Promise<Me> {
  return fetchJSON<Me>("/api/me");
}

export async function logout(): Promise<void> {
  const response = await fetch("/auth/logout", { method: "POST" });
  const body = await response.json().catch(() => ({}) as { logout_url?: string });
  window.location.assign(body.logout_url || "/");
}

export function fetchMachines(): Promise<Machine[]> {
  return fetchJSON<Machine[]>("/api/machines");
}

export function fetchSessions(): Promise<SessionInfo[]> {
  return fetchJSON<SessionInfo[]>("/api/sessions");
}

export function fetchPolicy(machineId: string): Promise<ClipboardPolicy> {
  return fetchJSON<ClipboardPolicy>(`/api/machines/${encodeURIComponent(machineId)}/policy`);
}

export async function updatePolicy(machineId: string, body: Pick<ClipboardPolicy, "clipboard_mode" | "max_bytes" | "capture_content" | "connection_path">): Promise<ClipboardPolicy> {
  const response = await fetch(`/api/machines/${encodeURIComponent(machineId)}/policy`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  requireAuth(response);
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json() as Promise<ClipboardPolicy>;
}

export function fetchLogins(opts: { machine?: string; limit?: number; before?: number } = {}): Promise<LoginAuditRow[]> {
  const params = new URLSearchParams();
  if (opts.machine) params.set("machine", opts.machine);
  if (opts.limit) params.set("limit", String(opts.limit));
  if (opts.before) params.set("before", String(opts.before));
  const query = params.toString();
  return fetchJSON<LoginAuditRow[]>(`/api/audit/logins${query ? `?${query}` : ""}`);
}

export function fetchClipboardAudit(opts: { machine?: string; limit?: number; before?: number } = {}): Promise<ClipboardAuditRow[]> {
  const params = new URLSearchParams();
  if (opts.machine) params.set("machine", opts.machine);
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.before != null) params.set("before", String(opts.before));
  const query = params.toString();
  return fetchJSON<ClipboardAuditRow[]>(`/api/audit/clipboard${query ? `?${query}` : ""}`);
}

export function fetchClipboardEvent(id: number): Promise<ClipboardAuditDetail> {
  return fetchJSON<ClipboardAuditDetail>(`/api/audit/clipboard/${id}`);
}

export async function disconnectSession(id: string): Promise<void> {
  const response = await fetch(`/api/sessions/${encodeURIComponent(id)}/disconnect`, { method: "POST" });
  requireAuth(response);
  if (response.status === 404) throw new Error("This session has already ended.");
  if (!response.ok && response.status !== 204) throw new Error(`HTTP ${response.status}`);
}

function usePolling<T>(fetcher: () => Promise<T>, intervalMs: number): PollingState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const refresh = useCallback(async () => {
    try {
      setData(await fetcher());
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Unable to reach the relay");
    } finally {
      setLoading(false);
    }
  }, [fetcher]);

  useEffect(() => {
    const initial = window.setTimeout(() => void refresh(), 0);
    const timer = window.setInterval(() => void refresh(), intervalMs);
    return () => {
      window.clearTimeout(initial);
      window.clearInterval(timer);
    };
  }, [intervalMs, refresh]);

  return { data, error, loading, refresh };
}

export function useMachines(): PollingState<Machine[]> {
  return usePolling(fetchMachines, 5000);
}

export function useSessions(): PollingState<SessionInfo[]> {
  return usePolling(fetchSessions, 3000);
}
