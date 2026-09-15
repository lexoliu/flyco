#!/bin/sh
set -eu

# Two machines, one installer.
#
#   flycod.sh                     the session image cloud-init builds: the
#                                 daemon, its harnesses, and flycod.service.
#   flycod.sh host enroll <token> a machine the user owns: rootless Podman,
#                                 the flyco user, `flycod host enroll` against
#                                 the control plane, and flycod-host.service.
#
# The second is the one line the enrollment wizard hands out:
#   curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_…

if [ "$(id -u)" -ne 0 ]; then
  echo "flycod installer must run as root" >&2
  exit 1
fi

asset_base=${FLYCO_ASSET_BASE:-https://dev.flyco.dev/install}
binary_base=${FLYCO_BINARY_BASE:-$asset_base}
# The control plane a host enrols with is the origin serving this installer:
# a machine is enrolled into the deployment whose wizard printed the command,
# and there is nothing else on the machine that could know which that is.
control_plane=${FLYCO_CONTROL_PLANE:-${asset_base%/install}}
install_root=/usr/local/bin
service_root=/etc/systemd/system
runtime_user=flyco
runtime_group=flyco
runtime_home=/home/flyco
host_config_dir=/etc/flyco
host_config=$host_config_dir/host.toml
# Where rootless Podman keeps this machine's containers and volumes. Its free
# space is what the control plane schedules sessions against.
volume_root=$runtime_home/.local/share/containers
# One subordinate id range per user, 65536 wide, as useradd itself allocates
# them. Rootless Podman maps the container's users into it.
subid_count=65536
subid_base=100000

case "$(uname -m)" in
  x86_64) artifact=flycod-linux-x86_64 ;;
  aarch64|arm64) artifact=flycod-linux-aarch64 ;;
  *)
    echo "flycod does not publish a binary for architecture $(uname -m)" >&2
    exit 1
    ;;
esac

download() {
  curl --fail --silent --show-error --location --retry 3 --output "$2" "$1"
}

usage() {
  echo "usage: flycod.sh                     set this machine up as a flyco session VM" >&2
  echo "       flycod.sh image <flycod>      set up a session container image, flycod from a file" >&2
  echo "       flycod.sh host enroll <token> enrol this machine as a flyco host" >&2
  exit 64
}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
chmod 0755 "$scratch"

# The flyco user, with the login shell the caller's machine actually has.
create_runtime_user() {
  login_shell=$1
  if ! getent group "$runtime_group" >/dev/null; then
    groupadd --system "$runtime_group"
  fi
  if ! id "$runtime_user" >/dev/null 2>&1; then
    useradd --system --gid "$runtime_group" --create-home --home-dir "$runtime_home" --shell "$login_shell" "$runtime_user"
  fi
}

# The daemon binary, checked against the checksum published beside it.
install_flycod() {
  download "$binary_base/$artifact" "$scratch/$artifact"
  download "$binary_base/$artifact.sha256" "$scratch/$artifact.sha256"
  (cd "$scratch" && sha256sum --check "$artifact.sha256")
  install -m 0755 "$scratch/$artifact" "$install_root/flycod"
}

# The image build has the binary it just cross-compiled on hand, and no
# control plane to fetch one from: an image is published *with* the daemon
# it carries, by the same publish, so downloading here would be downloading
# the previous release.
install_flycod_from() {
  install -m 0755 "$1" "$install_root/flycod"
}

# One systemd unit, downloaded and installed under its published name.
install_unit() {
  download "$asset_base/$1" "$scratch/$1"
  install -m 0644 "$scratch/$1" "$service_root/$1"
}

# The first free subordinate id range in `$1`, so two users on one machine
# never share a range.
next_subid() {
  [ -f "$1" ] || : >"$1"
  awk -F: -v base="$subid_base" '
    { end = $2 + $3; if (end > base) base = end }
    END { print base }
  ' "$1"
}

# ── The session image ──

