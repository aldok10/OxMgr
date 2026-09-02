import { BYTE, EMPTY, LIVE, SEC } from "../core/const.js";

// Pure formatters
export const fmt = {
  esc(val) { const div = document.createElement("div"); div.textContent = val ?? ""; return div.innerHTML; },
  /// Drops a trailing `.0` from a fixed-decimal string.
  ///
  /// `16.0` carries exactly as much information as `16` and costs two more characters, which in a
  /// 340px sidebar is width the mount paths and interface names need. Only a trailing ZERO is
  /// removed: `13.6` keeps its decimal, because that digit is a real measurement.
  trim: str => String(str).replace(/\.0+$/, ""),
  bytes(val) {
    let size = Number(val) || 0, idx = 0;
    while (size >= BYTE.KILOBYTE && idx < 3) { size /= BYTE.KILOBYTE; idx++; }
    return `${fmt.trim(size.toFixed(BYTE.DECIMALS[idx]))} ${BYTE.UNITS[idx]}`;
  },
  /// A byte pair sharing one unit suffix: `13.6 of 16 GB`.
  ///
  /// Was `${bytes(a)} of ${bytes(b)}`, which printed the unit twice — `13.6 GB of 16.0 GB`. When
  /// both values land in the same unit the first suffix is redundant, and dropping it saves ~3
  /// characters on the gauge detail lines where space is tightest. When they DIFFER (a 900 MB
  /// used against a 16 GB total) both units are kept, because `900 of 16 GB` would be wrong.
  bytePair(used, total) {
    const unit = (val) => {
      let size = Number(val) || 0, idx = 0;
      while (size >= BYTE.KILOBYTE && idx < 3) { size /= BYTE.KILOBYTE; idx++; }
      return { size, idx };
    };
    const a = unit(used), b = unit(total);
    if (a.idx !== b.idx) return `${fmt.bytes(used)} of ${fmt.bytes(total)}`;
    return `${fmt.trim(a.size.toFixed(BYTE.DECIMALS[a.idx]))} of `
      + `${fmt.trim(b.size.toFixed(BYTE.DECIMALS[b.idx]))} ${BYTE.UNITS[b.idx]}`;
  },
  // Percentages lose a trailing `.0` for the same reason: `87.6%` is a reading, `100.0%` is
  // three characters spent on one fact.
  pct: val => fmt.trim((Number(val) || 0).toFixed(1)),
  /// Throughput over the interval the reading actually covers.
  ///
  /// The interface list used to print `total_received_bytes`, and that is why the network
  /// figures looked frozen: the totals are lifetime counters, so on this host en0 sat at
  /// 16,398,227,518 → 16,398,366,189 → 16,398,438,355 bytes across three collections — really
  /// moving, but all three render as "15.3 GB" once divided down to gigabytes. A 138 KB change
  /// inside a 15 GB total cannot survive one decimal place, so the row was numerically correct
  /// and completely useless.
  ///
  /// The payload already carries what a speed needs: `received_bytes` is the delta for the
  /// window and `interval_ms` is that window's real length (10000 or 11999 in practice, because
  /// the collector skips missed ticks rather than pretending to a fixed cadence). Dividing the
  /// two is the rate, and a rate moves visibly.
  ///
  /// Returns null when the interval is unusable, so the caller can render an absence rather
  /// than a fabricated 0 B/s.
  rate(deltaBytes, intervalMs) {
    const bytes = Number(deltaBytes);
    const ms = Number(intervalMs);
    if (!Number.isFinite(bytes) || !Number.isFinite(ms) || ms <= 0) return null;
    const perSec = (bytes * 1000) / ms;
    // Below 1 KB/s reads as idle. Printing "37 B/s" invites the reader to care about a figure
    // that is indistinguishable from noise at this sampling rate.
    if (perSec < BYTE.KILOBYTE) return "idle";
    return `${this.bytes(perSec)}/s`;
  },
  // Dot-grouped thousands: 10123 -> "10.123". Fixed separator rather than
  // toLocaleString, whose result varies with the browser locale for no reason
  // the operator chose.
  count(val) {
    const num = ~~(Number(val) || 0);
    return String(Math.abs(num)).replace(/\B(?=(\d{3})+(?!\d))/g, ".").replace(/^/, num < 0 ? "-" : "");
  },
  when(secs) {
    if (!secs) return EMPTY;
    return new Date(secs * 1000).toLocaleString();
  },
  dur(sec, opts = {}) {
    const day = ~~(sec / SEC.DAY);
    const hours = ~~(sec / SEC.HOUR);
    const min = ~~(sec / SEC.MINUTE);
    if (day) {
      const out = `${day}d ${~~((sec % SEC.DAY) / SEC.HOUR)}h`;
      return opts.seconds ? `${out} ${~~((sec % SEC.HOUR) / SEC.MINUTE)}m ${sec % SEC.MINUTE}s` : out;
    }
    if (hours) {
      const out = `${hours}h ${~~((sec % SEC.HOUR) / SEC.MINUTE)}m`;
      return opts.seconds ? `${out} ${sec % SEC.MINUTE}s` : out;
    }
    return min ? `${min}m ${sec % SEC.MINUTE}s` : `${sec}s`;
  },
  up(proc) {
    if (!LIVE.includes(proc.status) || !proc.last_started_at) return EMPTY;
    return this.dur(Math.max(0, ~~(Date.now() / 1000) - proc.last_started_at), { seconds: true });
  },
  time: () => new Date().toLocaleTimeString(),
};
