import { EMPTY, EVENTS } from "../core/const.js";
import { fmt } from "../format/fmt.js";

export class Stats {
  #elem; #store; #bus;
  /// Releases this component's listeners in one call (section 5 teardown).
  #life = new AbortController();
  _signal() { return { signal: this.#life.signal }; }
  destroy() { this.#life.abort(); }
  static CHIPS = [{ status: "running", cls: "ok" }, { status: "restarting", cls: "warn" }, { status: "stopped", cls: "" }, { status: "crashed", cls: "bad" }, { status: "errored", cls: "bad" }];
  constructor(elem, store, bus) { this.#elem = elem; this.#store = store; this.#bus = bus; this.#bind(); }
  #bind() {
    this.#elem.addEventListener("click", evt => {
      const chip = evt.target.closest(".stat");
      if (!chip) return;
      const status = chip.dataset.status || "";
      const current = this.#store.get("status");
      this.#store.set("status", current === status ? "" : status);
      this.#bus.emit(EVENTS.FILTER_CHANGED);
    }, this._signal());
  }
  #chip(label, num, cls, status) {
    const active = this.#store.get("status") === status ? " active" : "";
    return `<span class="stat ${cls}${active}" data-status="${status}"><b>${num}</b> ${label}</span>`;
  }
  render() {
    const counts = this.#store.counts();
    this.#elem.innerHTML = this.#chip("total", this.#store.get("procs").length, "", "") + Stats.CHIPS.map(chip => this.#chip(chip.status, counts[chip.status] ?? 0, chip.cls, chip.status)).join("");
  }
}
/// Severity rank, so "the worst one" is a comparison rather than a chain of conditionals.
const ADVISORY_RANK = { info: 1, warning: 2, critical: 3 };

/// A marker beside the process name when its configuration has advisories.
///
/// Attached to the name cell rather than given a column of its own: the ten-column table already
/// needs 845px, and an eleventh column would reintroduce the horizontal overflow the responsive
/// work removed. It also keeps the marker beside the thing it describes at every width, including
/// the phone card layout where most columns are hidden.
///
/// Text, not colour alone. A coloured dot conveys nothing to an operator who cannot distinguish
/// the colours, and nothing at all in a terminal screenshot.
export const advisoryMarker = (proc) => {
  const list = advisoryState.get(proc.name) ?? [];
  if (!list.length) return "";
  // The worst severity decides the marker: showing "info" while a critical advisory is also
  // present would be actively misleading.
  const worst = list.reduce((acc, item) =>
    (ADVISORY_RANK[item.severity] ?? 0) > (ADVISORY_RANK[acc] ?? 0) ? item.severity : acc, "info");
  const count = list.length;
  // The consequences go in the tooltip, so the reason is one hover away rather than requiring the
  // detail panel. Truncated to the first three: a tooltip of eight paragraphs is not read.
  const detail = list.slice(0, 3).map(item => `• ${item.consequence}`).join("\n\n");
  const more = count > 3 ? `\n\n…and ${count - 3} more` : "";
  return `<span class="advisory-flag ${fmt.esc(worst)}" title="${fmt.esc(detail + more)}">`
    + `${count > 1 ? count : ""}!</span>`;
};

/// Findings by process name, plus the engine's own state.
///
/// Module-level for the same reason `advisoryState` is: `tableCells` is a pure function with no
/// store reference, and threading one through every call site to reach a display hint would be a
/// worse trade than a shared map.
export const findingState = new Map();
/// Processes whose baselines are still warming, and what tuning has withheld. Kept beside the
/// findings because an EMPTY findings list is only interpretable with them: "nothing is wrong",
/// "not checked yet" and "you switched this off" are three different states.
export const findingEngine = { warming: new Set(), suppressed: null };

export const FINDING_POLL_MS = 10_000;

/// A finding's severity band, derived from its confidence.
///
/// Findings carry a continuous 0..1 confidence rather than a severity, so the band is derived here
/// rather than read. The thresholds match the rule engine's own `DEFAULT_MIN_CONFIDENCE` of 0.6:
/// below that nothing would act on it, so presenting it as critical would overstate what the
/// daemon itself is willing to conclude.
export const findingBand = (confidence) => {
  const score = Number(confidence);
  if (!Number.isFinite(score)) return "warning";
  if (score >= 0.85) return "critical";
  if (score >= 0.6) return "warning";
  return "info";
};

