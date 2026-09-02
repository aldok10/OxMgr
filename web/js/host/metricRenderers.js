import { EMPTY } from "../core/const.js";
import { severityBand, severityCue } from "../core/severity.js";
import { fmt } from "../format/fmt.js";

/// Renders the host panel's figures: gauges, CPU stats, the summary strip,
/// storage + network, cores, and the consumer listing.
///
/// One of HostPanel's two rendering collaborators (see shell.js). Holds no
/// stream, no timers and no DOM region of its own: every value it reads or
/// writes lives in the panel and is reached through the context object handed
///
/// Every figure's canonical surface is documented in the figure-ownership map
/// (`openspec/changes/archive/2026-08-23-dashboard-bento-redesign/figure-ownership-map.md`).
/// When adding a new surface, update that map and the `FIGURES` constant
/// below so no figure is rendered by two surfaces.
/// to the constructor, and the spark/region/style primitives it composes into
/// figures are reached through `ctx.sparks`. Method bodies are unchanged from
/// when they lived on HostPanel — only the receiver of cross-boundary state
/// and calls changed (`this.#x` became `this.#ctx.x`) — which is what makes
/// the behaviour-preservation contract checkable byte for byte.
export class MetricRenderer {
  #ctx;
  constructor(ctx) { this.#ctx = ctx; }

  /// One labelled figure. `available: false` renders the absence marker, never a zero.
  ///
  /// `reason` overrides the default tooltip. A subsystem that *failed* is a different absence
  /// from one the platform never had, and an operator can act on the first — so its reason
  /// has to survive to the tooltip rather than being replaced by the generic text.
  metric(key, value, available = true, bar = null, reason = null) {
    const why = reason ?? "Not reported on this platform";
    const body = available
      ? `<span class="host-metric-val">${fmt.esc(value)}</span>`
      : `<span class="host-metric-val na" title="${fmt.esc(why)}">${EMPTY}</span>`;
    const meter = (available && bar !== null) ? this.#bar(bar, `metric:${key}`) : "";
    // The non-colour cue sits with the label, so severity is readable without colour and is
    // announced rather than being a decorative pseudo-element.
    const cue = (available && bar !== null) ? severityCue(severityBand(null, bar)) : "";
    return `<span class="host-metric-key">${fmt.esc(key)}${cue}</span>${body}${meter}`;
  }

  #bar(percent, key = null) {
    const value = Math.max(0, Math.min(100, Number(percent) || 0));
    // Keyed per figure: hysteresis compares against the band THIS figure is showing, and a shared
    // key would let one metric's band suppress another's change.
    const cls = severityBand(key, value);
    return `<div class="host-bar"><span class="${cls}" data-scale="${value / 100}"></span></div>`;
  }

  /// One gauge: a percentage with a length-based bar, the number beside it.
  ///
  /// The bar is drawn by CSS `scaleX()` off a `data-scale` attribute, so there is
  /// no SVG, no canvas and no per-frame JS — the element carries a number and CSS
  /// does the rest. This replaces the former donut ring (conic-gradient) per the
  /// NN/g finding that length and 2D position are more accurate than angle and area.
  ///
  /// `detail` is the byte figure under the number. Absent for CPU, which has no "of N".
  /// The `data-figure` identity for each gauge, used by the duplication test (§D7).
  static FIGURES = {
    CPU: "cpu.used_percent",
    RAM: "memory.used_percent",
    SWAP: "memory.swap.used_percent",
  };

  #gauge(label, percent, detail, available = true, reason = null, dim = null) {
    const why = reason ?? "Not reported on this platform";
    const figureId = MetricRenderer.FIGURES[label] ?? `host.${label}`;
    if (!available || percent === null) {
      // The absence marker, never a zero bar: a 0% bar reads as "measured, and idle", which
      // is a different claim from "not reported".
      return `<div class="host-gauge host-gauge-na" data-figure="${figureId}" title="${fmt.esc(why)}">`
        + `<span class="host-gauge-label">${fmt.esc(label)}</span>`
        + `<div class="host-gauge-bar"><span class="host-gauge-bar-fill" data-scale="0"></span>`
        + `<span class="host-gauge-num">${EMPTY}</span></div></div>`;
    }
    const value = Math.max(0, Math.min(100, Number(percent) || 0));
    const cls = severityBand(`gauge:${label}`, value);
    // Rounded for the glance-level label: a bar is read by length, and "81%" reads
    // faster than "81.4%" at 13px. The precise figure stays in the title and in `detail`.
    const shown = Math.round(value);
    const base = detail ? `${label}: ${fmt.pct(value)}% — ${detail}` : `${label}: ${fmt.pct(value)}%`;

    // A gauge that opens the consumer listing is a real `<button>`, not a div with a click
    // handler: it has to be reachable by keyboard and announce its expanded state, and both
    // come free from the element. SWAP passes no dim and stays a plain figure, because the
    // sampler ranks by CPU and memory only — a swap button would open a listing that cannot
    // answer for swap.
    const expanded = this.#ctx.consumersOpen && this.#ctx.consumerDim === dim;
    const inner = `<span class="host-gauge-label">${fmt.esc(label)}</span>`
      + `<div class="host-gauge-bar ${cls}" role="img" aria-label="${fmt.esc(base)}">`
      + `<span class="host-gauge-bar-fill" data-scale="${value / 100}"></span>`
      + `<span class="host-gauge-num">${shown}<i>%</i>${severityCue(cls)}</span></div>`
      + (detail ? `<span class="host-gauge-detail">${fmt.esc(detail)}</span>` : "");

