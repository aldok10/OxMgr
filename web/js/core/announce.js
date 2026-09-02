import { sel } from "./const.js";

// Live-region announcer (WCAG 2.2 SC 4.1.3).
//
// Writes to the two containers declared in index.html. What belongs here and what must
// NEVER come here is specified by `dashboard-status-messaging` and enumerated in the
// change's status-message inventory. The short version:
//
//   announce.status(msg)  outcomes, results, waiting states  -> role="status" (polite)
//   announce.alert(msg)   the daemon went away, nothing else  -> role="alert"  (assertive)
//
// Do NOT call either from a per-tick render path. CPU, memory, disk, per-core figures and
// log lines all update on the collection tick; announcing them makes the dashboard
// unusable through a screen reader, which SC 4.1.3 itself warns about. A screen-reader
// user reads a current figure by navigating to it. There is a test that fails if a live
// region starts changing during the stream, and it exists precisely because adding one
// looks like an improvement.
export const announce = (() => {
  // Resolved per call rather than cached: the standalone log page tears down `<main>`
  // in `#chrome()`, and a cached reference to a detached node announces nothing.
  const region = (id) => sel(`#${id}`);

  // Re-announcing an identical string is a no-op for most screen readers, because the
  // region's text did not change. Clearing first forces it to be seen as new — needed
  // for a repeated action ("restart failed" twice in a row is two events, not one).
  const write = (id, msg) => {
    const el = region(id);
    if (!el || !msg) return;
    if (el.textContent === msg) el.textContent = "";
    el.textContent = msg;
  };

  return {
    status: (msg) => write("status-live", msg),
    alert: (msg) => write("alert-live", msg),
    // Test seam: lets a check read what was last announced without scraping the DOM.
    read: () => ({
      status: region("status-live")?.textContent ?? "",
      alert: region("alert-live")?.textContent ?? "",
    }),
  };
})();
