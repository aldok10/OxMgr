#!/bin/sh
# Idles for 90 seconds, then burns one core continuously.
#
# The idle stretch is deliberate: baselines need ~30 samples (~60s at the 2s
# tick) before any comparison is made. When the burn starts, the CPU metric
# departs well above the baseline this process itself established, which is
# what a level_departure finding (direction "above") looks like.
echo "spike: idle for 90s (baseline warm-up), then burn one core forever"
sleep 90
while :; do :; done