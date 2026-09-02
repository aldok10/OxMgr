#!/usr/bin/env bash
set -euo pipefail
# Enforce zero-suppression contract from openspec/specs/cast-suppression-discipline.
# src/numeric.rs is the ONLY exempt module: it owns every remaining lossy `as`
# in the crate (f64→usize, u32→f32, f64→u32, usize→f32) because std provides no
# checked conversion primitive for those directions. See the module's doc comment.
if grep -rnE '#\[(allow|expect)\b' crates/ --include='*.rs' \
  | grep -v 'numeric.rs:'; then
  echo "ERROR: suppression attributes (#[allow]/#[expect]) are banned outside numeric.rs" >&2
  echo "Resolve the lint with code structure instead (see spec cast-suppression-discipline)." >&2
  exit 1
fi
