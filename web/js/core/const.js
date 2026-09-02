export const EMPTY = "\u2013", UNKNOWN = "unknown";
export const SEC = { DAY: 86400, HOUR: 3600, MINUTE: 60 };
export const BYTE = { KILOBYTE: 1024, UNITS: ["B", "KB", "MB", "GB"], DECIMALS: [0, 0, 1, 1] };
export const LIVE = ["running", "restarting"];
export const SPIN_MS = 400, INT = { MIN: 200, MAX: 10000, DEF: 2000 };
export const STREAMS = ["stdout", "stderr"];
// Log viewer bounds. Lines caps how many we retain; BYTES caps their combined
// size, because a line cap alone is not a memory bound (10k x 4KB = 40MB).
// Whichever binds first evicts from the front.
// MAX_BYTES counts retained line TEXT only (`line.bytes` is the display string's
// length); the per-line record overhead is separate and governed by the heap-driven
// capacity controller below.
//
// Raised from 4MB deliberately: at the 152 bytes/line measured on a real log, 4MB
// bound retention to ~27.600 lines, so it — not the line cap — was the binding
// constraint, and any line ceiling above that was decoration. 192MB of text permits
// ~1.26M lines at that size, so the line ceiling binds first and means what it says.
export const LOG = { MAX_LINES: 5000, MAX_BYTES: 192 * 1024 * 1024, MAX_LINE_CHARS: 4000, OVERSCAN: 12, POOL_MAX: 240 };
// Adaptive retention. A fixed 5000-line cap is wrong in both directions: it throws
// away history a workstation could hold, and may be too much for a small VM. These
// bound how far the controller may move it.
export const CAP = {
FLOOR: 1000,          // a weak machine must still be usable
// One million lines is the hard stop. Reaching it needs real headroom: at the
// 152 bytes/line measured on a live log that is ~145MB of text plus ~181MB of
// line records, so ~326MB of heap. The controller only climbs here while
// `performance.memory` reports the room to do so, and falls back fast when it
// does not — the ceiling is a limit, not a target.
CEILING: 1000000,
NO_MEMORY_CEILING: 8000, // when heap cannot be measured, do not grow far
GROW: 1.35,           // multiplicative, capped per step
SHRINK: 0.6,          // fall faster than we rise: stutter is a present problem
SETTLE_MS: 4000,      // observe the effect of a step before taking another
GOOD_RUNS: 3,         // consecutive good evaluations required to grow
HEAP_HEADROOM: 0.35,  // grow only while this share of the heap limit is free
HEAP_TIGHT: 0.15,     // shrink when free headroom falls below this
};
// Per-retained-line overhead beyond the log text itself: the record object, its
// fields, and the engine's string header. Measured empirically against heap
// snapshots rather than assumed, and deliberately on the generous side so an
// estimate shown to an operator is not an underestimate.
export const LINE_RECORD_BYTES = 190;
// Arrival-rate window: 15 buckets of 1s = a 15s sliding window. Short enough to
// follow a change in output speed, long enough that a 3-lines-per-minute process
// is not reported as idle between lines.
export const RATE = {
BUCKETS: 15,
BUCKET_MS: 1000,
// Hysteresis on the unit switch: a rate hovering at 1/s would otherwise
// alternate between "1/s" and "60/min" on consecutive updates.
UP: 1.25,   // must exceed this to move to a finer unit
DOWN: 0.8,  // must fall below this to move to a coarser unit
};
// Threshold announcement rate limit: 1 minute.
export const THRESHOLD_ANNOUNCE_MS = 60000;
// Tail settings, replaced from /api/config at boot.
// `retain: null` means adaptive; a number pins capacity and disables adaptation.
export const tailCfg = { lines: 200, warnAbove: 500, retain: null };
// When an overlay panel fills the viewport there is no backdrop to tap and nowhere to drag
// to, so both gestures are gated on this.
//
// The query string is duplicated from dashboard.css deliberately: asking matchMedia the
// *same question the stylesheet asks* is what keeps behaviour and layout from drifting. A
// hand-rolled `innerWidth <= 520` check looked equivalent and was not — it missed a phone
// in landscape (844x390), where the panel was full-screen by CSS while JS still allowed
// dragging and backdrop dismissal.
const PANEL_FULL_VIEWPORT_QUERY =
"(max-width: 520px), (max-height: 520px) and (any-pointer: coarse)";
export const isPanelFullViewport = () =>
typeof window !== "undefined" &&
typeof window.matchMedia === "function" &&
window.matchMedia(PANEL_FULL_VIEWPORT_QUERY).matches;
// The filter panel collapses into a phone-only slide-up below 700px (the toolbar
// becomes a bottom nav at the same breakpoint). Duplicated from dashboard.css
// deliberately, for the same reason as PANEL_FULL_VIEWPORT_QUERY: asking matchMedia
// the *same question the stylesheet asks* keeps behaviour and layout from drifting.
const FILTER_COLLAPSE_QUERY = "(max-width: 700px)";
export const isFilterCollapsible = () =>
typeof window !== "undefined" &&
typeof window.matchMedia === "function" &&
window.matchMedia(FILTER_COLLAPSE_QUERY).matches;
export const ERROR_STATUSES = ["crashed", "errored"];
export const CONFIRM_ACTS = ["restart", "stop", "reload"];
export const ERROR_MAX_LEN = 120;
export const EVENT_STATUS = {
"process:started": "starting",
"process:online": "running",
"process:stopped": "stopped",
"process:exited": "stopped",
"process:crashed": "crashed",
"process:restarting": "restarting",
"process:errored": "errored",
};
// Bus event catalogue: the single source of truth for every event the UI
// publishes or subscribes to. A typo here is a loud ReferenceError at call
// time, never a silent dead subscriber.
// Payload shapes (emitted with each event):
//   ACT_DONE  -> { msg }
//   ACT_ERR   -> { target, act, err }
//   PROC_START/PROC_ERR  -> {} (no payload; the refresh itself carries state)
//   PROC_DATA -> Process[] (the full current process table)
//   LOG_TAIL  -> { gen, lines, bytes }
//   LOG_DATA  -> { gen, line }
//   LOG_ERR   -> {}
//   FILTER_CHANGED -> {} (UI state already in store; consumers re-read it)
//   EVENT_PROCESS -> BusEvent (daemon event, mapped via EVENT_STATUS)
export const EVENTS = {
ACT_DONE: "act:done",
ACT_ERR: "act:err",
PROC_START: "proc:start",
PROC_DATA: "proc:data",
PROC_ERR: "proc:err",
LOG_TAIL: "log:tail",
LOG_DATA: "log:data",
LOG_ERR: "log:err",
FILTER_CHANGED: "filter:changed",
EVENT_PROCESS: "event:process",
// Unified wire events (named SSE event: fields on /api/stream)
PROCESSES: "processes",
SNAPSHOT: "snapshot",
MEMORY: "memory",
CPU: "cpu",
LOAD_AVERAGE: "load_average",
FILESYSTEMS: "filesystems",
NETWORK: "network",
COMPONENTS: "components",
// Lifecycle wire events that a process:* subscription may deliver.
PROC_STARTED: "process:started",
PROC_ONLINE: "process:online",
PROC_STOPPED: "process:stopped",
PROC_EXITED: "process:exited",
PROC_CRASHED: "process:crashed",
PROC_RESTARTING: "process:restarting",
PROC_ERRORED: "process:errored",
LOG_OUT: "log:out",
LOG_ERR: "log:err",
HEALTHY: "health:healthy",
UNHEALTHY: "health:unhealthy",
ANOMALY_DETECTED: "anomaly:detected",
ANOMALY_CLEARED: "anomaly:cleared",
REMEDIATION_DECIDED: "remediation:decided",
DAEMON_SHUTDOWN: "daemon:shutdown",
};
// Membership set over EVENTS *values* (what callers pass, e.g. "log:tail").
// The bus guard checks this, not `in EVENTS` (which matches keys, not values).
export const EVENT_NAMES = new Set(Object.values(EVENTS));
// Lifecycle wire event names a `process:*` subscription may deliver, in the
// order the daemon's LIFECYCLE_EVENT_NAMES declares them. Used to register one
// `addEventListener` per name on the unified stream (task 6.2): the browser
// routes on the SSE `event:` field, so a name without a listener is a visible
// gap rather than an empty `onmessage`.
export const PROCESS_LIFECYCLE_EVENTS = [
EVENTS.PROC_STARTED, EVENTS.PROC_ONLINE, EVENTS.PROC_STOPPED,
EVENTS.PROC_EXITED, EVENTS.PROC_CRASHED, EVENTS.PROC_RESTARTING,
EVENTS.PROC_ERRORED,
];

export const sel = (query, root = document) => root.querySelector(query);
export const clamp = (val, min, max) => Math.max(min, Math.min(max, val));