export const FINDING_RANK = { info: 1, warning: 2, critical: 3 };

/// The per-row indicator for active findings.
///
/// Deliberately a SECOND marker beside the advisory one rather than a merged badge. They answer
/// different questions: an advisory is about configuration and is answerable by editing a setting,
/// a finding is about observed behaviour and is not. Merging them would produce a count an operator
/// cannot act on, because half of it needs a config change and half needs an investigation.
export const findingMarker = (proc) => {
  const list = findingState.get(proc.name) ?? [];
  const active = list.filter(item => item.status === "active");
  if (!active.length) return "";
  // The worst band decides the marker, same rule as advisories: showing "info" while something
  // critical is also active would be actively misleading.
  const worst = active.reduce((acc, item) => {
    const band = findingBand(item.confidence?.score);
    return (FINDING_RANK[band] ?? 0) > (FINDING_RANK[acc] ?? 0) ? band : acc;
  }, "info");
  const detail = active.slice(0, 3).map(item => {
    const key = item.key ?? {};
    const score = Number(item.confidence?.score);
    const pct = Number.isFinite(score) ? ` (${Math.round(score * 100)}% confidence)` : "";
    return `• ${key.detector ?? "?"} on ${key.metric ?? "?"}${pct}`
      + (item.summary ? `\n  ${item.summary}` : "");
  }).join("\n\n");
  const more = active.length > 3 ? `\n\n…and ${active.length - 3} more` : "";
  // `~` rather than `!`: the advisory marker owns `!`, and two markers with the same glyph would be
  // indistinguishable in a monochrome screenshot — which is exactly when an operator is reading a
  // pasted terminal capture rather than the live page.
  return `<span class="finding-flag ${fmt.esc(worst)}"`
    + ` title="${fmt.esc(`Resource findings (observed behaviour)\n\n${detail}${more}`)}">`
    + `${active.length > 1 ? active.length : ""}~</span>`;
};

/// The findings section of the detail panel.
///
/// Where the whole finding goes, including its evidence: the row marker is a signal, this is the
/// explanation. Cleared findings are shown too, because "this resolved" is only observable if the
/// clearing is visible for a while.

