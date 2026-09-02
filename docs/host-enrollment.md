# Host enrollment: a machine the user owns

Design for issue #64. This replaces the SSH-from-the-Worker path, which
cannot work: the control plane runs on Cloudflare Workers, skyzen exposes
no TCP sockets, and `ProviderCredentials::ByoSsh` provisioning is
`Unsupported` on every hosted deployment today.

## Shape

A user-owned Linux machine is **enrolled**, not dialled. The host runs
`flycod host` as a systemd unit, keeps one outbound WebSocket to the
control plane, and executes container jobs the control plane sends it. The
control plane never opens a connection to the host. This is the shape of
GitHub self-hosted runners and Tailscale nodes, and it is the only shape a
Worker can drive.

```
browser ── REST ──▶ Worker ──▶ HostRoom (DO) ◀── outbound WS ── flycod host ── podman ── session container ── flycod (session) ── outbound WS ──▶ SessionRoom (DO)
```

Two daemons, one binary: `flycod host` manages containers on the host;
`flycod` inside a container drives the harness exactly as it does on a
cloud VM. Nothing about sessions changes; only where the machine comes
from.

## Enrollment

1. `POST /v1/hosts/enrollment-tokens` → `{ id, token: "fh_…", expires_at_unix, command }`.
   The token is single-use, ten minutes, bound to the user, stored hashed
   in D1 like daemon tokens. `command` is the one line the wizard shows:
   `curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_…`.
2. The installer (the same `flycod.sh` the cloud image uses, with a `host`
   subcommand) installs `flycod`, checks for rootless Podman and installs it
   when absent, creates the `flyco` user, and runs `flycod host enroll`.
3. `flycod host enroll` calls `POST /v1/hosts/enroll` with the token and
   the host's facts — architecture, vCPUs, memory, free disk, Podman
   version, kernel, hostname — and receives `{ host_id, host_token: "fh_…" }`.
   The host token is long-lived, hashed at rest, rotated on demand, and
   stored root-only on the host. The enrollment token is spent.
4. The unit starts `flycod host run`, which opens the outbound WebSocket to
   `/v1/hosts/{id}/relay` (hibernating, tag `host`) and reports
   `HostToControl::Hello { facts }`. The wizard, polling
   `GET /v1/hosts/enrollment-tokens/{id}`, flips from "waiting for the
   machine…" to the compute card.

## Data model

- `hosts` (id, user_id, label, facts JSON, token_hash, state:
  `online | offline | draining | removed`, last_seen_unix, created_at).
- A host is also a **provider account** of kind `host` (replacing
  `byo_ssh`), so the compute chip, the curated catalog, session creation
  and the usage panel need no special case. Its catalog is one entry: the
  host's own capacity, `pricing: user_owned`, architecture from its facts.
- `machines` rows for host sessions carry the container name and the
  Podman volume name instead of a cloud instance id.

## Provisioning

`ProvisioningJob` already carries a typed machine spec. For a host
session the Worker plans a `ContainerJob` — the planner in
`flyco_provider::byo_ssh` moves to `flyco_provider::host` unchanged — and
posts it to the `HostRoom` Durable Object, which forwards it down the host
socket as `ControlToHost::Run(ContainerJob)`. `flycod host` executes it
with Podman (the `SshExecutor` becomes a `LocalExecutor` running the same
rendered script), and answers `HostToControl::JobResult`. The session
container gets the usual `DaemonBootstrap`, so the session daemon inside it
connects to its `SessionRoom` and the rest of flyco does not know it is on
a host.

Machine operations map to container operations: stop = `podman stop`
(volume kept), start = `podman start`, destroy-keeping-disk = remove the
container and keep the volume, destroy-all = remove both. Resize is
`Unsupported`, as byo-ssh already says. Spot does not apply; the host's
eviction watcher is disabled.

## Lifecycle and failure

- A host whose socket drops is `offline`; sessions on it show
  `Interrupted · host offline` and resume when it reconnects (the container
  is still there). After a week offline the sessions auto-archive like any
  other.
- `DELETE /v1/hosts/{id}` drains: refuses while sessions are active unless
  `force`, then stops containers, keeps volumes for the archive snapshot,
  and revokes the host token. The unit on the host notices the revoked
  token and stops.
- Budgets: a host session accrues no compute or storage cost; the budget
  bar shows `your hardware` and the agent's `machine_status` says the
  same.

## UI

The `Your own machine` card in the compute chooser (docs/ux.md §7.5)
becomes linkable: it shows the one-line command with a copy button, how
long that command has left, the live "Waiting for the machine…" state, and
then the compute card (hostname, architecture, vCPUs, memory, free disk,
online dot). It skips the bonus-programme questions every cloud wizard
opens with — there is no free credit for hardware somebody already bought.
Settings › Compute lists hosts beside cloud accounts in the same run of
cards, with `Rename` and `Remove`; a removal refused because sessions are
still running names the count and offers `Remove anyway`.

## Order of work

1. `flyco_core`: host types, `HostToControl`/`ControlToHost` wire enums,
   the `host` provider kind replacing `byo_ssh`.
2. Control plane: hosts table, enrollment routes, `HostRoom` DO, host
   provider account, provisioning dispatch to a host.
3. `flycod host`: enroll, run loop, local Podman executor, facts.
4. Installer `host` subcommand and Podman bootstrap.
5. ~~Frontend wizard and settings card.~~ Done: the chooser's fourth card
   opens `components/link/HostWizard.tsx`, the enrollment poll is the state
   machine in `lib/hostEnrollment.ts`, and `components/HostCard.tsx` fills
   the compute card's shared frame with what the machine reported.
6. ~~Delete the SSH executor and its `ssh` feature.~~ Done with step 2:
   nothing that dials a machine survived the move to `flyco_provider::host`.
