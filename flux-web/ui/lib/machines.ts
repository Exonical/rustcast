import type { Machine } from "./api";

export type MachineState = "available" | "in-use" | "offline";
export type MachineFilter = "all" | "available" | "in-use" | "offline";

export function machineState(machine: Machine): MachineState {
  if (machine.status !== "online") return "offline";
  return machine.viewers > 0 ? "in-use" : "available";
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
