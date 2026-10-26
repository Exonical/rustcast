export type Candidate = {
  path: "direct" | "relay";
  url?: string;
  host?: string;
  port?: number;
  certHashes?: string[];
  ticket?: string;
};

export type DiscoveryCandidates = {
  host?: string;
  port?: number;
  certHashes?: string[];
  ticket?: string;
  candidates?: Candidate[];
};

export type CandidateTransport = {
  close(info?: { closeCode?: number; reason?: string }): void;
};

const directFailurePreferenceMs = 5 * 60 * 1000;

export function orderCandidates(discovery: DiscoveryCandidates, directFailedAt?: number, now = Date.now()): Candidate[] {
  const candidates = discovery.candidates?.length
    ? [...discovery.candidates]
    : [{
        path: "relay" as const,
        host: discovery.host,
        port: discovery.port,
        certHashes: discovery.certHashes,
        ticket: discovery.ticket,
      }];
  if (directFailedAt !== undefined && now >= directFailedAt &&
      now - directFailedAt < directFailurePreferenceMs) {
    const relayIndex = candidates.findIndex((candidate) => candidate.path === "relay");
    if (relayIndex > 0) candidates.unshift(...candidates.splice(relayIndex, 1));
  }
  return candidates;
}

export async function connectFirst<T extends CandidateTransport>(
  candidates: Candidate[],
  open: (candidate: Candidate) => { transport: T; ready: Promise<void> },
  timeoutMs: Partial<Record<Candidate["path"], number>> = { direct: 1500, relay: 10000 },
  onFailure?: (candidate: Candidate, error: unknown) => void,
): Promise<{ candidate: Candidate; transport: T }> {
  let lastError: unknown;
  for (const candidate of candidates) {
    let opened: { transport: T; ready: Promise<void> } | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      opened = open(candidate);
      opened.ready.catch(() => {});
      await Promise.race([
        opened.ready,
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new Error(`${candidate.path} candidate timed out`)), timeoutMs[candidate.path] ?? 10000);
        }),
      ]);
      return { candidate, transport: opened.transport };
    } catch (error) {
      lastError = error;
      opened?.transport.close({ closeCode: 0, reason: "candidate failed" });
      onFailure?.(candidate, error);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
  throw lastError ?? new Error("No connection candidates");
}

export function startDirectProbe<T extends CandidateTransport>(options: {
  hasDirectCandidates: boolean;
  fetchCandidates: () => Promise<Candidate[]>;
  open: (candidate: Candidate) => { transport: T; ready: Promise<void> };
  isHidden: () => boolean;
  onAvailable: () => void;
  timeoutMs?: number;
  schedule?: (callback: () => void, delayMs: number) => unknown;
  cancel?: (timer: unknown) => void;
}): () => void {
  if (!options.hasDirectCandidates) return () => {};
  const schedule = options.schedule ?? ((callback, delayMs) => setInterval(callback, delayMs));
  const cancel = options.cancel ?? ((timer) => clearInterval(timer as ReturnType<typeof setInterval>));
  let active = true;
  let running = false;
  const timer = schedule(() => {
    if (!active || running || options.isHidden()) return;
    running = true;
    void (async () => {
      try {
        const candidates = (await options.fetchCandidates()).filter((candidate) => candidate.path === "direct");
        const result = await connectFirst(candidates, options.open, { direct: options.timeoutMs ?? 1500 });
        result.transport.close({ closeCode: 0, reason: "direct probe" });
        if (!active) return;
        active = false;
        cancel(timer);
        options.onAvailable();
      } catch {
        return;
      } finally {
        running = false;
      }
    })();
  }, 60_000);
  return () => {
    if (!active) return;
    active = false;
    cancel(timer);
  };
}