# Everything a session runs on, whether the machine is a VM or a container:
# the packages, the runtime user, the directories the daemon writes, bun for
# the Claude Code sidecar, codex, and the shell the terminal opens. The two
# callers differ only in where `flycod` comes from and whether there is a
# systemd to hand it to, so this is the one list and they add their own line.
install_session_runtime() {
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  # xvfb + openbox + xfonts-base are the desktop a `computer_use` session
  # drives: Xvfb is the screen the daemon captures, openbox the window
  # manager its tools open windows under, and the fonts are what X clients
  # draw text with. They ship in every image because the daemon runs
  # unprivileged and cannot install them per-session.
  apt-get install --yes --no-install-recommends ca-certificates curl fish git unzip xvfb openbox xfonts-base
  rm -rf /var/lib/apt/lists/*

  create_runtime_user /usr/bin/fish

  install -d -o "$runtime_user" -g "$runtime_group" /srv/flyco/work /var/lib/flyco/transcripts /var/lib/flyco/sidecar /var/lib/flyco/claude /var/lib/flyco/codex
  # Claude Code reads its managed policy from here and the daemon writes it
  # on every start, so it exists before the unit does and belongs to the
  # user the unit runs as.
  install -d -o "$runtime_user" -g "$runtime_group" /etc/claude-code

  download https://github.com/oven-sh/bun/releases/latest/download/SHASUMS256.txt "$scratch/bun-sha256"
  case "$artifact" in
    flycod-linux-x86_64) bun_archive=bun-linux-x64.zip ;;
    flycod-linux-aarch64) bun_archive=bun-linux-aarch64.zip ;;
  esac
  download "https://github.com/oven-sh/bun/releases/latest/download/$bun_archive" "$scratch/$bun_archive"
  expected_bun_hash=$(awk -v archive="$bun_archive" '$2 == archive { print $1 }' "$scratch/bun-sha256")
  if [ -z "$expected_bun_hash" ]; then
    echo "Bun release did not publish a checksum for $bun_archive" >&2
    exit 1
  fi
  printf '%s  %s\n' "$expected_bun_hash" "$scratch/$bun_archive" | sha256sum --check -
  unzip -q "$scratch/$bun_archive" -d "$scratch/bun"
  install -d -o "$runtime_user" -g "$runtime_group" /home/flyco/.bun/bin
  install -m 0755 "$scratch/bun"/*/bun /home/flyco/.bun/bin/bun
  chown -R "$runtime_user:$runtime_group" /home/flyco/.bun

  # Codex, straight from the release `npm install @openai/codex` resolves
  # to: the npm package's own bin is a Node launcher that execs this same
  # vendored binary, so installing the binary directly skips Node and npm
  # entirely — several hundred megabytes of runtime nothing else used.
  # `digest` in the release API is GitHub's own sha256 of the asset, the
  # same verification `codex-package_SHA256SUMS` gives the bundle tarballs.
  case "$artifact" in
    flycod-linux-x86_64) codex_target=x86_64-unknown-linux-musl ;;
    flycod-linux-aarch64) codex_target=aarch64-unknown-linux-musl ;;
  esac
  download https://api.github.com/repos/openai/codex/releases/latest "$scratch/codex-release.json"
  codex_meta=$(CODEX_RELEASE_JSON="$scratch/codex-release.json" CODEX_ASSET="codex-$codex_target.tar.gz" "$runtime_home/.bun/bin/bun" -e '
const release = JSON.parse(require("fs").readFileSync(process.env.CODEX_RELEASE_JSON, "utf8"));
const asset = (release.assets ?? []).find((a) => a.name === process.env.CODEX_ASSET);
if (!asset || !asset.digest?.startsWith("sha256:")) process.exit(1);
console.log(asset.browser_download_url, asset.digest.slice(7));
')
  read codex_url codex_sha256 <<EOF
$codex_meta
EOF
  download "$codex_url" "$scratch/codex.tar.gz"
  printf '%s  %s\n' "$codex_sha256" "$scratch/codex.tar.gz" | sha256sum --check -
  tar xzf "$scratch/codex.tar.gz" -C "$scratch"
  install -m 0755 "$scratch/codex-$codex_target" "$install_root/codex"

  # `node` and `npx` are the commands MCP server registrations name, and
  # there is no Node on this runtime: bunx runs the same packages, so the
  # aliases keep those servers working.
  ln -sf "$runtime_home/.bun/bin/bun" "$install_root/bunx"
  printf '#!/bin/sh\ncase "${1-}" in -y|--yes) shift ;; esac\nexec %s "$@"\n' "$install_root/bunx" >"$install_root/npx"
  printf '#!/bin/sh\nexec %s "$@"\n' "$runtime_home/.bun/bin/bun" >"$install_root/node"
  chmod 0755 "$install_root/npx" "$install_root/node"

  download https://raw.githubusercontent.com/oh-my-fish/oh-my-fish/master/bin/install "$scratch/omf-install"
  chown "$runtime_user:$runtime_group" "$scratch/omf-install"
  runuser -u "$runtime_user" -- env HOME=/home/flyco fish "$scratch/omf-install" --noninteractive --yes
}

