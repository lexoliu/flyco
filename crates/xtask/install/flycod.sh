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
  echo "usage: flycod.sh                     build a flyco session image" >&2
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

install_session() {
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install --yes --no-install-recommends ca-certificates curl fish git npm unzip

  create_runtime_user /usr/bin/fish

  install -d -o "$runtime_user" -g "$runtime_group" /srv/flyco/work /var/lib/flyco/transcripts /var/lib/flyco/sidecar /var/lib/flyco/claude /var/lib/flyco/codex
  chgrp "$runtime_group" /etc/flycod/config.toml
  chmod 0640 /etc/flycod/config.toml

  install_flycod
  install_unit flycod.service

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

  npm install --global @openai/codex@latest

  download https://raw.githubusercontent.com/oh-my-fish/oh-my-fish/master/bin/install "$scratch/omf-install"
  chown "$runtime_user:$runtime_group" "$scratch/omf-install"
  runuser -u "$runtime_user" -- env HOME=/home/flyco fish "$scratch/omf-install" --noninteractive --yes

  systemctl daemon-reload
  "$install_root/flycod" --version
  command -v fish >/dev/null
  runuser -u "$runtime_user" -- env HOME=/home/flyco /home/flyco/.bun/bin/bun --version
  runuser -u "$runtime_user" -- env HOME=/home/flyco codex --version
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
  host)
    shift
    [ "${1:-}" = "enroll" ] || usage
    shift
    [ -n "${1:-}" ] || usage
    install_host "$1"
    ;;
  *) usage ;;
esac
