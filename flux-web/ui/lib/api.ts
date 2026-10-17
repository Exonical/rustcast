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
}

export interface SessionInfo {
  id: string;
  machine_id: string;
  machine_name: string;
  client_ip: string;
  user_agent: string;
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

async function fetchJSON<T>(path: string): Promise<T> {
  const response = await fetch(path, { cache: "no-store" });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json() as Promise<T>;
}

export function fetchMachines(): Promise<Machine[]> {
  return fetchJSON<Machine[]>("/api/machines");
}

export function fetchSessions(): Promise<SessionInfo[]> {
  return fetchJSON<SessionInfo[]>("/api/sessions");
}

export async function disconnectSession(id: string): Promise<void> {
  const response = await fetch(`/api/sessions/${encodeURIComponent(id)}/disconnect`, { method: "POST" });
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
