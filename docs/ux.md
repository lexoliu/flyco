# Flyco UX specification

This document is the source of truth for how the flyco web app is laid out
and how it behaves. `docs/proposal.md` says what the product is;
`docs/ARCHITECTURE.md` says how the backend is built; this says what the
user sees and does. Frontend work is measured against it.

## 1. The one fact the UI must teach

A session cannot exist until three things are true:

1. the user is signed in with GitHub (identity and repositories),
2. a **harness account** is linked (Claude Code or Codex),
3. **compute** is linked (Azure, AWS, GCP, or a machine the user owns).

Every screen is designed around that readiness state. The app never shows a
form whose submission is guaranteed to fail; it shows the missing
prerequisite and the one action that resolves it.

Readiness is derived client-side from `GET /v1/harness-accounts` and
`GET /v1/providers`. It is loaded once by the app shell and shared through a
context, not refetched per screen.

## 2. Visual system

Direction: **cool neutral**, in the family of Codex Cloud and Grok Bot.

- Ground is pure white in light, near-black in dark. Surfaces are one step
  off the ground, never elevated with heavy shadows.
- Text is ink (`#111`) and two grays. No brand color on chrome.
- There is **no accent color** on buttons. The primary action is solid ink
  on white (white on ink in dark). Semantic colors exist only for status:
  green (working / done), amber (needs input / warning), red (failed /
  danger), and a neutral pulse for provisioning.
- Chips and buttons are pills. Inputs are large, low-contrast, rounded
  (16px) containers with the controls inside them, like the Codex composer.
- Type: system stack, 14px base, 1.45 line height. Headings are
  weight 600, never larger than 28px. Mono for anything the user copies.
- Icons: a consistent set (Lucide via `lucide-solid`). Provider and harness
  identity uses real logomarks as inline SVG assets under
  `frontend/src/assets/logos/`.
- Motion: status dots breathe while something is running; panels slide;
  nothing bounces. `prefers-reduced-motion` disables all of it.

Tokens live in `frontend/src/styles/tokens.css` and are the only place a
color, radius, or size is defined. Component CSS reads tokens.

## 3. Information architecture

```
/                       Home: composer + sessions
/sessions/:id           Session
/connect/harness        Connect Claude Code or Codex (wizard, also reachable from a chip)
/connect/compute        Connect compute (wizard, also reachable from a chip)
/settings               Redirects to /settings/agents
/settings/agents        Harness accounts, usage, capability matrix
/settings/compute       Providers, spend, defaults
/settings/tools         MCP servers, skills
/settings/instructions  AGENTS.md, memory tree
/settings/account       Identity, API keys, notifications, appearance
/login                  GitHub sign-in
/welcome                First-run introduction (three screens)
```

The shell is a slim top bar: wordmark left; `Sessions` and `Settings`
right, plus the account avatar with a menu (theme, sign out). No sidebar.
The home page is the product; the top bar exists to get back to it.

## 4. First run

After the first sign-in, and whenever readiness is incomplete and the user
has never dismissed it, `/welcome` shows three screens in one card, Grok
Bot style, with `Next` and `Back`:

1. **Meet flyco.** "Flyco runs the official Claude Code and Codex on a
   computer you own. You bring the agent and the machine; flyco runs the
   session, keeps the budget, and gets out of the way."
2. **Give it a brain.** The harness chooser from §8, inline. `Next` stays
   disabled, with a title naming what is missing, until an agent is linked.
   There is no skipping: a session cannot exist without one.
3. **Give it a computer.** The compute chooser from §7, inline. Same rule:
   `Start building` is disabled until compute is linked.

Finishing lands on `/`. The readiness cards on the home page (§5) exist for
an account that later unlinks something, not as a way around this flow.

## 5. Home

Layout, top to bottom, centered at 720px:

1. Greeting line: "What should we build?" (or "Welcome back, {login}").
2. **Composer** (the only primary control on the page):
   - a multi-line textarea, placeholder "Describe a task",
   - a chip row beneath it: `Harness`, `Compute`, `Repository`, `Budget`,
   - a send button (ink circle with an up arrow) at the right of the row.
3. Readiness cards, only while readiness is incomplete: one row per
   missing prerequisite, with its action.
4. **Sessions**, as a list of rows, grouped and ordered:
   `Needs input` → `Working` → `Idle` → (tab) `Archived`. Each row:
   status dot and label, title, `repo · relative time`, and on the right
   the harness mark. Rows are links to `/sessions/:id`. A search field
   filters by title and repo.
