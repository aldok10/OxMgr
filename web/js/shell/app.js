import { CONFIRM_ACTS, EVENTS, EVENT_STATUS, INT, THRESHOLD_ANNOUNCE_MS, clamp, isFilterCollapsible, isPanelFullViewport, sel } from "../core/const.js";
import { applySeverityConfig } from "../core/severity.js";
import { Bus } from "../core/bus.js";
import { Store } from "../core/store.js";
import { Api, apiRequest } from "../core/api.js";
import { Spin } from "../core/spin.js";
import { announce } from "../core/announce.js";
import { fmt } from "../format/fmt.js";
import { applyTailConfig } from "../log/LogView.js";
import { HostPanel } from "../host/shell.js";
import { LogModal } from "../modals/log-modal.js";
import { DetailModal } from "../modals/detail-modal.js";
import { ConfirmModal } from "../modals/confirm-modal.js";
import { network } from "./network.js";
import { ADVISORY_POLL_MS, FINDING_POLL_MS, Stats, TYPICAL_POLL_MS, advisoryState, findingEngine, findingState, typicalState } from "./stats.js";
import { Table } from "./table.js";
import { GroupSel, ThemeSwitch } from "./controls.js";

// Collect the optional fields a process event may carry, skipping absent ones
// Collect the optional fields a process event may carry, skipping absent ones
const eventPatch = (event) => {
  const patch = {};
  if (event.process?.pid) patch.pid = event.process.pid;
  if (event.data?.exit_code !== undefined) patch.last_exit_code = event.data.exit_code;
  if (event.data?.restart_count !== undefined) patch.restart_count = event.data.restart_count;
  return patch;
};

// Show the environment label in the header, tinted with the configured color
const applyLabel = (label, color) => {
  if (!label) return;
  const elem = sel("#env-label");
  elem.textContent = label;
  elem.classList.add("visible");
  if (!color) return;
  elem.style.color = color;
  if (color.startsWith("#")) {
    elem.style.borderColor = `${color}4d`;
    elem.style.backgroundColor = `${color}26`;
    return;
  }
  elem.style.borderColor = color.replace(")", ", 0.3)").replace("rgb(", "rgba(");
  elem.style.backgroundColor = color.replace(")", ", 0.15)").replace("rgb(", "rgba(");
};

