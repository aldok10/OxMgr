/// Severity banding, with the thresholds served by the daemon.
///
/// Extracted from what used to be the boot file so the modules that band
/// figures import a cohesive leaf instead of reaching into startup code.
///
/// The boundaries used to be duplicated here as `WARN_PERCENT = 75` / `BAD_PERCENT = 90` beside
/// `severity.rs`'s own `DEFAULT_WARNING_PERCENT` / `DEFAULT_CRITICAL_PERCENT` — two copies of one
/// decision, so changing the documented default would silently have left the dashboard on the old
/// boundary with nothing failing. The Rust constants are now the single source of truth and
/// `/api/config` carries them.
///
/// The built-in values below are a fallback for a config request that fails, not a second
/// definition: without them a failed request would leave every figure unbanded.

/// Severity banding, with the thresholds served by the daemon.
///
/// The boundaries used to be duplicated here as `WARN_PERCENT = 75` / `BAD_PERCENT = 90` beside
/// `severity.rs`'s own `DEFAULT_WARNING_PERCENT` / `DEFAULT_CRITICAL_PERCENT` — two copies of one
/// decision, so changing the documented default would silently have left the dashboard on the old
/// boundary with nothing failing. The Rust constants are now the single source of truth and
/// `/api/config` carries them.
///
/// The built-in values below are a fallback for a config request that fails, not a second
/// definition: without them a failed request would leave every figure unbanded.
const severityCfg = {
warning: 75,
critical: 90,
hysteresis: 2,
enabled: true,
};

/// Remembers the band each figure is currently presented in, so hysteresis has something to
/// compare against. Keyed by an opaque string the caller chooses.
const severityState = new Map();

/// The band for a ratio, applying hysteresis against the band already shown.
///
/// Mirrors `severity::band_with_hysteresis`: leaving a band requires clearing its boundary by the
/// margin, and entering a higher one requires exceeding it by the margin. Without a previous band
/// there is nothing to flicker against, so the plain boundaries apply.
///
/// Returns "" for normal rather than "normal", because the class is applied directly and an
/// element with no severity class is the unstyled default.
export const severityBand = (key, percent) => {
if (!severityCfg.enabled) return "";
const value = Number(percent);
if (!Number.isFinite(value)) return "";

const plain = (v) =>
  v >= severityCfg.critical ? "bad" : v >= severityCfg.warning ? "warn" : "";
const rank = { "": 0, warn: 1, bad: 2 };
const previous = key === null ? undefined : severityState.get(key);
const candidate = plain(value);

let band = candidate;
if (previous !== undefined && candidate !== previous) {
  const margin = severityCfg.hysteresis;
  if (rank[candidate] > rank[previous]) {
    // Rising: the boundary being entered must be cleared upward by the margin.
    const boundary = candidate === "bad" ? severityCfg.critical : severityCfg.warning;
    band = value >= boundary + margin ? candidate : previous;
  } else {
    // Falling: the boundary of the band being LEFT must be cleared downward by the margin.
    const boundary = previous === "bad" ? severityCfg.critical : severityCfg.warning;
    band = value <= boundary - margin ? candidate : previous;
  }
}
if (key !== null) severityState.set(key, band);
return band;
};

/// The non-colour cue for a band.
///
/// Task 5.3. Colour alone fails for a red-green colour-blind operator, in a monochrome terminal
/// screenshot, and in Windows High Contrast mode where author backgrounds are stripped entirely.
/// A glyph survives all three, and it is a real text node rather than a CSS pseudo-element so it
/// reaches a screen reader too.
export const severityCue = (band) => {
if (!severityCfg.enabled || !band) return "";
const label = band === "bad" ? "critical" : "warning";
const glyph = band === "bad" ? "!!" : "!";
return `<i class="sev-cue ${band}" role="img" aria-label="${label}">${glyph}</i>`;
};

/// Adopts the daemon's severity settings.
export const applySeverityConfig = (cfg) => {
const sev = cfg?.severity;
if (!sev) return;
const warning = Number(sev.warning_percent);
const critical = Number(sev.critical_percent);
// Both must be usable AND ordered before either is adopted: a warning boundary above the
// critical one would make the bands non-monotonic, which is exactly what 5.1 forbids.
if (Number.isFinite(warning) && Number.isFinite(critical) && warning < critical) {
  severityCfg.warning = warning;
  severityCfg.critical = critical;
}
const hysteresis = Number(sev.hysteresis_percent);
if (Number.isFinite(hysteresis) && hysteresis >= 0) severityCfg.hysteresis = hysteresis;
if (typeof sev.styling_enabled === "boolean") severityCfg.enabled = sev.styling_enabled;
// Bands remembered under the old thresholds would be compared against the new ones, so the
// hysteresis state is dropped rather than migrated.
severityState.clear();
};
