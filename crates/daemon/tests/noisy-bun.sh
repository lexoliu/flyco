#!/bin/sh
# A stand-in for bun that is loud and then fails, used by `tests/driver.rs`
# to prove `bun install`'s output never reaches flycod's stdout — which
# carries structured output and nothing else.
set -eu
echo "bun install v1.3.4 (5eb2145b)"
echo "Resolving dependencies"
echo "warning: something happened" >&2
exit 1
