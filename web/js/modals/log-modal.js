import { EMPTY, EVENTS, STREAMS, isPanelFullViewport, sel, tailCfg } from "../core/const.js";
import { apiRequest } from "../core/api.js";
import { fmt } from "../format/fmt.js";
import { makeLine } from "../ansi/ansi.js";
import { LineBuffer } from "../log/LineBuffer.js";
import { LogView, tailNote } from "../log/LogView.js";
import { Modal, fileRow } from "./modal.js";
import { advisoryState, detailFindings } from "../shell/stats.js";

// Log modal: stdout / stderr live streams plus a files listing.
export class LogModal extends Modal {
  #api; #spin; #bus; target = null; stream = "stdout";
  #buffer = new LineBuffer(); #view = null; #resizeObs = null; #scrollQueued = false; #tailNote = null;
  constructor(overlay, panel, api, bus, spin) {
    super(overlay, panel);
    this.#api = api; this.#bus = bus; this.#spin = spin;
    this.els = {
      title: sel("#log-title"),
      body: sel("#log-body"),
      meta: sel("#log-meta"),
      spin: sel("#log-spinner"),
      seg: sel("#log-stream-seg"),
      jump: sel("#log-jump"),
      download: sel("#log-download"),
      openPage: sel("#log-open-page"),
      search: sel("#log-search"),
      pause: sel("#log-pause"),
      files: null,
    };
    this.#view = new LogView(this.els.body, this.#buffer);
    this.#view.onState(() => this.#renderState());
    this.els.files = document.createElement("div");
    this.els.files.className = "log-files";
    this.els.files.style.display = "none";
    this.els.body.appendChild(this.els.files);
    this.#bind();
  }
  get #isFiles() { return this.stream === "files"; }
  #bind() {
    // Listeners registered once, at construction: never per open, so they
    // cannot accumulate across the viewer's lifetime.
    this.#bindBodyScroll();
    this.#bindBus();
    this.#bindControls();
    this.#bindMoreToggle();
    this.#bindGeometry();
    this.#bindVisibility();
  }
  #bindBodyScroll() {
    this.els.body.addEventListener("scroll", () => {
      if (this.#scrollQueued || this.#isFiles) return;
      this.#scrollQueued = true;
      requestAnimationFrame(() => { this.#scrollQueued = false; this.#view.onScroll(); });
    }, { passive: true, ...this._signal() });
  }
  #bindBus() {
    const sig = this._signal();
    this.#bus.on(EVENTS.LOG_TAIL, ({ gen, lines, bytes }) => {
      if (gen !== this.#api.logGen) return; // superseded request
      this.#spin.hide(this.els.spin, "log");
      this.#tailNote = tailNote(Array.isArray(lines) ? lines.length : 0, bytes);
      this.#view.load(lines, this.stream);
    }, sig);
    this.#bus.on(EVENTS.LOG_DATA, ({ gen, line }) => {
      if (gen !== this.#api.logGen || !line || this.#isFiles) return;
      this.#view.add(makeLine(line, this.stream, 0));
    }, sig);
    this.#bus.on(EVENTS.LOG_ERR, () => this.#spin.hide(this.els.spin, "log"), sig);
  }
  #bindControls() {
    this.els.seg.addEventListener("click", evt => {
      const btn = evt.target.closest("button[data-stream]");
      if (!btn || btn.dataset.stream === this.stream) return;
      this.stream = btn.dataset.stream;
      this.#highlight();
      this.#start();
    }, this._signal());
    // Debounced: a keystroke must not trigger a full window rebuild.
    let searchTimer = 0;
    this.els.search.addEventListener("input", evt => {
      clearTimeout(searchTimer);
      const value = evt.target.value;
      searchTimer = setTimeout(() => this.#view.setQuery(value), 120);
    }, this._signal());
    this.els.pause.addEventListener("click", () => {
      this.#view.setPaused(!this.#view.paused);
      this.#renderState();
    }, this._signal());
    this.els.jump.addEventListener("click", () => this.#view.jumpToLatest(), this._signal());
    this.els.download.addEventListener("click", () => this.#download(), this._signal());
    this.els.openPage.addEventListener("click", () => this.#openPage(), this._signal());
    sel("#log-close").addEventListener("click", () => this.close(), this._signal());
    sel("#log-refresh").addEventListener("click", () => this.#start(), this._signal());
  }
  #bindMoreToggle() {
    // Secondary-controls toggle. The button itself only exists at phone width — CSS keeps
    // the group inline for a pointer, so this stays inert on a desktop. Syncing on resize
    // rather than only at open, because a rotation crosses the breakpoint mid-session and
    // would otherwise leave the group hidden with no visible way to reveal it.
    const moreBtn = sel("#log-more");
    const moreGroup = sel("#log-more-group");
    const syncMore = () => {
      const collapsible = isPanelFullViewport();
      moreBtn.hidden = !collapsible;
      if (!collapsible) {
        moreGroup.classList.remove("open");
        moreBtn.setAttribute("aria-expanded", "false");
      }
    };
    moreBtn.addEventListener("click", () => {
      const open = moreGroup.classList.toggle("open");
      moreBtn.setAttribute("aria-expanded", open ? "true" : "false");
      // Revealing a row of controls changes the body height the log viewer measures.
      this.onGeometryChange?.();
    }, this._signal());
    window.addEventListener("resize", syncMore, this._signal());
    syncMore();
  }
  #bindGeometry() {
    if (typeof ResizeObserver === "function") {
      this.#resizeObs = new ResizeObserver(() => { if (!this.#isFiles) this.#view.relayout(); });
    }
    // Maximise/restore changes the viewport height, so the window capacity and
    // the bottom anchor both have to be recomputed. ResizeObserver covers it
    // where available; this makes the behaviour explicit and not dependent on
    // observer timing, which is what left a short panel with a stale window.
    this.onGeometryChange = () => { if (!this.#isFiles) this.#view.relayout(); };
  }
  #bindVisibility() {
    document.addEventListener("visibilitychange", () => {
      if (!this.target) return;
      if (document.hidden) this.#view.suspend(); else this.#view.resume();
    }, this._signal());
  }
  #highlight() { this.els.seg.querySelectorAll("button").forEach(btn => btn.classList.toggle("active", btn.dataset.stream === this.stream)); }
  #renderState() {
    if (this.#isFiles) {
      this.els.meta.textContent = "log files";
      this.els.jump.classList.remove("on");
      return;
    }
    const paused = this.#view.paused;
    this.els.pause.textContent = paused ? "Resume" : "Pause";
    this.els.pause.classList.toggle("primary", paused);
    const total = this.#buffer.length;
    // Under a filter the interesting number is the match count, with the
    // buffer size as context.
    const countText = this.#view.filtered
      ? `${fmt.count(this.#view.matches)} of ${fmt.count(total)} lines`
      : `${fmt.count(total)} lines`;
    const html = [
      fmt.esc(`${this.stream} · live`),
      `<span class="count">${fmt.esc(countText)}</span>`,
    ];
    // Arrival rate: describes what the process is emitting, so it is reported even
    // while paused — that is when an operator most wants to know what they are
    // missing.
    const rate = this.#view.rate();
    if (rate.idle) html.push(`<span class="rate idle">idle</span>`);
    else html.push(`<span class="rate">${fmt.esc(rate.text)}</span>`);
    // Two different pauses, never ambiguous: explicit (button) vs implicit
    // (scrolled away). Both keep retaining output, which the label says so the
    // operator knows nothing is being dropped beyond the buffer bounds.
    if (paused) html.push(`<span class="paused">paused · still retaining</span>`);
    else if (!this.#view.following) html.push(`<span class="paused">scrolled back</span>`);
    // Retention capacity in force, with why it last moved. Shown as a title so it
    // is available without adding another figure to a footer that already carries
    // several.
    const ret = this.#view.retention();
    html.push(`<span class="cap" title="${fmt.esc(ret.reason)}${ret.memoryMeasurable ? "" : " · heap not measurable here"}">cap ${fmt.esc(fmt.count(ret.capacity))}</span>`);
    if (this.#tailNote) html.push(`<span class="paused">${fmt.esc(this.#tailNote)}</span>`);
    this.els.meta.innerHTML = html.join(" · ");

    const missed = this.#view.missed;
    this.els.jump.classList.toggle("on", !this.#view.following);
    this.els.jump.textContent = missed ? `↓ ${fmt.count(missed)} new` : "↓ Jump to latest";
  }
  #logUrl(base) {
    const stream = this.#isFiles ? "stdout" : this.stream;
    return `${base}?stream=${encodeURIComponent(stream)}`;
  }
  #download() {
    if (!this.target) return;
    window.location.href = this.#logUrl(`/api/processes/${encodeURIComponent(this.target)}/logs/download`);
  }
  #openPage() {
    if (!this.target) return;
    window.open(this.#logUrl(`/logs/${encodeURIComponent(this.target)}`), "_blank", "noopener");
  }
  async #loadFiles() {
    this.#spin.show(this.els.spin, "log");
    try {
      const data = await apiRequest(`/api/processes/${encodeURIComponent(this.target)}/logs/files`);
      this.#spin.hide(this.els.spin, "log");
      if (!this.#isFiles) return; // tab changed while loading
      const frag = document.createDocumentFragment();
      for (const stream of STREAMS) {
        const entries = (data.files ?? []).filter(entry => entry.stream === stream);
        if (!entries.length) continue;
        const group = document.createElement("div");
        group.className = "log-files-group";
        const head = document.createElement("h3");
        head.textContent = stream;
        group.appendChild(head);
        for (const entry of entries) group.appendChild(fileRow(entry, this.target));
        frag.appendChild(group);
      }
      if (!frag.childNodes.length) {
        const empty = document.createElement("div");
        empty.className = "empty";
        empty.textContent = "No log files yet.";
        frag.appendChild(empty);
      }
      this.els.files.replaceChildren(frag);
    } catch (err) {
      this.#spin.hide(this.els.spin, "log");
      this.els.files.replaceChildren(Object.assign(document.createElement("div"), { className: "empty", textContent: `Failed to list log files: ${err.message}` }));
    }
  }
  #start() {
    // Switching modes tears the live subscription down unconditionally: this
    // is the single place a subscription can be replaced.
    this.#api.stopLog();
    this.#view.reset();
    this.#renderState();

    if (this.#isFiles) {
      this.els.body.classList.remove("stderr");
      this.els.files.style.display = "";
      this.els.files.replaceChildren();
      // Search has nothing to act on in a file listing: hide it rather than
      // leave it visible and inert.
      this.els.search.hidden = true;
      this.#view.setHidden(true);
      // A file listing has no arriving lines, so nothing to report a rate for.
      this.#view.stopRateTicker();
      this.els.body.scrollTop = 0;
      this.#resizeObs?.disconnect();
      this.#loadFiles();
      return;
    }
    this.els.search.hidden = false;
    this.#view.setHidden(false);
    // A configured retention value pins capacity; absent leaves it adaptive.
    if (tailCfg.retain !== null) this.#view.setFixedCapacity(tailCfg.retain);
    this.#view.startRateTicker();
    this.els.files.style.display = "none";
    this.els.body.classList.toggle("stderr", this.stream === "stderr");
    this.#resizeObs?.observe(this.els.body);
    this.#spin.show(this.els.spin, "log");
    this.#api.logStream(this.target, this.stream);
  }
  show(name, stream = "stdout") {
    this.target = name; this.stream = STREAMS.includes(stream) || stream === "files" ? stream : "stdout";
    this.els.title.textContent = `Logs — ${name}`;
    this.#highlight(); this.open(); this.#start();
  }
  close() {
    super.close();
    this.target = null;
    this.#api.stopLog();
    this.#resizeObs?.disconnect();
    this.#view.destroy();
    this.els.files.replaceChildren();
  }
  destroy() {
    super.destroy();
    this.close();
  }
  // The daemon came back while the modal was open: rebuild the live subscription
  // so the operator does not have to close and reopen the modal to see fresh lines.
  // `#start()` tears the old subscription down unconditionally, so the guard is
  // simply "is the modal open for a live process".
  restartIfOpen() {
    if (this.target && !this.#isFiles) this.#start();
  }
}
// Detail modal helpers (pure functions)
const detailGrid = (rows) => rows.map(([key, val]) => `<div class="k">${fmt.esc(key)}</div><div class="v">${fmt.esc(val ?? "-")}</div>`).join("");
const detailSection = (title, rows) => `<div class="detail-section"><h3>${fmt.esc(title)}</h3><div class="detail-grid">${detailGrid(rows)}</div></div>`;
const detailErrorSection = (error) => `<div class="detail-section error-section"><h3>Last Error</h3><pre class="error-pre">${fmt.esc(error)}</pre></div>`;
const detailOverview = (proc) => detailSection("Overview", [["ID", proc.id], ["Name", proc.name], ["Namespace", proc.namespace], ["Status", proc.status], ["Desired", proc.desired_state], ["PID", proc.pid],
  ["Uptime", fmt.up(proc)], ["Restarts", `${proc.restart_count}/${proc.max_restarts}`], ["CPU", `${fmt.pct(proc.cpu_percent)}%`], ["Memory", fmt.bytes(proc.memory_bytes)],
  ["Command", `${proc.command} ${(proc.args ?? []).join(" ")}`], ["CWD", proc.cwd], ["Exit Code", proc.last_exit_code]]);
const detailEnv = (proc) => detailSection("Environment", Object.keys(proc.env ?? {}).length ? Object.entries(proc.env) : [["(redacted)", EMPTY]]);
const detailLimits = (lim) => detailSection("Resource Limits", [["Max Memory", lim.max_memory_mb ? `${lim.max_memory_mb} MB` : "-"], ["Max CPU", lim.max_cpu_percent == null ? "-" : `${lim.max_cpu_percent}%`]]);
const detailHealth = (hck) => detailSection("Health Check", [["Command", hck.command], ["Interval", `${hck.interval_secs}s / timeout ${hck.timeout_secs}s`], ["Max Failures", hck.max_failures]]);
// Entry point to the standalone log page, per stream. Rendered as links rather
// than buttons so middle-click and open-in-new-tab work the way an operator
// expects of a page they may want to keep open beside the dashboard.
const detailLogLinks = (proc) => {
  const link = (stream) => {
    const href = `/logs/${encodeURIComponent(proc.name)}?stream=${stream}`;
    return `<a class="detail-link" href="${href}" target="_blank" rel="noopener">${stream} →</a>`;
  };
  return `<div class="detail-section"><h3>Log Pages</h3><div class="detail-links">${STREAMS.map(link).join("")}</div></div>`;
};

/// Configuration advisories, in the detail panel.
///
/// Placed second, above the paths and the environment: the marker in the table leads here, so the
/// reason an operator came looking has to be near the top rather than below four sections they
/// have to scroll past.
///
/// Each entry states its consequence in full. The table marker's tooltip truncates to three; this
/// is the surface where the whole thing belongs, and the evidence names the offending setting so
/// the operator knows what to change.
const detailAdvisories = (proc) => {
  const list = advisoryState.get(proc.name) ?? [];
  if (!list.length) return "";
  const rows = list.map(item => {
    const evidence = (item.evidence ?? [])
      .map(entry => `${fmt.esc(entry.setting)} = ${fmt.esc(entry.value)}`)
      .join(", ");
    return `<div class="advisory-row">`
      + `<span class="advisory-flag ${fmt.esc(item.severity)}">!</span>`
      + `<div class="advisory-text">`
      + `<div class="advisory-consequence">${fmt.esc(item.consequence)}</div>`
      + (evidence ? `<div class="advisory-evidence">${evidence}</div>` : "")
      + `</div></div>`;
  }).join("");
  // Titled with the count so the heading itself says how much there is to read.
  return `<div class="detail-section"><h3>Configuration Advisories (${list.length})</h3>${rows}`
    + `<p class="advisory-note">Informational. Nothing here has been changed or blocked — these `
    + `describe how the current settings will behave.</p></div>`;
};

/// Cluster shape, in the detail panel (cluster-instance-visibility).
///
/// Requested and observed as two separate figures side by side (§D5): they
/// legitimately differ during startup or after a worker death, so a gap is VISIBLE
/// without either figure being turned into a verdict about the other. A derived
/// requested count says so instead of borrowing the observed number — reporting the
/// observed count as requested would make every shortfall undetectable.
const detailCluster = (proc) => {
  if (!proc.cluster_mode || !proc.cluster) return "";
  const req = proc.cluster.requested ?? {};
  const obs = proc.cluster.observed ?? {};
  const requested = req.derived ? "derived by the runtime at startup" : fmt.esc(req.count);
  let observed;
  if (obs.status === "ok") {
    observed = `${fmt.esc(obs.workers)} running`;
  } else {
    observed = `unavailable (${fmt.esc(obs.reason ?? "unknown reason")})`;
  }
  return `<div class="detail-section"><h3>Cluster</h3>`
    + `<div class="detail-grid">`
    + `<div class="k">Requested workers</div><div class="v">${requested}</div>`
    + `<div class="k">Observed workers</div><div class="v">${observed}</div>`
    + `</div>`
    + `<p class="descendant-freshness">Observed count sampled ${fmt.esc(fmt.when(obs.observed_at))} `
    + `— up to one sampling cycle old.</p></div>`;
};

/// Attributed descendants, in the detail panel (managed-process-child-visibility).
///
/// Same three-shape contract as the table expansion: ok lists the children and the
/// descendants-only subtree total; observed-none says so in words; unavailable states
/// its reason. The freshness line is repeated here deliberately (§D4) — the detail
/// panel must not read as more current than its source.
const detailChildren = (proc) => {
  const d = proc.descendants;
  if (!d) return "";
  let body;
  if (d.status === "unavailable") {
    body = `<p class="descendant-unavailable">Descendants unavailable: ${fmt.esc(d.reason ?? "unknown reason")}</p>`;
  } else if (!d.descendants.length) {
    body = `<p class="descendant-unavailable">Observed by host-wide sampling: no descendant processes.</p>`;
  } else {
    const rows = d.descendants.map(child => {
      const depthLabel = child.depth > 1 ? ` (depth ${child.depth})` : "";
      return `<div class="k">${fmt.esc(child.name)}${depthLabel} · pid ${fmt.esc(child.pid)}</div>`
        + `<div class="v">${fmt.esc(fmt.pct(child.cpu_percent))}% CPU · ${fmt.esc(fmt.bytes(child.memory_bytes))}</div>`;
    }).join("");
    const truncatedNote = d.truncated
      ? ` <span class="descendant-truncated">(list truncated; totals cover all observed)</span>`
      : "";
    body = `<div class="detail-grid">${rows}</div>`
      + `<p class="descendant-total">Subtree total, incl. descendants: `
      + `<strong>${fmt.esc(fmt.pct(d.descendants_cpu_percent))}%</strong> CPU, `
      + `<strong>${fmt.esc(fmt.bytes(d.descendants_memory_bytes))}</strong> RAM${truncatedNote}</p>`;
  }
  return `<div class="detail-section"><h3>Descendants</h3>`
    + `<p class="descendant-freshness">Child figures sampled ${fmt.esc(fmt.when(d.observed_at))} `
    + `— up to one sampling cycle older than this panel's own figures.</p>${body}</div>`;
};

export const detailBody = (proc) => {
  const sections = [
    detailOverview(proc),
    detailAdvisories(proc),
    detailFindings(proc),
    detailCluster(proc),
    detailChildren(proc),
    detailLogLinks(proc),
    detailSection("Paths", [["Stdout Log", proc.stdout_log], ["Stderr Log", proc.stderr_log]]),
  ];
  if (proc.last_error) sections.push(detailErrorSection(proc.last_error));
  sections.push(detailEnv(proc));
  if (proc.resource_limits) sections.push(detailLimits(proc.resource_limits));
  if (proc.health_check) sections.push(detailHealth(proc.health_check));
  return sections.join("");
};