export const detailFindings = (proc) => {
  const list = findingState.get(proc.name) ?? [];
  if (!list.length) {
    // The empty case is where this earns its keep. Three different reasons produce no findings and
    // an operator responds differently to each, so the panel names which one applies.
    if (findingEngine.warming.has(proc.name)) {
      return `<div class="detail-section"><h3>Resource Findings</h3>`
        + `<p class="finding-note">No findings yet: this process is still building its baselines, `
        + `so its metrics are not being compared against anything.</p></div>`;
    }
    return `<div class="detail-section"><h3>Resource Findings</h3>`
      + `<p class="finding-note">No findings. Metrics are being compared against established `
      + `baselines and nothing has departed from them.</p></div>`;
  }

  const rows = list.map(item => {
    const key = item.key ?? {};
    const band = findingBand(item.confidence?.score);
    const score = Number(item.confidence?.score);
    const evidence = item.evidence ?? {};
    // Observed against expected, with the statistic that fired. This is what makes a finding
    // checkable rather than a claim: an operator can recompute it.
    const numbers = [
      Number.isFinite(evidence.observed) ? `observed ${fmt.pct(evidence.observed)}` : null,
      Number.isFinite(evidence.expected) ? `expected ${fmt.pct(evidence.expected)}` : null,
      Number.isFinite(evidence.statistic) && Number.isFinite(evidence.threshold)
        ? `${fmt.pct(evidence.statistic)} against a ${fmt.pct(evidence.threshold)} threshold`
        : null,
    ].filter(Boolean).join(" · ");
    const occurrence = (item.occurrence ?? 1) > 1
      ? `<span class="finding-recurrence">episode ${item.occurrence}</span>` : "";
    const guidance = item.guidance;
    return `<div class="finding-row ${item.status === "cleared" ? "is-cleared" : ""}">`
      + `<span class="finding-flag ${fmt.esc(band)}">~</span>`
      + `<div class="finding-detail">`
      + `<div class="finding-head"><b>${fmt.esc(key.detector ?? "?")}</b>`
      + ` on ${fmt.esc(key.metric ?? "?")}`
      + (key.variant ? ` (${fmt.esc(key.variant)})` : "")
      + `<span class="finding-status">${fmt.esc(item.status ?? "?")}</span>${occurrence}</div>`
      + (item.summary ? `<div class="finding-summary">${fmt.esc(item.summary)}</div>` : "")
      + (numbers ? `<div class="finding-evidence">${fmt.esc(numbers)}</div>` : "")
      + (Number.isFinite(score)
        ? `<div class="finding-evidence">confidence ${Math.round(score * 100)}%</div>` : "")
      // Guidance AFTER the evidence, never instead of it: the numbers are what make the finding
      // checkable, and advice presented above them would invite acting before reading. Suppressed
      // on a cleared finding — telling someone what to do about a condition that has stopped is
      // noise, and the row is dimmed for the same reason.
      + ((item.status !== "cleared" && guidance)
        ? `<details class="finding-guidance"><summary>What to check</summary><ul>`
          + guidance.map(step => `<li>${fmt.esc(step)}</li>`).join("")
          + `</ul></details>`
        : "")
      + `</div></div>`;
  }).join("");

  const active = list.filter(item => item.status === "active").length;
  return `<div class="detail-section"><h3>Resource Findings (${active} active of ${list.length})</h3>`
    + rows
    + `<p class="finding-note">Observed behaviour, not configuration. Nothing here has been acted `
    + `on: protection mode is observing by default.</p></div>`;
};
/// How often configuration advisories are re-derived. Thirty seconds: a config changes when
/// someone changes it, and the rules are pure functions over settings that were already read.
export const ADVISORY_POLL_MS = 30_000;

/// Advisories by process name. Module-level rather than on the store because `tableCells` is a
/// pure function with no store reference, and threading one through every call site to reach a
/// display hint would be a worse trade than a single shared map.
export const advisoryState = new Map();

/// Typical (median) values by process name, from `/api/typical`.
///
/// Module-level for the same reason `advisoryState` is: `tableCells` is a pure function with no
/// store reference, and threading one through every call site to reach a display hint would be a
/// worse trade than one shared map.
export const typicalState = new Map();

export const TYPICAL_POLL_MS = 30_000;

/// Renders a current figure with its typical value beneath, visually distinguishable.
///
/// Task 4b.4. The two are NOT interchangeable, so they are not rendered alike: the current reading
/// is the primary value at full strength, and the typical is a smaller muted line prefixed with `~`
/// — the convention for "approximately", and a non-colour cue so the distinction survives
/// colour-blindness and monochrome.
///
/// A stopped process shows the absence marker for `current` and STILL shows its typical (4b.7):
/// the typical describes what it did while running, which is the context an operator wants while
/// looking at a process that has stopped.
export const currentWithTypical = (proc, metric, render) => {
  const running = proc.status === "running" && proc.pid !== null && proc.pid !== undefined;
  // `null`, not zero, for a stopped process — a zero would read as a measurement of an idle
  // process rather than the absence of one.
  const current = running
    ? render(metric === "cpu" ? proc.cpu_percent : proc.memory_bytes)
    : EMPTY;

  const report = typicalState.get(proc.name);
  const entry = metric === "cpu" ? report?.cpu_percent : report?.memory_bytes;
  if (!entry || entry.state !== "available") {
    // No second line at all rather than "typical: unknown". A row that says nothing is quieter
    // than a row that says it has nothing to say, and the reason is available on /api/typical for
    // anyone who asks.
    return `<span class="cell-value">${fmt.esc(current)}</span>`;
  }

  const typical = render(entry.median);
  const title = `typical ${typical} over ${Math.round(entry.window_secs / 60)} min`
    + ` from ${entry.sample_count} samples`;
  return `<span class="cell-value">${fmt.esc(current)}</span>`
    + `<span class="cell-typical" title="${fmt.esc(title)}">~${fmt.esc(typical)}</span>`;
};
