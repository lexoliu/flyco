#!/bin/sh
# Stand-in shell for the PTY tests: echo a marker, then copy stdin to stdout.
set -eu
printf 'ready\n'
exec cat
