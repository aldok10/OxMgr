import { EMPTY } from "../core/const.js";
import { fmt } from "../format/fmt.js";
import { network } from "../shell/network.js";

/// Draws the host panel's structural output: SVG sparklines, the per-region
/// containers, the full-repaint orchestration, and CSSOM style promotion.
///
/// The second of HostPanel's two rendering collaborators (see shell.js).
/// Holds no stream and no timers; renders from panel state through the context
/// object (`ctx.state`, `ctx.regions`, ...) and reaches figure rendering
/// through `ctx.metrics`. The subsystem handler table lives here because it is
/// wiring — pure dispatch over whichever collaborator owns the region — and is
/// exercised without constructing either collaborator.
export class SparklineRenderer {
  #ctx;
  constructor(ctx) { this.#ctx = ctx; }

  /// A sparkline as an inline SVG polyline.
  ///
  /// SVG rather than btop's braille glyphs: the block/braille approach depends on a font that
  /// renders those code points at a predictable width, and in a browser that is not a safe
  /// assumption — a fallback font turns the graph into tofu. SVG scales with the column and
  /// carries no font dependency.
  ///
  /// `max` is the y-axis ceiling, and it is returned to the caller so it can be LABELLED. An
  /// unlabelled sparkline cannot be read: 60% of the way up an unknown axis is not a quantity.
  /// btop does the same thing — it prints the humanised max in the graph's corner.
  spark(samples, cls = "") {
    const points = samples.filter(v => Number.isFinite(v));
    if (points.length < 2) return "";
    // Auto-scaled with a floor, following btop's own rule: it never scales below 10 KiB/s, so
    // an idle interface shows a flat line near the bottom instead of amplifying noise into a
    // mountain range.
    const max = Math.max(...points, 10 * 1024);
    const W = 100, H = 24;
    const step = W / (points.length - 1);
    const coords = points
      .map((v, i) => `${(i * step).toFixed(1)},${(H - (v / max) * H).toFixed(1)}`)
      .join(" ");
    // `preserveAspectRatio="none"` so the line stretches to the column width rather than
    // letterboxing; the y-axis is labelled, so horizontal scale carries no meaning to distort.
    return `<svg class="host-spark ${cls}" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none"`
      + ` aria-hidden="true" focusable="false">`
      + `<polyline points="${coords}" /></svg>`;
  }

  /// The container for one region, created once and reused.
  ///
  /// Regions are addressed by name rather than rebuilt as a list, so an update writes into
  /// an existing node. Order is fixed by creation order, so a subsystem arriving late does
  /// not jump the layout.
  region(name, cls) {
    let el = this.#ctx.regions.get(name);
    if (!el) {
      el = document.createElement("div");
      el.className = cls;
      el.dataset.region = name;
      // Inserted at its DECLARED position, not appended. Appending made display order a race:
      // `#pollConsumers` resolves on its own fetch and regularly beat the host snapshot, which
      // put the consumer listing above the gauges it is supposed to explain — measured, with the
      // region order reading consumers/gauges/cores. Ordering by the table below makes the layout
      // independent of which response lands first.
      // Order arrives through the context: a collaborator reading another
      // class's statics directly would couple them beyond the context contract.
      const order = this.#ctx.regionOrder;
      const rank = order.indexOf(name);
      // An unknown region goes last rather than first: better appended than silently promoted
      // above the figures.
      const after = rank < 0 ? null : [...this.#ctx.body.children].find(child => {
        const other = order.indexOf(child.dataset.region);
        return other > rank;
      });
      this.#ctx.body.insertBefore(el, after ?? null);
      this.#ctx.regions.set(name, el);
    }
    return el;
  }

  renderAll() {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const id = data.identity ?? {};
    // Identity is static and arrives only in the snapshot, so it is written here and never
    // by a partial update.
    // Hostname and OS ONLY. The architecture and the core count moved to the cores section,
    // where the per-core grid they describe actually lives: `arm64 · 8 cores` here and
    // `8 logical` in the cores header stated the same count twice, in two different words, and
    // the arch belongs with the cores rather than with the machine's name.
    this.#ctx.identity.textContent = [id.host_name, id.os_name].filter(Boolean).join(" · ");

    // Creation order is display order.
    // "gauges" replaces the separate memory/swap/cpu entries: one region, painted once here
    // rather than three times by three keys that all repaint the same node.
    // `cpu_stats` MUST be in this list. It was omitted, so its only paint came from the consumer
    // poll — where `#state` is still null on a fresh load — and the region rendered a single
    // "N processes" chip while every core-derived figure was silently dropped. Measured: 1 chip
    // instead of 6, and re-delivering the snapshot did not fix it because nothing repainted the
    // region from host state at all.
    for (const key of ["gauges", "cpu_stats", "cores", "storage_net",
      "failures"]) {
      this.renderSubsystem(key);
    }
    // No auto-reveal here: the panel becomes visible only through #reveal, so data
    // arriving never opens a panel the operator did not ask for. While concealed the
    // stream is closed anyway, so this path runs only for an already-visible panel
    // (or the restore-on-load case, which revealed before subscribing).
  }