export class App {
  /// Whether the filter panel (phone layout) is open. Closed by default: see the
  /// constructor comment — the collapse exists only below 700px, so this state is
  /// irrelevant on the desktop where the panel always flows in the toolbar row.
  #filtersOpen = false;
  /// Releases this component's bus subscriptions and DOM listeners in one call.
  /// The app shell lives for the page's lifetime, so it is never torn down today;
  /// the controller exists so teardown (section 5) stays a single `abort()`.
  #life = new AbortController();
  #chromeObs = null;
  #intervals = [];
  #bufferedEvents = [];
  _signal() { return { signal: this.#life.signal }; }
  destroy() {
    this.#life.abort();
    this.#chromeObs?.disconnect();
    this.#intervals.forEach(id => clearInterval(id));
    this.#intervals = [];
    this.api.stopUnified();
    this.api.stopLog();
    this.hostPanel.destroy();
    this.logModal.destroy();
    this.detailModal.destroy();
    this.confirmModal.destroy();
    this.stats.destroy();
    this.themeSwitch.destroy();
  }

  constructor() {
    this.bus = new Bus();
    this.store = new Store();
    this.api = new Api(this.bus);
    this.spin = new Spin();
    // The banner announces its own transitions, because it is the only surface that
    // reports the daemon being gone and an operator who cannot see it otherwise learns
    // nothing. Two things make this less trivial than it looks:
    //
    // 1. `hide()` runs on EVERY `PROC_DATA` tick (see the handler below), so announcing
    //    unconditionally there would fire a recovery message several times a second —
    //    exactly the per-tick chatter `dashboard-status-messaging` forbids. Hence the
    //    transition guard: announce only when the banner was actually showing.
    // 2. `show()` has two callers with different urgency. Losing the daemon is
    //    assertive; a failed action is polite and is announced by its own handler. So
    //    the reason is passed in, and only a "daemon" banner announces here — otherwise
    //    an action error would be followed by a bogus "connection restored" on the next
    //    tick.
    this.banner = {
      elem: sel("#error-banner"),
      reason: null,
      show(msg, reason = "action") {
        this.elem.style.display = "block";
        this.elem.textContent = msg;
        if (reason === "daemon" && this.reason !== "daemon") announce.alert(msg);
        this.reason = reason;
      },
      hide(reason = null) {
        if (reason && this.reason !== reason) return;
        const wasDaemonDown = this.reason === "daemon";
        this.elem.style.display = "none";
        this.reason = null;
        if (wasDaemonDown) announce.alert("Connection to the daemon restored.");
      },
    };
    this.stats = new Stats(sel("#stats"), this.store, this.bus);
    this.table = new Table(sel("#tbody"), sel("#empty-state"), this.store, this.bus);
    this.grpSel = new GroupSel(sel("#group-select"), this.store);
    // Collapsible filter panel (phone layout). Closed by default: on a phone the toolbar
    // is a bottom nav and the filters are tucked away until asked for; on the desktop the
    // CSS disables the collapse entirely and `open` is irrelevant because the panel flows
    // in the toolbar row either way.
    this.filterPanel = sel("#filter-panel");
    this.filterToggle = sel("#filter-toggle");
    this.filterBadge = sel("#filter-badge");
    this.#filtersOpen = false;
    const logOverlay = sel("#log-overlay");
    const detailOverlay = sel("#detail-overlay");
    this.logModal = new LogModal(logOverlay, sel(".log-panel", logOverlay), this.api, this.bus, this.spin);
    this.detailModal = new DetailModal(detailOverlay, sel(".detail-panel", detailOverlay), this.store);
    this.confirmModal = new ConfirmModal(sel("#confirm-overlay"));
    // Its own poll, not a subscriber to the process stream: host collection runs on
    // separate intervals in the daemon, so sharing the process cadence would misreport
    // how fresh these figures are.
    this.hostPanel = new HostPanel();
    this.themeSwitch = new ThemeSwitch(document.querySelector("#theme-switch"));
    this.hint = sel("#updated-hint");
    this.spinEl = sel("#refresh-spinner");
    this.#bind();
    this.#trackChromeHeights();
  }

  /// Publishes the measured height of each fixed bottom strip as a CSS custom property.
  ///
  /// Both the control bar and the filter chip row are `position: fixed`, so they take no space
  /// in flow and the last process card would sit behind them. The clearance has to be the
  /// bars' real height, not a constant: the control bar is one row at 700px and two below
  /// 520px, and the chip row's height depends on the chip font metrics. A hardcoded value was
  /// wrong in one of those states either way — it is what left a card trapped behind the bar
  /// earlier in this work.
  ///
  /// `ResizeObserver` rather than a resize listener: the bars change height when their content
  /// wraps, which a viewport resize event does not always accompany.
  #trackChromeHeights() {
    const toolbar = sel(".toolbar");
    const root = document.documentElement;

    const publish = () => {
      // Zero when the element is not fixed (desktop), so the property falls back to
      // contributing nothing rather than reserving space that is already in flow.
      const fixedHeight = (el) =>
        getComputedStyle(el).position === "fixed"
          ? Math.round(el.getBoundingClientRect().height)
          : 0;
      // ONE height now, not two. `--stats-bar-h` is gone because `.stats` moved inside
      // `.toolbar`: the chips are part of the element being measured, so the toolbar's own
      // height already includes their row. Publishing a second property for a nested element
      // would double-count the same pixels.
      root.style.setProperty("--bottom-bar-h", `${fixedHeight(toolbar)}px`);
    };

    publish();
    if (typeof ResizeObserver === "function") {
      this.#chromeObs = new ResizeObserver(publish);
      // Observing the toolbar alone is enough: it is the fixed element, and the chips
      // wrapping onto a second line changes ITS height, which fires this.
      this.#chromeObs.observe(toolbar);
    }
    // A width change can flip an element between fixed and static without altering its box,
    // which a ResizeObserver would not report.
    window.addEventListener("resize", publish, this._signal());
  }
  #bind() {
    const sig = { signal: this.#life.signal };
    this.bus.on(EVENTS.PROC_START, () => this.spin.show(this.spinEl, "proc"), sig);
    this.bus.on(EVENTS.PROC_DATA, procs => {
      console.log('PROC_DATA received:', procs);
      this.spin.hide(this.spinEl, "proc");
      this.store.set("procs", procs);
      this.banner.hide("daemon");
      this.grpSel.render();
      this.#render();
      this.hint.textContent = `updated ${fmt.time()}`;

      for (const event of this.#bufferedEvents) {
        this.#onProcessEvent(event);
      }
      this.#bufferedEvents = [];
    }, sig);
    this.bus.on(EVENTS.PROC_ERR, () => this.spin.hide(this.spinEl, "proc"), sig);
    // Action outcomes are announced on BOTH paths. A failure that only paints a banner
    // is displayed, not reported: `dashboard-interaction-safety` draws a distinction
    // between quiet aborts and reported failures, and that distinction only means
    // something if the reported half actually reaches the operator.
    //
    // Polite, not assertive: the operator asked for this, so it queues behind whatever
    // they are reading rather than interrupting. Only losing the daemon interrupts.
    this.bus.on(EVENTS.ACT_DONE, ({ msg }) => {
      this.hint.textContent = msg;
      announce.status(msg);
    }, sig);
    this.bus.on(EVENTS.ACT_ERR, ({ target, act, err }) => {
      const msg = `Action ${act} on ${target} failed: ${err}`;
      this.banner.show(msg);
      announce.status(msg);
    }, sig);
    this.bus.on(EVENTS.FILTER_CHANGED, () => this.#render(), sig);

    // Real-time process status updates via BusEvent
    this.bus.on(EVENTS.EVENT_PROCESS, event => this.#onProcessEvent(event), sig);

    sel("#tbody").addEventListener("click", evt => this.#onTableClick(evt), this._signal());
    // Sort headers: delegated on the table so the click reaches from any <th> down to
    // the .sort-btn. Shift+click appends a secondary sort key.
    sel("#table").addEventListener("click", (e) => {
      const btn = e.target.closest(".sort-btn");
      if (!btn) return;
      const key = btn.dataset.sortKey;
      if (!key) return;
      this.store.toggleSort(key, e.shiftKey);
      this.#updateSortUI();
      this.table.render();
    }, this._signal());
    // Panel lifecycle buttons go through the same dispatch as the row buttons, so a Stop
    // pressed here gets the identical confirmation. One path, one set of guarantees.
    sel("#detail-actions").addEventListener("click", evt => {
      const btn = evt.target.closest("button");
      if (btn) this.#runAction(btn.dataset);
    }, this._signal());

    sel("#refresh-btn").addEventListener("click", () => this.#stream(), this._signal());
    sel("#stop-all-btn").addEventListener("click", () => {
      this.confirmModal.confirm("stop", "all").then(ok => { if (ok) this.api.action("all", "stop"); });
    }, this._signal());
    sel("#restart-all-btn").addEventListener("click", () => {
      this.confirmModal.confirm("restart", "all").then(ok => { if (ok) this.api.action("all", "restart"); });
    }, this._signal());
    sel("#search-input").addEventListener("input", evt => { this.store.set("search", evt.target.value); this.table.render(); this.#updateFilterBadge(); }, this._signal());
    sel("#group-select").addEventListener("change", evt => { this.store.set("group", evt.target.value); this.table.render(); this.#updateFilterBadge(); }, this._signal());
    // Filter toggle: only meaningful where the collapse exists (phone layout). On the
    // desktop the button is hidden by CSS, so this listener never fires there.
    sel("#filter-toggle").addEventListener("click", () => this.#toggleFilters(), this._signal());
    // Outside-tap closes a phone filter panel. The collapse exists only below 700px (the
    // CSS question, mirrored in const.js), and once the panel is open the table underneath
    // is the natural "I am done filtering" gesture — tapping it should get the list back,
    // not leave the panel floating over it. The toggle itself is excluded so a deliberate
    // second tap (or the open tap, which lands while the panel is still closed) does not
    // double-fire.
    document.addEventListener("click", evt => {
      if (!this.#filtersOpen || !isFilterCollapsible()) return;
      if (evt.target.closest("#filter-panel, #filter-toggle")) return;
      this.#toggleFilters();
    }, this._signal());

    // Backdrop-tap dismissal, but only while a backdrop exists. At phone width the panel
    // fills the viewport, so any tap that reaches the overlay is a tap on the panel's own
    // edge — dismissing there would close the log the operator just opened. The explicit
    // Close button and Escape still work at every width.
    [sel("#log-overlay"), sel("#detail-overlay")].forEach(overlay => overlay.addEventListener("click", evt => {
      if (evt.target === overlay && !isPanelFullViewport()) this.#closeAll();
    }, this._signal()));
    // NO global Escape handler. It used to live here as
    //   document.addEventListener("keydown", e => { if (e.key === "Escape") this.#closeAll(); })
    // and it was the bug: `#closeAll()` enumerated the log and detail modals by hand, so
    // the destructive confirm prompt — added later — was silently undismissable. A
    // hand-maintained list of dismissable dialogs is a list something will be missing from.
    //
    // Each dialog now handles its own Escape, because `showModal()` gives it one for free.
    // `Modal._bindNativeClose()` routes that native close back into the subclass's
    // `close()`, so teardown still runs; see the comment there for why that indirection is
    // load-bearing rather than ceremonial.
    // Reconnecting a stream that is already live throws away a working
    // connection and loses a couple of seconds of updates on every alt-tab.
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden && !this.api.unifiedLive) this.#stream();
    }, this._signal());
    window.addEventListener("beforeunload", () => {
      this.destroy();
    }, this._signal());
  }
  #closeAll() { this.logModal.close(); this.detailModal.close(); }
  #render() {
    this.stats.render();
    this.table.render();
    // Badge reflects the filter state, so it must repaint on the same cadence the chips
    // do. The chips emit `filter:changed` -> this method; search and group update it
    // directly in their own handlers because they bypass the bus.
    this.#updateFilterBadge();
  }
  /// Number of active filter dimensions: status chip, group select, search term.
  /// Shown on the Filters toggle so a collapsed panel (phone) still announces that the
  /// list is narrowed — otherwise "why is only half the table here" goes unanswered.
  #updateFilterBadge() {
    const n = (this.store.get("status") ? 1 : 0)
      + (this.store.get("group") ? 1 : 0)
      + ((this.store.get("search") ?? "").trim() ? 1 : 0);
    this.filterBadge.hidden = n === 0;
    this.filterBadge.textContent = n;
  }
  /// Sync every .sort-btn's `data-dir` and `data-sort-order` attributes with the
  /// store's current sort state. One pass over all buttons — the DOM has ~8 sort
  /// headers — keeps the visual indicators (direction arrow, priority badge) in
  /// step with the data after every toggle.
  #updateSortUI() {
    const sort = this.store.get("sort");
    const posMap = new Map(sort.map((s, i) => [s.key, i]));
    sel("#table").querySelectorAll(".sort-btn").forEach(btn => {
      const key = btn.dataset.sortKey;
      const entry = sort.find(s => s.key === key);
      if (entry) {
        btn.setAttribute("data-dir", String(entry.dir));
        const order = posMap.get(key);
        // Only show the priority badge when there are 2+ sort keys.
        if (sort.length > 1) {
          btn.setAttribute("data-sort-order", String(order + 1));
        } else {
          btn.removeAttribute("data-sort-order");
        }
      } else {
        btn.removeAttribute("data-dir");
        btn.removeAttribute("data-sort-order");
      }
    });
  }
  /// Opens or closes the filter panel (phone layout). The toolbar is fixed at the bottom
  /// and the panel lives inside it, so its height change is picked up by the same
  /// ResizeObserver that publishes `--bottom-bar-h` — the content clearance follows
  /// automatically without a second measurement.
  #toggleFilters() {
    this.#filtersOpen = !this.#filtersOpen;
    this.filterPanel.classList.toggle("open", this.#filtersOpen);
    this.filterToggle.setAttribute("aria-expanded", String(this.#filtersOpen));
  }
  #stream() { this.api.unifiedStream(this.store.get("interval")); }
  // Apply a status update from a BusEvent onto the matching process in the store
  #onProcessEvent(event) {
    if (!this.store.get("procsLoaded")) {
      this.#bufferedEvents.push(event);
      return;
    }
    const name = event.process?.name;
    const newStatus = EVENT_STATUS[event.event];
    if (!name || !newStatus) return;
    const procs = this.store.get("procs");
    const idx = procs.findIndex(proc => proc.name === name);
    if (idx === -1 || procs[idx].status === newStatus) return;
    procs[idx] = { ...procs[idx], status: newStatus, ...eventPatch(event) };
    this.store.set("procs", [...procs]);
    this.#render();
    this.hint.textContent = `${name}: ${newStatus} · ${fmt.time()}`;
  }
  // Route a click inside the table to the right target: error link, action button, or row
  #onTableClick(evt) {
    const detailLink = evt.target.closest(".error-detail-link");
    if (detailLink) {
      evt.preventDefault();
      this.detailModal.show(detailLink.dataset.name);
      return;
    }
    // Advisory marker opens detail, not logs — its tooltip is unreachable on touch
    // so the tap must lead to the reason. Falling through would have opened logs.
    const flag = evt.target.closest(".advisory-flag");
    if (flag) {
      evt.preventDefault();
      const row = flag.closest("tr[data-name]");
      if (row) this.detailModal.show(row.dataset.name);
      return;
    }
    // Cluster marker: same reasoning as the advisory marker — on touch its tooltip
    // is unreachable, and the detail panel is where requested vs observed is
    // spelled out in full.
    const clusterFlag = evt.target.closest(".cluster-flag");
    if (clusterFlag) {
      evt.preventDefault();
      const row = clusterFlag.closest("tr[data-name]");
      if (row) this.detailModal.show(row.dataset.name);
      return;
    }
    // Expansion control (§D7): handled here in the same delegated listener, so the
    // expansion adds NO new listener and reuses the App's AbortController teardown.
    const expandBtn = evt.target.closest(".expand-btn");
    if (expandBtn) {
      evt.preventDefault();
      this.table.toggle(expandBtn.dataset.expand);
      return;
    }
    const btn = evt.target.closest("button");
    if (btn) { this.#runAction(btn.dataset); return; }
    const row = evt.target.closest("tr[data-name]");
    if (row) this.logModal.show(row.dataset.name);
  }
  #runAction({ target, action }) {
    if (action === "logs") this.logModal.show(target);
    else if (action === "detail") this.detailModal.show(target);
    else if (CONFIRM_ACTS.includes(action)) this.#confirmAction(action, target);
  }
  #confirmAction(action, target) {
    this.confirmModal.confirm(action, target).then(ok => { if (ok) this.api.action(target, action); });
  }
  /// Polls configuration advisories.
  ///
  /// Far slower than the process stream: advisories are derived from configuration, which changes
  /// when an operator changes it rather than every 2s. Polling this at the process cadence would
  /// re-derive the same rules over an unchanged config 30 times a minute.
  ///
  /// A failure is swallowed on purpose. Advisories are informational, and an operator who cannot
  /// see the process table because an advisory fetch failed has been served badly.
  /// Fetches typical values.
  ///
  /// Polled at 30s rather than riding the 2s process stream: a median over a 15-minute window does
  /// not move meaningfully in two seconds, and recomputing it per tick would spend the tick budget
  /// on the figure that changes least.
  async #refreshTypical() {
    if (network.down) return; // daemon unreachable; the interval stays armed until resume
    try {
      const data = await apiRequest("/api/typical");
      typicalState.clear();
      for (const entry of data?.processes ?? []) {
        if (entry?.typical) typicalState.set(entry.process, entry.typical);
      }
      this.#render();
    } catch {
      // Left as-is rather than cleared: the last known typical values were true when they
      // arrived, and blanking them on a transient failure would flicker every row.
    }
  }

  // Track previous state for threshold announcement rate limiting
  #previousFindingState = new Map();
  #haveFindingBaseline = false;
  #thresholdAnnounceTimes = new Map();

  /// Fetches findings and the engine's own state.
  ///
  /// Polled at 10s rather than riding the 2s process stream: findings change on the daemon's own
  /// analysis tick, and a held finding deliberately produces no event at all — so there is nothing
  /// to stream and nothing gained by asking faster.
  async #refreshFindings() {
    if (network.down) return; // daemon unreachable; the interval stays armed until resume
    try {
      const data = await apiRequest("/api/findings");
      this.#previousFindingState = new Map(findingState);
      findingState.clear();
      for (const finding of data?.findings ?? []) {
        const name = finding?.key?.process;
        if (!name) continue;
        if (!findingState.has(name)) findingState.set(name, []);
        findingState.get(name).push(finding);
      }
      this.#checkThresholdsForAnnouncements();
      findingEngine.warming = new Set(data?.warming ?? []);
      findingEngine.suppressed = data?.suppressed ?? null;
      this.#render();
    } catch {
      // Left as-is rather than cleared: the last known findings were true when they arrived, and
      // blanking every marker on one failed request would flicker the table.
    }
  }

  /// Announces newly active threshold crossings, rate-limited per subject and finding key.
  /// The first poll establishes a baseline (nothing is "newly" crossed on load); the
  /// rate limit then guarantees a figure oscillating across a threshold produces at
  /// most one announcement per subject+threshold per window — the daemon re-evaluates
  /// every detector on its own tick, so without the window the polite region would
  /// replay the same crossing every poll.
  #checkThresholdsForAnnouncements() {
    if (!this.#haveFindingBaseline) {
      this.#haveFindingBaseline = true;
      return; // first poll: baseline, not a crossing.
    }
    const now = Date.now();
    for (const [proc, findings] of findingState) {
      for (const finding of findings) {
        if (finding.status !== "active") continue;

        const prevProcFindings = this.#previousFindingState.get(proc) ?? [];
        const wasActive = prevProcFindings.some(f => f.key?.detector === finding.key?.detector && f.key?.metric === finding.key?.metric && f.status === "active");
        if (wasActive) continue;

        const key = finding.key ?? {};
        const announceKey = `${proc}|${key.detector ?? "?"}|${key.metric ?? "?"}`;
        const last = this.#thresholdAnnounceTimes.get(announceKey) ?? 0;
        if (now - last < THRESHOLD_ANNOUNCE_MS) continue; // rate-limited

        this.#thresholdAnnounceTimes.set(announceKey, now);
        announce.status(`Resource finding: ${key.detector ?? "?"} on ${key.metric ?? "?"} is now active.`);
      }
    }
  }

  async #refreshAdvisories() {
    if (network.down) return; // daemon unreachable; the interval stays armed until resume
    try {
      const data = await apiRequest("/api/advisories");
      advisoryState.clear();
      for (const entry of data?.processes ?? []) {
        if (entry?.advisories?.length) advisoryState.set(entry.process, entry.advisories);
      }
      this.store.set("advisories", Object.fromEntries(advisoryState));
      this.#render();
    } catch {
      // Left as-is rather than cleared: the last known advisories are still true of the
      // configuration, and blanking them on a transient failure would flicker the markers.
    }
  }

