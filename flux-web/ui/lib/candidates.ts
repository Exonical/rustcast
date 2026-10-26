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