    if (!dim) return `<div class="host-gauge ${cls}" data-figure="${figureId}" title="${fmt.esc(base)}">${inner}</div>`;

    const hint = expanded ? "hide top consumers" : "show top consumers";
    return `<button type="button" class="host-gauge is-actionable ${cls}"`
      + ` data-figure="${figureId}"`
      + ` data-consumer-dim="${fmt.esc(dim)}" aria-expanded="${expanded}"`
      + ` aria-controls="host-consumers-body"`
      + ` title="${fmt.esc(`${base}\n\nClick to ${hint}`)}">${inner}</button>`;
  }

  /// The core count used as the CPU gauge denominator. The per-core sample length when
  /// present, else the identity's logical count. Denied to the gauge when neither exists,
  /// so "of 0 cores" can never render.
  ///
  /// The daemon trims `per_core` to the cgroup quota inside a limited container, so this
  /// denominator is the count of cores the process can actually use — a "of 4 cores"
  /// figure is the quota, not the host's full core count.
  #cpuCoreCount() {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const cpu = data.cpu;
    const ident = data.identity ?? {};
    if (Array.isArray(cpu?.per_core) && cpu.per_core.length) return cpu.per_core.length;
    return num(ident.logical_core_count) ? ident.logical_core_count : null;
  }

  /// The CPU | RAM | SWAP row, repainted whole.
  ///
  /// Reads current state rather than taking arguments, so whichever of the three events arrives
  /// paints all three gauges from what is known now. A per-gauge update would need three
  /// sub-regions and could leave one showing a stale neighbour's reading.
  renderGauges() {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const mem = data.memory;
    const swap = data.memory?.swap;
    const cpu = data.cpu;

    // CPU is withheld until sampled over a valid interval, so a starting daemon does not
    // report an idle machine.
    const cpuKnown = cpu && num(cpu.global_percent);
    // A host with no swap configured reports a zero total. That is "not applicable", not 0%
    // used — dividing by it is how a swapless machine ends up displaying a healthy green ring.
    const swapPresent = swap && num(swap.used_percent) && Number(swap.total_bytes) > 0;

    const el = this.#ctx.sparks.region("gauges", "host-gauges");
    el.innerHTML = [
      // Core count is stated once in the cores section header (host-cores-meta).
      // The CPU gauge shows only the percentage — the "N of M cores" detail is
      // removed to satisfy dashboard-information-architecture: one surface per figure.
      this.#gauge("CPU", cpuKnown ? cpu.global_percent : null,
        null,
        !!cpuKnown, "Awaiting a valid sampling interval", "cpu"),
      this.#_ramGauge(mem, num),
      this.#gauge("SWAP", swapPresent ? swap.used_percent : null,
        swapPresent ? fmt.bytePair(swap.used_bytes, swap.total_bytes) : null,
        !!swapPresent, swap ? "No swap configured on this host" : null),
    ].join("");

    // Clicking CPU or RAM opens the consumer listing sorted by that dimension. Clicking the
    // gauge that is already showing closes it again, so the same control both asks and dismisses
    // the question — a separate close button would be a second thing to find.
    for (const button of el.querySelectorAll("[data-consumer-dim]")) {
      button.addEventListener("click", () => {
        const dim = button.dataset.consumerDim;
        if (this.#ctx.consumersOpen && this.#ctx.consumerDim === dim) {
          this.#ctx.consumersOpen = false;
        } else {
          this.#ctx.consumersOpen = true;
          this.#ctx.consumerDim = dim;
        }
        // Both regions repaint: the listing changes, and the gauges own the expanded state.
        this.renderConsumers();
        this.renderGauges();
        // The consumer rows carry data-scale/data-depth attributes that need promoting to
        // CSSOM, and this call path bypasses #renderSubsystem's applyStyles.
        this.#ctx.sparks.applyStyles();
      });
    }
  }

  /// The RAM gauge: effective vs. raw, with container-limit detail when the daemon reports it.
  ///
  /// "of N" spelled out in the detail because these are the HOST's totals. Inside a
  /// container the applicable limit can be far lower, and the panel must not imply otherwise.
  ///
  /// When the daemon serves effective figures (2.2/2.3: present ONLY when a cgroup limit was
  /// actually found) they are the floor an operator is flying against: 4% against a 4 GB host
  /// is uninformative inside a 512 MB container where that same 20 MB is 4% of the ceiling.
  /// Their presence, not a separate flag, is the "this is a container" signal.
  #_ramGauge(mem, num) {
    const effPct = mem && num(mem.effective_used_percent) ? mem.effective_used_percent : null;
    const pct = effPct ?? (mem && num(mem.used_percent) ? mem.used_percent : null);
    const effTotal = mem && num(mem.effective_total_bytes) ? mem.effective_total_bytes : null;
    const detail = mem
      ? effTotal
        ? `${fmt.bytePair(mem.used_bytes, effTotal)} (container limit)`
        : fmt.bytePair(mem.used_bytes, mem.total_bytes)
      : null;
    return this.#gauge("RAM", pct, detail, !!mem, null, "memory");
  }

  /// CPU facts that are not a percentage, under the gauges.
  ///
  /// Everything here answers "what is this CPU doing" with a figure the gauge cannot show. Placed
  /// under the gauges rather than above the core grid, where it read as a property of the cores.
  ///
  /// Every value is derived from data already on the page — no new endpoint, no new metric. What
  /// is deliberately NOT here: a thread count, because nothing in the API reports one.
  ///
  /// Load average and uptime live in THIS region too, as the trailing two cells. They used to
  /// form their own `load_uptime` row directly below, and the split cost a second region to
  /// state two figures that scan best in the same two-column row as the core facts. Painted
  /// together, so a load from one tick can never sit beside an uptime from another.
  renderCpuStats() {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const items = this.#_cpuStatItems(data, num);

    const el = this.#ctx.sparks.region("cpu_stats", "host-metric host-pair host-cpu-stats");
    el.dataset.figure = "host.cpu_stats";
    el.style.setProperty("--cpu-facts", items.length);
    if (!items.length) {
      el.innerHTML = "";
      el.hidden = true;
      return;
    }
    el.innerHTML = items.map(([key, value, why]) => {
      const ok = value !== EMPTY;
      return `<div class="host-pair-cell" title="${fmt.esc(why)}">`
        + `<span class="host-metric-key">${fmt.esc(key)}</span>`
        + (ok
          ? `<span class="host-metric-val">${fmt.esc(value)}</span>`
          : `<span class="host-metric-val na">${EMPTY}</span>`)
        + `</div>`;
    }).join("");
  }

  /// Builds the [key, value, tooltip] triples for the CPU stats row.
  ///
  /// Label and value are SEPARATE, matching the load avg | uptime pair: the chips read
  /// "6/8 cores busy" as one run of text, where the eye has to find the figure inside the
  /// sentence. A dim label above a bright value makes the number the thing you see first, and
  /// puts every value on the same baseline so four of them scan as a column rather than as
  /// prose. Same `.host-pair-cell` markup, so the two regions cannot drift in style.
  ///
  /// NO THREAD COUNT. Investigated properly rather than dismissed, and the numbers say a
  /// client-side or daemon-side count would be wrong on this platform:
  ///
  ///   - `sysinfo::Process::tasks()` returns `None` on everything but Linux. Not a doc caveat:
  ///     the implementation is `cfg_select!`-gated to linux/android and returns `None` otherwise.
  ///   - `kern.num_threads` on macOS is a tunable CEILING (20480 here, unchanged while a busy
  ///     loop ran), not a live count.
  ///   - Summing `proc_pidinfo(PROC_PIDTASKINFO).pti_threadnum` over every pid — the way `top`
  ///     does it — reads only 300 of 483 processes as a normal user and yields 2641 threads
  ///     against top's 3816: a stable ~30% undercount across three paired samples.
  ///   - `top` gets the true figure because `/usr/bin/top` is setuid root (`-r-sr-xr-x root
  ///     wheel`). oxmgr is not, and should not be.
  ///
  /// A figure that is silently 30% low is worse than an absent one, because it looks
  /// authoritative. Linux could report this honestly, but a metric that appears on one platform
  /// and vanishes on another needs a platform capability row and an unavailable message —
  /// `Capability::HostThreadCount` — which is a daemon change, not a client one.
  #_cpuStatItems(data, num) {
    const cpu = data.cpu;
    const ident = data.identity ?? {};
    const cores = Array.isArray(cpu?.per_core) ? cpu.per_core : [];
    const load = data.load_average;
    const upKnown = num(data.uptime_secs);
    const total = this.#ctx.consumers?.total_processes;

    // Cores carrying real work, at the same 70% the severity bands treat as warning. This is the
    // figure a global percentage hides: 50% global is eight cores half-busy or four cores pinned,
    // and those are different problems.
    const busy = cores.filter(c => num(c.usage_percent) && c.usage_percent >= 70).length;
    const hottest = cores.reduce((acc, c) =>
      (num(c.usage_percent) && c.usage_percent > acc) ? c.usage_percent : acc, -1);

    return [
      typeof total === "number"
        ? ["processes", fmt.count(total), "Processes on this host, from the consumer sampler"]
        : null,
      cores.length
        ? ["cores busy", `${busy} of ${cores.length}`,
          "Cores at or above 70%, the same threshold the severity bands use"]
        : null,
      hottest >= 0
        ? ["peak core", `${fmt.pct(hottest)}%`, "The busiest single core in this reading"]
        : null,
      num(cpu?.sample_interval_ms)
        ? ["sample", `${(cpu.sample_interval_ms / 1000).toFixed(0)}s`,
          "The interval this CPU reading was averaged over"]
        : null,
      num(ident.physical_core_count) && num(ident.logical_core_count)
        && ident.logical_core_count > ident.physical_core_count
        // Only on an SMT host: on a machine where the two counts agree this says nothing.
        ? ["physical", fmt.count(ident.physical_core_count),
          "Physical cores behind the logical count"]
        : null,
      // Load and uptime close the row's second column. `load_average` is a 1/5/15 triple —
      // the direction over time is the only thing that makes it readable, so all three stay.
      !!load
        ? ["load avg", `${load.one.toFixed(2)} · ${load.five.toFixed(2)} · ${load.fifteen.toFixed(2)}`,
          "Load average over 1 / 5 / 15 minutes"]
        : ["load avg", EMPTY, "Not reported on this platform"],
      upKnown
        ? ["uptime", fmt.dur(data.uptime_secs), "Time since this host booted"]
        : ["uptime", EMPTY, "Boot time not reported on this platform"],
    ].filter(Boolean);
  }

  /// Host storage and interfaces, one merged region.
  ///
  /// Two SSE events repaint it, the same way the gauge row and the load/uptime pair
  /// do: whichever arrives, the whole region is painted from current state, so the
  /// two sides can never show readings from different ticks.
  /// ONE entry per storage source, the deduped filesystem list.
  ///
  /// Bind-mounted files and synthesized volumes report the same capacity snapshot as their
  /// backing store — same total, used and percent — so listing each would count the same
  /// storage several times over. Measured on the demo container: /etc/hosts, /etc/hostname
  /// and /etc/resolv.conf all carry the backing filesystem's 126 GB, and the overlay root
  /// mirrors it again. One entry per snapshot; the most useful mount wins (a real directory
  /// over a pseudo file, a writable volume over a read-only snapshot), ties broken by the
  /// shortest path so the choice is stable between refreshes.
  ///
  /// Shared by `#renderStorageNet` (the rows) and the storage tile's total:
  /// the total must never add capacity that the region refuses to show.
  #uniqFilesystems() {
    const data = this.#ctx.state ?? {};
    const bySource = new Map();
    const sourceKey = (fs) => `${fs.total_bytes}|${fs.used_bytes}|${fs.used_percent}`;
    const mountRank = (fs) =>
      (fs.pseudo ? 1 : 0) + (fs.is_read_only ? 2 : 0) + (fs.is_removable ? 4 : 0);
    for (const fs of data.filesystems ?? []) {
      const key = sourceKey(fs);
      const prev = bySource.get(key);
      if (!prev) {
        bySource.set(key, fs);
        continue;
      }
      const [a, b] = [mountRank(fs), mountRank(prev)];
      if (a < b || (a === b && fs.mount_point.length < prev.mount_point.length)) {
        bySource.set(key, fs);
      }
    }
    return [...bySource.values()];
  }

  renderStorageNet() {
    const data = this.#ctx.state ?? {};
    const num = (val) => typeof val === "number" && Number.isFinite(val);
    const el = this.#ctx.sparks.region("storage_net", "host-list host-storage-net");
    el.dataset.figure = "host.storage_net";

    // ── Filesystems, most-utilised first, ONE row per storage source. ──────────
    // Deduplication lives in #uniqFilesystems: the total capacity sums the same
    // deduped list, so the two can never disagree about how much storage this host has.
    const uniqFs = this.#uniqFilesystems();

    // Block-device I/O operation rates (task 4.x). The current wire payload carries
    // no per-device counters, so the region surfaces the "unavailable" marker
    // instead of a fabricated zero. The per-row path stays keyed off a counter
    // being present, so a future daemon that adds `iops`/`iops_read`/`iops_write`
    // lights the per-row figures up without further changes here.
    const ioCountersPresent = (data.filesystems ?? []).some(
      fs => num(fs.iops) || num(fs.iops_read) || num(fs.iops_write)
    );
    const ioLine = (fs) => {
      const r = num(fs.iops_read) ? `${fmt.count(fs.iops_read)}/s` : "unavailable";
      const w = num(fs.iops_write) ? `${fmt.count(fs.iops_write)}/s` : "unavailable";
      return `<div class="host-iops">r ${r} · w ${w}</div>`;
    };

    const fsRows = uniqFs.map(fs => {
      const pct = num(fs.used_percent) ? `${fmt.pct(fs.used_percent)}%` : EMPTY;
      const used = num(fs.used_percent) ? fs.used_percent : null;
      const band = used === null ? "" : severityBand(`fs:${fs.mount_point}`, used);
      const meter = used === null
        ? ""
        : `<div class="host-row-meter"><span class="${band}" data-scale="${used / 100}"></span></div>`;
      return `<div class="host-row host-row-stacked"><span class="host-row-name">${fmt.esc(fs.mount_point)}</span>`
        + `<span class="host-row-sub">${fmt.esc(fmt.bytePair(fs.used_bytes, fs.total_bytes))}`
        + ` · <b class="${band}">${fmt.esc(pct)}${severityCue(band)}</b></span>`
        + meter + (ioCountersPresent ? ioLine(fs) : "") + `</div>`;
    }).join("");

    // Total capacity across the DEDUPED sources: the number that says how much
    // storage this host actually has, rather than how many mount points claim it.
    const fsTotal = uniqFs.reduce((sum, fs) => sum + (num(fs.total_bytes) ? fs.total_bytes : 0), 0);
    const fsPressure = uniqFs.length ? fmt.bytes(fsTotal) : "";
    const ioFigures = ioCountersPresent ? "" : `<div class="host-iops host-iops-na">I/O: unavailable</div>`;

    // Interfaces: sorted by traffic. Never-carried-a-byte interfaces stay hidden
    // behind the idle control, so the region always fits its busiest traffic.
    const all = data.network?.interfaces ?? [];
    const active = all.filter(i => i.total_received_bytes > 0 || i.total_transmitted_bytes > 0);
    const rows = (this.#ctx.netShowIdle ? all : active)
      .sort((a, b) => ((b.received_bytes ?? 0) + (b.transmitted_bytes ?? 0)) - ((a.received_bytes ?? 0) + (a.transmitted_bytes ?? 0)))
      .map(i => {
        const down = fmt.rate(i.received_bytes, i.interval_ms);
        const up = fmt.rate(i.transmitted_bytes, i.interval_ms);
        const hist = this.#ctx.netHistory.get(i.name);
        const sparks = hist && hist.rx.length >= 2
          ? `<div class="host-net-graph">${this.#ctx.sparks.spark(hist.rx, "rx")}${this.#ctx.sparks.spark(hist.tx, "tx")}`
            + `<span class="host-net-scale">${fmt.esc(fmt.bytes(Math.max(...hist.rx, ...hist.tx, 10 * 1024)))}/s</span></div>`
          : "";
        return `<div class="host-net" title="${fmt.esc(`${i.name} — ${down ?? "?"} down, ${up ?? "?"} up`)}">`
          + `<div class="host-net-head"><span class="host-net-name">${fmt.esc(i.name)}</span>`
          + `<span class="host-net-rate"><b class="rx">↓</b>${fmt.esc(down ?? EMPTY)}<b class="tx">↑</b>${fmt.esc(up ?? EMPTY)}</span></div>`
          + sparks + `<span class="host-net-total">${fmt.esc(fmt.bytes(i.total_received_bytes))} down · ${fmt.esc(fmt.bytes(i.total_transmitted_bytes))} up · lifetime</span></div>`;
      }).join("");

    const fsEmpty = !fsRows;
    const netEmpty = !rows;

    if (fsEmpty && netEmpty) {
      el.innerHTML = "";
      el.hidden = true;
      return;
    }
    el.hidden = false;

    const idleCount = all.length - active.length;
    const counter = idleCount > 0 ? (this.#ctx.netShowIdle ? `${fmt.count(all.length)} of ${fmt.count(all.length)}` : `${fmt.count(active.length)} of ${fmt.count(all.length)}`) : "";
    const idleBtn = idleCount > 0 ? `<button type="button" class="host-net-more" data-net-idle aria-pressed="${this.#ctx.netShowIdle}">${this.#ctx.netShowIdle ? `hide ${idleCount} idle` : `show ${idleCount} idle`}</button>` : "";

    // No region-level heading: the two slot sub-headings ("storages", "networks")
    // already name the content, and a third label above them would repeat both.
    el.innerHTML = `<div class="host-storage-grid">`
      + (fsEmpty ? `<div class="host-storage-slot host-storage-fs" hidden></div>` : `<div class="host-storage-slot host-storage-fs">`
        + `<div class="host-consumers-head"><h3>storages</h3>`
        + (fsPressure ? `<div class="host-consumers-pressure">${fsPressure}</div>` : "") + `</div>`
        + `<div class="host-fs-body">${fsRows}${ioFigures}</div></div>`)
        + (netEmpty ? `<div class="host-storage-slot host-storage-net" hidden></div>` : `<div class="host-storage-slot host-storage-net">`
        + `<div class="host-consumers-head"><h3>networks</h3>`
        + (counter ? `<div class="host-consumers-pressure">${counter}</div>` : "") + `</div>`
        + `<div class="host-net-body" id="host-net-body">${rows}${idleBtn}</div></div>`)
      + `</div>`;

    for (const b of el.querySelectorAll("[data-net-idle]")) b.addEventListener("click", () => { this.#ctx.netShowIdle = !this.#ctx.netShowIdle; this.renderStorageNet(); });
  }

  /// Appends the current rates to each interface's ring, and prunes vanished interfaces.
  ///
  /// The API reports an instantaneous rate and keeps no history, so the trend only exists if
  /// the client accumulates it. Bounded at `SPARK_SAMPLES`, which at the 10s I/O refresh is
  /// about four minutes — long enough to show a burst, short enough that a dashboard left open
  /// overnight holds 24 numbers per interface rather than 8,640.
  ///
  /// A reading whose interval is unusable is skipped rather than recorded as 0: a zero would
  /// draw a trough that says "no traffic", which is a different claim from "no measurement".
  recordNetHistory(interfaces) {
    const seen = new Set();
    for (const iface of interfaces) {
      const ms = Number(iface.interval_ms);
      if (!Number.isFinite(ms) || ms <= 0) continue;
      seen.add(iface.name);
      let hist = this.#ctx.netHistory.get(iface.name);
      if (!hist) {
        hist = { rx: [], tx: [] };
        this.#ctx.netHistory.set(iface.name, hist);
      }
      const rx = (Number(iface.received_bytes) * 1000) / ms;
      const tx = (Number(iface.transmitted_bytes) * 1000) / ms;
      hist.rx.push(Number.isFinite(rx) ? rx : 0);
      hist.tx.push(Number.isFinite(tx) ? tx : 0);
      if (hist.rx.length > this.#ctx.sparkSamples) hist.rx.shift();
      if (hist.tx.length > this.#ctx.sparkSamples) hist.tx.shift();
    }
    // An interface that is gone takes its history with it, so a name reused later by a
    // different device cannot inherit a stranger's trace.
    for (const name of this.#ctx.netHistory.keys()) {
      if (!seen.has(name)) this.#ctx.netHistory.delete(name);
    }
  }

  /// Per-core CPU rows, btop's core box in a 300px column.
  ///
  /// btop gives each core `C<n> <sparkline> <pct>%`, and degrades under pressure by narrowing
  /// the graph (10 cells → 5 → none) and then splitting into columns — and on a short terminal
  /// it silently stops drawing cores. That last part is the one behaviour worth NOT copying: a
  /// panel that omits cores without saying so is indistinguishable from a host with fewer
  /// cores. So this scrolls instead, and every core is always present.
  ///
  /// The bar is instantaneous, not history. btop's per-core graph is a time series, but at 5-10
  /// cells that resolution says little, and the thing an operator actually reads from a core
  /// list is the SHAPE of the current distribution: one core pinned at 100% while seven idle is
  /// a single-threaded bottleneck, and all eight at 60% is honest saturation. A row of
  /// instantaneous bars shows that at a glance; eight tiny sparklines do not.
  ///
  /// Column count. Two constraints fight: the container (a 150px floor per track, like the CSS
  /// auto-fill it replaces) and even rows (8 cores across 6 columns leaves a ragged 2). When a
  /// divisor of the core count exists within the width cap, it wins — 8 → 2 rows × 4, 16 → 4 ×
  /// 4 — and only when no divisor exists does the grid degrade to the width cap plus one
  /// remainder row. Purely arithmetic, same inputs → same verdict.
  #coreColumns(count, width) {
    const byWidth = Math.max(2, Math.floor((width + 14) / 164));
    const cap = Math.min(6, byWidth);
    for (let c = cap; c >= 2; c--) {
      if (count % c === 0) return c;
    }
    return Math.min(count, cap);
  }

  renderCores() {
    const cores = this.#ctx.state?.cpu?.per_core;
    const el = this.#ctx.sparks.region("cores", "host-cores");
    if (!Array.isArray(cores) || cores.length === 0) {
      // Absent rather than empty: per-core is opt-out via OXMGR_HOST_CPU_PER_CORE, and a
      // disabled feature must not render as a host with no cores.
      el.innerHTML = "";
      el.hidden = true;
      return;
    }
    el.hidden = false;
    el.dataset.figure = "host.cores";

    // Even rows need the panel's real width, not a viewport guess. Read after unhiding so the
    // element has layout; repaints every 10s self-correct a stale count after a resize.
    const cols = this.#coreColumns(cores.length, el.clientWidth);
    el.style.setProperty("--core-cols", cols);

    // Frequency is reported per core but is almost always identical across them, so it is
    // stated once in the header rather than repeated on every row.
    const freqs = cores.map(c => c.frequency_mhz).filter(f => typeof f === "number" && f > 0);
    const freq = freqs.length
      ? (Math.max(...freqs) >= 1000
        ? `${(Math.max(...freqs) / 1000).toFixed(2)} GHz`
        : `${Math.max(...freqs)} MHz`)
      : null;

    const rows = cores.map((core, idx) => {
      const pct = Number(core.usage_percent);
      const known = Number.isFinite(pct);
      const value = known ? Math.max(0, Math.min(100, pct)) : 0;
      const cls = severityBand(`core:${idx}`, value);
      // The label is the core's ordinal, not its platform name: sysinfo reports "1".."N" on
      // this host but "cpu0" elsewhere, and a fixed-width ordinal keeps the column aligned.
      const label = `C${idx}`;
      const shown = known ? `${Math.round(value)}` : EMPTY;
      return `<div class="host-core ${cls}" title="${fmt.esc(`${label}: ${known ? fmt.pct(value) + "%" : "no reading"}`)}">`
        + `<span class="host-core-id">${fmt.esc(label)}</span>`
        + `<div class="host-core-track" role="img"`
        + ` aria-label="${fmt.esc(`core ${idx} at ${known ? Math.round(value) + " percent" : "no reading"}`)}">`
        + `<span data-scale="${value / 100}"></span></div>`
        + `<span class="host-core-pct">${shown}<i>%</i></span></div>`;
    }).join("");

    // Architecture belongs here rather than in the header: it describes the cores below it, and
    // it was previously stated beside a "N cores" count that this header repeats as "N logical".
    // One count, one place — `arm64 · 8 logical · 4.05 GHz` reads as a single fact about the CPU.
    const id = this.#ctx.state?.identity ?? {};
    const meta = [
      id.cpu_arch || null,
      `${cores.length} logical`,
      freq,
    ].filter(Boolean).join(" · ");

    // What the OS is actually running, stated with the CPU rather than beside the consumer
    // listing or the uptime. Both figures are CPU facts: how many processes exist, and how many
    // threads are queued to run on these cores.
    //
    // The process total comes from the consumer sampler's payload, which is the only place the
    // daemon reports it — so this region repaints when that poll lands, not only on a snapshot.
    // The process count and the other CPU facts live in the `cpu_stats` region under the gauges,
    // not here. Above the core grid they read as a property of the eight cores below them.
    el.innerHTML = `<div class="host-cores-head"><h3>cores</h3>`
      + `<span class="host-cores-meta">${fmt.esc(meta)}</span>`
      + `</div><div class="host-cores-grid">${rows}</div>`;
  }

  /// Host top consumers, managed and unmanaged.
  ///
  /// Presented as its own region below the managed figures and visually separated, because the
  /// whole point is attribution: an operator seeing a managed process at 99% CPU needs to know
  /// whether it is the cause or a victim of contention, and that answer is only useful if the
  /// unmanaged processes beside it are unmistakably NOT oxmgr's.
  ///
  /// No management controls are rendered here at all — not disabled ones. A greyed-out stop button
  /// on someone else's database would invite the click; an absent one cannot.
  renderConsumers() {
    const el = this.#ctx.sparks.region("consumers", "host-list host-consumers");
    el.dataset.figure = "host.consumers";
    const data = this.#ctx.consumers;

    if (data === null) {
      // Unavailable, not empty. "Sampling is off" and "this host has no processes" are different
      // claims and the second is never true, so an empty table here would be a lie.
      // Collapsed like the populated case. "Sampling is off" is worth being able to find, but
      // it is not worth a permanent row in the panel — the heading says it is there.
      el.innerHTML = this.#consumersHead()
        + `<div class="host-consumers-body" id="host-consumers-body"`
        + `${this.#ctx.consumersOpen ? "" : " hidden"}>`
        + `<div class="host-consumers-na">${EMPTY} sampling disabled or not yet sampled</div>`
        + `</div>`;
      el.hidden = false;
      this.#wireConsumerControls(el);
      return;
    }
    if (!data) {
      el.innerHTML = "";
      el.hidden = true;
      return;
    }

    const dim = this.#ctx.consumerDim;
    // Four modes. The flat listings carry each consumer's OWN figures; the tree listings carry
    // the attributed totals (self + descendants), so "which process tree is responsible for
    // this CPU" is answered by the tree view and "which single process" by the flat one.
    const isTree = dim === "cpu-tree" || dim === "memory-tree";
    const isCpu = dim === "cpu" || dim === "cpu-tree";
    const list = isTree
      ? (isCpu ? data.by_cpu_trees : data.by_memory_trees) ?? []
      : (isCpu ? data.by_cpu : data.by_memory) ?? [];
    if (list.length === 0) {
      el.innerHTML = "";
      el.hidden = true;
      return;
    }

    const body = isTree
      ? list.map(t => this.#treeRows(t, dim, 0)).join("")
      : list.map(c => {
      // The managed name is what an operator acts on; the platform name is the executable. Both
      // are shown for a managed consumer because the executable is how it appears in `top`.
      const label = c.managed && c.managed_name ? c.managed_name : c.name;
      const figure = isCpu
        ? `${fmt.pct(c.cpu_percent)}%`
        : fmt.bytes(c.memory_bytes);
      const band = isCpu ? severityBand(null, c.cpu_percent) : "";
      const sub = c.managed
        ? `pid ${c.pid} · ${fmt.esc(c.name)}`
        : `pid ${c.pid}${c.user ? ` · uid ${fmt.esc(c.user)}` : ""}`;
      return `<div class="host-consumer${c.managed ? " is-managed" : ""}"`
        + ` title="${fmt.esc(`${label} — ${fmt.pct(c.cpu_percent)}% cpu, ${fmt.bytes(c.memory_bytes)}`)}">`
        + `<span class="host-consumer-name">${fmt.esc(label)}</span>`
        + `<span class="host-consumer-val ${band}">${fmt.esc(figure)}${severityCue(band)}</span>`
        + `<span class="host-consumer-sub">${sub}</span>`
        + `</div>`;
    }).join("");

    // The host pressure line that used to sit here is gone: it restated the CPU and RAM gauges
    // now directly above this region. The context it provided is not lost — the gauges ARE the
    // context, and they are closer to the rows than the old line was.

    // Everything below the heading is wrapped in one collapsible body. Hidden with the `hidden`
    // attribute rather than `display: none` in a class, so the rows are removed from the
    // accessibility tree and from tab order too — a collapsed listing must not leave eight
    // focusable rows behind it.
    el.innerHTML = this.#consumersHead()
      + `<div class="host-consumers-body" id="host-consumers-body"`
      + `${this.#ctx.consumersOpen ? "" : " hidden"}>`
      + `<div class="host-consumers-toggle" role="group" aria-label="Choose consumer view">`
      + this.#consumerToggleButton(dim, "cpu", "cpu")
      + this.#consumerToggleButton(dim, "cpu-tree", "cpu tree")
      + this.#consumerToggleButton(dim, "memory", "mem")
      + this.#consumerToggleButton(dim, "memory-tree", "mem tree")
      + `</div>`
      + body
      + (data.command_lines_included
        // Stated on the surface, not only in the log: an operator looking at the page should see
        // that it is exposing more than its default.
        ? `<div class="host-consumers-note">command lines enabled</div>` : "")
      + `</div>`;
    el.hidden = false;
    this.#wireConsumerControls(el);
  }

  /// One mode toggle: a real button with `aria-pressed`, so the view in use is announced rather
  /// than being a visual-only state.
  #consumerToggleButton(current, dim, label) {
    const active = current === dim;
    return `<button type="button" data-dim="${dim}" class="${active ? "active" : ""}"`
      + ` aria-pressed="${active}">${label}</button>`;
  }

  /// One tree node and its visible subtree, recursively.
  ///
  /// Each row carries BOTH figures: the tree total in the value column (the number the view is
  /// sorted by) and the node's own figure in the sub line — the operator asked for "total for
  /// the tree AND single process", so collapsing one into the other would hide half the answer.
  /// Children are indented and a managed consumer keeps its name emphasis: the managed
  /// distinction is the most consequential fact in the list, tree or flat.
  #treeRows(node, dim, depth) {
    const isCpu = dim === "cpu-tree";
    const label = node.managed && node.managed_name ? node.managed_name : node.name;
    const treeFigure = isCpu
      ? `${fmt.pct(node.tree_cpu_percent)}%`
      : fmt.bytes(node.tree_memory_bytes);
    const ownFigure = isCpu
      ? `${fmt.pct(node.cpu_percent)}%`
      : fmt.bytes(node.memory_bytes);
    const band = isCpu ? severityBand(null, node.tree_cpu_percent) : "";
    const collapsed = this.#ctx.treeCollapsed.has(node.pid);
    const hasChildren = node.children && node.children.length > 0;
    // The caret is the expand control; absent (a leaf) it still reserves its slot so the grid
    // stays aligned. Leaf rows have no control at all — nothing to expand.
    const caret = hasChildren
      ? `<button type="button" class="host-consumer-caret" data-tree-toggle="${node.pid}"`
        + ` aria-expanded="${!collapsed}" aria-label="${collapsed ? "expand" : "collapse"} ${fmt.esc(label)}">`
        + `<span class="host-consumer-caret-glyph" aria-hidden="true"></span></button>`
      : `<span class="host-consumer-caret is-leaf"></span>`;
    const sub = `pid ${node.pid} · own ${ownFigure}`
      + `${node.managed ? ` · ${fmt.esc(node.name)}` : ""}`
      + (node.truncated ? ` · truncated` : "");
    let html = `<div class="host-consumer is-tree${node.managed ? " is-managed" : ""}"`
      + ` data-depth="${depth}"`
      + ` title="${fmt.esc(`${label} — tree ${treeFigure}, own ${ownFigure}, pid ${node.pid}`)}">`
      + caret
      + `<span class="host-consumer-name">${fmt.esc(label)}</span>`
      + `<span class="host-consumer-val tree ${band}">${fmt.esc(treeFigure)}${severityCue(band)}</span>`
      + `<span class="host-consumer-sub">${sub}${node.truncated ? ` <span class="host-consumer-truncated" title="children omitted: node budget reached">…</span>` : ""}</span>`
      + `</div>`;
    if (hasChildren && !collapsed) {
      for (const child of node.children) {
        html += this.#treeRows(child, dim, depth + 1);
      }
    }
    return html;
  }

  /// The consumers heading: a disclosure button, with host pressure kept visible.
  ///
  /// The pressure line stays OUTSIDE the collapsible body deliberately. The spec requires the
  /// listing to be read against host load, and a collapsed panel that hid the load would leave
  /// the reopened listing without the context that makes a 40% consumer meaningful.
  #consumersHead() {
    // NO heading and NO disclosure control of its own.
    //
    // The CPU and RAM gauges are the only way in and out of this listing now. Two controls for
    // one region meant an operator could open it from the gauge and close it from a heading that
    // left the gauge still marked expanded — two places holding one piece of state. The gauges
    // win because they are where the question starts: the listing exists to answer "who is using
    // this CPU", and the CPU reading is the thing being asked about.
    //
    // The `.host-consumers-body` it wraps keeps its `id`, so `aria-controls` on both gauges still
    // resolves and the relationship is announced.
    return "";
  }

  /// Binds the disclosure and the sort buttons.
  ///
  /// Bound after every paint because the region's innerHTML is replaced wholesale, so the
  /// previous listeners went with the nodes that carried them.
  #wireConsumerControls(el) {
    for (const button of el.querySelectorAll("[data-dim]")) {
      button.addEventListener("click", () => {
        this.#ctx.consumerDim = button.dataset.dim;
        this.renderConsumers();
        this.renderGauges();
        // The consumer rows carry data-scale/data-depth attributes that need promoting to
        // CSSOM, and this call path bypasses #renderSubsystem's applyStyles.
        this.#ctx.sparks.applyStyles();
      });
    }
    // Expand/collapse a tree node. State lives in the panel, so a re-render (a new sample, a
    // mode switch) does not reset what the operator folded away.
    for (const caret of el.querySelectorAll("[data-tree-toggle]")) {
      caret.addEventListener("click", () => {
        const pid = Number(caret.dataset.treeToggle);
        if (this.#ctx.treeCollapsed.has(pid)) this.#ctx.treeCollapsed.delete(pid);
        else this.#ctx.treeCollapsed.add(pid);
        this.renderConsumers();
        // Tree depth is a data-depth attribute; promote it to CSSOM for the indent.
        this.#ctx.sparks.applyStyles();
      });
    }
  }
}
