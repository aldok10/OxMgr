#!/bin/sh
# Grows memory without bound: appends 512 KiB of data to a string variable
# every second and never frees it.
#
# Sized so the configured memory limit (64 MiB) is reached in about two
# minutes. Two findings cooperate here:
#   - resource_leak needs a >=600s observation window, so it matures later;
#   - the memory limit restarts the process in the meantime, which is the
#     "bounded leak" behaviour: usage returns to the old level and the
#     episode counter advances instead of the daemon dying with it.
set -u
mem=""
size=0
while :; do
  chunk=$(head -c 524288 /dev/zero | tr '\0' 'a')
  mem="$mem$chunk"
  size=$((size + 524288))
  if [ $((size % 5242880)) -eq 0 ]; then
    echo "leak: retained ~$((size / 1048576)) MiB"
  fi
  sleep 1
done