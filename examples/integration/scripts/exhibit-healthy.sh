#!/bin/sh
# A healthy control process: low CPU, trivial memory, steady cadence. Its job
# is the "no findings" contrast - the dashboard shows the compared-and-clean
# state next to the chaos-lab findings instead of a table of nothing.
while :; do
  echo "ticker: $(date)"
  sleep 5
done