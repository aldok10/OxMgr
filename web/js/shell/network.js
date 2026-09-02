import { sel } from "../core/const.js";
import { announce } from "../core/announce.js";

// Daemon reachability gate. Every poller and stream reports success through
// reportSuccess and failure through reportFailure; when reachability flips,
// the holder (App on the dashboard, LogPage on the log page) is told to stop
// and restart its channels. A single failure does not flip the state: streams
// blip, and the dashboard must not tear itself down every time one does.
// Three consecutive failures are a dead daemon (EventSource's own reconnect
// retries every few seconds, so "consecutive" here is a few seconds of
// silence). Recovery goes through a slow liveness probe so the daemon coming
// back is noticed without the full poller set hammering it before it is up.
class NetworkMonitor {
  static THRESHOLD = 3;       // consecutive failures before declaring the daemon down
  static PROBE_MS = 5000;     // liveness probe cadence while down
  static PROBE_TIMEOUT_MS = 3000; // per-probe cap; a dead daemon answers nothing
  #failed = 0; #down = false; #probeTimer = null;
  /** Called when the daemon is declared down. The holder stops its channels. */
  onDown = null;
  /** Called when the daemon is declared reachable again. The holder restarts. */
  onUp = null;
  get down() { return this.#down; }
  reportFailure() {
    if (this.#down) return;
    if (++this.#failed < NetworkMonitor.THRESHOLD) return;
    this.#failed = 0;
    this.#down = true;
    this.#startProbe();
    this.onDown?.();
  }
  reportSuccess() {
    if (!this.#down) { this.#failed = 0; return; }
    this.#down = false;
    this.#stopProbe();
    this.onUp?.();
  }
  #startProbe() {
    this.#stopProbe();
    // Probe the config endpoint, not a stream: it is a plain GET served by the
    // API surface, costs nothing on the daemon side, and succeeds the moment
    // the daemon is accepting connections again.
    this.#probeTimer = setInterval(async () => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), NetworkMonitor.PROBE_TIMEOUT_MS);
      try {
        const res = await fetch("/api/config", { signal: controller.signal });
        clearTimeout(timer);
        if (res.ok) this.reportSuccess();
      } catch (err) {
        clearTimeout(timer);
        if (err.name !== 'AbortError') { /* still unreachable; stay down */ }
      }
    }, NetworkMonitor.PROBE_MS);
  }
  #stopProbe() { if (this.#probeTimer !== null) { clearInterval(this.#probeTimer); this.#probeTimer = null; } }
}

// The single reachability gate. `sel#error-banner` lives inside `<main>`, which
// the log page tears down in `#chrome()`, so the banner helpers fall back to a
// dynamically-created element on that page rather than dangling a reference to
// a detached node.
export const network = new NetworkMonitor();
export const showDaemonBanner = (msg) => {
  // Assertive, and the only thing that is: every figure on screen just became stale, and
  // an operator who cannot see the red banner otherwise has no way to know the daemon
  // went away. Announced for BOTH banner paths below, including the dynamically created
  // one — a banner built at runtime is still a banner.
  announce.alert(msg);
  const el = sel("#error-banner");
  if (el && document.body.contains(el)) { el.style.display = "block"; el.textContent = msg; return; }
  let b = sel("#log-offline-banner");
  if (!b) { b = document.createElement("div"); b.id = "log-offline-banner"; b.className = "error-banner"; document.body.prepend(b); }
  b.textContent = msg; b.style.display = "block";
};
export const hideDaemonBanner = () => {
  const el = sel("#error-banner");
  const wasShown = (el && el.style.display === "block") || !!sel("#log-offline-banner");
  if (el) el.style.display = "none";
  sel("#log-offline-banner")?.remove();
  // Recovery is STATED, not implied. The banner disappearing conveys nothing to someone
  // who could not see it in the first place, so silence here would leave them believing
  // the daemon is still gone. Guarded on `wasShown` so a routine successful poll does not
  // announce a recovery from an outage that never happened.
  if (wasShown) announce.alert("Connection to the daemon restored.");
};
// Stats view
