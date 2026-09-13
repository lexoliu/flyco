/**
 * Reading a machine the user owns out loud.
 *
 * A host reports its own facts on every `Hello` — architecture, vCPUs,
 * memory, free disk, Podman, kernel, hostname — and flyco has no other way
 * to learn them (docs/host-enrollment.md). What is left is presentation:
 * the two lines the compute card states about the hardware, and the word
 * for where the machine is in its life.
 */
import type { HostFacts, HostState, HostView } from "../api/client";

/** How many MiB are in a GiB. Host memory is reported in MiB. */
const MIB_PER_GIB = 1024;

/** How a host's state is written where a person reads it. */
export const HOST_STATE_LABEL: Record<HostState, string> = {
  online: "Online",
  offline: "Offline",
  draining: "Draining",
  removed: "Removed",
};

/** `8 vCPU / 32 GiB`, the same shape a cloud machine's capacity reads in. */
export function hostCapacityLabel(facts: HostFacts): string {
  return `${facts.vcpus} vCPU / ${Math.round(facts.memory_mib / MIB_PER_GIB)} GiB`;
}

/** `210 GiB free`, which is what decides whether another session fits. */
export function hostDiskLabel(facts: HostFacts): string {
  return `${facts.disk_free_gib} GiB free`;
}

/** `Podman 5.4.0 · Linux 6.8.0-45-generic`, the machine's own software. */
export function hostSoftwareLabel(facts: HostFacts): string {
  return `Podman ${facts.podman_version} · Linux ${facts.kernel}`;
}

/**
 * Whether a machine can take a session right now.
 *
 * Only `online` can: a host whose attachment has dropped still holds its
 * containers, but nothing can be sent to it until it comes back.
 */
export function isHostReachable(host: HostView): boolean {
  return host.state === "online";
}