verify_session_runtime() {
  "$install_root/flycod" --version
  command -v fish >/dev/null
  runuser -u "$runtime_user" -- env HOME=/home/flyco /home/flyco/.bun/bin/bun --version
  runuser -u "$runtime_user" -- env HOME=/home/flyco codex --version
}

install_session() {
  install_session_runtime
  # cloud-init wrote the configuration as root before this ran; the daemon
  # runs as the runtime user and has to read it.
  chgrp "$runtime_group" /etc/flycod/config.toml
  chmod 0640 /etc/flycod/config.toml
  install_flycod
  install_unit flycod.service
  systemctl daemon-reload
  verify_session_runtime
}

# A session container image. The daemon binary is a file the image build
# has on hand, there is no systemd, and the configuration is not on disk
# yet: the entrypoint writes it at start from the environment the platform
# hands the container, so the directory it lands in belongs to the runtime
# user the entrypoint runs as.
install_image() {
  install_session_runtime
  install -d -o "$runtime_user" -g "$runtime_group" -m 0750 /etc/flycod
  install_flycod_from "$1"
  verify_session_runtime
}

# ── A machine the user owns ──

install_host() {
  enrollment_token=$1

  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install --yes --no-install-recommends ca-certificates curl
  # Podman, and the two packages rootless containers need beside it:
  # `uidmap` provides newuidmap/newgidmap, `dbus-user-session` is what gives
  # the flyco user a session bus for its own systemd instance.
  if ! command -v podman >/dev/null 2>&1; then
    apt-get install --yes --no-install-recommends podman uidmap dbus-user-session
  fi

  # No login shell: this account exists to own containers, not to be logged
  # into, and the host is somebody's real machine.
  create_runtime_user /usr/sbin/nologin

  if ! grep -q "^$runtime_user:" /etc/subuid 2>/dev/null; then
    start=$(next_subid /etc/subuid)
    usermod --add-subuids "$start-$((start + subid_count - 1))" "$runtime_user"
  fi
  if ! grep -q "^$runtime_user:" /etc/subgid 2>/dev/null; then
    start=$(next_subid /etc/subgid)
    usermod --add-subgids "$start-$((start + subid_count - 1))" "$runtime_user"
  fi

  # Linger is what keeps the flyco user's systemd instance and its
  # /run/user/<uid> alive with nobody logged in, which is where rootless
  # Podman keeps its runtime state.
  loginctl enable-linger "$runtime_user"
  install -d -o "$runtime_user" -g "$runtime_group" -m 0755 "$volume_root"

  install_flycod

  # Root-only: the file `flycod host enroll` writes carries this machine's
  # host token.
  install -d -m 0700 "$host_config_dir"
  "$install_root/flycod" host enroll \
    --token "$enrollment_token" \
    --control-plane "$control_plane" \
    --volume-root "$volume_root" \
    --config "$host_config"

  install_unit flycod-host.service
  systemctl daemon-reload
  systemctl enable --now flycod-host.service

  "$install_root/flycod" --version
  runuser -u "$runtime_user" -- env HOME="$runtime_home" podman --version
  systemctl is-active flycod-host.service >/dev/null
}

case "${1:-}" in
  "") install_session ;;
  image)
    shift
    [ -n "${1:-}" ] && [ -f "$1" ] || usage
    install_image "$1"
    ;;
  host)
    shift
    [ "${1:-}" = "enroll" ] || usage
    shift
    [ -n "${1:-}" ] || usage
    install_host "$1"
    ;;
  *) usage ;;
esac
