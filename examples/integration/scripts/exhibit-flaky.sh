#!/bin/sh
# Fails intermittently: one 45s success cycle, then an exit-1 failure, over
# and over. Declared as depending on `db` (which crash-loops with status 3),
# so dependency correlation can implicate the dependency when both are
# failing around the same time - the shape the detector reports.
n=0
while :; do
  if [ $((n % 2)) -eq 0 ]; then
    echo "flaky: ok cycle $n"
    sleep 45
  else
    echo "flaky: failing cycle $n at $(date)"
    exit 1
  fi
  n=$((n + 1))
done