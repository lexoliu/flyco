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
/connect/harness        Connect Claude Code or Codex (stage B of §4 alone, also reachable from a chip)
/connect/compute        Connect compute (stage C of §4 alone, also reachable from a chip)
/settings               Redirects to /settings/agents
/settings/agents        Harness accounts, usage, capability matrix
/settings/compute       Providers, spend, defaults
/settings/tools         MCP servers, skills
/settings/instructions  AGENTS.md, memory tree
/settings/account       Identity, API keys, notifications, appearance
/login                  GitHub sign-in
/welcome                First run (§4): one linear sequence of pages
```

The shell is a slim top bar: wordmark left; `Sessions` and `Settings`
right, plus the account avatar with a menu (theme, sign out). No sidebar.
The home page is the product; the top bar exists to get back to it.

## 4. First run

After the first sign-in, and whenever readiness is incomplete and the user
has never dismissed it, `/welcome` walks the user through one **linear
sequence of pages**. The rules are absolute, and they are what the
previous design broke by rendering the compute wizard *inside* a wizard
step (two questions side by side, an inner `Continue` above an outer
`Start building`):

- **One page, one question or one action.** A page asks exactly one thing
  (a choice, a field, a paste, a file) or shows exactly one thing to do
  (run a command, sign in). Nothing on a page has its own `Continue`.
- **One primary button, always in the footer.** The footer is the only
  navigation: `Back` on the left, the page's primary on the right. The
  primary's label is the page's verb (`Next`, `Sign in with Claude`,
  `Link Azure`, `Start building`), and it is disabled — styled as
  disabled, with a `title` naming what is missing — until the page's
  prerequisite is met. There is never a second primary anywhere on the
  page, and there is no `Skip`.
- **The sequence is data, not nesting.** The pages are a list computed
  from the answers so far (`lib/flow.ts`); choosing Azure appends Azure's
  pages, answering "student: yes" appends a credit page only when a
  programme matches. `Back` goes one page back and keeps the answers. The
  three progress bars are the three stages; each fills with the page
  position inside its stage.
- **Secondary paths are links to other pages**, never disclosures with
  their own submit. "Use an API key instead" is a quiet link on the
  sign-in page that leads to a page whose single question is the key.
- **The user has a browser and nothing else.** No page assumes a
  terminal, an installed CLI, or a file on disk. Where a vendor's own API
  is only reachable through a command, the page sends the user to that
  vendor's terminal *in the browser* — Azure Cloud Shell, Google Cloud
  Shell — with a link that opens it, the command with `Copy`, shown whole
  (wrapped, never clipped), and a sentence saying what it prints and how
  that gets back here. A file a command produces is downloaded to the
  browser by the command itself (`cloudshell download`), never left in a
  home directory the user cannot see. The one exception is a machine the
  user owns, which by definition has to be reached on that machine.

The pages, by stage:

**A. Meet flyco** — one page: "Flyco runs the official Claude Code and
Codex on a computer you own. You bring the agent and the machine; flyco
runs the session, keeps the budget, and gets out of the way." `Next`.

**Answer controls fill the page.** A question page's answer is the page's
body, not a widget under it: a single-choice question (Yes / No, the
provider, an option) renders as full-width selectable cards — the same
component as *Where should sessions run?*, radio semantics, one line each,
a check on the chosen one — never as a row of small pills. A text answer is
one full-width field. The card, the field and the footer share one width,
so the eye has one column to read. Every page is screenshotted at 1100 px
and 390 px wide and looked at before the flow is called done: a control
that reads as an afterthought at either width is a defect.

**B. Give it a brain**

The agent is chosen **per task**, in the composer's agent chip, never
here: flyco supports several agents at once, and this stage only links
them. There is no "which agent do you use?" question, and there is no
confirmation page after a link — a finished link returns to the list,
which now reads `Linked` beside the agent.

1. *Link the agents you use.* **One page, however many agents flyco
   runs**: a hundred harnesses would still be one page, never a hundred
   steps. One sentence ("Each task picks its agent when you start it.
   Link every agent you use; one is enough to begin."), then one
   full-width selectable card per agent — the logomark, the name, and its
   status on the line beneath: `Linked · me@lexo.cool`, or `Not linked · a
   Claude subscription or an Anthropic API key`. Choosing an unlinked card
   turns the primary into `Link Claude Code` / `Link Codex`, which walks
   that agent's sign-in pages (2–4) and comes back here. Otherwise the
   primary is `Next`, disabled with "Link at least one agent to continue"
   until something is linked: a session cannot exist without an agent, so
   there is no way past this page with nothing linked, and nothing to
   skip.
2. *Link Claude Code.* One sentence ("Runs on your Claude subscription.
   Flyco opens Anthropic's own sign-in page; your password never reaches
   flyco."). Primary `Sign in with Claude` (opens the OAuth page in a new
   tab and advances to the paste page), with the quiet link *Use an API
   key instead*. `Back` returns to the list with the agent still chosen.
3. *Paste the code Anthropic shows you.* One field; primary `Link Claude
   Code`, disabled until the field has a code; a rejected code is an
   inline `ProblemNotice` under the field. Success returns to the list.
2′. *Link Codex.* The one-time code is requested when the page opens (no
   button to ask for it): the code with `Copy code`, the link `Open
   auth.openai.com/codex/device`, "Waiting for you to approve in the
   browser…" while it polls; approval returns to the list by itself. An
   expired code turns the primary into `Get a new code`; until then the
   primary is `Next`, disabled with "Approve the code in the browser to
   continue". The quiet link *Use an API key instead* leads to page 4.
4. *Paste your API key* (either agent, reached only by the link). One
   field that accepts an API key or a `claude setup-token`, told apart by
   prefix. Primary `Link Claude Code` / `Link Codex`; success returns to
   the list as the sign-in path does.

Settings › Agents › `Connect …` names one agent, and a flow opened for
one agent has nothing to list: it walks that agent's pages (2–4) alone
and a finished link ends it, back on settings.

The agent chip on the home composer is where a task picks its agent; with
one agent linked it states that agent, with two it is a choice.

**C. Give it a computer**

1. *Where should sessions run?* Four selectable cards — Azure, AWS,
   Google Cloud, Your own machine — one line each. `Next`.
2. Cloud providers — *New to {provider}?* Yes / No pills (a radio group,
   never a toggle). `Next`.
3. Cloud providers — *Are you a student?* Yes / No. `Next`.
4. Cloud providers — *{provider} gives you credit* — only when
   `POST /v1/providers/quickstart` matches a programme: its name, the
   credit, the quiet link `Sign up` (new tab). `Next`. No page when
   nothing matches.
5. Azure — *Sign in with Microsoft.* The road: the consent screen
   Microsoft offers, like GitHub's. One sentence ("Microsoft's own sign-in
   page opens in a new tab. Flyco creates its own limited identity in the
   subscription you pick and never keeps your password or your sign-in.").
   Primary `Sign in with Microsoft`: opens the consent in a new tab and
   the page waits ("Waiting for you to finish in the other tab…"), polling
   the attempt; the consent's own tab lands on `/connect/return`, which
   says the tab can be closed. When the consent is back the flow moves on
   by itself. A lost attempt is a `ProblemNotice` and the primary becomes
   `Try again`. A consent the vendor refused (declined, or an organization
   that needs an administrator to approve apps) is reported by the poll as
   `failed`: this page shows the vendor's words, says Cloud Shell needs no
   approval, and the primary becomes `Try again`; the vendor's own tab
   lands on `/connect/return?problem=…&reason=…`, which explains the same
   and carries one action, `Try another way`, back to the compute stage.
   The quiet link *Use Cloud Shell instead* leads to pages
   5a–5b, for a tenant whose administrator has switched consent off.
6. Azure — *Which subscription?* "Signed in as me@lexo.cool." The
   subscriptions the account may link, as selectable cards (name, id).
   `Next`.
5a. Azure, Cloud Shell — *Run this in Azure Cloud Shell.* "Cloud Shell is
   a terminal in your browser, already signed in to your Azure account.
   Nothing to install." The one command with `Copy`, shown whole, the
   link *Open Azure Cloud Shell* (new tab), and the sentence that it
   prints a JSON block the next page asks for. `Next`.
5b. Azure, Cloud Shell — *Paste the JSON block.* One textarea; the parsed
   `clientId`, `tenantId`, `subscriptionId` appear as read-only rows
   under it once it parses; a missing key is an inline error naming it (a
   block without a subscription appends a *Which subscription?* page that
   asks for the id). `Link Azure`, disabled until it parses; a block
   without a subscription shows `Next` and the *Which subscription?* page
   links instead. Behind the consent, *Which subscription?* is the page
   that links. Either validates the credential live; a refusal is a
   `ProblemNotice` above the footer and the primary stays. Success
   **finishes the flow**: there is no "Azure is linked" page; the home
   page's compute chip already states the account, region, default machine
   and price. There is no key page: the machine login key is flyco's,
   minted and sealed by the control plane when the account links.
5′. AWS — *Create an access key.* The minimal IAM policy JSON with `Copy`
   and the link to the IAM console page. `Next`.
6′. AWS — *Enter the access key.* Two fields (Access key ID, Secret access
   key); the quiet link *I have a session token* reveals the third field
   in place. Primary `Link AWS`; success finishes the flow.
5″. Google Cloud — *Sign in with Google.* The same shape as Azure's
   consent page: Google's own sign-in in a new tab, the wait, the return
   tab, `Try again` on a lost attempt, and *Use Cloud Shell instead* as
   the quiet link to 5″a–6″a.
6″. Google Cloud — *Which project?* "Signed in as me@lexo.cool." The
   active projects as selectable cards. Primary `Link Google Cloud`: the
   control plane creates flyco's service account in the project with the
   Compute Admin role, links it, and the flow finishes.
5″a. Google Cloud, Cloud Shell — *Run this in Google Cloud Shell.* The
   `gcloud` commands with `Copy`, ending in `cloudshell download
   flyco-key.json` so the key lands in the browser's downloads, the link
   *Open Google Cloud Shell*, and the sentence that the next page asks for
   that file. `Next`.
6″a. Google Cloud, Cloud Shell — *Drop the key file.* One drop zone;
   `project_id` and `client_email` as confirmation rows. Primary `Link
   Google Cloud`; success finishes the flow.
2‴. Your own machine — *Run this on the machine.* No bonus questions.
   The one installer command with `Copy`, the Linux + Podman sentence, how
   long the command has left, and "Waiting for the machine…" while it
   polls; enrollment finishes the flow by itself. An expired command turns
   the primary into `Mint a new command`; until then the primary is
   `Next`, disabled with "Run the command on the machine to continue".

Finishing lands on `/`. Settings › Agents › `Connect …` opens the agent's
own page (stage B, that page alone, returning to settings on success) and
Settings › Compute › `Add compute` opens stage C alone, in the same frame,
so the flow exists once; there is no second wizard implementation and no
inline chooser anywhere else. The readiness cards on the home page (§5)
exist for an account that later unlinks something, not as a way around
this flow.

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
| Harness | logomark + `Claude Code` (or `Codex`); popover lists the linked agents with the chosen one marked, ending in `Connect another agent` → `/connect/harness` | `+ Connect an agent` → `/connect/harness` |
| Compute | provider logomark + `Azure · B2s · $0.04/hr` and `Auto` or `Chosen`; region, spot and the account live in the popover | `+ Add compute` → `/connect/compute` |
| Repository | `owner/name`; popover with a search box, recent repositories first | `Select repository` opens the same popover |
| Budget | `$10`; popover with a slider (1–200) and the sentence "Covers the machine and its disk. Model tokens are billed by your Claude or Codex plan." | always shown, default `$10` |

The compute chip shows the machine flyco will actually choose
(`GET /v1/machines/default?spot=`), not a dropdown of the catalog. A
popover lets the user switch account, region, spot, or pick another
curated type (§7.5).

The chips sit on one line at desktop widths. The compute chip is the one
that gives way (its label shrinks to an ellipsis), so choosing a machine —
which rewrites that label on every slider detent — never reflows the row
under the open popover. Below 900px the row wraps and the compute chip takes
a whole line for the same reason. A chip never leaves the page: a linked
harness or compute chip opens its picker, and only the missing-prerequisite
form of a chip is a link.

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

Every cloud flow opens with two pages, one question each (§4 C2, C3):
"New to {provider}?" and "Are you a student?", both Yes / No radio pills.
The answers go to `POST /v1/providers/quickstart`; a matching programme
gets its own page with the credit and a quiet `Sign up` link, and nothing
matching means no page at all. The user is never asked to sign up.

### 7.2 Azure

The road is Microsoft's consent screen; Cloud Shell is the quiet link.

1. `POST /v1/providers/azure/oauth/start` mints an attempt and the
   Microsoft authorize URL (`common` tenant, scopes `openid
   offline_access`, Azure Service Management `user_impersonation`,
   Microsoft Graph `Application.ReadWrite.All`). The page opens it in a
   new tab and polls `GET /v1/providers/azure/oauth/{attempt}`.
2. `GET /v1/providers/azure/oauth/callback` (public) exchanges the code,
   reads who signed in and their tenant from the id token, lists the
   enabled subscriptions, stores them on the attempt, and sends that tab
   to `/connect/return?provider=azure`.
3. The poll answers `authorized` with the account and the subscriptions;
   the page asks which, and links from there.
4. `POST /v1/providers/azure/oauth/{attempt}/finish` with the subscription:
   the control plane creates the `flyco`
   application and service principal through Graph, assigns it
   Contributor on the subscription (retrying while the new principal is
   not yet visible to ARM), and links the resulting credential through
   the same path `POST /v1/providers` uses. The user's own tokens are
   discarded with the attempt: nothing runs as the user afterwards.

Cloud Shell, behind the quiet link, is the older two-field path: one
command with a copy button —
   ```
   az ad sp create-for-rbac --name flyco --role Contributor \
     --scopes /subscriptions/$(az account show --query id -o tsv) --json-auth
   ```
— with the link that opens Azure Cloud Shell in the browser, then one
textarea for the JSON block it prints (`clientId`, `clientSecret`,
`tenantId`, `subscriptionId` read out as confirmation rows), then `Link
Azure`. No terminal or CLI is ever assumed.

The service principal is scoped to the subscription, so flyco creates and
owns the resource group itself (`flyco`, in the account's default region)
at link time. The user never names a resource group.

The machine login key is **flyco's**. Azure refuses a Linux machine with
neither a password nor a key, and flyco sets no passwords; the user has a
browser and signs in from anywhere, so there is nobody to hand a private key
to and nowhere to keep it. The control plane mints an Ed25519 pair per
linked account (`ssh-key`, never hand-rolled), seals the private half beside
the account's credentials, installs the public half on every machine it
builds, and shows the browser nothing.

### 7.3 AWS

1. Show the minimal IAM policy JSON flyco needs (rendered from a template
   in the repo, with a copy button) and a link to the IAM console page
   that creates an access key.
2. Fields: Access key ID, Secret access key. Session token is under
   `Advanced`.
3. `Link AWS`.

The break-glass key pair is optional on AWS and stays under `Advanced`.

### 7.4 Google Cloud

The road is Google's consent screen; Cloud Shell is the quiet link.

1. `POST /v1/providers/gcp/oauth/start` mints an attempt and the Google
   authorize URL (scopes `cloud-platform` and `userinfo.email`). The page
   opens it in a new tab and polls `GET /v1/providers/gcp/oauth/{attempt}`.
2. `GET /v1/providers/gcp/oauth/callback` (public) exchanges the code,
   reads the email from the id token, lists the active projects, stores
   them on the attempt, and sends that tab to
   `/connect/return?provider=gcp`.
3. The poll answers `authorized`; the page asks which project.
4. `POST /v1/providers/gcp/oauth/{attempt}/finish` with the project: the
   control plane creates the `flyco` service account, binds it to
   `roles/compute.admin` on the project, mints a key, and links the key
   file through the same path `POST /v1/providers` uses. The user's own
   token is discarded with the attempt.

Cloud Shell, behind the quiet link: the `gcloud` commands that create the
service account, bind the role, mint the key and `cloudshell download` it,
then a drop zone for the file (`project_id`, `client_email` as
confirmation), then `Link Google Cloud`.

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

`Resize` is the tiered slider of §7.7, on the machine the session is
already on: the same detents, prices and license-bound badge, opened on the
current type, with no `Auto` — flyco choosing again is not one of the
outcomes — and without the account, region and capacity filters, because
the resize carries a machine type and nothing else. The commit reads
`Resize to <type>` over the line `Restarts the machine; the disk is kept.`
`/resize` in the composer and `Resize` in the header's `⋯` menu both open
that control, not merely the tab it lives on.

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