  async init() {
    const params = new URLSearchParams(location.search);
    const urlInt = parseInt(params.get("interval_ms"), 10);
    const cfg = await Api.config();
    if (!Number.isNaN(urlInt)) this.store.set("interval", clamp(urlInt, INT.MIN, INT.MAX));
    else if (cfg?.interval_ms) this.store.set("interval", clamp(cfg.interval_ms, INT.MIN, INT.MAX));
    if (cfg?.label) applyLabel(cfg.label, cfg.label_color);
    applyTailConfig(cfg);
    applySeverityConfig(cfg);
    // Daemon reachability wiring. Down: tear the streams and pollers down so a
    // dead daemon is not hammered by reconnects; the banner tells the operator
    // the figures on screen are now stale. Up: rebuild every channel and refresh
    // the polled panels once so the dashboard is current immediately, not on the
    // next interval tick.
    network.onDown = () => {
      this.api.stopUnified();
      this.api.stopLog();
      this.hostPanel.stop();
      // The panel stays visible with its last known values and the "reconnecting…"
      // indicator (host-stale class). Hiding it on daemon-down erased the very
      // figures an operator needs during an incident.
      this.banner.show("Daemon unreachable — reconnecting…", "daemon");
    };
    network.onUp = () => {
      this.banner.hide();
      this.#stream();
      this.hostPanel.start();
      this.logModal.restartIfOpen();
      this.#refreshAdvisories();
      this.#refreshTypical();
      this.#refreshFindings();
    };
    this.#stream();
    this.hostPanel.start();
    this.#refreshAdvisories();
    this.#intervals.push(setInterval(() => this.#refreshAdvisories(), ADVISORY_POLL_MS));
    this.#refreshTypical();
    this.#intervals.push(setInterval(() => this.#refreshTypical(), TYPICAL_POLL_MS));
    this.#refreshFindings();
    this.#intervals.push(setInterval(() => this.#refreshFindings(), FINDING_POLL_MS));
  }
}
