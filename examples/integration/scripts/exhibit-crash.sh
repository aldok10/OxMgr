#!/bin/sh
# Exits immediately with a fixed status code - the raw material of crash-loop
# and repeated-exit findings. Each app passes its own signature code so the
# detectors see distinct deterministic faults rather than one shared cause:
#   exhibit-crash.sh 1  (crash-a)  status 1
#   exhibit-crash.sh 2  (crash-b)  status 2
#   exhibit-crash.sh 3  (db)       status 3
code="${1:-1}"
echo "crash: exiting with status $code at $(date)"
exit "$code"