import type { Machine } from "./api";

export type MachineState = "available" | "in-use" | "locked" | "offline";
export type MachineFilter = "all" | "available" | "in-use" | "offline";

export function machineState(machine: Machine): MachineState {
  if (machine.status !== "online") return "offline";
  if (machine.login_state === "locked") return "locked";
  return machine.viewers > 0 ? "in-use" : "available";
}

/** True when this viewer may attach to the machine's host login. */
export function canJoinMachine(
  machine: Machine,
  viewer: { username?: string; admin?: boolean } | null,
): boolean {
  const state = machineState(machine);
  if (state === "offline") return false;
  if (viewer?.admin) return true;
  const osUser = machine.logged_in_user;
  if (!osUser || machine.login_state === "idle" || machine.login_state == null) {
    return state === "available";
  }
  return viewer?.username != null && viewer.username === osUser;
}

export function dedupeDefaultMachine(machines: Machine[]): Machine[] {
  const registeredEndpoints = new Set(
    machines.filter((machine) => machine.id !== "default" && machine.status === "online")
      .map((machine) => machine.frame_endpoint),
  );
  return machines.filter((machine) =>
    machine.id !== "default" || !registeredEndpoints.has(machine.frame_endpoint),
  );
}

export function filterMachines(
  machines: Machine[],
  query: string,
  filter: MachineFilter,
): Machine[] {
  const needle = query.trim().toLocaleLowerCase();
  return dedupeDefaultMachine(machines).filter((machine) => {
    if (filter !== "all" && machineState(machine) !== filter) return false;
    if (!needle) return true;
    return [
      machine.name,
      machine.display_name,
      machine.os,
      machine.gpu_vendor,
      machine.encoder_backend,
    ].some((value) => value?.toLocaleLowerCase().includes(needle));
  });
}
