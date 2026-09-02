import type { CloudProviderKind } from "../api/client";

/** Human-readable label for each cloud provider kind, shared by every screen that lists providers. */
export const PROVIDER_LABEL: Record<CloudProviderKind, string> = {
  azure: "Azure",
  aws: "AWS",
  gcp: "Google Cloud",
  host: "Your own machine",
};