5. The session cap appears as one muted line under the list only when
   active sessions are at or above cap minus one.

### Chips

Every chip is both a status readout and the entry point to change it.

| Chip | Ready | Not ready |
|---|---|---|
| Harness | logomark + `Claude Code` (or `Codex`), plus a thin usage bar when usage is known | `+ Connect an agent` → `/connect/harness` |
| Compute | provider logomark + `Azure · eastus · Standard_B2s · $0.04/hr · spot` | `+ Add compute` → `/connect/compute` |
| Repository | `owner/name`; popover with a search box, recent repositories first | `Select repository` opens the same popover |
| Budget | `$10`; popover with a slider (1–200) and the sentence "Covers the machine and its disk. Model tokens are billed by your Claude or Codex plan." | always shown, default `$10` |

The compute chip shows the machine flyco will actually choose
(`GET /v1/machines/default?spot=`), not a dropdown of the catalog. A
popover lets the user switch account, region, spot, or pick another
curated type (§7.5).

### Sending

Send is enabled only when the textarea is non-empty and all three
prerequisites are present; otherwise it is disabled and a tooltip names the
missing one. Sending calls `POST /v1/sessions` with the prompt included
(§11), then navigates to the new session. The user types once.

## 6. Session states as the user reads them

`SessionState` is a lifecycle enum; the UI shows a **status**, derived from
the state, the session's `activity`, and the latest relay events.

`activity` is `working | needs_input | idle` on every `SessionSummary`. The
three middle rows below need facts a lifecycle enum does not carry, and the
home list has no relay to fold them out of — so the control plane maintains
them itself, from the turn events the session's daemon reports
(`POST /v1/sessions/{id}/turn-started`, `turn-completed`, `turn-failed`),
from the messages the user sends, and from whether an approval against the
session is still undecided. A page that *does* hold a relay open reads the
same two facts off the live stream and prefers them, because they are the
newer of the two. `activity` is meaningless for a session that is not
`active`: it keeps whatever it had when it was paused, interrupted or
archived, and the UI ignores it there.

| Status | Derived from |
|---|---|
| Provisioning · 2m | `provisioning`; elapsed since `created_at_unix` |
| Working | `active` and a turn is in flight |
| Needs input | `active` and (a pending approval exists, or the last turn completed and no user message followed it) |
| Idle | `active`, no turn in flight, last event is a user message or nothing yet |
| Paused · budget exhausted | `paused` |
| Interrupted · spot reclaimed | `interrupted`; the clause is `interrupted_reason` |
| Migrating · 40s | `provisioning` **and** an `interrupted_reason`: flyco is putting the session back on the disk it never lost. Elapsed since the machine went |
| Failed | `failed`; the row shows the failure reason |
| Archived | `archived` |

Amber for `Needs input`, green breathing for `Working`, neutral breathing
for `Provisioning` and `Migrating`, gray for the rest, red for `Failed`.

`interrupted_reason` is cleared when the session's daemon reaches the
control plane again, which is the moment a migration is genuinely over and
the session goes back to whatever status it had before.

## 7. Connect compute

`/connect/compute` is a chooser of four cards (Azure, AWS, Google Cloud,
Your own machine), each with a logomark and one line. Choosing one opens a
wizard in place. Every wizard ends by linking the account
(`POST /v1/providers`), which validates the credential live, then shows the
resulting **compute card**: logomark, label, region, the default machine
and its hourly price, a spot toggle, and month-to-date spend from
`GET /v1/usage/cloud`.

### 7.1 Bonus programmes

The first screen of every cloud wizard asks two questions with toggles:
"New to {provider}?" and "Are you a student?". The answers go to
`POST /v1/providers/quickstart` and any matching programme is shown as a
card with its credit and a `Sign up` link. The user can continue without
signing up.

### 7.2 Azure

Two fields, not six.

1. Show one command with a copy button:
   ```
   az ad sp create-for-rbac --name flyco --role Contributor \
     --scopes /subscriptions/$(az account show --query id -o tsv) --sdk-auth
   ```
   and the sentence "Run this in Azure Cloud Shell or a terminal with the
   Azure CLI signed in. It prints a JSON block."
2. One textarea: "Paste the JSON block." The frontend parses `clientId`,
   `clientSecret`, `tenantId`, `subscriptionId` from it and shows them as
   read-only confirmation rows. Malformed input is an inline error naming
   which key is missing.
3. `Link Azure`.

