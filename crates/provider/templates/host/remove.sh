set -eu
podman rm --force --ignore {{ container }}
{%- if !keep_volume %}
podman volume rm --force {{ volume }}
{%- endif %}
