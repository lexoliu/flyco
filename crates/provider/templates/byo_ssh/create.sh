set -eu
podman rm --force --ignore {{ container }}
podman run --detach \
  --name {{ container }} \
  --label flyco.machine={{ machine }} \
  --label flyco.session={{ session }} \
  --restart on-failure \
  --env-file /dev/stdin \
  {{ image }} <<'FLYCO_ENV_EOF'
{{ config_env }}={{ config_base64 }}
FLYCO_ENV_EOF