The service principal is scoped to the subscription, so flyco creates and
owns the resource group itself (`flyco`, in the account's default region)
at link time. The user never names a resource group.

The break-glass SSH key is generated **in the browser** (Ed25519 via a
library, never hand-rolled): the private key is offered once as a download
and a copy button, the public key is sent with the credentials. Flyco
never holds the private key, which keeps the existing design stance. An
`Advanced` disclosure lets a user paste their own public key instead.

### 7.3 AWS

1. Show the minimal IAM policy JSON flyco needs (rendered from a template
   in the repo, with a copy button) and a link to the IAM console page
   that creates an access key.
2. Fields: Access key ID, Secret access key. Session token is under
   `Advanced`.
3. `Link AWS`.

The break-glass key pair is optional on AWS and stays under `Advanced`.

### 7.4 Google Cloud

1. Show the `gcloud` commands that create a service account with the
   Compute Admin role and download its key file.
2. A file drop zone that accepts one `.json` file; the frontend reads it
   and shows `project_id` and `client_email` as confirmation.
3. `Link Google Cloud`.

### 7.5 Your own machine

The control plane runs on Cloudflare Workers and has no TCP sockets, so it
cannot open an SSH connection to a user's host. A machine the user owns is
therefore **enrolled**, not dialled: the user runs one installer command
on the host, the host connects outbound and registers itself, and flyco
schedules session containers onto it. This is the same shape as GitHub
self-hosted runners and Tailscale.

The wizard shows: one command with a copy button, the sentence that the
machine has to be Linux with Podman (the installer installs Podman when it
is absent), how long the command has left, and a live "Waiting for the
machine…" state polling `GET /v1/hosts/enrollment-tokens/{id}`. A command
nobody ran in ten minutes reads "The command expired" and offers `Mint a
new command`. Once the host has enrolled the wizard shows its compute
card: hostname, architecture, vCPUs, memory, free disk, and an online dot.
There is no credential step, because there is no credential — the machine
authenticates itself.

The compute card for a host states `your hardware` where a cloud account
states a price, and carries `Rename` and `Remove`. A removal the control
plane refuses with `host-has-active-sessions` names how many sessions are
still running there — read off the refusal's `active_sessions` extension
member (RFC 9457 §3.2), never parsed out of its `detail` — and offers
`Remove anyway`, which passes `force`.

### 7.6 Catalog curation

Cloud catalogs are large and mostly redundant. Flyco shows the user and
the agent a **curated** catalog, computed in `flyco-core` from the raw
provider catalog:

- keep only the newest CPU generation of each family the provider
  offers (newest is also usually cheapest),
- within one architecture (x86-64, arm64) and one region, drop any type
  that costs the same or more than another type with at least the same
  vCPUs and memory (strict Pareto frontier on price vs. capacity),
- order by price.

The default machine is the cheapest Linux entry of the curated catalog.
The agent's `machine_resize` tool sees the same curated list.

### 7.7 Choosing a machine by hand

The compute chip's popover is a **tiered slider**, not a table. Its detents are the curated catalog of the selected account and region ordered by price; the thumb snaps to a detent and the label above it reads `Standard_D4s_v6 · 4 vCPU / 16 GiB · $0.19/hr`. The leftmost position is `Auto`. `Auto` is the cheapest curated Linux type with at least 4 vCPU and 16 GiB; the label says so. An `Advanced ›` disclosure above the slider reveals account, region, architecture (x86-64 / arm64), OS family, and spot. Choosing any detent other than `Auto` sets `machine_origin: user`; the chip then reads `Chosen by you`.

License-bound types (macOS on EC2 Mac, and any type with a billing minimum) render with an amber badge on their detent and a sentence under the slider: "Starts a 24-hour minimum charge of $X the moment it boots." The sentence must be visible before send is enabled.

## 8. Connect an agent

`/connect/harness` is two cards: Claude Code and Codex. Each expands in
place.

### 8.1 Claude Code

Flyco runs the same OAuth flow the Claude CLI runs, in the browser:

1. `Sign in with Claude` opens `https://claude.ai/oauth/authorize` in a
   new tab with `code=true`, `response_type=code`, the Claude Code client
   id, `redirect_uri=https://console.anthropic.com/oauth/code/callback`,
   `scope=org:create_api_key user:profile user:inference`, a PKCE S256
   challenge, and a `state`. The verifier and state are minted by the
   backend (`POST /v1/harness-accounts/claude/oauth/start`) and held in KV
   for ten minutes.
2. Anthropic shows the user a code (`CODE#STATE`). The wizard's second
   step is a single field: "Paste the code Anthropic shows you."
3. `POST /v1/harness-accounts/claude/oauth/complete` exchanges the code at
   `https://console.anthropic.com/v1/oauth/token` with the stored
   verifier, stores access and refresh tokens encrypted, and returns the
   linked account. Refresh happens server-side before expiry.

An `Advanced` disclosure keeps the two existing paths (setup token, API
key) for people who prefer them.

After linking, the card shows the plan and usage from `GET /v1/usage/llm`.

### 8.2 Codex

`OpenAI API key` field with a link to the keys page. The card shows usage
after linking.

## 9. Session

### 9.1 Header

Title (editable inline; defaults to the first prompt's excerpt),
`repo · branch`, the status pill from §6, the machine chip
(`Standard_B2s · $0.04/hr · spot`), a budget ring (`$1.20 / $10`) and a
context ring (`41k / 200k`). Actions: `Archive` and a `⋯` menu (stop
machine, start machine, resize, edit `.env`, copy session id).

The budget ring is a control and the context ring is not: one of its two
numbers is something the user set. Clicking it opens the same slider the
composer's budget chip does, floored at the first whole dollar above what
the session has already spent, and an explicit `Set budget to $25` commits
it. That is the only way out of `Paused · budget exhausted`: a limit above
the spend puts the session back to `active` and tells its daemon to carry
on from where the pause interrupted it. A paused session therefore also
carries a notice above the transcript — "The $10.00 budget is spent. Raise
it to continue." — with the same control as its action, because the header
is not where a user looks when the page tells them the session stopped.

### 9.2 Transcript

- User messages are right-aligned bubbles.
- Assistant text renders as Markdown (a library, sanitized) with code
  blocks and copy buttons.
- Tool calls are one-line rows: icon, a human summary
  (`Read src/main.rs`, `Ran cargo test`, `Edited 3 files`), duration,
  and a status glyph. Expanding a row shows input and output.
- A completed turn ends with `Worked for 8m 23s`.
- Provisioning renders **inside** the transcript as a timeline: `Reserving
  a machine on Azure` → `Booting` → `Installing flycod` → `Cloning
  owner/repo` → `Agent ready`, each with elapsed time, driven by
  `session_state_changed` and daemon events. "No turns yet" never
  appears while a machine is being built. A session reclaimed from spot
  gets **another timeline in the place it happened**, headed `Migrating`
  and holding only the stages a restart goes through — nothing is
  installed or cloned, because the disk already has both.
- Approvals appear inline as an **action card** (title, the exact
  operation, `Approve` / `Deny`) and, while any is pending, as a sticky
  amber banner at the top of the transcript.
- Budget signals (50 / 80 / 90 / paused), spot notices with a countdown,
  usage-limit pauses with the reset time, and compaction all appear as
  inline notices with an icon.

### 9.3 Composer

Same component as the home composer, without chips. While a turn is in
flight the send button becomes `Stop`. `/` opens a command palette
(`/compact`, `/archive`, `/resize`); a message beginning with `!` runs in
the machine's bash, and the composer says so under the field while typing
one.

### 9.4 Drawer

A right-side drawer, collapsed by default, with tabs `Terminal`, `Files`,
`Diff`, `Machine`, `Env`. `Terminal` is the existing xterm panel.
`Machine` shows spec, hourly, storage hourly, state, and the
start/stop/resize controls. `Env` is the existing editor. Keyboard: `⌘.`
toggles the drawer.

`Files` is the session's checkout, read-only: a lazy tree that fetches one
directory at a time, files git ignores shown and marked rather than hidden,
and a syntax-highlighted view of any text file up to 128 KiB. A file past
that, or one that is not text, says so and points at the terminal.

`Diff` is what this session changed, against the branch it started from:
the working tree — committed, uncommitted and untracked — diffed against
`origin/<branch>`, one collapsible section per file, each headed by the
path, what happened to it, and `+n −m`. A file whose diff runs past ~900
lines opens as `Large diff · N lines · Load diff` instead of rendering.

Both are answered live by the session's machine, over the relay: there is
no copy of a working tree in the control plane, so a session with no daemon
connected says exactly that rather than showing an empty tree.

### 9.5 Machine changes during a session

When the agent resizes, the transcript shows a notice: `Switched to Standard_D8s_v6 · restarted the machine · disk kept`. When the agent asks to move to a license-bound type, the request is an approval card, not a silent resize, and the card quotes the minimum charge. The agent is told whether the machine was chosen by the user and to be conservative about switching it.

What the agent sees of all this is `flycod`'s local MCP server (§11): `machine_status` reports the machine and who chose it, `budget_status` reports what is left, and `machine_resize` moves the session. The resize tool's description states that resizing restarts the machine, lists only the curated catalog of §7.6 with each entry's hourly price and — for a license-bound type — the minimum charge in dollars, and refuses while the working tree is dirty unless it is called again with `force` and a reason. A resize to a license-bound type is never performed on the agent's own authority: the daemon raises the approval, the tool answers that the request is pending the user's decision, and the control plane performs the resize if the user approves. When the machine was the user's choice, every one of those places says so in the same words: "The user chose this machine themselves; do not switch it unless the task cannot proceed on it, and say why when you do."

## 10. Settings

Left-hand vertical navigation, five sections. Each section is built from
cards, not from forms mirroring database rows.

| Section | Contents |
|---|---|
| Agents | one card per harness: linked state, plan, usage bars, `Relink`, `Unlink`; below, `What works on each harness`, a collapsed matrix |
| Compute | one compute card per linked account (§7); defaults: spot on/off, preferred region; `Add compute` |
| Tools | MCP servers as cards with an enable toggle and `Edit`; skills as cards with scope, version, and a drop zone for a zip |
| Instructions | `AGENTS.md` editor with save; pending change requests from agents render as diffs with `Accept` / `Reject`; memory as an outliner tree |
| Account | GitHub identity, API keys (create shows the key once), notifications with a single `Enable push` button, appearance, sign out |

## 11. API changes this specification requires

Pre-1.0, the API changes to fit the product; no compatibility shims.

- `POST /v1/sessions` gains `prompt: string` (required). The prompt is
  queued for the daemon and delivered as the first user message as soon as
  the harness starts.
- `SessionSummary` gains `title: string` (first prompt excerpt, editable
  via `PATCH /v1/sessions/{id}`).
- `GET /v1/machines/default?spot=` returns the curated default choice and
  its catalog entry. `GET /v1/machines/catalog` returns the curated
  catalog (§7.6).
- Azure credentials: `resource_group` and `admin_ssh_public_key` are
  replaced by `admin_ssh_public_key` only; flyco creates the resource
  group at link time and stores its name on the account.
- `POST /v1/harness-accounts/claude/oauth/start` and
  `.../oauth/complete`; `HarnessCredentialInput` gains `ClaudeOauth`
  storing access and refresh tokens and expiry.
- Host enrollment for user-owned machines is specified in its own issue.
- Four daemon-scoped routes are what `flycod`'s local MCP server is made
  of, authenticated by the session's `fd_` token and by nothing else:
  `GET /v1/sessions/{id}/agent/machine` (the machine and who chose it,
  as `AgentMachineView`), `GET /v1/sessions/{id}/agent/machine/catalog`
  (the curated catalog of §7.6, narrowed to the account and region the
  session's disk lives in — the two a resize cannot cross),
  `POST /v1/sessions/{id}/agent/machine/resize`, and
  `GET /v1/sessions/{id}/agent/budget`. The resize route refuses a
  license-bound type with `409 license-bound-resize-needs-approval`: the
  agent has to raise an approval instead, and the rule is enforced by
  the control plane rather than described to the model.
- `ApprovalPayload` gains `machine_resize_license_bound { machine_type,
  minimum, reason }`. Approving it *is* the resize — the control plane
  performs it, because nothing is waiting on the daemon's side.
- `ClientEvent` gains `machine_changed { machine_type, hourly, spot,
  restarted }`, which the transcript renders as the notice in §9.5.
  `ControlToDaemon` gains the same variant, and it is the one command a
  room holds for a daemon that is not connected: a resize restarts the
  machine, so there is never a daemon listening at the moment it is sent.

## 12. Delivery order

Each item is one issue, one branch, one PR to `dev`, deployed to
`dev.flyco.dev` on merge.

1. Design system, shell, home (composer, chips, session list, readiness
   cards), welcome flow, `prompt` on session creation, `title` on
   summaries, `machines/default`.
2. Connect compute: wizards for Azure, AWS, GCP; catalog curation;
   Azure resource-group creation; browser-side key generation.
3. Connect an agent: Claude OAuth, Codex, usage on cards.
4. Session page: transcript rendering, provisioning timeline, inline
   approvals, composer with stop and commands, drawer.
5. Settings regrouped into five sections.
6. Host enrollment for user-owned machines (backend design first).
