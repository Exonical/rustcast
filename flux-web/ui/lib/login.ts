// Host OS-login protocol between viewer, relay and flux-hostd. The relay
// forwards hostd 0x06 frames as `login` control messages; the viewer answers
// with `login-response`. Rendered as a login card while ConnectionState is
// "login".

export interface HostLoginMessage {
  state: "prompt" | "ok" | "failed" | "busy";
  style?: "echo_on" | "echo_off" | "info" | "error";
  message?: string;
  user?: string;
  uid?: number;
  reattached?: boolean;
}

export interface LoginLine {
  kind: "prompt" | "info" | "error";
  text: string;
}

export interface LoginCardState {
  lines: LoginLine[];
  /** Current prompt awaiting an answer; null while waiting for the host. */
  prompt: { style: "echo_on" | "echo_off"; message: string } | null;
  /** True after a submit while the next host message is outstanding. */
  pending: boolean;
  /** Terminal-ish status shown above the input (ok/busy/failure summary). */
  status: string | null;
}

export function initialLoginCard(): LoginCardState {
  return { lines: [], prompt: null, pending: true, status: null };
}

/** Fold an incoming `login` control message into card state. */
export function reduceLoginMessage(prev: LoginCardState, msg: HostLoginMessage): LoginCardState {
  switch (msg.state) {
    case "prompt": {
      const style = msg.style ?? "info";
      if (style === "echo_on" || style === "echo_off") {
        return {
          ...prev,
          lines: msg.message ? [...prev.lines, { kind: "prompt", text: msg.message }] : prev.lines,
          prompt: { style, message: msg.message ?? "" },
          pending: false,
        };
      }
      return {
        ...prev,
        lines: [...prev.lines, { kind: style === "error" ? "error" : "info", text: msg.message ?? "" }],
      };
    }
    case "ok":
      return {
        ...prev,
        prompt: null,
        pending: false,
        status: msg.reattached
          ? `Resumed your session as ${msg.user}`
          : `Signed in as ${msg.user} (uid ${msg.uid})`,
      };
    case "failed":
      return {
        ...prev,
        lines: [...prev.lines, { kind: "error", text: msg.message ?? "Login failed" }],
        prompt: null,
        pending: false,
      };
    case "busy":
      return {
        ...prev,
        prompt: null,
        pending: false,
        status: msg.user ? `In use by ${msg.user}` : "Host is in use",
      };
  }
}

/** Record a submitted answer: the input clears and waits for the host. */
export function reduceLoginSubmit(prev: LoginCardState): LoginCardState {
  return { ...prev, pending: true };
}

export function okStatus(msg: Pick<HostLoginMessage, "user" | "uid" | "reattached">): string {
  return msg.reattached ? `Resumed your session as ${msg.user}` : `Signed in as ${msg.user} (uid ${msg.uid})`;
}
