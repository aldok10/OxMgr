import { sel } from "../core/const.js";
import { MetricRenderer } from "./metricRenderers.js";
import { SparklineRenderer } from "./canvas.js";
import { network } from "../shell/network.js";

// Host panel.
//
// Polls /api/host on its own cadence rather than riding the process stream: host
// collection is a separate task in the daemon with its own intervals, so tying this to
// the process refresh would either over-poll or misreport how fresh the figures are.
//
// Every figure goes through `#metric`, which takes an explicit `available` flag. That is
// the whole design: a host figure that is missing must render as an absence, never as a
// zero. A load average of 0 reads as an idle machine and a temperature of 0 reads as a
// cold one, when both actually mean "this platform does not report it".
export class HostPanel {
  #panel; #body; #identity; #toggle; #side; #hostOpenBtn; #hostCloseBtn;
  #open = true; #userChose = false;
  // The two rendering collaborators. Constructed in the constructor over a
  // context that exposes THIS panel's state through accessors: ownership of
  // every field, timer and stream stays here, so teardown remains the single
  // AbortController path and no state is duplicated between classes.
  #metrics; #sparks;
  // The stream connection, the last complete state received, and one DOM node per region so
  // an update writes into an existing element instead of rebuilding the panel.
  #sourceAbort = null; #state = null; #regions = new Map();
  // Rate history per interface, keyed by name, for the sparklines. Bounded rings: a dashboard
  // left open overnight must not accumulate 30,000 samples per interface.
  //
  // Held here rather than fetched, because the API reports an instantaneous rate and no
  // history — the trend only exists if the client keeps it. That is also why the buffer is
  // per interface name: an interface that disappears takes its history with it on the next
  // prune rather than leaving a series that silently belongs to nothing.
  #netHistory = new Map();
  /// The last consumer sample: `undefined` before the first fetch, `null` when the daemon reports
  /// sampling unavailable, and an object once sampled. Three states because "not asked yet" and
  /// "asked and told it is off" need different displays.
  #consumers = undefined;
  /// Which view the listing shows. Four modes: the flat top-CPU / top-memory listings, and the
  /// attributed tree versions of each. CPU first: contention is the usual question.
  #consumerDim = "cpu";
  /// Which tree nodes the operator has collapsed, by pid. ABSENT means expanded: entering a
  /// tree view shows the whole attributed structure at once (that is the point of the view), and
  /// collapsing is something done to a node that is in the way, not the default.
  #treeCollapsed = new Set();
  /// Whether interfaces that never carried a byte are shown. Hidden by DEFAULT — this host
  /// enumerates 23 of which 14 are permanently empty — but revealable, because "does this box
  /// even have that interface" is a real question and the payload already carries the answer.
  /// Presentational only: the API always returned the full set.
  #netShowIdle = false;
  /// Whether the listing is expanded. CLOSED by default: it is the tallest region in the panel,
  /// and "who is using this?" is a question asked after a gauge already looks wrong. Opened by
  /// clicking the CPU or RAM gauge, which is the moment the question arises, so the listing
  /// appears already sorted by the dimension that prompted it.
  #consumersOpen = false;
  /// The poll timer, cleared when the panel stops.
  #consumerTimer = null;
  /// Releases this component's listeners, stream and intervals in one call (section 5 teardown).
  #life = new AbortController();
  _signal() { return { signal: this.#life.signal }; }
  destroy() { this.#life.abort(); this.stop(); }
  /// Display order for the panel's regions, top to bottom.
  ///
  /// The single source of truth for layout: `#region` inserts by this list rather than appending,
  /// so a region's position no longer depends on which network response arrived first. Consumers
  /// sit BELOW the gauges and cores because the listing attributes the pressure those figures
  /// report — above them it would be an answer printed before its question.
static REGION_ORDER = [
    "gauges", "cpu_stats", "consumers", "cores", "storage_net",
    "failures",
  ];
  static SPARK_SAMPLES = 24;
  /// Polled rather than streamed, and at the sampler's own cadence rather than the 2s tick: the
  /// daemon refreshes this every 30s, so asking more often would return the same listing.
  static CONSUMER_POLL_MS = 15_000;
  // Below this width the panel is a banner above the table rather than a sidebar beside it,
  // so an expanded body pushes the process list down the page. Kept in sync with the
  // `min-width: 1250px` layout block in dashboard.css.
  static SIDEBAR_MIN_WIDTH = 1250;
  // At and below this width the panel is not an inline banner at all: it is a full-viewport
  // overlay reached from the bottom bar's "Host" cell. Kept in sync with the
  // `max-width: 700px` overlay block in dashboard.css.
  static PHONE_WIDTH = 700;
  /// Visibility preference for the whole panel, persisted in three states.
  ///
  /// An explicit operator open stores "open"; an explicit operator close stores
  /// "closed"; an absent key means no choice was ever made, which defers to the
  /// width default: open above PHONE_WIDTH (the sidebar costs the process list
  /// nothing there), hidden at or below it (the overlay steals the viewport and
  /// a stream nobody asked for). Only OPERATOR actions write the key — lifecycle
  /// dismissal such as daemon-down hides the panel without touching storage, so
  /// a transient outage cannot erase a deliberate choice and rewrite every later
  /// reload. Data arriving never changes visibility by itself either way.
  static HOST_OPEN_KEY = "oxmgr-host-open";
  // Severity thresholds are NOT held here. They live in `severity.rs` and arrive over
  // `/api/config`; `severityBand` applies them plus the documented hysteresis margin. Two copies
  // of one boundary is how a documented default and a rendered one drift apart silently.

  constructor() {
    this.#panel = sel("#host-panel");
    this.#body = sel("#host-body");
    this.#identity = sel("#host-identity");
    this.#toggle = sel("#host-toggle");
    this.#side = sel("#host-side");
    this.#hostOpenBtn = sel("#host-open");
    this.#hostCloseBtn = sel("#host-close");

    // The context contract (D1): collaborators read and write THROUGH the
    // owner. Getters expose live values; setters exist only for the three
    // fields collaborator click-handlers flip; Map/Set-valued fields
    // (netHistory, regions, treeCollapsed) are mutated in place through one
    // getter. The metrics/sparks slots are filled immediately below — nothing
    // renders before a snapshot arrives, by which time both exist.
    const self = this;
    const hostCtx = {
      get state() { return self.#state; },
      get consumers() { return self.#consumers; },
      get consumerDim() { return self.#consumerDim; },
      set consumerDim(v) { self.#consumerDim = v; },
      get treeCollapsed() { return self.#treeCollapsed; },
      get netShowIdle() { return self.#netShowIdle; },
      set netShowIdle(v) { self.#netShowIdle = v; },
      get consumersOpen() { return self.#consumersOpen; },
      set consumersOpen(v) { self.#consumersOpen = v; },
      get netHistory() { return self.#netHistory; },
      get body() { return self.#body; },
      get identity() { return self.#identity; },
      get regions() { return self.#regions; },
      get regionOrder() { return HostPanel.REGION_ORDER; },
      get sparkSamples() { return HostPanel.SPARK_SAMPLES; },
    };
    this.#metrics = new MetricRenderer(hostCtx);
    this.#sparks = new SparklineRenderer(hostCtx);
    hostCtx.metrics = this.#metrics;
    hostCtx.sparks = this.#sparks;
    // Restored before the first paint, so the panel does not visibly jump from one side to the
    // other after load.
    // LEFT is the default. Only the non-default is stored, matching how the theme switch treats
    // "auto": an absent key means "the default", so changing the default later is not overridden
    // by a stale stored value that merely agreed with the old one. That is also why this flipped
    // from storing "left" to storing "right" when the default changed.
    this.#applySide(localStorage.getItem(HostPanel.SIDE_KEY) === "right" ? "right" : "left");
    this.#side?.addEventListener("click", () => {
      const layout = this.#panel.closest(".layout");
      const next = layout?.dataset.hostSide === "left" ? "right" : "left";
      if (next === "right") localStorage.setItem(HostPanel.SIDE_KEY, "right");
      else localStorage.removeItem(HostPanel.SIDE_KEY);
      this.#applySide(next);
    }, this._signal());
    this.#toggle.addEventListener("click", () => {
      // Once the operator has expressed a preference it outranks the width default, so a
      // rotation does not undo their choice.
      this.#userChose = true;
      this.#setOpen(!this.#open);
    }, this._signal());
    this.#syncDefaultOpen();
    window.addEventListener("resize", () => {
      this.#syncDefaultOpen();
      // A resize above the phone breakpoint while the overlay is open would leave
      // the full-viewport layer attached at a width where the panel is a sidebar or
      // banner — drop just the overlay class so the panel returns to its normal CSS
      // role instead of closing outright: the operator opened it, a rotation must not.
      if (window.innerWidth > HostPanel.PHONE_WIDTH
        && this.#panel.classList.contains("host-overlay-open")) {
        this.#panel.classList.remove("host-overlay-open");
      }
    }, this._signal());
    // The "Host" control in the nav bar is the panel's only door, at every width: it reveals
    // a hidden panel (overlay at phone width, sidebar/banner above) and conceals a visible
    // one. Revealing starts the stream and the consumer poll; concealing stops both — a
    // hidden panel must cost nothing, which is the same lifecycle a hidden tab already gets.
    this.#hostOpenBtn.addEventListener("click", () => {
      if (this.#panel.hidden) this.#reveal();
      else this.#conceal();
    }, this._signal());
    this.#hostCloseBtn.addEventListener("click", () => this.#conceal(), this._signal());
    // Escape closes the panel at any width, mirroring the log/detail overlays.
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && !this.#panel.hidden) {
        this.#conceal();
      }
    }, this._signal());
    // A hidden tab holds no stream: the daemon sends to every subscriber, so staying
    // connected would cost a send per collection for a panel nobody can see.
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) this.stop(); else this.start();
    }, this._signal());

    // Restore the visibility choice across reloads. Three states: "open" wins
    // outright; "closed" outranks the width default; an absent key defers to it —
    // open above the phone breakpoint, hidden at or below. The restore path
    // persists nothing: the stored state already describes reality.
    const pref = localStorage.getItem(HostPanel.HOST_OPEN_KEY);
    if (pref === "open"
      || (pref === null && window.innerWidth > HostPanel.PHONE_WIDTH)) {
      this.#reveal({ persist: false });
    }
  }

  /// Collapsed by default while the panel is a banner, expanded while it is a sidebar.
  ///
  /// As a banner an expanded body measured 515px of vertical space at 1024px and 805px at
  /// 390px, putting the first process at or past the fold — the host figures displacing the
  /// content they are context for. As a sidebar it costs the list no height at all, so there
  /// is no reason to hide it.
  #syncDefaultOpen() {
    if (this.#userChose) return;
    this.#setOpen(window.innerWidth >= HostPanel.SIDEBAR_MIN_WIDTH);
  }

  #setOpen(open) {
    this.#open = open;
    this.#body.hidden = !open;
    // The glyph is static and CSS rotates it from `aria-expanded`, so the state lives in exactly
    // one place. The accessible name is updated because an icon-only control has no text to read:
    // without this a screen reader announces "button" and nothing else.
    const label = open ? "Hide the host figures" : "Show the host figures";
    this.#toggle.setAttribute("aria-expanded", open ? "true" : "false");
    this.#toggle.setAttribute("aria-label", label);
    this.#toggle.title = label;
  }

  /// Reveals the panel — the only path that makes it visible.
  ///
  /// The body is forced open at phone width: a collapsed banner body has no meaning inside an
  /// overlay that exists to show the figures. Focus moves to the close control there, the
  /// natural landing point for a modal dismiss; above the phone breakpoint no focus moves, so
  /// restoring an open preference on load does not steal focus from the process list.
  ///
  /// `persist` is false only for the restore path, which would just rewrite the state it read.
  #reveal({ persist = true } = {}) {
    this.#panel.hidden = false;
    if (persist) localStorage.setItem(HostPanel.HOST_OPEN_KEY, "open");
    this.#hostOpenBtn.setAttribute("aria-expanded", "true");
    if (!this.#userChose) this.#syncDefaultOpen();
    if (window.innerWidth <= HostPanel.PHONE_WIDTH) {
      this.#panel.classList.add("host-overlay-open");
      this.#body.hidden = false;
      this.#hostCloseBtn.focus();
    }
    this.start();
  }

  /// Conceals the panel and stops everything it was paying for.
  ///
  /// `stop()` is the same teardown a hidden tab gets: the stream closes and the consumer timer
  /// is disarmed, so a concealed panel holds no subscription, arms no poll, and renders nothing.
  ///
  /// An operator close stores "closed" — with absent now meaning "follow the width default",
  /// removing the key on close would make the panel pop back open on the next desktop load,
  /// the opposite of what was asked. Lifecycle dismissal passes `persist: false`: daemon-down
  /// hides the panel but must not overwrite a deliberate choice with a transient condition.
  #conceal({ returnFocus = true, persist = true } = {}) {
    this.#panel.classList.remove("host-overlay-open");
    this.#panel.hidden = true;
    if (persist) localStorage.setItem(HostPanel.HOST_OPEN_KEY, "closed");
    this.#hostOpenBtn.setAttribute("aria-expanded", "false");
    this.stop();
    // On close the door is the "Host" control the operator came through.
    if (returnFocus) this.#hostOpenBtn.focus();
  }

  /// Public dismissal for lifecycle events outside the panel (daemon-down, teardown).
  ///
  /// A stale full-viewport overlay left open behind the connectivity banner is exactly
  /// the "panel still showing when it is no longer in use" failure the panel-lifecycle
  /// spec forbids: the figures behind it are frozen mid-incident and the operator has
  /// to hit Close to reach the table. No focus move on return — the caller is taking
  /// over the screen with a banner of its own. Storage stays untouched: this dismissal
  /// describes the daemon's state, not the operator's preference, so the choice made
  /// before the outage survives both the outage and the reload after it.
  closeOverlay() {
    if (!this.#panel.hidden) {
      this.#conceal({ returnFocus: false, persist: false });
    }
  }

  /// Which side of the content the panel sits on, persisted.
  ///
  /// Stored rather than session-only: a side preference is about how someone reads the page, and
  /// having to re-set it on every load would make the control not worth using. Same
  /// `localStorage` approach the theme switch already uses.
  static SIDE_KEY = "oxmgr-host-side";

  #applySide(side) {
    const left = side === "left";
    // A data attribute on the layout, not a class on the panel: the grid that positions both
    // columns lives on `.layout`, so the side has to be readable from there.
    this.#panel.closest(".layout")?.setAttribute("data-host-side", left ? "left" : "right");
    if (this.#side) {
      const label = left ? "Move the panel to the right" : "Move the panel to the left";
      this.#side.setAttribute("aria-label", label);
      this.#side.title = label;
    }
  }

  start() {
    // A concealed panel holds no subscription: the daemon sends to every subscriber, so
    // connecting for figures nobody can see costs a send per collection. `start()` is called
    // unconditionally by app.js and by the visibilitychange handler, so the guard lives here
    // rather than at every call site.
    if (this.#panel.hidden) return;
    this.#connect();
    this.#pollConsumers();
    this.#consumerTimer ??= setInterval(
      () => this.#pollConsumers(),
      HostPanel.CONSUMER_POLL_MS,
    );
  }

  /// Fetches the consumer listing.
  ///
  /// A 503 is a real answer, not an error: it means sampling is disabled or has not yet produced a
  /// sample, and the panel must say so rather than showing an empty table. A network failure is
  /// different and leaves the previous listing in place, because the last known consumers were
  /// true when they arrived.
  async #pollConsumers() {
    if (network.down) return; // daemon unreachable; the timer stays armed until resume
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    try {
      const response = await fetch("/api/host/consumers", { signal: controller.signal });
      clearTimeout(timer);
      if (response.status === 503) {
        this.#consumers = null;
      } else if (response.ok) {
        this.#consumers = await response.json();
      } else {
        return;
      }
      this.#metrics.renderConsumers();
      // The CPU stats line shows the process total from THIS payload, so it repaints too.
      // Without this it would only appear when the next host snapshot arrived.
      this.#metrics.renderCpuStats();
      // The consumer rows carry data-scale/data-depth attributes that need promoting to
      // CSSOM, and this call path bypasses #renderSubsystem's applyStyles.
      this.#sparks.applyStyles();
    } catch (err) {
      clearTimeout(timer);
      if (err.name !== 'AbortError') {
        network.reportFailure();
        // Transient: keep what we have rather than blanking the region on one failed request.
      }
    }
  }

  /// Closes the connection, which is what releases the daemon's subscriber.
  ///
  /// A hidden tab is disconnected rather than left subscribed: the daemon publishes to every
  /// subscriber, so an abandoned tab would keep costing a send per collection for figures
  /// nobody is looking at.
  stop() {
    if (this.#sourceAbort) {
      this.#sourceAbort.abort();
      this.#sourceAbort = null;
    }
    // The timer goes with the stream: a hidden tab polling every 15s for a listing nobody can see
    // is the same waste as an abandoned SSE subscriber.
    if (this.#consumerTimer) {
      clearInterval(this.#consumerTimer);
      this.#consumerTimer = null;
    }
  }

  /// Opens the event stream, replacing the poll.
  ///
  /// One connection instead of a request every 5s, and each event carries only the subsystem
  /// that moved — a tick where only processor utilisation changed sends ~570 bytes rather than
  /// a 5.6 KB snapshot. Collection itself is unchanged and still samples on an interval: the
  /// platforms provide no notification for a change in memory, processor or filesystem usage,
  /// so streaming changes which side initiates delivery, nothing more.
  async #connect() {
    if (this.#sourceAbort) return;
    this.#sourceAbort = new AbortController();
    const { signal } = this.#sourceAbort;
    const url = "/api/stream?subscribe=processes,snapshot,memory,cpu,load_average,filesystems,network,components";

    try {
      const response = await fetch(url, { signal, credentials: 'include' });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);

      network.reportSuccess();
      this.#panel.classList.remove("host-stale");

      const reader = response.body.getReader();
      const decoder = new TextDecoder();

      const handlers = {
        snapshot: (data) => this.#onSnapshot(data),
        memory: (data) => this.#onSubsystem("memory", data),
        cpu: (data) => this.#onSubsystem("cpu", data),
        load_average: (data) => this.#onSubsystem("load_average", data),
        filesystems: (data) => this.#onSubsystem("filesystems", data),
        network: (data) => this.#onSubsystem("network", data),
        components: (data) => this.#onSubsystem("components", data),
      };

      let buffer = "";
      let eventType = "message";
      let data = "";

      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });

        let lines = buffer.split("\n");
        buffer = lines.pop();

        for (const line of lines) {
          if (line === "") {
            const handler = handlers[eventType];
            if (handler) handler(data);
            data = "";
            eventType = "message";
          } else if (line.startsWith("event:")) {
            eventType = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            data = line.slice(5).trim();
          }
        }
      }
    } catch (err) {
      if (err.name !== "AbortError") {
        network.reportFailure();
        this.#panel.classList.add("host-stale");
      }
    } finally {
      this.#sourceAbort = null;
    }
  }

  #onSnapshot(data) {
    const val = this.#parse(data);
    if (!val) return;
    this.#state = val;
    this.#sparks.renderAll();
  }

  #onSubsystem(key, data) {
    if (!this.#state) return; // nothing to merge into yet; the snapshot is still coming
    const value = this.#parse(data, true);
    if (value === undefined) return;
    this.#state[key] = value;
    this.#sparks.renderSubsystem(key);
  }


  #parse(data, allowNull = false) {
    try {
      const value = JSON.parse(data);
      if (value === null && !allowNull) return null;
      return value;
    } catch {
      return allowNull ? undefined : null;
    }
  }

}
