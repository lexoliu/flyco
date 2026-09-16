/**
 * Every page of the first run, by id (docs/ux.md §4).
 *
 * The registry is typed against the page union, so a page that
 * `lib/flow.ts` can produce and nobody draws fails the build rather than
 * the user.
 */
import type { PageRegistry } from "../page";
import { Agents } from "./agents";
import { ApiKey } from "./apiKey";
import { AwsKeys, AwsPolicy } from "./aws";
import { AzureCommand, AzurePaste, AzureSubscription } from "./azure";
import { Credit, NewToProvider, Student } from "./bonus";
import { ClaudePaste, ClaudeSignIn } from "./claude";
import { CloudChoice, CloudSignIn } from "./cloud";
import { CodexSignIn } from "./codex";
import { ComputeChoice } from "./computeChoice";
import { DevinPaste, DevinSignIn } from "./devin";
import { GcpCommands, GcpKeyFile } from "./gcp";
import { HostEnroll } from "./host";
import { Meet } from "./meet";

export const PAGES: PageRegistry = {
  meet: Meet,
  agents: Agents,
  "claude-sign-in": ClaudeSignIn,
  "claude-paste": ClaudePaste,
  "codex-sign-in": CodexSignIn,
  "devin-sign-in": DevinSignIn,
  "devin-paste": DevinPaste,
  "api-key": ApiKey,
  "compute-choice": ComputeChoice,
  "new-to-provider": NewToProvider,
  student: Student,
  credit: Credit,
  "cloud-sign-in": CloudSignIn,
  "cloud-choice": CloudChoice,
  "azure-command": AzureCommand,
  "azure-paste": AzurePaste,
  "azure-subscription": AzureSubscription,
  "aws-policy": AwsPolicy,
  "aws-keys": AwsKeys,
  "gcp-commands": GcpCommands,
  "gcp-key-file": GcpKeyFile,
  "host-enroll": HostEnroll,
};
