import { LINE_RECORD_BYTES, LOG, RATE, clamp, tailCfg } from "../core/const.js";
import { fmt } from "../format/fmt.js";
import { LEVEL_RE, makeLine } from "../ansi/ansi.js";
import { RowPool } from "./RowPool.js";
import { CapacityController, heap } from "./CapacityController.js";
import { ChunkRunner } from "./ChunkRunner.js";

// ── DOM building ──────────────────────────────────────────────────────────
const spanWith = (cls, text) => {
  const span = document.createElement("span");
  if (cls) span.className = cls;
  span.textContent = text;
  return span;
};

const JSON_CLS = { string: "j-str", number: "j-num", boolean: "j-bool" };
// Render a parsed JSON value as coloured tokens. Depth-limited: a pathological
// nesting depth would otherwise build a very deep DOM subtree for one row.
const jsonNodes = (val, out, depth = 0) => {
  if (val === null) { out.push(spanWith("j-null", "null")); return; }
  const type = typeof val;
  if (type !== "object") { out.push(spanWith(JSON_CLS[type] ?? "", type === "string" ? JSON.stringify(val) : String(val))); return; }
  if (depth > 6) { out.push(spanWith("j-punct", Array.isArray(val) ? "[…]" : "{…}")); return; }

  const isArr = Array.isArray(val);
  out.push(spanWith("j-punct", isArr ? "[" : "{"));
  const entries = isArr ? val : Object.keys(val);
  for (let idx = 0; idx < entries.length; idx++) {
    if (idx) out.push(spanWith("j-punct", ", "));
    if (isArr) {
      jsonNodes(entries[idx], out, depth + 1);
    } else {
      out.push(spanWith("j-key", JSON.stringify(entries[idx])));
      out.push(spanWith("j-punct", ": "));
      jsonNodes(val[entries[idx]], out, depth + 1);
    }
  }
  out.push(spanWith("j-punct", isArr ? "]" : "}"));
};

// Nodes for a plain (non-JSON) line: dim the leading timestamp and colour the
// level token so both are scannable without reading the message.
const plainNodes = (line, out) => {
  let rest = line.text;
  if (line.tsLen) {
    out.push(spanWith("log-ts", rest.slice(0, line.tsLen)));
    rest = rest.slice(line.tsLen);
  }
  if (line.level) {
    const match = rest.match(LEVEL_RE);
    if (match) {
      const at = match.index + match[0].indexOf(match[1]);
      if (at > 0) out.push(document.createTextNode(rest.slice(0, at)));
      out.push(spanWith(`log-lvl ${line.level}`, match[1]));
      rest = rest.slice(at + match[1].length);
    }
  }
  if (rest) out.push(document.createTextNode(rest));
};

const lineNodes = (line) => {
  const out = [];
  if (line.json) {
    // Keep the daemon's timestamp prefix visible; only the object is
    // pretty-printed.
    if (line.preLen) {
      const prefix = line.text.slice(0, line.preLen);
      out.push(spanWith("log-ts", line.tsLen ? prefix : ""));
      if (!line.tsLen) out.push(document.createTextNode(prefix));
    }
    jsonNodes(line.json, out);
  } else if (line.segs) {
    for (const seg of line.segs) {
      if (seg.text) out.push(seg.cls ? spanWith(seg.cls, seg.text) : document.createTextNode(seg.text));
    }
  } else {
    plainNodes(line, out);
  }
  if (line.truncated) out.push(spanWith("log-trunc", " … truncated"));
  return out;
};

// Render a line with every occurrence of `query` marked. Built at render time
// from the stored display text: the styled/JSON forms are dropped while a query
// is active, because splitting arbitrary nested nodes on match offsets costs
// more than it gives when the operator is scanning for a term.
const highlightNodes = (line, query) => {
  const out = [];
  const text = line.text;
  const haystack = text.toLowerCase();
  let cursor = 0;
  for (;;) {
    const at = haystack.indexOf(query, cursor);
    if (at < 0) break;
    if (at > cursor) out.push(document.createTextNode(text.slice(cursor, at)));
    out.push(spanWith("log-hit", text.slice(at, at + query.length)));
    cursor = at + query.length;
  }
  if (!out.length) return lineNodes(line);
  if (cursor < text.length) out.push(document.createTextNode(text.slice(cursor)));
  if (line.truncated) out.push(spanWith("log-trunc", " … truncated"));
  return out;
};
// Adopt the daemon's tail settings. Invalid or absent values leave the
// built-in defaults in place rather than disabling the tail.
export const applyTailConfig = (cfg) => {
  const lines = parseInt(cfg?.log_tail_lines, 10);
  if (Number.isInteger(lines) && lines >= 1) tailCfg.lines = lines;
  const warn = parseInt(cfg?.log_tail_warn_above, 10);
  if (Number.isInteger(warn) && warn >= 1) tailCfg.warnAbove = warn;
  // Absent means adaptive; a value pins retention and turns adaptation off.
  const retain = parseInt(cfg?.log_retain_lines, 10);
  tailCfg.retain = Number.isInteger(retain) && retain >= 1 ? retain : null;
};

