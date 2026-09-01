#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "flycod installer must run as root" >&2
  exit 1
fi

release_base=${FLYCO_RELEASE_BASE:-https://github.com/lexoliu/flyco/releases/download/dev}
asset_base=${FLYCO_ASSET_BASE:-https://dev.flyco.dev/install}
install_root=/usr/local/bin
service_root=/etc/systemd/system
runtime_user=flyco
runtime_group=flyco

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

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install --yes --no-install-recommends ca-certificates curl fish git npm unzip

if ! getent group "$runtime_group" >/dev/null; then
  groupadd --system "$runtime_group"
fi
if ! id "$runtime_user" >/dev/null 2>&1; then
  useradd --system --gid "$runtime_group" --create-home --home-dir /home/flyco --shell /usr/bin/fish "$runtime_user"
fi

install -d -o "$runtime_user" -g "$runtime_group" /srv/flyco/work /var/lib/flyco/transcripts /var/lib/flyco/sidecar /var/lib/flyco/claude /var/lib/flyco/codex
chgrp "$runtime_group" /etc/flycod/config.toml
chmod 0640 /etc/flycod/config.toml

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
chmod 0755 "$scratch"

download "$release_base/$artifact" "$scratch/$artifact"
download "$release_base/$artifact.sha256" "$scratch/$artifact.sha256"
(cd "$scratch" && sha256sum --check "$artifact.sha256")
install -m 0755 "$scratch/$artifact" "$install_root/flycod"

download "$asset_base/flycod.service" "$scratch/flycod.service"
install -m 0644 "$scratch/flycod.service" "$service_root/flycod.service"

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