  /// One handler per subsystem, keyed by SSE event name.
  ///
  /// Groups that repaint the same region share one handler: gauges/memory/swap all repaint the
  /// donut row from current state; storage_net/filesystems repaint the merged storage+network
  /// region; network adds the sparkline history recording before the shared repaint.
  ///
  /// Each handler receives the panel instance, the current state snapshot, and a numeric
  /// predicate.  The handler dispatches to rendering methods — it does not read mutable panel
  /// state directly, so the mapping can be inspected and exercised without constructing the
  /// full panel.
  static _SUBSYSTEM_HANDLERS = (() => {
    const repaintGauges = (d) => { d.metrics.renderGauges(); };
    const repaintStorageNet = (d) => { d.metrics.renderStorageNet(); };
    return {
      // CPU, RAM and SWAP share one region as three donut gauges on a single row. All three
      // cases paint the same region: swap arrives inside the memory payload, and CPU arrives
      // separately, so any of the three events repaints the row from current state.
      gauges: repaintGauges, memory: repaintGauges, swap: repaintGauges,
      cpu_stats: (d) => { d.metrics.renderCpuStats(); },
      // CPU repaints the gauge row AND the core rows: both read from the same payload, and a
      // core list showing a different tick from the total it sits under would be a visible
      // contradiction.
      cpu: (d) => { d.metrics.renderGauges(); d.metrics.renderCores(); d.metrics.renderCpuStats(); },
      cores: (d) => { d.metrics.renderCores(); },
      // Load average and uptime share ONE row, repainted from current state.
      load_average: (d) => { d.metrics.renderCpuStats(); },
      uptime: (d) => { d.metrics.renderCpuStats(); },
      load_uptime: (d) => { d.metrics.renderCpuStats(); },
      // Filesystems and interfaces share one merged region. The network event additionally
      // feeds the sparkline history below, so only it records history: recording on both
      // events would double the sample rate of every trace.
      storage_net: repaintStorageNet, filesystems: repaintStorageNet,
      network: (d, data) => {
        d.metrics.recordNetHistory(data.network?.interfaces ?? []);
        d.metrics.renderStorageNet();
      },
      components: (d, data, num) => d.sparks.renderComponents(data, num),
      failures: (d, data) => d.sparks.renderFailures(data),
    };
  })()

  /// Repaints the components (temperature) region from the given state snapshot.
  renderComponents(data, num) {
    const rows = (data.components ?? [])
      .filter(c => num(c.temperature_celsius))
      .slice(0, 8)
      .map(c => `<div class="host-row"><span class="host-row-name">${fmt.esc(c.label)}</span>`
        + `<span class="host-row-detail">${fmt.pct(c.temperature_celsius)} °C</span></div>`)
      .join("");
    const el = this.region("components", "host-list");
    el.dataset.figure = "host.components";
    el.innerHTML = rows ? `<h3>temperatures</h3>${rows}` : "";
    el.hidden = !rows;
  }

  /// Repaints the failures region from the given state snapshot.
  ///
  /// A failed subsystem is named rather than left as a gap: a missing figure could be a
  /// collection failure or a platform that never had it, and those need different
  /// responses from an operator.
  renderFailures(data) {
    const el = this.region("failures", "host-failures");
    el.dataset.figure = "host.failures";
    el.innerHTML = (data.failures ?? []).map(f => `<div class="host-metric">` + this.#ctx.metrics.metric(
      `${f.subsystem} failed`, EMPTY, false, null,
      // `message`, not `reason`: the API's field name. An earlier version read
      // `failure.reason` and silently fell back to the generic text for every real
      // failure — the mock in the browser probe used the wrong name too, so the check
      // passed against the same mistake.
      f.message || "Collection failed; no reason reported",
    ) + `</div>`).join("");
    el.hidden = !(data.failures ?? []).length;
  }

  /// Repaints exactly one region by dispatching through the handler map.
  renderSubsystem(key) {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const handler = SparklineRenderer._SUBSYSTEM_HANDLERS[key];
    if (handler) {
      handler(this.#ctx, data, num);
      this.applyStyles();
    }
  }

  /// Pushes visualising values from data attributes to CSSOM properties.
  ///
  /// Templates carry the numbers as `data-*` attributes rather than inline
  /// `style="--x:…"`, which a strict Content-Security-Policy (`style-src 'self'`)
  /// blocks. After a repaint this scans the whole panel and promotes each
  /// attribute to the custom property / transform the CSS reads. Scanning the
  /// full body rather than one region keeps the call correct no matter which
  /// render method ran.
  applyStyles() {
    const root = this.#ctx.body;
    const elements = root.querySelectorAll('[data-scale], [data-depth]');

    // 1. Suppress transitions on freshly built elements, so a rebuilt meter does
    //    not replay its animation from the initial value.
    elements.forEach(e => e.style.transition = 'none');

    // 2. Apply the values instantly, with no animation.
    //    For donut gauges (.host-gauge-bar-fill), set --gauge-scale CSS variable
    //    instead of transform, since the donut uses conic-gradient.
    root.querySelectorAll('[data-scale]').forEach(e => {
      if (e.classList.contains('host-gauge-bar-fill')) {
        // Donut gauge: set the CSS variable that conic-gradient reads
        const bar = e.closest('.host-gauge-bar');
        if (bar) bar.style.setProperty('--gauge-scale', e.dataset.scale);
      } else {
        // Linear bars: use scaleX as before
        e.style.transform = `scaleX(${e.dataset.scale})`;
      }
    });
    root.querySelectorAll('[data-depth]').forEach(e => e.style.setProperty('--tree-depth', e.dataset.depth));

    // 3. Force a reflow so the suppression above is flushed before transitions
    //    are restored — without it the restore coalesces with the value change
    //    and the animation plays anyway.
    void root.offsetWidth;

    // 4. Restore transitions, so subsequent in-place value changes animate.
    elements.forEach(e => e.style.transition = '');
  }
}
