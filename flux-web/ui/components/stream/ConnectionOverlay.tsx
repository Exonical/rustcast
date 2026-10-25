"use client";

import type { ConnectionState, ResolutionStatus } from "@/lib/webtransport-client";
import type { LoginCardState } from "@/lib/login";
import { Button } from "@/components/ui/button";

export function ConnectionOverlay({ state, machineName, error, endedReason, resolutionStatus, login, onLoginSubmit, onReconnect, onBack }: {
  state: ConnectionState;
  machineName: string;
  error: string | null;
  endedReason: string | null;
  resolutionStatus: ResolutionStatus | null;
  login: LoginCardState | null;
  onLoginSubmit: (response: string) => void;
  onReconnect: () => void;
  onBack: () => void;
}) {
  return (
    <>
      {resolutionStatus?.state === "transitioning" && (
        <div data-local-control className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-amber-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-amber-100 shadow-xl backdrop-blur-xl">
          Changing resolution to {resolutionStatus.width}x{resolutionStatus.height}…
        </div>
      )}
      {resolutionStatus?.state === "recovering" && (
        <div data-local-control role="status" className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-amber-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-amber-100 shadow-xl backdrop-blur-xl">
          <p>Recovering video</p>
          {resolutionStatus.error && <p className="mt-1 text-amber-100/70">{resolutionStatus.error}</p>}
        </div>
      )}
      {resolutionStatus?.state === "succeeded" && (
        <div data-local-control className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-emerald-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-emerald-100 shadow-xl backdrop-blur-xl">
          Resolution applied · waiting for video
        </div>
      )}
      {resolutionStatus?.state === "failed" && (
        <div data-local-control role="alert" className="absolute left-1/2 top-24 z-20 -translate-x-1/2 rounded-xl border border-red-300/20 bg-[#111318]/90 px-4 py-3 text-center text-xs text-red-100 shadow-xl backdrop-blur-xl">
          Resolution change failed{resolutionStatus.error ? `: ${resolutionStatus.error}` : "."}
        </div>
      )}
      {state === "login" && login && (
        <div data-local-keyboard className="absolute inset-0 z-10 grid place-items-center bg-black/30 px-4 backdrop-blur-[2px]">
          <section className="w-full max-w-md rounded-3xl border border-white/10 bg-[#111318]/95 p-7 text-left shadow-2xl sm:p-9">
            <div className="mx-auto grid h-14 w-14 place-items-center rounded-2xl bg-blue-300/10 text-blue-200">⌨</div>
            <p className="mt-5 text-center text-xl font-semibold text-white">Sign in to {machineName}</p>
            {login.lines.length > 0 && (
              <div className="mt-4 space-y-1.5 text-sm">
                {login.lines.map((line, i) => (
                  <p key={i} className={line.kind === "error" ? "text-red-300" : line.kind === "prompt" ? "text-zinc-300" : "text-zinc-500"}>
                    {line.text}
                  </p>
                ))}
              </div>
            )}
            {login.status && <p className="mt-4 text-sm text-emerald-200">{login.status}</p>}
            {login.prompt ? (
              <form
                className="mt-5"
                onSubmit={(e) => {
                  e.preventDefault();
                  const form = e.currentTarget;
                  const input = form.elements.namedItem("login-response") as HTMLInputElement;
                  onLoginSubmit(input.value);
                  input.value = "";
                }}
              >
                <label className="sr-only" htmlFor="login-response">{login.prompt.message || "Response"}</label>
                <input
                  id="login-response"
                  name="login-response"
                  type={login.prompt.style === "echo_off" ? "password" : "text"}
                  autoFocus
                  autoComplete="off"
                  placeholder={login.prompt.message || "Response"}
                  className="w-full rounded-xl border border-white/10 bg-white/5 px-4 py-3 text-sm text-white placeholder:text-zinc-600 focus:border-blue-300/40 focus:outline-none"
                />
              </form>
            ) : (
              <div className="mt-5 flex items-center gap-3 text-sm text-zinc-500">
                <span className="h-4 w-4 animate-spin rounded-full border-2 border-zinc-600 border-t-blue-300" />
                Waiting for {machineName}…
              </div>
            )}
          </section>
        </div>
      )}
      {(state !== "connected" && state !== "login" || endedReason) && (
        <div data-local-control className="absolute inset-0 z-10 grid place-items-center bg-black/20 px-4 backdrop-blur-[2px]">
          <section className="w-full max-w-md rounded-3xl border border-white/10 bg-[#111318]/95 p-7 text-center shadow-2xl sm:p-9">
            <div className={`mx-auto grid h-14 w-14 place-items-center rounded-2xl ${endedReason ? "bg-amber-300/10 text-amber-200" : state === "connected" ? "bg-emerald-300/10 text-emerald-200" : "bg-blue-300/10 text-blue-200"}`}>
              {endedReason ? "Ⅱ" : state === "connected" ? "✓" : state === "failed" ? "!" : "◌"}
            </div>
            <p className="mt-5 text-xl font-semibold text-white">
              {endedReason ? "Session ended" : state === "connecting" ? "Opening your workspace" : state === "failed" ? "Couldn’t connect" : "Stream disconnected"}
            </p>
            <p className="mx-auto mt-2 max-w-sm text-sm leading-6 text-zinc-500">
              {endedReason ? `${endedReason} · ${machineName}` : error || (state === "connecting"
                ? `Reaching ${machineName}. This usually takes a few seconds.`
                : "The connection was interrupted. Flux will try again automatically.")}
            </p>
            {(state !== "connecting" || endedReason) && (
              <div className="mt-6 flex flex-col justify-center gap-2 sm:flex-row">
                <Button variant="primary" onClick={onReconnect}>{endedReason ? "Reconnect" : "Try again"}</Button>
                <Button onClick={onBack}>Back to machines</Button>
              </div>
            )}
          </section>
        </div>
      )}
    </>
  );
}
