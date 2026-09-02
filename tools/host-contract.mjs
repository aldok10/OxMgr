// Observable-contract harness for HostPanel (host-panel-decomposition 1.1).
//
// Loads the SAME concatenated bundle the daemon serves (header + core modules +
// host files), drives the panel with recorded live payloads, and dumps every
// region's innerHTML plus structural counters to a JSON file.
//
// Run:  node tools/host-contract.mjs <out.json>
// The BEFORE snapshot lives in openspec/changes/host-panel-decomposition/;
// after the refactor this must produce byte-identical output.

import { readFileSync, writeFileSync } from "node:fs";
import vm from "node:vm";

// Mirrors DASHBOARD_JS_BUNDLE in src/daemon/http/mod.rs EXACTLY — same files,
// same order. The host renderers call severityBand/severityCue which live in
// dashboard.js (evaluated later in the same IIFE), so the full order matters.
const parts = [
  "web/js/header.js",
  "web/js/core/const.js",
  "web/js/core/bus.js",
  "web/js/core/store.js",
  "web/js/core/api.js",
  "web/js/core/spin.js",
  "web/js/core/announce.js",
  "web/js/format/fmt.js",
  "web/js/ansi/ansi.js",
  "web/js/log/LineBuffer.js",
  "web/js/log/RowPool.js",
  "web/js/log/CapacityController.js",
  "web/js/log/ChunkRunner.js",
  "web/js/log/LogView.js",
  "web/js/host/shell.js",
  "web/js/host/metricRenderers.js",
  "web/js/host/canvas.js",
  "web/js/modals/modal.js",
  "web/js/modals/log-modal.js",
  "web/js/modals/detail-modal.js",
  "web/js/modals/confirm-modal.js",
  "web/js/shell/network.js",
  "web/js/shell/stats.js",
  "web/js/shell/table.js",
  "web/js/shell/controls.js",
  "web/js/shell/app.js",
  "web/js/logpage/logpage.js",
  "web/dashboard.js",
];
const src = parts.map((p) => readFileSync(p, "utf8")).join("\n")
  + "\nglobalThis.__HostPanel = HostPanel;\n"
  + readFileSync("web/js/footer.js", "utf8");

// ---- recorded payloads (live daemon, 2026-08-21) --------------------------
const hostSnapshot = JSON.parse(readFileSync("/tmp/opencode/host-snapshot.json", "utf8"));
const consumersPayload = JSON.parse(readFileSync("/tmp/opencode/consumers-payload.json", "utf8"));

