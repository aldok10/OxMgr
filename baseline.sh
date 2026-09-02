#!/bin/bash
# Must run while daemon is up.
echo "=== Baseline ==="
# Raw
curl -i -H "Accept-Encoding: identity" localhost:46002/ > raw.http
stat -c%s raw.http # Size
# Gzip
curl -i -H "Accept-Encoding: gzip" localhost:46002/ > gzip.http
stat -c%s gzip.http # Size
grep "ETag" raw.http
