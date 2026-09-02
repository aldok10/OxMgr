import { EVENTS, PROCESS_LIFECYCLE_EVENTS, tailCfg } from "./const.js";
import { network } from "../shell/network.js";

// Fetch with a timeout. The caller's signal and the timeout signal are
// composed with AbortSignal.any: whichever fires first aborts the request.
// Returns { signal, controller, clear }. The controller lets a caller abort a
// superseded request directly; the timeout aborts the same signal.
export const withTimeout = (timeoutMs, callerSignal) => {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  const signal = callerSignal ? AbortSignal.any([callerSignal, controller.signal]) : controller.signal;
  return { signal, controller, clear: () => clearTimeout(timer) };
};

// Standalone fetch helper (no class dependency). Reports reachability: a
// resolved fetch means the daemon answered; a network error is a failure.
export const apiRequest = async (path, opts = {}) => {
  const { signal, clear } = withTimeout(10000, opts.signal);
  try {
    const res = await fetch(path, { ...opts, signal, credentials: 'include' });
    clear();
    const body = await (res.headers.get("content-type")?.includes("json") ? res.json().catch(() => null) : res.text());
    if (!res.ok) throw new Error(body?.message ?? `HTTP ${res.status}`);
    network.reportSuccess();
    return body;
  } catch (err) {
    // the daemon unreachable and let the caller surface it.
    if (err.name === "AbortError") {
      if (opts.signal?.aborted) return undefined; // caller cancelled: quiet
      network.reportFailure();
      throw err;
    }
    // A non-OK HTTP status still proves the daemon is up (it answered); only
    // a thrown network error is evidence of unavailability. Broad catch here
    // is deliberate: an HTTP 503 is "sampling disabled", not "daemon gone".
    if (!(err instanceof Error && err.message?.startsWith("HTTP "))) network.reportFailure();
    throw err;
  }
};

// API client
export class Api {
  #bus; #procSrc = null; #logSrc = null; #tailAbort = null; #eventSrc = null; #logGen = 0; #eventAbort = null;
  constructor(bus) { this.#bus = bus; }
  async action(target, act) {
    try {
      const body = await apiRequest(`/api/processes/${encodeURIComponent(target)}/${act}`, { method: "POST" });
      // `apiRequest` returns `undefined` for a CALLER-CANCELLED request (see its
      // AbortError branch): cancellation is an expected outcome and must stay quiet,
      // per `dashboard-interaction-safety`. Without this guard the `?? \`${act} ok\``
      // fallback below would turn that `undefined` into a cheerful "restart ok" —
      // announcing a success for a request that never completed.
      //
      // Not reachable from here TODAY, because `action()` passes no `signal`, so the
      // caller-cancel branch cannot fire. Guarded anyway: the day someone threads an
      // AbortSignal through this call — which is the obvious next change, since every
      // other request in this file has one — the bug would appear in the announcement
      // channel, where a false success is worse than silence.
      if (body === undefined) return undefined;
      this.#bus.emit(EVENTS.ACT_DONE, { msg: body?.message ?? `${act} ok` });
      return body;
    } catch (err) { this.#bus.emit(EVENTS.ACT_ERR, { target, act, err: err.message }); throw err; }
  }
  static async config() {
    const { signal, clear } = withTimeout(3000);
    try {
      const res = await fetch("/api/config", { signal });
      clear();
      if (res.ok) network.reportSuccess();
      return res.ok ? res.json() : null;
    } catch {
      clear();
      // Any thrown error here is the daemon not answering, whether a network
      // error or the timeout firing. Both mean unreachable, so both report
      // failure. A resolved non-OK HTTP status is handled above (daemon
      // answered; no failure reported).
      network.reportFailure();
      return null;
    }
  }
  // Fetch last 100 lines from the log file so the viewer opens with context.
  // Tagged with the generation that requested it: an await here means a newer
  // request may have superseded us before the response lands. The tail fetch
  // keeps its own AbortController so a superseding logStream call aborts it
  // rather than races it.
  async #fetchTail(target, stream, gen) {
    const { signal, controller, clear } = withTimeout(5000);
    this.#tailAbort = controller;
    try {
      const res = await fetch(`/api/processes/${encodeURIComponent(target)}/logs?stream=${stream}&lines=${tailCfg.lines}`, { signal });
      clear();
      if (!res.ok || gen !== this.#logGen) return;
      const data = await res.json();
      if (gen !== this.#logGen) return;
      if (data.lines) this.#bus.emit(EVENTS.LOG_TAIL, { gen, lines: data.lines, bytes: data.bytes ?? 0 });
    } catch (err) {
      clear();
      if (err.name !== "AbortError") { /* ignore fetch errors, continue to stream */ }
    }
  }
  // Subscribe to live log lines via BusEvent. Closes any existing handle first:
  // overwriting it without closing is what leaked a connection per tab switch.
  #subscribeLog(target, stream, gen) {
    if (gen !== this.#logGen) return;
    this.#logSrc?.close();
    const eventType = stream === "stderr" ? "log:err" : "log:out";
    const src = new EventSource(`/api/events/stream?subscribe=${encodeURIComponent(eventType)}&process=${encodeURIComponent(target)}`);
    this.#logSrc = src;
    src.onmessage = evt => {
      if (gen !== this.#logGen) { src.close(); return; }
      try {
        const event = JSON.parse(evt.data);
        if (event.data?.line) this.#bus.emit(EVENTS.LOG_DATA, { gen, line: event.data.line });
      } catch { /* ignore parse errors */ }
    };
    src.onerror = () => { if (gen === this.#logGen) { network.reportFailure(); this.#bus.emit(EVENTS.LOG_ERR); } };
  }
  // Fetch tail from log file first, then subscribe to BusEvent stream for new
  // logs. Each call takes a new generation, so an in-flight predecessor cannot
  // install its subscription or deliver its lines after being superseded.
  async logStream(target, stream) {
    this.stopLog();
    const gen = ++this.#logGen;
    await this.#fetchTail(target, stream, gen);
    this.#subscribeLog(target, stream, gen);
  }
  get logGen() { return this.#logGen; }
  stopLog() {
    this.#logGen++;
    this.#tailAbort?.abort();
    this.#tailAbort = null;
    this.#logSrc?.close();
    this.#logSrc = null;
  }
  // Global event stream for process status updates
  /// Unified stream for process data and events
  async unifiedStream(interval) {
    this.stopUnified();
    this.#bus.emit(EVENTS.PROC_START);

    this.#eventAbort = new AbortController();
    const { signal } = this.#eventAbort;
    const url = `/api/stream?subscribe=processes,snapshot,memory,cpu,load_average,filesystems,network,components&interval_ms=${interval}`;
    
    try {
      const response = await fetch(url, { signal, credentials: 'include' });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      network.reportSuccess();
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
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
            if (eventType === "processes") {
              try { this.#bus.emit(EVENTS.PROC_DATA, JSON.parse(data)); } catch (e) { console.error("Parse error:", e); }
            } else if (PROCESS_LIFECYCLE_EVENTS.includes(eventType)) {
              try { this.#bus.emit(EVENTS.EVENT_PROCESS, JSON.parse(data)); } catch (e) { console.error("Parse error:", e); }
            }
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
      console.error("Unified stream failed:", err);
      if (err.name !== "AbortError") {
        network.reportFailure();
        this.#bus.emit(EVENTS.PROC_ERR);
      }
    }
  }
  get unifiedLive() { return this.#eventAbort !== null; }
  stopUnified() { 
    this.#eventAbort?.abort(); 
    this.#eventAbort = null; 
  }
}