// ---- fake DOM --------------------------------------------------------------
const allEls = [];
class FakeElement {
  constructor(tag = "div") {
    allEls.push(this);
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.dataset = {};
    this.style = { setProperty() {} };
    this._html = "";
    this.className = "";
    this._classes = new Set();
    this.clientWidth = 800;
    this.attributes = {};
    const self = this;
    this.classList = {
      add(...cs) { cs.forEach((c) => self._classes.add(c)); },
      remove(...cs) { cs.forEach((c) => self._classes.delete(c)); },
      contains(c) { return self._classes.has(c); },
      toggle(c, force) {
        if (force === undefined) { self._classes.has(c) ? self._classes.delete(c) : self._classes.add(c); }
        else if (force) self._classes.add(c); else self._classes.delete(c);
      },
    };
    this._listeners = {};
  }
  get innerHTML() { return this._html; }
  set innerHTML(v) { this._html = v; this.children = []; this._qCache = {}; }
  set textContent(v) { this._text = String(v); }
  get textContent() { return this._text ?? ""; }
  appendChild(c) { this.children.push(c); return c; }
  insertBefore(c) { this.children.push(c); return c; }
  removeChild(c) { this.children = this.children.filter((x) => x !== c); return c; }
  addEventListener(type, fn) { (this._listeners[type] ??= []).push(fn); }
  removeEventListener() {}
  querySelector(q) {
    this._qCache ??= {};
    if (!this._qCache[q]) {
      const child = new FakeElement("div");
      this._qCache[q] = [child];
    }
    return this._qCache[q][0];
  }
  /// Parses simple attribute selectors ("[data-dim]") against this element's
  /// innerHTML and returns one cached FakeElement per distinct value, so the
  /// REAL wiring code attaches listeners we can later dispatch. This is how the
  /// four consumer modes are driven through the genuine click handlers.
  querySelectorAll(q) {
    const m = /^\[([a-zA-Z-]+)\]$/.exec(q);
    if (!m) return [this.querySelector(q)];
    this._qCache ??= {};
    if (this._qCache[q]) return this._qCache[q];
    const seen = new Set();
    const out = [];
    for (const match of this._html.matchAll(new RegExp(`${m[1]}="([^"]*)"[^>]*`, "g"))) {
      const val = match[1];
      if (seen.has(val)) continue;
      seen.add(val);
      const child = new FakeElement("button");
      child.dataset[m[1].replace(/^data-/, "").replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = val;
      out.push(child);
    }
    this._qCache[q] = out;
    return out;
  }
  closest() { return null; }
  // Counted rather than silent: the phone-overlay scenario asserts that reveal
  // moves focus to the close control and conceal restores it to the door. The
  // counter never enters a dump, so recorded contracts are unaffected.
  focus() { this._focusCount = (this._focusCount ?? 0) + 1; }
  setAttribute(k, v) { this.attributes[k] = String(v); }
  getAttribute(k) { return this.attributes[k] ?? null; }
}

const registry = new Map();
function elFor(selector) {
  if (!registry.has(selector)) registry.set(selector, new FakeElement());
  return registry.get(selector);
}

const documentStub = {
  hidden: false,
  documentElement: new FakeElement("html"),
  querySelector: (q) => elFor(q),
  querySelectorAll: () => [],
  // Recorded, not discarded: the Escape-dismissal path registers on document,
  // and the phone-overlay scenario must be able to dispatch a real keydown.
  addEventListener(t, fn) { (docListeners[t] ??= []).push(fn); },
  createElement: (t) => new FakeElement(t),
  body: new FakeElement("body"),
};
documentStub.body.querySelectorAll = () => [];
// index.html ships the host panel with the `hidden` attribute; the harness must
// start from the same state or every hidden-guard reads falsy and passes vacuously.
elFor("#host-panel").hidden = true;

const winListeners = {};
const docListeners = {};
const windowStub = {
  addEventListener(t, fn) { (winListeners[t] ??= []).push(fn); },
  removeEventListener() {},
  innerWidth: 1280,
  location: { pathname: "/", href: "http://localhost/" },
  history: { pushState() {}, replaceState() {} },
  localStorage: {
    _s: new Map(),
    getItem(k) { return this._s.get(k) ?? null; },
    setItem(k, v) { this._s.set(k, String(v)); },
    removeItem(k) { this._s.delete(k); },
  },
};

class FakeEventSource {
  constructor(url) { this.url = url; this._l = {}; FakeEventSource.last = this; }
  addEventListener(t, fn) { (this._l[t] ??= []).push(fn); }
  close() { this.closed = true; }
  emit(type, dataObj) {
    for (const fn of this._l[type] ?? []) fn({ data: JSON.stringify(dataObj) });
  }
}

let fetchMode = "pending"; // "pending" | "payload" | "503"
const fetchCalls = [];
async function fetchStub(url, opts) {
  fetchCalls.push({ url, mode: fetchMode });
  if (fetchMode === "pending") return new Promise(() => {});
  if (fetchMode === "503") return { ok: false, status: 503, json: async () => ({}) };
  return { ok: true, status: 200, json: async () => consumersPayload };
}

// CSSOM stubs for #applyStyles
const fakeSheet = { cssRules: [], insertRule() { return 0; } };
Object.defineProperty(documentStub, "styleSheets", { value: [fakeSheet] });
documentStub.createElement("style").sheet = fakeSheet;

const timers = { count: 0, fns: [] };
const sandbox = {
  document: documentStub,
  window: windowStub,
  location: windowStub.location,
  history: windowStub.history,
  localStorage: windowStub.localStorage,
  getComputedStyle: () => ({ position: "static" }),
  matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
  URLSearchParams,
  URL,
  CustomEvent: class CustomEvent { constructor(type, opts = {}) { this.type = type; Object.assign(this, opts); } },
  Event: class Event { constructor(type) { this.type = type; } },
  EventSource: FakeEventSource,
  fetch: fetchStub,
  setTimeout,
  clearTimeout,
  setInterval: (fn) => { timers.count += 1; timers.fns.push(fn); return timers.count; },
  clearInterval: () => { timers.count -= 1; },
  AbortController,
  requestAnimationFrame: (fn) => fn(),
  console,
  JSON,
  Math,
  Number,
  String,
  Array,
  Object,
  Map,
  Set,
  Promise,
  Date,
  isNaN,
  parseInt,
  parseFloat,
  globalThis: {},
};
sandbox.window.document = documentStub;
sandbox.globalThis = sandbox;
vm.createContext(sandbox);

vm.runInContext(src, sandbox, { filename: "bundle.js" });

const HostPanel = sandbox.__HostPanel;

// ---- drive -----------------------------------------------------------------
const tick = () => new Promise((r) => setTimeout(r, 20));

// Dumps the CURRENT innerHTML of every live element, keyed by class (region
// identity). Re-renders overwrite the same DOM node, so a mode click changes
// existing elements rather than creating new ones — the dump must reflect
// present state, not creation deltas. Later elements win key collisions,
// which keeps the most recent panel's regions authoritative.
const dumpCurrent = () => {
  const out = {};
  for (let i = 0; i < allEls.length; i++) {
    const el = allEls[i];
    if (!el._html) continue;
    const key = el.className || `el#${i}`;
    out[key] = el._html;
  }
  return out;
};

const report = {
  meta: {
    generatedBy: "tools/host-contract.mjs",
    note: "byte-identical innerHTML per scenario key is the behaviour contract",
    regionOrder: HostPanel.REGION_ORDER,
  },
  eventSourceUrl: null,
  fetchCalls,
  teardown: null,
  scenarios: {},
};

// The nav-bar "Host" control is the panel's only door. It was obtained through
// sel("#host-open") inside the constructor, so it lives in the registry. Each
// constructed panel attaches ANOTHER listener to the same shared element, so a
// click must dispatch the LAST attached handler — the one belonging to the
// panel under test.
const click = (btn) => btn._listeners.click[btn._listeners.click.length - 1]();

// Scenario H — hidden default: a fresh load with no stored choice must be
// fully inert. No EventSource constructed, zero consumer fetches, even when a
// snapshot somehow arrives (it cannot — the stream is not subscribed).
{
  const m = allEls.length;
  const before = fetchCalls.length;
  const pA = new HostPanel();
  pA.start(); // app.js calls this unconditionally; the guard must hold
  const esCreated = !!FakeEventSource.last;
  report.scenarios.hidden_default = {
    ...dumpCurrent(),
    __assert: { esCreated, fetchesIssued: fetchCalls.length - before },
  };
}

// Scenario O — explicit open: clicking "Host" reveals the panel, subscribes
// the stream, and issues the first consumer poll. A snapshot then renders all
// regions into the now-visible panel.
let consumerButtons = null;
{
  const m = allEls.length;
  fetchMode = "payload";
  const panel = new HostPanel();
  panel.start(); // still hidden — must stay inert until the click below
  click(registry.get("#host-open"));
  await tick();
  report.eventSourceUrl = FakeEventSource.last?.url ?? null;
  FakeEventSource.last.emit("snapshot", hostSnapshot);
  await tick();
  report.scenarios.opened_sampled_cpu_flat = {
    ...dumpCurrent(),
    __assert: { esCreated: !!FakeEventSource.last },
  };

  // Scenarios C–E — the four modes via the REAL click handlers. The buttons
  // were parsed out of the consumers region's innerHTML by querySelectorAll.
  const consumersEl = [...allEls.slice(m)]
    .reverse()
    .find((el) => String(el.className).includes("host-consumers"));
  consumerButtons = consumersEl ? consumersEl.querySelectorAll("[data-dim]") : [];
  for (const dim of ["cpu-tree", "memory", "memory-tree"]) {
    const btn = consumerButtons.find((b) => b.dataset.dim === dim);
    if (!btn) continue;
    click(btn);
    await tick();
    report.scenarios[`opened_sampled_${dim.replace("-", "_")}`] = dumpCurrent();
  }

  // Scenario F — unavailable: a 503 is an answer, not an error. Fired through
  // the captured poll-timer callback, exactly as the browser timer would.
  fetchMode = "503";
  timers.fns[timers.fns.length - 1]();
  await tick();
  report.scenarios.opened_unavailable_503 = dumpCurrent();

  // Scenario X — conceal: closing stops everything. The source closes, the
  // timer disarms, and a subsequent start() (app.js calls it unconditionally)
  // must NOT reconnect while hidden.
  click(registry.get("#host-close"));
  const closedAfterConceal = FakeEventSource.last.closed === true;
  const intervalsWhileHidden = timers.count;
  panel.start();
  report.teardown = {
    sourceClosedOnConceal: closedAfterConceal,
    intervalsStillArmed: intervalsWhileHidden,
    // start() while hidden must be a no-op: same source, still closed, no new timer.
    reconnectBlockedWhileHidden:
      FakeEventSource.last.closed === true && timers.count === 0,
  };
}

writeFileSync(process.argv[2] ?? "/tmp/opencode/host-contract.json", JSON.stringify(report, null, 1));
console.log(`scenarios: ${Object.keys(report.scenarios).join(", ")}`);
console.log(`teardown: ${JSON.stringify(report.teardown)}`);

// Scenario P (opt-in, `--phone`) — phone overlay at <=700px. Kept out of the
// default run so the recorded bundle contracts stay byte-stable: this scenario
// exercises reveal/conceal BEHAVIOUR at the DOM level, not region rendering.
// Focus is counted on the fake elements; real paint and focus ORDER at true
// viewport widths remain a manual browser check.
if (process.argv[3] === "--phone") {
  const panel = new HostPanel();
  panel.start(); // hidden — inert until the door is clicked
  windowStub.innerWidth = 640;
  const openBtn = registry.get("#host-open");
  click(openBtn);
  const panelEl = registry.get("#host-panel");
  const closeBtn = registry.get("#host-close");
  const overlayClassOnOpen = panelEl.classList.contains("host-overlay-open");
  const bodyForcedOpen = panelEl.hidden === false;
  const focusMovedToClose = (closeBtn._focusCount ?? 0) > 0;
  for (const fn of docListeners.keydown ?? []) fn({ key: "Escape" });
  report.phone_overlay = {
    overlayClassOnOpen,
    bodyForcedOpen,
    focusMovedToClose,
    escapeConceals: panelEl.hidden === true,
    overlayClassRemoved: !panelEl.classList.contains("host-overlay-open"),
    focusRestoredToDoor: (openBtn._focusCount ?? 0) > 0,
  };
  windowStub.innerWidth = 1280;
  console.log(`phone_overlay: ${JSON.stringify(report.phone_overlay)}`);
}

