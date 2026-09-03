#!/usr/bin/env bash
set -euo pipefail
# Enforce the cast-suppression-discipline contract (see crates/oxmgr-core/src/numeric.rs).
# numeric.rs is the ONLY exempt module: it owns every remaining lossy `as`
# (f64→usize, u32→f32, f64→u32, usize→f32) because std provides no checked
# conversion primitive for those directions. See the module's doc comment.
#
# The discipline targets CAST suppressions specifically: a cast that can
# truncate, lose precision, or lose sign must be auditable and concentrated in
# numeric.rs. Non-cast lint suppressions (e.g. clippy::unwrap_used,
# clippy::let_underscore_must_use) are legitimate and carry a reason; they are
# not what this gate polices.
if grep -rnE '#\[(allow|expect)\([^]]*cast_(possible_truncation|precision_loss|sign_loss|lossless|possible_wrap)' crates/ --include='*.rs' \
  | grep -v 'numeric.rs:'; then
  echo "ERROR: cast suppression attributes (#[allow]/#[expect] with cast_*) are banned outside numeric.rs" >&2
  echo "Resolve the cast with code structure instead (see cast-suppression-discipline)." >&2
  exit 1
fi
