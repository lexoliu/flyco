/**
 * Every page of the first run, by id (docs/ux.md §4).
 *
 * The registry is typed against the page union, so a page that
 * `lib/flow.ts` can produce and nobody draws fails the build rather than
 * the user.
 */
import type { PageRegistry } from "../page";
import { AgentChoice } from "./agentChoice";
import { AgentLinked } from "./agentLinked";
import { ApiKey } from "./apiKey";
import { AwsKeys, AwsPolicy } from "./aws";
import { AzureCommand, AzureKey, AzurePaste, AzureSubscription } from "./azure";
import { Credit, NewToProvider, Student } from "./bonus";
import { ClaudePaste, ClaudeSignIn } from "./claude";
import { CodexSignIn } from "./codex";
import { ComputeChoice } from "./computeChoice";
import { ComputeLinked } from "./computeLinked";
import { GcpCommands, GcpKeyFile } from "./gcp";
import { HostEnroll } from "./host";
import { Meet } from "./meet";

export const PAGES: PageRegistry = {
  meet: Meet,
  "agent-choice": AgentChoice,
  "claude-sign-in": ClaudeSignIn,
  "claude-paste": ClaudePaste,
  "codex-sign-in": CodexSignIn,
  "api-key": ApiKey,
  "agent-linked": AgentLinked,
  "compute-choice": ComputeChoice,
  "new-to-provider": NewToProvider,
  student: Student,
  credit: Credit,
  "azure-command": AzureCommand,
  "azure-paste": AzurePaste,
  "azure-subscription": AzureSubscription,
  "azure-key": AzureKey,
  "aws-policy": AwsPolicy,
  "aws-keys": AwsKeys,
  "gcp-commands": GcpCommands,
  "gcp-key-file": GcpKeyFile,
  "host-enroll": HostEnroll,
  "compute-linked": ComputeLinked,
};
