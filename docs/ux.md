# Flyco UX specification

This document is the source of truth for how the flyco web app is laid out
and how it behaves. `docs/proposal.md` says what the product is;
`docs/ARCHITECTURE.md` says how the backend is built; this says what the
user sees and does. Frontend work is measured against it.

## 1. The one fact the UI must teach

A session cannot exist until three things are true:

1. the user is signed in with GitHub (identity and repositories — the
   grant also carries the Codespaces scopes, dormant until linked),
2. a **harness account** is linked (Claude Code or Codex),
3. **compute** is linked (GitHub Codespaces, Azure, AWS, GCP, or a machine
   the user owns).

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

`/login` is the wordmark, one line — `Claude Code and Codex, on a machine
you own.` — and the sign-in button. Nothing else: a paragraph on what
flyco holds and does not hold, and a footnote on why it reads
repositories, were read by nobody who had not already decided to sign in,
and the sign-in button is where those questions get answered anyway
(GitHub's own consent page lists the scopes).

The shell is a left rail and nothing else. There is no top bar: a bar
above the work is a second place to look, and the work is the page.

The rail, top to bottom: the wordmark, `New session`, a search field, the
session list, one quiet `Archived · 3` line at its foot, and the account
row — avatar and login, opening a menu upward with `Settings`, appearance,
and `Sign out`. The rail is the only region that scrolls independently of
the page.

The list is grouped **by repository**, the way the official apps group by
project, because that is how a person remembers a session: "the helios
one", never "the idle one". Repositories are ordered by their most recent
session and sessions within one newest first. A row is a dot and a title.
The dot is coloured only while it says something — green while the agent
is working, red for a failure, neutral while a machine is being built —
and a session at rest has no dot at all, only the slot, so titles line up.
Neither does a session whose agent has answered and is waiting for a
reply: after a day's work that is every session there is, and a column of
amber dots is the `Needs input` badge forty times over. Nothing in the rail moves: a row of
breathing dots in the corner of the eye competes with the work in the
middle of the screen. There are no `NEEDS INPUT / WORKING / IDLE` headings
and no `SESSIONS / ARCHIVED` tabs: a heading that names a status is chrome
repeating what the dot already says, and archived sessions are the same
list read for a different reason, which one line at the foot is enough
for. That line toggles the list to its archived half and back.

In settings the rail becomes the settings nav: `Sessions` in the slot
`New session` occupies everywhere else, then the five sections of §10. There
is never a second column of links beside the first — a person in settings
came to change one thing and leave, and the list they left is one click away
at the top of the same column.

Below 900px the rail is off-canvas: a floating button opens it over the
page behind a scrim, and choosing anything closes it.

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
2′′. *Link Devin.* Devin's OAuth client admits only localhost redirect
   addresses, so flyco runs the CLI's port-free flow (`devin auth login
   --force-manual-token-flow`): after sign-in Devin's page shows the
   code. `Sign in with Devin` opens the authorize page and advances to
   the paste page, *Paste the code Devin shows you*, one field for the
   code, the same shape as Claude's. A refused code is an inline
   `ProblemNotice` under the field; the quiet link *Start the sign-in
   again* mints a fresh attempt in place. The *Use an API key instead*
   link leads to page 4, where Devin's field takes a token from its
   settings.
4. *Paste your API key* (whichever agent the link came from, reached only
   by the link). One field that accepts an API key or a `claude
   setup-token`, told apart by prefix. Primary `Link Claude Code` / `Link
   Codex` / `Link Devin`; success returns to the list as the sign-in path
   does.

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

The stepper across the top of the card has one bar per stage. It is drawn
only when there is more than one: the same pages reached from settings are a
single stage, and one bar is a dark rule that reads as a divider — or as a
finished flow — rather than as progress.

## 5. Home

Layout, top to bottom, centered at 720px:

1. Greeting line: "What should we build?" (or "Welcome back, {login}").
2. **Composer** (the only primary control on the page):
   - a multi-line textarea, placeholder "Describe a task",
   - a chip row beneath it: `Harness`, `Compute`, `Repository`, `Budget`,
   - a send button (ink circle with an up arrow) at the right of the row.
3. Readiness cards, only while readiness is incomplete: one row per
   missing prerequisite, with its action.
4. The session cap appears as one muted line only when active sessions are
   at or above cap minus one.

The page is centered vertically as well as horizontally, because those four
things are the whole page. The session list is not among them: it lives in
the rail (§3), where it is reachable from every page instead of only this
one, and where it does not compete with the composer for the attention of
somebody about to describe a task.

### Chips

Every chip is both a status readout and the entry point to change it.

| Chip | Ready | Not ready |
|---|---|---|
| Harness | logomark + `Claude Code` (or `Codex`); popover lists the linked agents with the chosen one marked, ending in `Connect another agent` → `/connect/harness` | `+ Connect an agent` → `/connect/harness` |
| Model | `Fable 5.1`, and `Fable 5.1 · High` once an effort is chosen. The name is the head of the harness's description where it has one (`Fable 5.1 · Most capable…` → `Fable 5.1`; Claude's rows are menu labels like `Default (recommended)`) and the row's label otherwise (`GPT-5.5`). The chip sits at the right of the row beside send, where both official apps keep theirs. The popover asks the question in two views the way Codex's picker nests them: the effort slider first — the detented rail the machine picker already teaches (§7.8), the level the thumb sits on as the way into the model list, the model's name under it — and the agent's own models behind that link (`GET /v1/harness-accounts` carries each account's list, as its last session's agent reported it, or flyco's built-in one until then), each with the harness's one-line description. `Default` is the leftmost stop — the model keeps the choice, the same position `Auto` holds on the machine picker's form row, since not every harness says which level it starts on — a reset sits beside the level, and the harness's `default_effort` is named in the subline where it says one (`GPT-5.6-Terra · Medium`). Picking a different model lands on its rail rather than closing, the effort reset to the model's own default; a model that names no effort levels still closes on the pick, and the popover opens on the list when the chosen model has none — there is nothing to slide. Switching agents drops the choice, because a Claude id means nothing to Codex | absent until an agent is linked, since there is no list to show |
| Compute | provider logomark + `B2s · $0.04/hr`; the logomark names the provider, and region, spot, the account and whether flyco or the user chose the type live in the popover — the chip does not say `Auto` or `Chosen`, which is a word about how the choice was made on a row that is for what was chosen. A **managed container** reads `Container · 4 vCPU · 8 GiB · $0.21/hr` instead: `aca-4x8` is flyco's key for a size billed by the second, not a name anybody picked, so the size is what identifies the row — and where the provider covers it out of a monthly allowance the chip ends ` · Free this month`. A machine the user enrolled is a container too and keeps its own hostname, which is the name they gave it. While an account is still being read the chip says `Reading Azure…` and the popover carries the whole sentence | `+ Add compute` → `/connect/compute` |
| Repositories | `owner/name`, or `owner/name +N` past the first; popover with a search box, recent repositories first, and the picked set pinned on top — first picked is the session's primary repository, each picked row carries its own branch picker (defaulting to the repository's default branch) and `↑`/× controls to promote or drop it. A session can work across up to sixteen repositories | `Select repositories` opens the same popover |
| Budget | `$10`; popover with a slider (1–200) and the sentence "Covers the machine and its disk. Model tokens are billed by your Claude or Codex plan." | always shown, default `$10` |

The compute chip shows the machine flyco will actually choose
(`GET /v1/machines/default?spot=`), not a dropdown of the catalog. A
popover lets the user switch account, region, spot, or pick another
curated type (§7.7).

The chips sit on one line at desktop widths, and one line is a hard rule:
a pill never takes a second line inside the card. The compute chip is the
one that gives way first (its label shrinks to an ellipsis), so choosing
a machine — which rewrites that label on every slider detent — never
reflows the row under the open popover. When even that is not enough —
a viewport under ~900px, a composer under ~600px (the drawer's width at
any window size), or simply more chips than the row has room for at any
width — the chips leave the box entirely and float above it as an island,
wrapping layer on layer there instead of squeezing the send row into a
second line inside the card; the compute chip takes a whole line of the
island for the same reason. A popover never hangs below the viewport: it
takes the room between its top edge and the bottom of the window and
scrolls inside it.

GitHub refusing the token flyco holds (`424 github-token-revoked`) is not a
sign-out. The repository and branch chips show the refusal as a notice with
`Reconnect GitHub`, which runs the GitHub authorization again and returns to
the page; the flyco session is untouched. A chip never leaves the page: a linked
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
| Paused · budget exhausted | `paused` and `paused_reason` is `budget` |
| Waiting on the plan | `paused` and `paused_reason` is `usage_limit`: a harness usage limit is spent and flyco is waiting the window out (§9.8) |
| Interrupted · suspended | `interrupted`; the clause is `interrupted_reason`: `suspended`, `machine lost` or `spot reclaimed` |
| Migrating · 40s | `provisioning` **and** an `interrupted_reason`: flyco is putting the session back. Elapsed since the machine went |
| Failed | `failed`; the row shows the failure reason |
| Archived | `archived` |

Amber for `Needs input`, green breathing for `Working`, neutral breathing
for `Provisioning` and `Migrating`, amber unbreathing for `Waiting on the
plan`, gray for the rest, red for `Failed`. `Waiting on the plan` is the one
resting state with a colour, because it is the one that ends by itself: a
session that will be working again this evening without anyone touching it is
worth seeing in the corner of the eye.

**Where a status is shown.** The rail's dot (§3) is the only place a
status is a colour beside a name. The session page has no status pill: a
pill reading `Needs input` or `Working` above a conversation is chrome
telling the reader what the conversation already shows, and next to the
official apps it reads as generated. Each status is said where it
happens, and only there:

| Status | On the session page |
|---|---|
| Provisioning, Migrating | the timeline in the transcript (§9.2), which folds to one line once the agent is ready |
| Working | a breathing dot and `Working…` at the foot of the transcript, where the next line will land, and the composer's `Stop` button |
| Needs input | the approval card and the amber banner when a decision is pending; otherwise nothing — the agent's last message is the page, and the composer has focus |
| Idle | nothing |
| Disconnected · machine not reachable | a notice above the composer: the machine dropped off the network, it reconnects on its own, anything sent waits for it |
| Paused, Failed, Archived | the state notice in the composer's place (§9.1) |
| Interrupted | the state notice **above** a composer that still works, because a message sent to it is what starts the machine again (§9.9) |
| Waiting on the plan | the state notice **above** a composer that still works, because a message typed now is worth sending (§9.8) |
| Reconnecting…, Connecting… | one amber word at the right of the header, only while the browser's own socket is not carrying events |

`interrupted_reason` is cleared when the session's daemon reaches the
control plane again, which is the moment a migration is genuinely over and
the session goes back to whatever status it had before.

A refusal from a provider is copy, not a log line. Every number a message
carries names what it counts — `allows 10 vCPUs`, never `allows 10` — and
the provider's own quota identifier stays verbatim, because the way out runs
through that provider's console and a name flyco prettified would not be
findable there.

## 7. Connect compute

`/connect/compute` is a chooser of five cards (GitHub Codespaces first —
the free-hours answer — then Azure, AWS, Google Cloud, Your own machine),
each with a logomark and one line. Choosing one opens a
wizard in place. Every wizard ends by linking the account — the credential
validates live on the way in (`POST /v1/providers`, or the provider's own
link route where the credential is a grant flyco already holds) — then shows the
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

### 7.5 GitHub Codespaces

The road is one click, not another sign-in: sign-in already asks GitHub
for `repo codespace read:packages` in a single grant, so the account the
user signed in with usually *is* the credential the link stores.

1. `POST /v1/providers/codespaces/link` proves the stored grant carries
   the scopes (`GET /user` reports them), creates the private
   `flyco-sessions` repository with its devcontainer — the image a
   codespace boots into — and links the account. No tab opens, nothing is
   chosen; there is one GitHub account and one environment.
2. A grant written before flyco asked for `codespace` answers
   `403 github-scope-missing` — a routing answer, not a failure: the page
   says the sign-in predates the grant and falls back to the OAuth road
   (`…/codespaces/oauth/start`, the same poll-and-finish shape as the
   other clouds), which widens it. The finish runs the same link the
   direct route does. That road shares the sign-in's callback —
   `/v1/auth/github/callback` — because the GitHub OAuth app registers one
   URI per hostname; the `state`, not the path, decides which flow is
   coming back.

Sessions then run inside the user's own codespaces — GitHub bills the
account's monthly free hours first, and a codespace's own idle clock stops
it (§9.5's suspension rule). A user who never picks the card is never
asked for anything: the extra scopes sit dormant on the sign-in grant.

### 7.6 Your own machine

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

### 7.7 Catalog curation

Cloud catalogs are large and mostly redundant. Flyco shows the user and
the agent a **curated** catalog, computed in `flyco-core` from the raw
provider catalog:

- keep only the newest CPU generation of each family the provider
  offers (newest is also usually cheapest),
- never compare a **container** against a virtual machine: a container
  whose filesystem ends with its execution is not a cheap VM, and a cheap
  container dominating a whole line-up would leave a user who needs a disk
  with nothing to pick,
- within one architecture (x86-64, arm64) and one region, drop any type
  that costs the same or more than another type with at least the same
  vCPUs and memory (strict Pareto frontier on price vs. capacity),
- order by price.

The default machine is the cheapest Linux entry of the curated catalog,
with two preferences ahead of price. A **container the provider gives away
this month** wins over everything, hardware the user owns included: that
allowance expires unspent at the end of the month and the machine at home
does not. And within a billing class the **nearest** entry wins: a session
machine is an interactive box, and `Auto` follows the caller's geography —
read off `request.cf` at the edge — rather than the alphabet, which once
sorted `EuropeWest` ahead of `UsEast` for a user in Virginia. An entry
whose region the provider cannot place is not demoted for our ignorance.
The floor for `Auto` is 4 vCPU and 16 GiB, and a granted container clears
it at 4 vCPU alone: a container's memory is sized with its cores, and a
rule that let a 4×16 VM through while turning down the free 4×8 container
spent money to avoid the one machine the user is not paying for. The
agent's `machine_resize` tool sees the same curated list.

### 7.8 Choosing a machine by hand

The compute chip's popover is two choices, in order: the **product form** as a row of segments — `Container`, `VM`, `Codespace`, whichever the linked accounts offer — and then a **tiered slider** of that form's machines, not a table. The three are different products rather than different prices of one: a container's filesystem ends when the run does, a VM's disk survives a stop, and a codespace is bought from GitHub in core-hours — so the choice between them is never a detent among machine types. `Auto` leads the segment row: the curated Linux type with at least 4 vCPU and 16 GiB nearest the caller, unless the account's catalog offers a container covered by a monthly grant, in which case that is what it picks (§7.7). Under `Auto` there is no track — flyco is keeping the size as well as the form — only the machine it would pick and the rule it picked by. A `Codespace` segment exists only once a Codespaces account is linked, so where a session is being chosen and none is, a quiet line under the slider says "GitHub Codespaces give free hours every month" and links to `/connect/compute` — a suggestion where the catalog would otherwise be silent, never a segment that opens an empty track.

Choosing a segment lands the thumb on the cheapest entry of that form the current scope reaches, moving the scope onto it when the account and region on screen hold none — a segment that opened an empty track would be a control that lies. The slider's detents are the chosen account, region and form, ordered by price; the thumb snaps to a detent and the label above it reads `Standard_D4s_v6 · 4 vCPU / 16 GiB · $0.19/hr`, or for a managed container `Container · 4 vCPU · 8 GiB · $0.21/hr · Free this month`. Its left end reads `Cheapest`. An `Advanced ›` disclosure above the slider reveals account, region, architecture (x86-64 / arm64), OS family, and spot — each scoped to what the chosen form offers, and spot only where some detent can quote a spot price. Choosing any segment other than `Auto` sets `machine_origin: user`; the popover's label then reads `Chosen by you`.

On a phone the popover is a **bottom sheet** rather than a panel hanging off the chip: fixed to the bottom of the viewport, edge to edge, at most 70% of its height. A panel anchored to a chip near the bottom of a phone screen had nowhere to open into.

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
`repo · branch` (`+N` past the first, a popover that lists every checkout
and offers `Add a repository` — slug plus its own branch picker, answered
by `POST /v1/sessions/{id}/repos` and cloned live by the daemon), the
drawer's toggle (§9.4), and one `⋯` menu: rename,
archive, then start machine, stop machine, resize, edit `.env`, copy
session id, with the machine's provider, region and state as the menu's
footer. Nothing else. The header
is one quiet row, the way the official apps' is; the status is read off
the transcript (§6), and the machine, the budget and the context window
are the composer's row (§9.3), where they are acted on.

The budget and the context ring are both controls: one of the budget's two
numbers is something the user set, and the ring opens the usage panel the
readings are explained in. The composer's budget chip
(`$1.20 / $10`) opens the same slider the home composer's does, floored at
the first whole dollar above what the session has already spent, and an
explicit `Set budget to $25` commits it. That is the only way out of
`Paused · budget exhausted`: a limit above the spend puts the session back
to `active` and tells its daemon to carry on from where the pause
interrupted it. A paused session therefore also carries a notice — "The
$10.00 budget is spent. Raise it to continue." — with the same control as
its action, because the composer is gone while the session is paused.

That notice takes the composer's place rather than sitting above the
transcript, and the composer is not rendered at all while it shows. A
session that is `failed`, `paused` or `archived` will not take a message,
so a field to type one in is an offer the page cannot keep; what belongs
in that space is the one action that will change the state. The same rule
covers every refusing state, which is why the composer no longer carries a
`refusal` of its own. An `interrupted` session is the one exception: a
message sent to it is what starts its machine again (§9.9), so its composer
stays open and reads `Sent when the machine is back`.

### 9.2 Transcript

- User messages are right-aligned bubbles.
- Assistant text renders as Markdown (a library, sanitized) with code
  blocks and copy buttons.
- Tool calls are one-line rows: icon, a human summary, duration, and a
  status glyph. The summary is the harness's own `description` when the
  call carries one ("Show top-level directory name"), because that sentence
  was written for a person to read; only when there is none does flyco
  describe the call itself (`Read src/main.rs`, `Ran cargo test`,
  `Edited 3 files`). A failed call keeps the same sentence and says it did
  not happen: `Could not show top-level directory name`. The row is set in
  the prose face, not the monospace one.
- A turn renders in the order the agent worked it: prose and tool rows
  interleaved as they happened, not all the prose and then all the calls. A
  turn that ran a command and then explained what it found must not show the
  explanation above the command it came from.
- Expanding a row shows the call, never its wire format. A call whose input
  is source — a shell `command`, an `apply_patch` patch — opens into that
  source, highlighted as the language it is. Everything else opens into
  named values (`File · src/main.rs`), flattened two levels deep and
  counted below that. `JSON.stringify` appears nowhere in the interface:
  braces, quoted keys and `\"` around the shell quotes the command actually
  contained are a transport between two programs, and the reader is not
  one of them.
- A completed turn ends with `Worked for 8m 23s`.
- Provisioning renders **inside** the transcript as a timeline: `Reserving
  a machine on Azure` → `Booting and installing flycod` → `Cloning
  owner/repo` (`owner/repo +N` when the session spans several) → `Agent
  ready`, each with elapsed time, driven by
  `session_state_changed` and daemon events. Every row on it has a real
  interval behind it, which is why boot and install are one: nothing can see
  where one ends and the other begins — the control plane hears nothing
  between handing the request to the provider and the daemon calling in, and
  the daemon exists only once the install has finished. Announced as two,
  the boot timed at `0s` on every session, which is a row that teaches
  nothing and reads as broken. "No turns yet" never
  appears while a machine is being built, and neither does a transcript
  holding only the prompt: until the queue announces its first stage the
  page holds the timeline's place from when the session was opened. A session reclaimed from spot
  gets **another timeline in the place it happened**, headed `Migrating`
  and holding only the stages a restart goes through — its second row reads
  `Starting the machine`, because nothing is installed or cloned there: the
  disk already has both.
- A stage is identified by the instant it happened. A reconnect replays
  frames the page has already shown, and a replayed `reserving` must never
  open a second timeline headed `Migrating`: only a `reserving` at a new
  instant is a new machine.
- A machine that stops reporting stages for fifteen minutes is **failed**
  by the control plane, in the daemon's own words when it managed to send
  any (`its agent never started: …`) and otherwise with `the machine was
  built but never reported its agent ready`, and the machine is released
  with it: a machine that never
  came up is still a machine running up a bill. Nothing else would notice:
  the queue's job finished, and the daemon that would report the failure
  is the thing that is failing.
- A build that **stops** — failed, or archived before it finished — stops
  the timeline where it was: the stage in progress turns red with an ✕ and
  the time it had run, its clock frozen at the moment the session stopped,
  and nothing on the page still spins or reads `starting`. What stopped it
  is the notice's sentence, not the timeline's. The failed notice (§6) says why, as its own sentence, with
  `Resume` beside it; the control plane releases the machine reservation
  and tells the room, so an open page learns of the failure without a
  reload.
- Approvals appear inline as an **action card** (title, the exact
  operation, `Approve` / `Deny`) and, while any is pending, as a sticky
  amber banner at the top of the transcript.
- Budget signals (50 / 80 / 90 / paused), spot notices with a countdown,
  usage-limit pauses with the reset time, and compaction all appear as
  inline notices with an icon.

### 9.3 Composer

Same component as the home composer. Its row carries what a running
session still has a say over, as the official composers do, left to right:

- the **machine**, `D4ps_v6 · $0.17/hr · spot` (or `D4ps_v6 · Stopped`),
  which opens the machine's own popover — the one card for the one
  machine a session has, §9.4;
- the **budget**, `$1.20 / $10`, which opens the budget slider of §9.1;
- the **goal** chip, only where the running harness said it takes one —
  `Target` opens a small panel that states the condition and sets it,
  because a goal is a setting of the session rather than a line in it and
  so gets a control instead of a `/goal` command row;
- then, at the right beside send, the **mode chip** — `ShieldCheck` and
  the mode's name (`Auto`, `Plan`, `Accept edits`, `Yolo`) — whose popover
  lists the modes the session's harness offers, each with a line about
  what it lets the agent do. One union across both harnesses: Claude takes
  the six natively (`auto`, `default`, `plan`, `acceptEdits`,
  `bypassPermissions`, `dontAsk`), Codex takes the same modes as the
  approval/sandbox pair each maps to, and its list stops at five because
  `dontAsk` maps to the pair `plan` already does — two rows for one
  behavior is a lie about a choice. A choice is sent as
  `PATCH /v1/sessions/{id}` with `permission_mode`, persisted on the
  session like the model is, and reaches the running agent through its
  room (held for an absent daemon, replayed on reconnect, reapplied at
  boot from the control plane's record rather than the machine's stale
  config). The chip dims until the answer lands, and the transcript
  records the change as one line, `Switched to Plan mode`;
- the **model chip** of §5, `Fable 5.1 · High`, where the official apps
  keep it beside send: the panel is the chosen model's detented slider
  first, its list behind the level link. Either choice is sent as
  `PATCH /v1/sessions/{id}` with `model` — the choice carries the
  effort — and reaches the running agent through its room; the chip dims
  until the answer lands, and the transcript records the change as one
  line, `Switched to Sonnet 5 · High`;
- the **context ring**, `41k / 200k`, once the harness has reported a
  turn — before that there is nothing to draw. The ring is a control, not
  an ornament: opening it shows the usage panel — the context window's
  fill, the fill at which the harness compacts on its own
  (`Compacts automatically at 80%`), and the plan's rolling windows beside
  it, shortest first, each with its turnover (`5-hour · Resets in 2h 10m ·
  26%`). The plan is read from the harness itself (the Claude Agent SDK's
  usage call, Codex's `account/rateLimits/read` and the `updated`
  notification it pushes) at session start and again after every turn, so
  "how much of my plan is left" has an answer before the limit is hit
  rather than a `Usage limit` notice after it. The panel ends with `See
  the detailed breakdown`, which is what a `/context` command would have
  been: a control request the daemon answers out of band — and the answer
  unfolds in the panel itself, one category per row beneath the segmented
  bar, then the MCP tools, memory files, agents and skills behind
  disclosures, each with its deferred weight, and the model that reported
  it. Asked once, the breakdown stays rendered and the action becomes
  `Refresh the breakdown`. Nothing is added to the transcript: the answer
  is a reading, not something that happened. The ask is delivered or it
  is nothing, so the control says what it is doing rather than greying in
  silence: `Asking the machine…` while the answer is in flight, `No
  answer — try again` when the machine stays silent past the wait, and
  where no machine is connected the click either becomes `Wake the
  machine for the breakdown` on a session that can be resumed — the
  question goes the moment the daemon is back — or is refused with the
  reason written in the panel, `The machine is not connected — it
  answers the breakdown`. The plan section is always named, `Plan
  usage`: where the harness or the account has reported nothing it says
  `No plan limits reported`, which is the truth for a key-backed account
  too — there is no plan to spend down.
  The session's own accounting lives here too, as `This session` between
  the window and the plan — tokens in and out, the harness's reported
  cost, and how long its turns have run: what a `/usage` command used to
  spell out as text, stated as figures because that is what they are.
  Nothing is drawn until a harness has answered: a ring at zero over a
  plan flyco has never asked about is an invention. Until a context
  window has been reported at all, the fullest plan window stands in for
  the ring's reading, labelled for what it is. On a row too narrow for
  every control the readout is what gives way first — the arc still says
  how full, and the number lives one tap inside the panel;
- then send, which becomes `Stop` while a turn is in flight.

The island of §5 applies here under the same rule, split along what each
control is for: when the row cannot hold it, the chips that say where the
session runs — the machine, the budget, the goal — float above the box
and stack, while the ones that say what the next turn runs under — the
mode, the model, the ring — stay beside send inside it. The move is a
move: the chips' live subtree changes parents rather than re-rendering,
so a popover open mid-crossing rides along and re-anchors to where its
chip landed.

The composer sits at the foot of the window even when the transcript is
three lines long: the page is at least a window tall, and the composer is
pushed to its bottom, where the hands already expect it.

`/` opens a command palette listing what the session can actually be told
to do. Flyco's own three come first, marked `flyco` — `/compact`, which is
the control plane's compaction request rather than a message, so every
browser watching sees the same one; `/archive`; `/resize` — and after them
everything the running harness reported: `/advisor`, and every skill of
the checkout, ninety of them on a well-equipped machine. A command a
control already answers is not listed a second time — the ring's panel is
the door to `/context` and `/usage`, the model chip's to
`/model` and `/effort`, the goal chip's to `/goal` — so the harness's
copies of all five are dropped
rather than shown, as is the harness's copy of any name flyco owns. The
list is the harness's own, sent over the relay
when the agent starts and again whenever it discovers more, so a
repository that adds a skill has it in the palette without a reload. Until
the machine has reported one, the three are all there is.

Typing after the slash filters by prefix on the name; each row shows the
name, the argument it expects, and the harness's one-line description,
clipped to the row. Arrow keys move, Enter chooses, Escape closes the list
and leaves what was typed. Choosing a command that takes no argument sends
it there and then; one that expects an argument is written into the field —
a skill like `/review ` — for the user to finish. A chosen command reaches the agent as the
ordinary message `/name args`, because that is how both harnesses take a
slash command; flyco adds no plumbing per command and none of them is a
feature flyco has to know about.

A message beginning with `!` runs in the machine's bash, and the composer
says so under the field while typing one. The placeholder is one line:
`Reply, / for a command, ! for the shell`.

Everything a message carries decides whether it can be held for a machine
that is not there. A prompt waits in the room's mailbox, so the composer
takes it in every connected state. A `!` run, a `/compact`, a context
breakdown and a terminal keystroke are delivered to the daemon or they are
nothing, so while no machine is connected the composer refuses them rather
than sending them to die: the `!` line's hint becomes the refusal and the
palette keeps `/compact` listed but greyed, next to `/archive` and
`/resize`, which the control plane runs itself and which still work.

### 9.4 Drawer

A right-side drawer, closed by default, with tabs `Terminal`, `Files`,
`Diff`, `Env`. `Terminal` is the xterm panel, **connected the
moment the tab shows**: a user who opened a terminal asked for a terminal,
not for a button that opens one. It fills the drawer, and the machine's
PTY is kept at the size the pane shows (`terminal_resize`, on open and
on every resize), so a line wraps where the pane wraps it; the shell
runs under `TERM=xterm-256color`. `Env` is the existing editor. Its
toggle is a panel icon in the header beside `⋯`, where the official apps
keep theirs; a handle at the top of the transcript column sat exactly
where the first user message lands and read as an avatar on it. Keyboard:
`⌘.` toggles the drawer. Closed, it takes no room at all.

`Machine` is not a tab at all: the machine is **one card for the one
machine** a session has, and a card that is a control belongs on the
readout that names it — the composer's machine chip opens it as a
popover. Its name and state on the first line (`Standard_D4s_v6 ·
Running`, or `Container · 4 vCPU · 8 GiB · Stopped`), where and at what
price on the second (`Azure · westeurope · Spot · $0.19/hr`), then Stop
or Start and Resize. A session never has more than one machine, so a
table of provider / type / region / capacity rows was a form for a fleet
that does not exist.

On a phone (under 900px) the drawer **covers the transcript** at the full
height of the screen and carries its own close button at the end of the
tab row: stacked under the transcript it was a block the user had to
scroll to, with a terminal too short to type in.

`Resize` is the tiered slider of §7.8, on the machine the session is
already on: the same detents, prices and license-bound badge, opened on the
current type, with no `Auto` — flyco choosing again is not one of the
outcomes — and the form row naming the one form the machine can move
within, because a resize keeps the provider, account, region and runtime:
a move between forms is a different machine, not a different size. The
account, region and capacity filters are absent for the same reason — the
resize carries a machine type and nothing else. The commit reads
`Resize to <type>` over the line `Restarts the machine; the disk is kept.`
`/resize` in the composer and `Resize` in the header's `⋯` menu both open
the chip's panel on that control, not merely beside it.

`Files` is the session's checkout, read-only: a lazy tree that fetches one
directory at a time, files git ignores shown and marked rather than hidden,
and a syntax-highlighted view of any text file up to 128 KiB. A file past
that, or one that is not text, says so and points at the terminal.

`Diff` is what this session changed, against the branch it started from:
the working tree — committed, uncommitted and untracked — diffed against
`origin/<branch>`, one collapsible section per file, each headed by the
path, what happened to it, and `+n −m`. A file whose diff runs past ~900
lines opens as `Large diff · N lines · Load diff` instead of rendering.
A session across several repositories diffs one checkout at a time — the
workspace root is not a repository — so a row of checkout tabs heads the
panel, primary first, and the status list above it is one line per
checkout rather than one per session.

Both are answered live by the session's machine, over the relay: there is
no copy of a working tree in the control plane, so a session with no daemon
connected says exactly that rather than showing an empty tree.

### 9.5 Machine changes during a session

When the agent resizes, the transcript shows a notice: `Switched to Standard_D8s_v6 · restarted the machine · disk kept`. When the agent asks to move to a license-bound type, the request is an approval card, not a silent resize, and the card quotes the minimum charge. The agent is told whether the machine was chosen by the user and to be conservative about switching it.

What the agent sees of all this is `flycod`'s local MCP server (§11): `machine_status` reports the machine and who chose it, `budget_status` reports what is left, and `machine_resize` moves the session. The resize tool's description states that resizing restarts the machine, lists only the curated catalog of §7.7 with each entry's hourly price and — for a license-bound type — the minimum charge in dollars, and refuses while the working tree is dirty unless it is called again with `force` and a reason. A resize to a license-bound type is never performed on the agent's own authority: the daemon raises the approval, the tool answers that the request is pending the user's decision, and the control plane performs the resize if the user approves. When the machine was the user's choice, every one of those places says so in the same words: "The user chose this machine themselves; do not switch it unless the task cannot proceed on it, and say why when you do."

The same machinery covers a repository the agent wants but the user never picked: `repo_add` raises an approval card that names the repository, the branch and the directory it would clone into, with the agent's reason underneath — cloning a repository is fetching code the user did not choose, so it is never performed on the agent's authority. Approving sends the daemon an `AddRepo`, and the clone's arrival is a transcript notice (`Added owner/name to the workspace as dir/`); denying lands as an ordinary message telling the agent so. A repository the *user* adds mid-session — the header's `Add a repository` — skips the card entirely, because the asker is the approver.

### 9.6 When the machine stops answering

The command stream is quiet for as long as the agent is thinking, and a quiet TCP flow is what a cloud NAT reclaims — Azure's outbound idle timeout is four minutes and it drops the flow without a FIN. So the room pings the stream on an interval whether or not it has commands, and a daemon that has heard nothing — no command, no ping — for long enough abandons the flow and attaches again. Neither end may ever wait on a stream it cannot prove is alive.

The user is told. The room is the only party that knows whether a daemon is holding it — a browser sees events arrive and stop, and cannot tell an agent that is thinking from a machine that fell off the network — so it announces the machine attaching and detaching, and the header reads `Disconnected · machine not reachable` instead of a `Working` pill that breathes forever over a turn nobody is running. Attention, not failure: the daemon comes back on its own, and the pill goes back to what the session was doing when it does.

Nothing a browser sends is discarded in silence — and the composer does not let the undeliverable be sent at all. A user message waits in the mailbox and is delivered on the daemon's next attach. What is delivered-or-nothing — a `!` run, `/compact`, a context breakdown, a terminal keystroke — the composer refuses while no machine is connected, saying so where the `!` hint sits. Should one still reach the room — a race against the daemon's departure — the sender is told the machine is off the room rather than left watching a Stop button that did nothing.

The same rule governs the drawer: `Terminal`, `Files`, and `Diff` are answered live by the machine, so with no daemon connected the tabs themselves grey out — a dead end shows as one before it is opened, and one that was open when the machine left yields to the tab that still answers. `Env` stays lit: the `.env` is control-plane data, and starting a stopped machine is the machine chip's popover.

### 9.7 When the agent process dies

The agent runs as a process on the machine, and a process can stop: a refused credential, an out-of-memory kill, a crash. When it does, the session is over — `flycod` has nothing left to drive — and the user is owed the reason rather than a spinner.

So the daemon keeps the last lines the agent process wrote to its stderr and the status it exited with, and says both on its way out. It says them however it stopped: the agent closing its output, a protocol error, or a command that could not be written to a process already gone are three ways of learning the same thing, and all three end with one sentence. The machine's own journal does not count as having said it — `flycod` is stopping too, and a log on a machine nobody can reach is not an explanation.

What the control plane does with that sentence depends on whether the session had gone live, which is a fact only it holds. A session still provisioning may yet be saved, because `flycod` is restarted on failure: the reason is recorded and shown only if the machine never does come up. A session that had gone live will not be, because a daemon that reports this exits cleanly and systemd does not restart a clean exit — so the session fails there and then, releases its machine, and reads `Failed` with the sentence. A session whose agent died must never bill for a machine nobody is using.

### 9.8 When the plan's usage limit is hit

A subscription plan stops the agent at a limit, and the limit resets by itself: Claude's five-hour and weekly windows, Codex's primary and secondary ones. A session that dies at a usage limit and a session that waits it out are the same session an hour later, and the difference is whether the user has to be at the keyboard for the turnover. So flyco waits it out for them.

The signal is the harness's own, and both harnesses give it three ways: an explicit rate-limit event (the Claude SDK's `rate_limit_event` with `status: rejected`, Codex's `rateLimitReachedType`), the refusal of the turn that hit it, and a plan window the vendor reports at 100 %. All three become one fact — which window is spent, and when it turns over — because the user is owed the same page whichever way flyco learned it. A limit that names no reset is not waited out: there is nothing to wait for, and the session says so and stops as it always did.

The transcript records it where it happened, naming the window first because a five-hour limit and a weekly one are the same event with wildly different consequences: `The 5-hour usage limit is spent. Flyco continues the session in 1h 30m (7:35 PM).`

Then the session pauses, and what happens to the machine depends on how long the wait is. **More than thirty minutes** and the machine is released: an idle VM for four hours is money for nothing, and the session's disk is untouched by the stop. **Less than thirty minutes** and it is kept, because a stop and a start cost more in provisioning time than the wait saves in cents.

The session page says all of it, above a composer that still works:

> **Waiting on the plan**
> The 5-hour usage limit on this session's plan is spent. It resets at 7:35 PM, in 4h. The machine is stopped and costs nothing until then; flyco starts it again at 7:25 PM. Flyco then asks the agent to continue on your behalf.

Both the countdown and the clock time, because they answer different questions — the countdown decides whether to wait for it, the clock time decides when to come back — and what the machine is doing, because a user who is not told the machine is off reads the whole wait as money burning.

Ten minutes before the reset flyco starts the machine again, so that the agent is up and ready when the window turns over rather than provisioning through it. At the reset the session is continued: `usage limit reset, please continue`, sent on the user's behalf and marked in the transcript as flyco's, because a reader coming back to a session that carried on overnight has to be able to tell that sentence from one they typed. The session reads `Active` again.

The composer stays open the whole time and says where a message goes: `Sent when the window resets, at 7:35 PM`. What is typed there is held against the pause and sent **instead of** flyco's canned continuation, because a user who has said what to do next has said something better than "please continue". A `!` command is not held — it runs on the machine there and then, whatever the plan's limits are doing — and while the machine is stopped the composer refuses it, as in §9.6.

Both ends of the wait are a web push, because the whole point is that the user does not have to sit there: one when the session pauses, saying which window and when it resets, and one when it starts working again.

### 9.9 When the session sits idle

A session with nothing to do still has a machine running, and a machine running is money or the user's own hardware held open. So thirty minutes after the last thing anybody did — the last message, turn, terminal keystroke or approval — flyco suspends the machine: compute is released, the disk is kept, and the session reads `Interrupted · suspended`. A turn in flight is never suspended — the agent mid-answer is the one thing on the machine worth paying for — and a session that is `paused` already has its machine decided by the mechanism that paused it.

The same rule covers every provider. A codespace gets it from GitHub's own idle clock; everything else — an Azure VM, an AWS spot instance, a container on the user's own machine — is suspended by flyco on the same threshold, because a session should not cost differently for being idle on one cloud than another.

Coming back is one word. A message sent to a suspended session starts the machine on its own disk and is delivered when the daemon attaches — the composer stays open for exactly this — and deciding a pending approval does the same, because the answer has to reach somebody. The timeline gains a `Migrating` row (`Starting the machine`, then `Agent ready`) where the gap happened, and the agent is told the machine was suspended and started again, so a process it left running is not mistaken for one still alive. Thirty seconds of cold start is the whole cost of the thirty minutes that were not billed.

A session nobody ever speaks to again is still archived at a week — suspension changed what it costs to wait, not how long flyco waits.

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

The usage bars under an account are the same windows the composer draws as
rings, in the same order and with the same words — `5-hour`, `26% · Resets
in 2h 10m` — because they are the same reading, filed by whichever session
last asked and kept on the account. They are bars here and rings there for
one reason: a settings card has horizontal room to spend on a track and a
label, and a composer row is a line of chips where a bar would be the only
thing asking for width. Below them sits what flyco itself observed — the
cost the harness reported over the last day, and, once the vendor has
actually refused a call, how much of the wait for the reset has passed.
That half is deliberately not a quota; the plan windows above it are the
quota, and they come from the vendor.

A linked credential with an expiry says how near it is rather than printing
a date the reader has to subtract from today. Within a week the card's pill
reads `Expires soon` (or `Expired`), the meta line reads `Expires tomorrow`
or `Expires in 4 days` in the warning colour, and `Relink` — the only thing
that fixes it — becomes the card's primary rather than one of two equal
pills. A credential with no expiry says nothing at all: an API key does not
run out, and a reassurance that never changes is one more thing to read.

## 11. API changes this specification requires

Pre-1.0, the API changes to fit the product; no compatibility shims.

- `POST /v1/sessions` gains `prompt: string` (required). The prompt is
  queued for the daemon and delivered as the first user message as soon as
  the harness starts.
- `SessionSummary` gains `title: string` (first prompt excerpt, editable
  via `PATCH /v1/sessions/{id}`).
- The model is a session property. `ModelOption` (`id`, `label`,
  `description`, `is_default`, `efforts`, `default_effort`) is what a
  harness lists; `ModelChoice` (`model`, optional `effort`) is what a
  session runs on. `POST /v1/sessions` takes `model?: ModelChoice` (the
  agent's default when omitted), `SessionSummary` carries `model`,
  `PATCH /v1/sessions/{id}` changes it — the room delivers
  `ControlToDaemon::SetModel`, held across a disconnect like a machine
  change, and echoes `ClientEvent::ModelChanged` — and a choice outside
  the account's list is `400 invalid-model`. `HarnessAccountView` carries
  `models`, the list the account's last session reported or flyco's
  built-in one; the daemon reports the list it gets from the harness at
  start over `PUT /v1/sessions/{id}/models`, which records it on the
  account and broadcasts `ClientEvent::Models`. `GET
  /v1/sessions/{id}/harness-session` carries the model, so a machine that
  comes back reads it beside the conversation it continues.
- The permission mode is a session property on the model's terms.
  `PermissionMode` (`default`, `acceptEdits`, `bypassPermissions`,
  `plan`, `dontAsk`, `auto` — spelled the way the Claude Agent SDK spells
  them, because Claude takes the union natively and Codex maps each to an
  approval/sandbox pair) is what a session runs under.
  `SessionSummary` carries `permission_mode`, a `NULL` row resolving to
  `PermissionMode::PRODUCT_DEFAULT` (`auto`) the way a `NULL` model does;
  `PATCH /v1/sessions/{id}` changes it — the room delivers
  `ControlToDaemon::SetPermissionMode`, held across a disconnect like a
  model change, and echoes `ClientEvent::PermissionModeChanged` — and
  `GET /v1/sessions/{id}/harness-session` carries it, so a machine that
  comes back runs under the mode the control plane recorded rather than
  the one its disk was provisioned with.
- `GET /v1/machines/default?spot=` returns the curated default choice and
  its catalog entry. `GET /v1/machines/catalog` returns the curated
  catalog (§7.7).
- A machine is a virtual machine or a managed container.
  `MachineCatalogEntry`, `MachineSpec` and `MachineChoice` carry
  `runtime: "vm" | "container"`, defaulting to `vm` so a document written
  before the axis existed reads back as what it described; an entry the
  provider covers out of a monthly allowance also carries
  `free_grant { vcpu_seconds_per_month, gib_seconds_per_month }`.
  `POST /v1/sessions` checks the runtime against the catalog entry the type
  names and refuses a request that contradicts itself with
  `400 machine-runtime-mismatch` — never by provisioning whichever runtime
  is on offer, because a session that asked for a disk would lose its
  working tree every time the platform stopped it.
- `POST /v1/sessions/{id}/stopping` (daemon-scoped, `{ reason: "sigterm" }`,
  answers `202`) is what a container session's `flycod` files on its way
  out, after it has flushed the transcript and stored the working tree as
  the workdir patch. A route of its own rather than a flag on
  `POST /v1/sessions/{id}/spot-notice`: a reclaimed virtual machine keeps
  its disk and has a recovery queued against it, and a stopping container
  has nothing left to schedule.
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
  (the curated catalog of §7.7, narrowed to the account and region the
  session's disk lives in — the two a resize cannot cross),
  `POST /v1/sessions/{id}/agent/machine/resize`, and
  `GET /v1/sessions/{id}/agent/budget`. The resize route refuses a
  license-bound type with `409 license-bound-resize-needs-approval`: the
  agent has to raise an approval instead, and the rule is enforced by
  the control plane rather than described to the model.
- `ApprovalPayload` gains `machine_resize_license_bound { machine_type,
  minimum, reason }`. Approving it *is* the resize — the control plane
  performs it, because nothing is waiting on the daemon's side.
- A session's repositories are a list, not a field. `POST /v1/sessions`
  takes `repos: [{ repo, branch? }]` — one to sixteen, deduped, first is
  primary — instead of `repo`/`branch`; `SessionSummary` carries
  `repos: SessionRepo[]` (`slug`, resolved `branch`, checkout `dir`,
  `added_by: user | agent`), and `session_repos` is the table. The user's
  mid-session add is `POST /v1/sessions/{id}/repos` (`{ repo, branch? }`,
  answered with the updated detail); the agent's is
  `ApprovalPayload::RepoAdd { repo, branch?, reason }`, whose approval
  writes the row and sends `ControlToDaemon::AddRepo { slug, branch, dir }`
  — held for a detached daemon like every command — and whose denial is a
  flyco-origin user message. The daemon's `DaemonToControl::RepoAdded
  { slug, branch, dir }` becomes `ClientEvent::RepoAdded`, the notice of
  §9.5. `RepoDirty` gains `dir` (absent on a developer machine, whose
  workdir is itself the checkout), `WorkdirRequest::Diff` and
  `GET /v1/sessions/{id}/diff` gain `?repo=<dir>` selecting the checkout,
  `GET /v1/sessions/{id}/repo-status` answers `checkouts[]`, and the
  workdir patch routes are per-checkout — `?repo=` again, absent for the
  developer-machine shape.
- `ClientEvent` gains `machine_changed { machine_type, hourly, spot,
  restarted }`, which the transcript renders as the notice in §9.5.
  `ControlToDaemon` gains the same variant, and it is the one command a
  room holds for a daemon that is not connected: a resize restarts the
  machine, so there is never a daemon listening at the moment it is sent.
- `POST /v1/sessions/{id}/usage-limit` is how the daemon reports a spent
  plan window, daemon-scoped like the four routes above and taking
  `UsageLimitHit { window: UsageWindow }`. It answers `202`, or
  `422 usage-limit-without-reset` for a window the vendor gave no turnover
  for — there is nothing to wait for, and the control plane says so rather
  than inventing a time (§9.8).
- `SessionSummary` gains `paused_reason: budget | usage_limit`, which is
  what tells the two pauses apart in a list, and `SessionDetail` gains
  `usage_limit: UsageLimitPause { window, resets_at_unix, resume_at_unix?,
  queued_message? }`. `resume_at_unix` is present exactly when the machine
  was released, so one field answers both "when does flyco start it again"
  and "is it costing anything".
- `ClientEvent::UserMessage` and `ControlToDaemon::UserMessage` gain
  `origin: user | flyco`, so the continuation flyco sends at a reset is
  marked as flyco's in the transcript. The origin never reaches the
  harness: a model told that its next instruction was written by a program
  would reason about the framing instead of the work.
- `HarnessEvent::UsageLimited` carries the whole `UsageWindow` rather than
  a bare reset time, because which window is spent is the first thing the
  notice says.

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
