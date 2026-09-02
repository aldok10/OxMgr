import { EMPTY, INT, STREAMS, UNKNOWN, sel, tailCfg } from "../core/const.js";
import { applySeverityConfig } from "../core/severity.js";
import { Api, withTimeout } from "../core/api.js";
import { fmt } from "../format/fmt.js";
import { makeLine } from "../ansi/ansi.js";
import { LineBuffer } from "../log/LineBuffer.js";
import { LogView, applyTailConfig } from "../log/LogView.js";
import { hideDaemonBanner, network, showDaemonBanner } from "../shell/network.js";

// Standalone log page: same document, booted into a single-log view from the
// path. Reuses LogView so rendering, scrolling and autoscroll match; chrome differs.
export class LogPage {
  #buffer = new LineBuffer(); #view; #src = null; #statsSrc = null; #query = "";
  // Paging state for a finished file.
  #loadedFrom = 0;   // lines held, counted back from the end of the file
  #newestOffset = 0; // gap to the file end; non-zero once paging back discarded newer content
  #reachedStart = false;  // latched once the server says there is nothing earlier
  #loading = false;       // one page in flight at a time
  #pageGen = 0;           // invalidates pages whose request has been superseded
  #pageAbort = null;      // AbortController for the in-flight page fetch
  #searchTimer = 0;       // debounce for the query input, cleared on destroy
  /// Releases this page's listeners, streams and intervals in one call (section 5 teardown).
  #life = new AbortController();
  _signal() { return { signal: this.#life.signal }; }
  destroy() {
    this.#life.abort();
    this.#pageAbort?.abort();
    this.#stop(); this.#stopStats(); this.#view.stopRateTicker();
    if (this.#searchTimer) { clearTimeout(this.#searchTimer); this.#searchTimer = 0; }
  }
  constructor(name, stream, index) {
    this.name = name;
    this.stream = STREAMS.includes(stream) ? stream : "stdout";
    // An archive is a finished file: it never grows, so it is rendered whole
    // and reported complete rather than sat in a follow loop forever.
    this.index = Number.isInteger(index) && index > 0 ? index : 0;
    this.els = {
      body: sel("#log-body"),
      meta: sel("#log-meta"),
      spin: sel("#log-spinner"),
      jump: sel("#log-jump"),
      search: sel("#log-search"),
      pause: sel("#log-pause"),
      older: sel("#log-older"),
      newer: sel("#log-newer"),
      seg: sel("#log-stream-seg"),
      stats: sel("#log-stats"),
    };
    this.#view = new LogView(this.els.body, this.#buffer);
    this.#view.onState(() => this.#renderState());
    this.#chrome();
    this.#bind();
  }
  // Strip the dashboard down to the log panel and make it fill the window.
  #chrome() {
    document.querySelector("header")?.remove();
    document.querySelector("main")?.remove();
    sel("#detail-overlay")?.remove();
    sel("#confirm-overlay")?.remove();
    const overlay = sel("#log-overlay");
    // showModal(), not classList.add("open"): CSS gates display on the [open]
    // attribute (`.log-overlay[open]`), and showModal() is what sets it.
    // Prevent Escape from closing the standalone page — there is nothing to
    // go "back" to inside this document.
    overlay.addEventListener("cancel", e => e.preventDefault());
    overlay.showModal();
    overlay.style.padding = "0";
    overlay.style.background = "var(--bg)";
    const panel = sel(".log-panel");
    panel.classList.add("fullscreen");
    panel.querySelectorAll(".resize-handle").forEach(handle => handle.remove());
    sel("#log-close")?.remove();
    sel("#log-open-page")?.remove();
    const label = this.index ? `${this.stream} #${this.index}` : this.stream;
    sel("#log-title").textContent = `${this.name} — ${label}`;
    document.title = `${this.name} · ${label} · oxmgr`;
    // Stream tabs stay useful here as navigation between the two streams.
    this.els.seg.querySelectorAll("button[data-stream]").forEach(btn => {
      const target = btn.dataset.stream;
      if (target === "files") { btn.remove(); return; }
      btn.classList.toggle("active", target === this.stream && !this.index);
    });
    sel("#log-download").addEventListener("click", () => {
      window.location.href = `/api/processes/${encodeURIComponent(this.name)}/logs/download?stream=${encodeURIComponent(this.stream)}${this.index ? `&index=${this.index}` : ""}`;
    }, this._signal());
    sel("#log-refresh").addEventListener("click", () => this.#start(), this._signal());
  }
  #bind() {
    this.els.body.addEventListener("scroll", () => this.#view.onScroll(), { passive: true, ...this._signal() });
    // Explicit rather than scroll-triggered. Four scroll-based variants were measured
    // and all failed structurally: the windowed renderer makes `scrollHeight` describe
    // the ~67 rendered rows (0.23px per retained line), so it cannot say whether the
    // operator is at the top of the FILE, and height-delta compensation has no delta to
    // read. A button states the intent exactly once and cannot misread its precondition.
    this.els.older.addEventListener("click", () => this.#loadOlder(), this._signal());
    this.els.newer.addEventListener("click", () => this.#loadNewer(), this._signal());
    this.els.jump.addEventListener("click", () => this.#view.jumpToLatest(), this._signal());
    this.els.pause.addEventListener("click", () => { this.#view.setPaused(!this.#view.paused); this.#renderState(); }, this._signal());
    this.els.search.addEventListener("input", evt => {
      clearTimeout(this.#searchTimer);
      const value = evt.target.value;
      this.#searchTimer = setTimeout(() => this.#view.setQuery(value), 120);
    }, this._signal());
    this.els.seg.addEventListener("click", evt => {
      const btn = evt.target.closest("button[data-stream]");
      if (!btn) return;
      // Navigate rather than re-subscribe: the URL is the source of truth for
      // which file this page shows.
      window.location.search = `?stream=${encodeURIComponent(btn.dataset.stream)}`;
    }, this._signal());
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) this.#view.suspend(); else this.#view.resume();
    }, this._signal());
    window.addEventListener("beforeunload", () => { this.destroy(); }, this._signal());
  }
  #renderState() {
    const total = this.#buffer.length;
    const countText = this.#view.filtered
      ? `${fmt.count(this.#view.matches)} of ${fmt.count(total)} lines`
      : `${fmt.count(total)} lines`;
    const source = this.index ? `archive #${this.index} · complete` : `${this.stream} · live`;
    const html = [fmt.esc(source), `<span class="count">${fmt.esc(countText)}</span>`];
    // A completed archive never grows, so an arrival rate would be meaningless there.
    if (!this.index) {
      const rate = this.#view.rate();
      html.push(rate.idle ? `<span class="rate idle">idle</span>` : `<span class="rate">${fmt.esc(rate.text)}</span>`);
    } else {
      // Paging state. A partially loaded archive must be distinguishable from a whole
      // one, otherwise the operator cannot tell whether there is more to read.
      if (this.#loading) html.push(`<span class="paging">loading earlier…</span>`);
      else if (this.#reachedStart) html.push(`<span class="paging done">start of file</span>`);
      else html.push(`<span class="paging">scroll up for earlier</span>`);
    }
    // Retention capacity, same as the embedded viewer reports: the controller runs
    // here too, so hiding the figure would leave its behaviour unobservable.
    const ret = this.#view.retention();
    html.push(`<span class="cap" title="${fmt.esc(ret.reason)}${ret.memoryMeasurable ? "" : " · heap not measurable here"}">cap ${fmt.esc(fmt.count(ret.capacity))}</span>`);
    if (this.#view.paused) html.push(`<span class="paused">paused · still retaining</span>`);
    else if (!this.#view.following) html.push(`<span class="paused">scrolled back</span>`);
    this.els.meta.innerHTML = html.join(" · ");
    this.els.pause.textContent = this.#view.paused ? "Resume" : "Pause";
    this.els.pause.classList.toggle("primary", this.#view.paused);
    this.els.pause.hidden = this.index > 0; // nothing to pause on a finished file
    // Only offered where it can act: a finished file that still has unloaded history.
    // Hidden on a live stream (nothing above the tail) and once the start is reached.
    this.els.older.hidden = !this.index || this.#reachedStart;
    this.els.older.disabled = this.#loading;
    this.els.older.textContent = this.#loading ? "Loading…" : "↑ Load earlier";
    // Only offered once paging back has actually discarded newer content, so it never
    // appears on a file whose tail is already held.
    this.els.newer.hidden = !this.index || this.#newestOffset <= 0;
    this.els.newer.disabled = this.#loading;
    this.els.newer.textContent = this.#loading ? "Loading…" : "↓ Load newer";
    this.els.jump.classList.toggle("on", !this.#view.following);
    const missed = this.#view.missed;
    this.els.jump.textContent = missed ? `↓ ${fmt.count(missed)} new` : "↓ Jump to latest";
  }
  #stop() { this.#src?.close(); this.#src = null; }
  #stopStats() { this.#statsSrc?.close(); this.#statsSrc = null; }
  // Resource strip. Reuses the dashboard's own unified stream, subscribed
  // ONLY to the process snapshot — a standalone log page must not start
  // receiving host metrics it never renders (Decision 5).
  #watchStats() {
    this.#stopStats();
    const src = new EventSource(`/api/stream?subscribe=processes&interval_ms=${INT.DEF}`);
    this.#statsSrc = src;
    src.addEventListener("processes", evt => {
      try {
        const procs = JSON.parse(evt.data);
        const proc = Array.isArray(procs) ? procs.find(item => item.name === this.name) : null;
        if (proc) this.#renderStats(proc);
      } catch { /* ignore malformed frame */ }
    });
    src.onerror = () => network.reportFailure();
  }
  #renderStats(proc) {
    const running = proc.status === "running";
    // Not running means no usage to report. Showing 0% would read as a
    // measurement rather than an absence.
    const val = (text, ok = true) => ok
      ? `<span class="ls-val">${fmt.esc(text)}</span>`
      : `<span class="ls-val na">${EMPTY}</span>`;
    const item = (key, body) => `<span class="ls-item"><span class="ls-key">${fmt.esc(key)}</span>${body}</span>`;
    // A disk amount is only a measurement when the daemon recorded the interval it
    // covers. Without one — first sample after a start, a PID change, or a stopped
    // process — the amount is zero because nothing was measured, not because nothing
    // happened. Rendering that zero would be the same lie as the old `/s` label.
    const ioMeasured = proc.disk_read_bytes !== undefined
      && proc.status === "running"
      && typeof proc.metrics_interval_ms === "number"
      && proc.metrics_interval_ms > 0;
    // Only call it a rate when we know the interval it covers. The maintenance tick
    // skips missed ticks, so dividing by an assumed 2s overstated the figure whenever
    // the tick stretched — measured 1999-2000ms against a nominal 2000.
    const perSec = (bytes) => `${fmt.bytes((bytes * 1000) / proc.metrics_interval_ms)}/s`;
    // Per-process network I/O is reported as unsupported rather than omitted, because an
    // absent row is indistinguishable from a process doing no network I/O. sysinfo keys
    // network counters by interface, not PID, so the only figure available here is the
    // host's — and presenting that as one process's traffic would be a plausible-looking
    // lie. The host figures live in the host panel, labelled as the host's.
    const NETWORK_UNSUPPORTED_REASON =
      "Not measurable per process: the platform reports network counters per interface, "
      + "not per process. Host-level figures are shown separately.";
    this.els.stats.innerHTML = [
      item("status", `<span class="ls-val"><span class="badge"><span class="dot ${fmt.esc(proc.status)}"></span>${fmt.esc(proc.status ?? UNKNOWN)}</span></span>`),
      item("pid", val(proc.pid ?? EMPTY, running && proc.pid != null)),
      item("uptime", val(fmt.up(proc), running)),
      item("cpu", val(`${fmt.pct(proc.cpu_percent)}%`, running)),
      item("ram", val(fmt.bytes(proc.memory_bytes), running)),
      item("disk read", val(ioMeasured ? perSec(proc.disk_read_bytes) : EMPTY, ioMeasured)),
      item("disk write", val(ioMeasured ? perSec(proc.disk_write_bytes) : EMPTY, ioMeasured)),
      // `unsupported`, not a dash: a dash here would read as "no measurement yet", the
      // same as a process that has only just started. The two are different answers.
      item("network", `<span class="ls-val na" title="${fmt.esc(NETWORK_UNSUPPORTED_REASON)}">unsupported</span>`),
      item("restarts", val(fmt.count(proc.restart_count ?? 0))),
    ].join("");
    this.els.stats.hidden = false;
  }
  // Archives render whole; a page is addressed backwards from the file's end.
  // `before` is how many lines back from the end this page ends. The response says
  // whether it reached the file's first line, which is the only reliable signal for
  // "stop asking" — deriving it from counts would guess at a fact the server knows.
  async #fetchPage(before, gen, lines = tailCfg.lines) {
    // Composed with the page's teardown signal so destroy() aborts the request
    // too; the timeout controller is what #start() aborts when superseding.
    const {
      signal, controller, clear,
    } = withTimeout(5000, this.#life.signal);
    this.#pageAbort = controller;
    try {
      const url = `/api/processes/${encodeURIComponent(this.name)}/logs`
        + `?stream=${encodeURIComponent(this.stream)}`
        + `&index=${this.index}&lines=${lines}&before=${before}`;
      const res = await fetch(url, { signal });
      clear();
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      // A page that landed after the operator moved on must not be applied.
      if (gen !== this.#pageGen) return null;
      return {
        lines: data.lines ?? [],
        reachedStart: data.reached_start === true,
        // How far back from the end this page ends, echoed by the server. Positions are
        // derived from it once the file's length is known.
        before: Number.isInteger(data.before) ? data.before : before,
      };
    } catch (err) {
      clear();
      // Abort is expected when the page was superseded (life aborted, or a new
      // #start() aborted the controller): quiet, no error surfaced.
      if (err.name === 'AbortError') return null;
      throw err;
    }
  }

  // First page: the end of the file, which is what an operator wants to see first.
  async #loadArchive() {
    const gen = this.#pageGen;
    try {
      const page = await this.#fetchPage(0, gen);
      if (!page) return;
      this.#loadedFrom = page.lines.length;
      this.#newestOffset = 0;
      this.#reachedStart = page.reachedStart;
      this.#view.setMoreAbove(!page.reachedStart);
      // The last line of the file is line N, and this page holds the final
      // `lines.length` of them — so its first line is at that offset from the end.
      // Absolute file numbering needs the file's length, which a tail-addressed read
      // does not know, so positions here are relative to what has been loaded and
      // shift as earlier pages arrive. Recorded as a gap below rather than papered
      // over with a guess.
      this.#view.load(page.lines, this.stream, null, 1);
    } catch (err) {
      this.els.meta.textContent = `Failed to load archive: ${err.message}`;
    } finally {
      this.#spinOff();
      this.#renderState();
    }
  }

  /// Loads a page only when needed: called from the scroll handler AND after a page
  /// lands, because scroll events alone stall — once `scrollTop` is 0 and the
  /// compensation leaves it there, no further event fires (measured: four scrolls,
  /// zero events). Loads what follows what is held, for content discarded while
  /// paging back. Symmetric with `#loadOlder`; both are explicit buttons because
  /// scroll-triggered variants failed structurally (see #maybeLoadOlder's removal).
  async #loadNewer() {
    if (this.#newestOffset <= 0 || this.#loading || !this.index) return;
    this.#loading = true;
    const gen = this.#pageGen;
    this.#renderState();
    try {
      // Ask for the page that ends `#newestOffset` lines from the end, minus the page we
      // are about to take — that is the section immediately after what is held.
      const before = Math.max(0, this.#newestOffset - tailCfg.lines);
      const want = this.#newestOffset - before;
      const page = await this.#fetchPage(before, gen, want);
      if (!page) return;
      if (page.lines.length) {
        const heldBefore = this.#view.retention().retained;
        await new Promise(done => this.#view.append(page.lines, this.stream, done));
        const heldAfter = this.#view.retention().retained;
        // Appending evicts from the front, so older content may have gone; that makes
        // earlier history re-fetchable again.
        const dropped = heldBefore + page.lines.length - heldAfter;
        if (dropped > 0) {
          this.#reachedStart = false;
          this.#view.setMoreAbove(true);
        }
        // The oldest held line moved forward by whatever was evicted from the front, and
        // the tail gap follows from the same two figures as above.
        this.#loadedFrom = Math.max(heldAfter, this.#loadedFrom - dropped);
        this.#newestOffset = Math.max(0, this.#loadedFrom - heldAfter);
      }
    } catch (err) {
      this.els.meta.textContent = `Failed to load newer lines: ${err.message}`;
    } finally {
      this.#loading = false;
      this.#renderState();
    }
  }

  /// Loads the page preceding what is already held, once, guarded against repetition.
  async #loadOlder() {
    if (this.#reachedStart || this.#loading || !this.index) return;
    this.#loading = true;
    const gen = this.#pageGen;
    const body = this.els.body;
    this.#renderState();
    try {
      const page = await this.#fetchPage(this.#loadedFrom, gen);
      if (!page) return;
      this.#reachedStart = page.reachedStart;
      this.#view.setMoreAbove(!page.reachedStart);
      if (page.lines.length) {
        // Compensate the scroll for the height the new rows add. Measured across the
        // insertion because `overflow-anchor: none` is set on the log body, so the
        // browser will not hold position for us.
        this.#loadedFrom += page.lines.length;
        const heldBefore = this.#view.retention().retained;
        // No height-delta compensation: the windowed renderer gives no delta
        // to measure. `prepend` shifts the window anchor by the number of lines added,
        // which is what actually keeps the operator on the same content.
        await new Promise(done => {
          this.#view.prepend(page.lines, this.stream, 1, done);
        });
        // Retention may have dropped lines from the newest end to make room. Derive the
        // newest held line's distance from the end of the file from positions rather
        // than by accumulating eviction counts: `loadedFrom` is how far back the oldest
        // held line sits, and `retained` is how many are held, so the difference is
        // exactly the gap to the tail. Accumulating deltas undercounted it, so the
        // "load newer" control vanished after one click with thousands of lines still
        // ahead.
        const heldAfter = this.#view.retention().retained;
        void heldBefore;
        this.#newestOffset = Math.max(0, this.#loadedFrom - heldAfter);
      }
    } catch (err) {
      this.els.meta.textContent = `Failed to load earlier lines: ${err.message}`;
    } finally {
      this.#loading = false;
      this.#renderState();
      // No auto-chaining: one click loads one page. Chained loads produced 494ms long
      // tasks (74.5ms of repaint-plus-layout each, back to back) and paged the whole
      // file whether or not the operator wanted it.
    }
  }
  #spinOff() { this.els.spin.classList.remove("on"); }
  // The active file follows the authoritative file tail, which handles rotation
  // by reopening — this is what makes the page behave like `tail -f`.
  #follow() {
    this.#stop();
    const src = new EventSource(`/api/processes/${encodeURIComponent(this.name)}/logs/stream?stream=${encodeURIComponent(this.stream)}`);
    this.#src = src;
    src.onmessage = evt => {
      this.#spinOff();
      if (evt.data !== "") this.#view.add(makeLine(evt.data, this.stream, 0));
    };
    src.onerror = () => { this.#spinOff(); network.reportFailure(); };
  }
  #start() {
    this.#stop();
    this.#pageAbort?.abort(); // supersede any in-flight page fetch, not race it
    this.#pageAbort = null;
    this.#view.reset();
    this.els.spin.classList.add("on");
    this.els.search.value = "";
    // Invalidate any page still in flight, and clear the paging state: a restart is a
    // different view of the file even when it is the same file.
    this.#pageGen++;
    this.#loadedFrom = 0;
    this.#newestOffset = 0;
    this.#reachedStart = false;
    this.#loading = false;
    // A live stream holds everything that has arrived, so nothing is unloaded above.
    this.#view.setMoreAbove(false);
    this.#renderState();
    if (tailCfg.retain !== null) this.#view.setFixedCapacity(tailCfg.retain);
    if (this.index) {
      // A completed archive never grows: no rate to report, no ticker to run.
      this.#view.stopRateTicker();
      this.#loadArchive();
    } else {
      this.#view.startRateTicker();
      this.#follow();
    }
  }
  async init() {
    // The standalone page has its own bootstrap, so it must fetch config too —
    // without this, tail length and retention settings applied to the dashboard
    // would silently not apply here.
    const cfg = await Api.config();
    applyTailConfig(cfg);
    applySeverityConfig(cfg);
    // No `applyLabel` here: `#chrome()` removes the dashboard header, so the
    // environment label element does not exist on this page.
    // Daemon reachability wiring, mirroring App: down tears down the follow and
    // stats streams so a dead daemon is not hammered by their reconnects; up
    // rebuilds both. `#start()` decides follow vs. archive, so resume goes
    // through it for the live case and does nothing extra for a finished file
    // (an archive has no stream to rebuild).
    network.onDown = () => {
      this.#stop();
      this.#stopStats();
      showDaemonBanner("Daemon unreachable — reconnecting…");
    };
    network.onUp = () => {
      hideDaemonBanner();
      if (!this.index) {
        this.#follow();
        this.#watchStats();
      }
    };
    this.#start();
    // The strip tracks the process, not the file: an archive view still shows
    // what the process is doing now.
    this.#watchStats();
  }
}