// Estimated retained memory for a tail, and a warning when the configured
// length is above the comfortable threshold. Two terms, because a line's cost
// is its text plus the record that wraps it: a 60-byte line does not cost 60
// bytes to retain. `bytes` is the exact size the daemon measured while reading
// the tail, so this is not built on a guessed average line length.
export const tailNote = (loaded, bytes) => {
  const configured = tailCfg.lines;
  if (configured <= tailCfg.warnAbove) return null;
  const perLine = loaded > 0 ? bytes / loaded : 0;
  const estimate = configured * (perLine + LINE_RECORD_BYTES);
  const parts = [`tail ${fmt.count(configured)} · est. ${fmt.bytes(estimate)} retained`];
  // Retention stays authoritative: say so plainly rather than letting a large
  // configured tail appear to work and quietly get clipped.
  if (configured > LOG.MAX_LINES) {
    parts.push(`clipped to ${fmt.count(LOG.MAX_LINES)} by retention`);
  }
  return parts.join(" · ");
};
// Renders a bounded window of real rows anchored to the buffer's tail.
//
// There is no total-height estimate and no fixed line height: rows are real
// elements, so scrollHeight is exact and the bottom is always reachable no
// matter how lines wrap. The trade-off is that the scrollbar maps the rendered
// window rather than the whole buffer, which is what `journalctl -f` does too.
export class LogView {
  #body; #viewport; #pool = new RowPool(); #buffer; #rows = [];
  #first = 0;      // buffer index of #rows[0]
  #follow = true;  // autoscroll attached
  #missed = 0;     // lines arrived while detached
  #pending = [];   // arrivals waiting for the next frame
  #frame = 0;      // scheduled rAF id
  #stateFrame = 0; // scheduled counter-only update
  #dirty = false;  // window needs a rebuild
  #suspended = false;
  #runner = new ChunkRunner();
  // Separate runner for prepends: sharing one would let a scroll-triggered older page
  // cancel the initial tail load mid-parse.
  #prependRunner = new ChunkRunner();
  #appendRunner = new ChunkRunner();
  #onState = null;
  #query = "";      // active filter, lowercased; "" = no filter
  #matches = 0;     // matching lines in the buffer, for the footer
  #win = [];        // reused buffer of visible indices: no per-frame allocation
  #anchorEnd = -1;  // exclusive buffer index the window ends at; -1 = tail
  #emptyEl = null;  // "no output" / "no matches" placeholder
  #keys = [];       // reused: absolute line numbers of the window being built
  #shown = [];      // absolute line numbers currently painted, for slide detection
  #paused = false;  // explicit operator pause, outranks visibility suspension
  #capacityCtl = new CapacityController(); // adaptive retention capacity
  #nextSeq = 0;     // last position assigned; live streams count arrivals from 1
  // Whether earlier content exists but is not loaded. Set by a paging owner; false for
  // a live stream, where the buffer already holds everything that has arrived.
  #moreAbove = false;
  // Arrival-rate window: bucketed counts, not timestamps. One integer per bucket
  // regardless of how many lines land in it, so a process emitting thousands of
  // lines a second costs the same as one emitting three.
  #rateBuckets = new Array(RATE.BUCKETS).fill(0);
  #rateSlot = 0;    // index of the bucket currently accumulating
  #rateAt = 0;      // when that bucket opened
  #rateUnit = "s";  // last unit chosen, kept for hysteresis
  #rateTimer = 0;   // decays the displayed rate to idle when output stops
  constructor(body, buffer) {
    this.#body = body;
    this.#buffer = buffer;
    this.#viewport = document.createElement("div");
    this.#viewport.className = "log-viewport";
    this.#body.appendChild(this.#viewport);
  }
  onState(func) { this.#onState = func; }
  get following() { return this.#follow; }
  get missed() { return this.#missed; }

  // Capacity is derived from the viewport, so node count tracks screen height
  // rather than buffer size. Measured against the shortest plausible row so we
  // never render fewer rows than fill the viewport.
  #capacity() {
    const rowH = 18;
    // Bounded by the retention cap in force, not the static maximum: the window can
    // never ask for more rows than the buffer is allowed to hold.
    return Math.min(this.#buffer.maxLines, Math.ceil(this.#body.clientHeight / rowH) + LOG.OVERSCAN * 2);
  }

  // Retention state, for the footer and for verification.
  retention() {
    return {
      capacity: this.#buffer.maxLines,
      retained: this.#buffer.length,
      bytes: this.#buffer.bytes,
      reason: this.#capacityCtl.reason,
      adaptive: this.#capacityCtl.enabled,
      memoryMeasurable: heap.available(),
    };
  }

  // Drives the controller and applies the result. Runs on the coalesced state
  // cadence, not per line: capacity does not need to change more often than the
  // settling period allows anyway.
  #adaptCapacity() {
    const next = this.#capacityCtl.evaluate();
    if (next === this.#buffer.maxLines) return;
    const before = this.#buffer.dropped;
    this.#buffer.setMaxLines(next);
    const shifted = this.#buffer.dropped - before;
    // Lowering the cap evicts from the front, which shifts every index. The anchor
    // and window start follow it so a detached reader's rows stay put.
    if (shifted > 0) {
      if (this.#anchorEnd >= 0) this.#anchorEnd = Math.max(0, this.#anchorEnd - shifted);
      this.#first = Math.max(0, this.#first - shifted);
      if (this.#query) this.#recount();
    }
  }

  /// Adds an older page at the front, keeping the window on the same content.
  ///
  /// Every buffer index shifts UP by the number of lines inserted — the opposite of
  /// front-eviction, which shifts them down. The anchor and window start therefore
  /// move the other way, and the caller compensates the scroll position separately
  /// (the inserted rows have real height, and `overflow-anchor: none` is set).
  prepend(rawLines, stream, firstSeq, onApplied = null) {
    // Parsing is chunked through the same runner `load` uses. Doing it inline made one
    // long task per page: measured 74.5ms of repaint-plus-layout per prepend, stacking
    // into 494ms tasks, and adding a delay between pages only moved the stall (p95
    // 1065ms -> 158ms but the longest task grew to 707ms) because the work itself was
    // still one uninterruptible block. Yielding is what actually bounds it.
    // `onApplied` fires once the records are in the buffer and the window has been
    // marked dirty. The caller compensates scroll there: measuring in the next frame
    // instead raced the chunked parse, so the delta read as 0, the position was never
    // corrected, and the auto-chain kept firing — 11 pages from one gesture.
    this.#prependRunner.run(
      this.#prependGen(rawLines, stream, firstSeq),
      () => onApplied?.(),
    );
    return rawLines.reduce((n, raw) => (raw !== "" ? n + 1 : n), 0);
  }

  *#prependGen(rawLines, stream, firstSeq) {
    const CHUNK = 50;
    const records = [];
    let seq = firstSeq - 1;
    for (let idx = 0; idx < rawLines.length; idx += CHUNK) {
      const end = Math.min(idx + CHUNK, rawLines.length);
      for (let inner = idx; inner < end; inner++) {
        seq++;
        if (rawLines[inner] !== "") records.push(makeLine(rawLines[inner], stream, seq));
      }
      yield;
    }
    if (!records.length) return;
    this.#applyPrepend(records);
  }

  #applyPrepend(records) {
    const added = records.length;
    // Shift every retained position up by what we are inserting, then number the new
    // lines from 1. Without this each page would be numbered 1..L independently and
    // positions would collide across pages — which breaks row identity and the slide
    // detection that depends on it.
    //
    // O(retained) per prepend, once per page: at 200-line pages over a 6.8k-line file
    // that is a few hundred thousand integer writes in total, against the network and
    // parse cost of the pages themselves.
    //
    // It also converges on the right answer: once paging reaches the start of the
    // file, line 1 is line 1, and every number above it is its true file position.
    // Until then the numbers are positions within what is loaded.
    for (let idx = 0; idx < this.#buffer.length; idx++) {
      const held = this.#buffer.at(idx);
      if (held) held.seq += added;
    }
    const droppedFromBack = this.#buffer.prependMany(records);
    // Anchor by content, not by index: without this the window would appear to jump
    // backwards by `added` lines the moment older history arrives.
    if (this.#anchorEnd >= 0) this.#anchorEnd += added - droppedFromBack;
    this.#first += added;
    if (this.#query) this.#recount();
    // A prepend moves the window backwards, which the tail-slide optimisation cannot
    // express — force a full repaint of the visible rows.
    this.#shown.length = 0;
    this.#dirty = true;
    this.#schedule(true);
  }

  /// Adds a newer page at the back, numbering it above what is already held.
  ///
  /// The mirror of `prepend`: appending evicts from the front, so the window anchor and
  /// `#first` move down rather than up. Positions continue from the newest held line, so
  /// no existing number changes.
  append(rawLines, stream, onApplied = null) {
    this.#appendRunner.run(this.#appendGen(rawLines, stream), () => onApplied?.());
    return rawLines.reduce((n, raw) => (raw !== "" ? n + 1 : n), 0);
  }

  *#appendGen(rawLines, stream) {
    const CHUNK = 50;
    const records = [];
    // Continue from the newest line actually held, not from `#nextSeq`: prepends shift
    // every retained position, so a standalone counter would drift out of step.
    const newest = this.#buffer.length ? this.#buffer.at(this.#buffer.length - 1) : null;
    let seq = newest ? newest.seq : 0;
    for (let idx = 0; idx < rawLines.length; idx += CHUNK) {
      const end = Math.min(idx + CHUNK, rawLines.length);
      for (let inner = idx; inner < end; inner++) {
        if (rawLines[inner] !== "") records.push(makeLine(rawLines[inner], stream, ++seq));
      }
      yield;
    }
    if (!records.length) return;
    const droppedFromFront = this.#buffer.appendMany(records);
    if (this.#anchorEnd >= 0) this.#anchorEnd = Math.max(0, this.#anchorEnd - droppedFromFront);
    this.#first = Math.max(0, this.#first - droppedFromFront);
    this.#nextSeq = Math.max(this.#nextSeq, seq);
    if (this.#query) this.#recount();
    // Content changed at both edges; slide detection cannot express that.
    this.#shown.length = 0;
    this.#dirty = true;
    this.#schedule(true);
  }

  /// Tells the view that earlier content exists but is not loaded, so a no-matches
  /// message can be scoped honestly instead of claiming the whole file has none.
  setMoreAbove(more) {
    const next = !!more;
    if (next === this.#moreAbove) return;
    this.#moreAbove = next;
    // Only matters while a filter is showing an empty result.
    if (this.#query) { this.#dirty = true; this.#schedule(true); }
  }

  // Fixed capacity, adaptation disabled.
  setFixedCapacity(lines) {
    this.#capacityCtl.setFixed(lines);
    this.#buffer.setMaxLines(this.#capacityCtl.capacity);
  }

  // Match test, memoised on the record against the current query. A scroll
  // must not re-test lines, and the memo is keyed by query so changing it
  // invalidates rather than accumulating an entry per query ever typed.
  #hit(line) {
    if (!this.#query) return true;
    if (line.mq !== this.#query) {
      line.mq = this.#query;
      // Escapes were stripped at admission, so a query can never match them.
      line.mi = line.text.toLowerCase().indexOf(this.#query);
    }
    return line.mi >= 0;
  }

  // Fill one row from a line record. Called for new rows and recycled ones
  // alike; replaceChildren is what makes reuse safe without teardown.
  #paint(row, line, seq) {
    row.className = line.level ? `log-row lvl-${line.level}` : "log-row";
    row.firstChild.textContent = fmt.count(seq);
    // Highlighting is applied here, not at admission: the query changes far
    // more often than the lines, so baking markers in would mean reparsing
    // every line on every keystroke.
    row.lastChild.replaceChildren(...(this.#query ? highlightNodes(line, this.#query) : lineNodes(line)));
  }

  // Collect the buffer indices the window should show, newest-last, walking
  // backwards from the anchor and skipping non-matching records. Fills a reused
  // array: filtering never builds a second copy of the buffer.
  #collect(cap) {
    const total = this.#buffer.length;
    const win = this.#win;
    win.length = 0;
    // A frozen anchor outranks follow: an explicitly paused viewer stays where it
    // is even though follow is still the mode it will return to on resume.
    const anchored = this.#anchorEnd >= 0 && (this.#paused || !this.#follow);
    const end = anchored ? clamp(this.#anchorEnd, 0, total) : total;
    for (let idx = end - 1; idx >= 0 && win.length < cap; idx--) {
      const line = this.#buffer.at(idx);
      if (line && this.#hit(line)) win.push(idx);
    }
    win.reverse();
    return win;
  }

  // Rebuild the visible window. Iterates the buffer directly: no slice, no
  // intermediate array, no joined string.
  #rebuild() {
    const cap = this.#capacity();
    const win = this.#collect(cap);
    const count = win.length;

    // Grow or shrink the row set to match, recycling the difference.
    while (this.#rows.length > count) this.#pool.release(this.#rows.pop());
    while (this.#rows.length < count) {
      const row = this.#pool.acquire();
      this.#rows.push(row);
      this.#viewport.appendChild(row);
    }

    // Absolute line positions for the window, so a row can be recognised as already
    // showing the right line. Read from the record rather than derived from the
    // eviction count: `dropped + index + 1` only holds while the buffer is the tail
    // of the stream, which prepending older pages makes false.
    const keys = this.#keys;
    keys.length = 0;
    for (let idx = 0; idx < count; idx++) {
      const line = this.#buffer.at(win[idx]);
      keys.push(line ? line.seq : 0);
    }

    // While following, each arriving line slides the window forward by one, and
    // repainting every row for that costs a full rebuild per frame — measured at
    // a 120ms p95 against 15ms when paused. The rows that survive the slide
    // already hold the right content, so rotate them into place and repaint only
    // the tail that is genuinely new.
    const shift = this.#slideAmount(keys, count);
    if (shift > 0 && shift < count) {
      for (let idx = 0; idx < shift; idx++) {
        // appendChild on an attached node moves it, keeping DOM order correct.
        const row = this.#rows.shift();
        this.#viewport.appendChild(row);
        this.#rows.push(row);
      }
      for (let idx = count - shift; idx < count; idx++) {
        const line = this.#buffer.at(win[idx]);
        if (line) this.#paint(this.#rows[idx], line, keys[idx]);
      }
    } else if (shift !== 0) {
      for (let idx = 0; idx < count; idx++) {
        const line = this.#buffer.at(win[idx]);
        if (line) this.#paint(this.#rows[idx], line, keys[idx]);
      }
    }

    // Remember what is on screen so the next pass can recognise a slide.
    this.#shown.length = 0;
    for (let idx = 0; idx < count; idx++) this.#shown.push(keys[idx]);

    this.#first = count ? win[0] : 0;
    this.#empty(count === 0);
  }

  // How far the window slid since the last paint: 0 when nothing changed, N when
  // the first N shown rows fell off the front and the rest still line up, and -1
  // when the content differs in a way that needs a full repaint.
  #slideAmount(keys, count) {
    const shown = this.#shown;
    if (shown.length !== count || count === 0) return -1;
    let same = true;
    for (let idx = 0; idx < count; idx++) {
      if (shown[idx] !== keys[idx]) { same = false; break; }
    }
    if (same) return 0;
    // Find the offset at which the previous window's tail matches this one's head.
    for (let shift = 1; shift < count; shift++) {
      let matches = true;
      for (let idx = 0; idx + shift < count; idx++) {
        if (shown[idx + shift] !== keys[idx]) { matches = false; break; }
      }
      if (matches) return shift;
    }
    return -1;
  }

  // Distinguish "no lines yet" from "no lines match": the second is a dead end
  // the operator needs told about, not an empty panel.
  #empty(show) {
    if (!show) {
      this.#emptyEl?.remove();
      this.#emptyEl = null;
      return;
    }
    if (!this.#emptyEl) {
      this.#emptyEl = document.createElement("div");
      this.#emptyEl.className = "empty";
      this.#viewport.appendChild(this.#emptyEl);
    }
    // Scoped to what is loaded. Saying "no lines match" while earlier pages are still
    // unfetched claims something about the whole file that we have not checked, and an
    // operator would read it as "not in this file".
    this.#emptyEl.textContent = this.#query
      ? (this.#moreAbove
        ? `No loaded line matches "${this.#query}" — scroll up to load earlier lines.`
        : `No lines match "${this.#query}".`)
      : "No output yet.";
  }

  // Total matches, needed for the footer, so this scans the buffer rather than
  // the window. Bounded by the retention cap and run once per settled query,
  // and it reuses the same memo the renderer consults.
  #recount() {
    if (!this.#query) { this.#matches = this.#buffer.length; return; }
    let hits = 0;
    for (let idx = 0; idx < this.#buffer.length; idx++) {
      const line = this.#buffer.at(idx);
      if (line && this.#hit(line)) hits++;
    }
    this.#matches = hits;
  }

  get matches() { return this.#matches; }
  get filtered() { return this.#query !== ""; }

  // Roll the bucket window forward to `now`, clearing whatever it passed over.
  // Called from the arrival path, so it must stay O(buckets) worst case and O(1)
  // in the common case where no boundary was crossed.
  #rollRate(now) {
    if (!this.#rateAt) { this.#rateAt = now; return; }
    let elapsed = now - this.#rateAt;
    if (elapsed < RATE.BUCKET_MS) return;
    const steps = Math.floor(elapsed / RATE.BUCKET_MS);
    if (steps >= RATE.BUCKETS) {
      this.#rateBuckets.fill(0);
      this.#rateSlot = 0;
    } else {
      for (let idx = 0; idx < steps; idx++) {
        this.#rateSlot = (this.#rateSlot + 1) % RATE.BUCKETS;
        this.#rateBuckets[this.#rateSlot] = 0;
      }
    }
    this.#rateAt += steps * RATE.BUCKET_MS;
  }

  // A stopped process emits nothing, so nothing would trigger a footer update and
  // the last rate would stay frozen on screen. This ticker lets the figure decay to
  // idle on its own. It only updates counters — no render — so it costs nothing
  // visual, and it stands down while the output is hidden.
  startRateTicker() {
    this.stopRateTicker();
    this.#rateTimer = setInterval(() => {
      // Paused still reports (the process is still emitting); hidden does not.
      if (this.#suspended && !this.#paused) return;
      this.#onState?.();
    }, RATE.BUCKET_MS);
  }
  stopRateTicker() {
    if (this.#rateTimer) { clearInterval(this.#rateTimer); this.#rateTimer = 0; }
  }

  // One increment per admitted line: the whole per-line cost of rate reporting.
  #countArrival() {
    this.#rollRate(performance.now());
    this.#rateBuckets[this.#rateSlot]++;
  }

  // Lines per second over the window, or null when the window is empty.
  #ratePerSecond() {
    this.#rollRate(performance.now());
    let total = 0;
    for (let idx = 0; idx < RATE.BUCKETS; idx++) total += this.#rateBuckets[idx];
    if (total === 0) return null;
    return total / ((RATE.BUCKETS * RATE.BUCKET_MS) / 1000);
  }

  // The rate as a figure plus its unit. Unit is chosen by magnitude with
  // hysteresis, so a process sitting on a boundary does not flicker between
  // "1/s" and "60/min".
  rate() {
    const perSec = this.#ratePerSecond();
    if (perSec === null) return { idle: true };

    const perMin = perSec * 60;
    let unit = this.#rateUnit;
    // Move to a finer unit only once clearly above 1, and to a coarser one only
    // once clearly below: the two thresholds differ, which is what stops the
    // oscillation.
    if (unit === "s") {
      if (perSec < RATE.DOWN) unit = perMin >= RATE.UP ? "min" : "h";
    } else if (unit === "min") {
      if (perSec >= RATE.UP) unit = "s";
      else if (perMin < RATE.DOWN) unit = "h";
    } else {
      if (perSec >= RATE.UP) unit = "s";
      else if (perMin >= RATE.UP) unit = "min";
    }
    this.#rateUnit = unit;

    const value = unit === "s" ? perSec : unit === "min" ? perMin : perMin * 60;
    // One decimal below 10 so a slow trickle is not rounded to a flat integer.
    return { idle: false, unit, value, text: `${value < 10 ? value.toFixed(1) : fmt.count(Math.round(value))}/${unit}` };
  }

  setQuery(raw) {
    const next = (raw ?? "").trim().toLowerCase();
    if (next === this.#query) return;
    this.#query = next;
    // Changing the query re-aims the window at the tail of the *matching* lines,
    // except while explicitly paused: there the anchor is what holds the view
    // still, and discarding it would jump the operator forward to output that
    // arrived while they were reading.
    if (!this.#paused) this.#anchorEnd = -1;
    // The same lines can stay in the window while their highlighting changes, so
    // slide detection must not conclude "nothing to repaint" here.
    this.#shown.length = 0;
    this.#recount();
    this.#dirty = true;
    // Forced: filtering is an operator action, so it must take effect even while
    // paused or hidden. The frozen anchor keeps the window from following the tail.
    this.#schedule(true);
  }

  #stickBottom() {
    if (this.#follow) this.#body.scrollTop = this.#body.scrollHeight;
  }

  // Single frame callback for every kind of pending work. A second arrival in
  // the same frame does not schedule a second callback.
  // `force` lets an operator action render while suspended. Pause must stop new
  // output from moving the view, not stop the viewer from responding to input:
  // filtering a paused buffer is exactly what pausing is for.
  #schedule(force = false) {
    if (this.#frame) return;
    if (this.#suspended && !force) return;
    this.#frame = requestAnimationFrame(() => {
      this.#frame = 0;
      // Measure our OWN work per frame, not the gap between frames. The gap is not
      // a usable signal here: we only schedule a frame when lines arrive, so at 20
      // lines/sec on a 120Hz display just one frame in six is ours and the gap
      // reads ~50ms instead of 8.3ms — which would make the health check
      // meaninglessly lenient. Our work duration against the frame budget is the
      // thing that actually decides whether we are the cause of a dropped frame.
      try { this.#adaptCapacity(); this.#flush(); } finally { this.#onState?.(); }
    });
  }
  #flush() {
    if (this.#pending.length) {
      for (const line of this.#pending) {
        this.#buffer.push(line);
        if (!this.#query || this.#hit(line)) this.#matches++;
      }
      this.#pending.length = 0;
      // Eviction may have dropped matching lines: the count is only exact if
      // we recount, and doing it here keeps it off the per-line path.
      if (this.#buffer.dropped) this.#recount();
      this.#dirty = true;
    }
    if (!this.#dirty) return;
    this.#dirty = false;
    // Measure before mutating so layout is not flushed twice in one frame.
    this.#rebuild();
    this.#stickBottom();
  }

  add(line) {
    // Position is assigned here, not by the caller: only the view knows whether a
    // line is the next arrival on a stream or a line at a known offset in a file.
    // For a live stream this is the running arrival count, which is exactly what
    // `dropped + index + 1` used to compute — so the numbering an operator sees is
    // unchanged, it is just no longer derived from the buffer's shape.
    line.seq = ++this.#nextSeq;
    // Counted on every path, including while paused: the rate describes what the
    // process is emitting, not what the viewer happens to be rendering.
    this.#countArrival();
    // An explicit pause freezes the view exactly as scrolling away does, so it
    // takes the same path: retain the line, move nothing. Without this, a forced
    // render (from filtering) would pull in output the operator paused to avoid.
    if (!this.#follow || this.#paused) {
      // Detached: retain the line and touch nothing visual. Any DOM work here
      // would change content height and move what the operator is reading.
      // Eviction shifts every buffer index down, so the window start follows
      // it to keep referring to the same lines.
      const before = this.#buffer.dropped;
      this.#buffer.push(line);
      // Eviction shifts every index down; the anchor follows so the rendered
      // rows keep referring to the same lines.
      const shifted = this.#buffer.dropped - before;
      if (this.#anchorEnd >= 0) this.#anchorEnd = Math.max(0, this.#anchorEnd - shifted);
      this.#first = Math.max(0, this.#first - shifted);
      this.#missed++;
      if (this.#query ? this.#hit(line) : true) this.#matches++;
      this.#pokeState();
      return;
    }
    this.#pending.push(line);
    this.#schedule();
  }
  // Coalesced footer/counter update. Separate from #schedule because it must
  // not imply a rebuild: while detached the counters move but the rows do not.
  #pokeState() {
    if (this.#stateFrame || this.#suspended) return;
    this.#stateFrame = requestAnimationFrame(() => {
      this.#stateFrame = 0;
      this.#adaptCapacity();
      this.#onState?.();
    });
  }
  // Bulk load as a generator: yields every chunk so a large tail never blocks
  // the main thread, and abandoning it is just dropping the reference.
  *#loadGen(rawLines, stream, firstSeq) {
    const CHUNK = 200;
    // `firstSeq` is the position of rawLines[0]. For a tail load that is unknown, so
    // the caller passes null and positions continue from the running counter; for a
    // paged file the caller knows the line number and passes it, which is what makes
    // the gutter show true file positions.
    let seq = firstSeq === null ? this.#nextSeq : firstSeq - 1;
    for (let idx = 0; idx < rawLines.length; idx += CHUNK) {
      const end = Math.min(idx + CHUNK, rawLines.length);
      for (let inner = idx; inner < end; inner++) {
        const raw = rawLines[inner];
        if (raw !== "") this.#buffer.push(makeLine(raw, stream, ++seq));
      }
      this.#dirty = true;
      yield;
    }
    this.#nextSeq = Math.max(this.#nextSeq, seq);
    this.#rebuild();
    this.#stickBottom();
  }
  load(rawLines, stream, onDone = null, firstSeq = null) {
    const arr = Array.isArray(rawLines) ? rawLines : String(rawLines ?? "").split("\n");
    this.#runner.run(this.#loadGen(arr, stream, firstSeq), () => { this.#onState?.(); onDone?.(); });
  }
  reset() {
    this.#runner.cancel();
    // A prepend still parsing belongs to the stream being torn down: letting it finish
    // would apply an older page's lines to a fresh buffer.
    this.#prependRunner.cancel();
    this.#appendRunner.cancel();
    this.#pending.length = 0;
    this.#buffer.clear();
    this.#pool.releaseAll(this.#rows);
    this.#rows.length = 0;
    this.#viewport.replaceChildren();
    this.#emptyEl = null;
    // Slide detection compares against what is painted; a reset means nothing is.
    this.#shown.length = 0;
    this.#keys.length = 0;
    this.#first = 0;
    this.#anchorEnd = -1;
    this.#follow = true;
    this.#missed = 0;
    this.#matches = 0;
    this.#win.length = 0;
    this.#dirty = false;
    this.#paused = false;
    this.#suspended = false;
    // A new stream starts from an empty window: carrying the previous stream's
    // arrival counts would report a rate for output that is no longer being shown.
    this.#rateBuckets.fill(0);
    this.#rateSlot = 0;
    this.#rateAt = 0;
    this.#rateUnit = "s";
    // Positions restart with the stream: a new source numbers from its own first line.
    this.#nextSeq = 0;
  }
  // Called from the scroll handler.
  // Hysteresis: the two transitions use different thresholds on purpose. One
  // symmetric threshold cannot both survive a tall wrapped line being appended
  // (needs to be wide) and avoid reattaching when the operator scrolls to
  // almost-the-bottom (needs to be narrow).
  onScroll() {
    const gap = this.#body.scrollHeight - this.#body.clientHeight - this.#body.scrollTop;
    if (this.#follow) {
      // Detach only on a deliberate move away: following pins the gap at ~0,
      // so anything beyond a row's height came from the operator.
      const tallest = this.#rows.length ? this.#rows[this.#rows.length - 1].offsetHeight : 18;
      if (gap > Math.max(24, tallest + 8)) {
        this.#follow = false;
        this.#missed = 0;
        this.#onState?.();
      }
      return;
    }
    // Reattach only at the true bottom. REATTACH_EPS is sub-pixel rounding
    // room, not a "near enough" allowance: near the bottom is not the bottom.
    // An explicit pause is never undone by scrolling: the button would
    // otherwise silently cancel itself.
    const REATTACH_EPS = 2;
    if (gap <= REATTACH_EPS && !this.#paused) {
      this.#follow = true;
      this.#missed = 0;
      this.#dirty = true;
      this.#schedule();
      return;
    }
    // Detached and staying detached: slide the window with the scroll position
    // so scrolling up reaches older lines. This is operator-driven, so moving
    // rows here is expected; we still never move scrollTop ourselves.
    const total = this.#buffer.length, cap = this.#capacity();
    if (total > cap) {
      const ratio = this.#body.scrollTop / Math.max(1, this.#body.scrollHeight - this.#body.clientHeight);
      // The window is anchored by its END, because #collect walks backwards.
      const wantEnd = clamp(Math.round(cap + ratio * (total - cap)), cap, total);
      if (wantEnd !== this.#anchorEnd) { this.#anchorEnd = wantEnd; this.#dirty = true; this.#schedule(); }
    }
    this.#onState?.();
  }
  jumpToLatest() {
    this.#follow = true;
    this.#missed = 0;
    this.#anchorEnd = -1;
    this.#dirty = true;
    this.#schedule();
    this.#stickBottom();
  }
  relayout() { this.#dirty = true; this.#schedule(); }
  // Take the line viewport out of the layout entirely when another mode owns
  // the body. It carries `min-height: 100%`, so merely leaving it empty would
  // still reserve a full screen of blank space above whatever renders next.
  setHidden(hidden) {
    this.#viewport.style.display = hidden ? "none" : "";
    if (hidden) this.suspend(); else this.resume();
  }
  // Suspension is simply not scheduling: arrivals keep buffering, no render
  // work happens, and one catch-up pass runs on resume.
  suspend() {
    this.#suspended = true;
    if (this.#frame) { cancelAnimationFrame(this.#frame); this.#frame = 0; }
    if (this.#stateFrame) { cancelAnimationFrame(this.#stateFrame); this.#stateFrame = 0; }
  }
  resume() {
    if (!this.#suspended) return;
    // An explicit pause outranks the reason we were suspended: resuming from a
    // tab regaining visibility must not undo the operator's Pause button.
    if (this.#paused) return;
    this.#suspended = false;
    this.#dirty = true;
    this.#schedule();
  }
  // Explicit pause. Same render gate as visibility suspension — the
  // subscription stays open, so resuming leaves no hole in the log and adds no
  // second connection.
  get paused() { return this.#paused; }
  setPaused(paused) {
    if (paused === this.#paused) return;
    this.#paused = paused;
    if (paused) {
      // Freeze the window where it stands. Filtering while paused forces a
      // render, and without a fixed anchor that render would follow the tail —
      // showing exactly the new output the operator paused to keep out.
      if (this.#anchorEnd < 0) this.#anchorEnd = this.#buffer.length;
      this.suspend();
      this.#onState?.();
      return;
    }
    this.#suspended = false;
    // Resuming returns to the tail; the frozen anchor has served its purpose.
    if (this.#follow) this.#anchorEnd = -1;
    this.#dirty = true;
    this.#schedule();
  }
  destroy() {
    this.suspend();
    this.stopRateTicker();
    this.#runner.cancel();
    this.#prependRunner.cancel();
    this.#appendRunner.cancel();
    this.reset();
    this.#suspended = false;
  }
}
