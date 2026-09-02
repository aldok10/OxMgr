import { LOG } from "../core/const.js";

// Retention buffer. Evicts from the front on either bound (line count or total
// bytes). Eviction advances a head index instead of reslicing, and the backing
// array is compacted only when the dead prefix grows large, so the per-line
// cost stays flat as the buffer fills. The previous `slice(-N)` per line
// allocated a fresh N-element array for every arriving line.
export class LineBuffer {
  #items = []; #head = 0; #bytes = 0; #dropped = 0;
  // Line cap is variable so the capacity controller can move it. The byte bound
  // stays fixed: it is a memory ceiling, not a tuning knob.
  #maxLines = LOG.MAX_LINES;
  get length() { return this.#items.length - this.#head; }
  get bytes() { return this.#bytes; }
  get dropped() { return this.#dropped; }
  get maxLines() { return this.#maxLines; }
  at(idx) { return this.#items[this.#head + idx]; }
  clear() { this.#items = []; this.#head = 0; this.#bytes = 0; this.#dropped = 0; }
  // Lowering the cap evicts immediately, oldest first, which is the same path
  // ordinary eviction takes — so a reduction cannot move rendered rows.
  setMaxLines(lines) {
    const next = ~~lines;
    if (next < 1 || next === this.#maxLines) return;
    this.#maxLines = next;
    this.#evict();
  }
  push(line) {
    this.#items.push(line);
    this.#bytes += line.bytes;
    this.#evict();
  }
  /// Adds older lines at the front, in file order, and evicts from the BACK to stay
  /// within bounds.
  ///
  /// Paging backwards is the one case where the newest lines are the expendable ones:
  /// the operator is reading history, and the tail can be re-fetched from a finished
  /// file. Evicting from the front here would discard the very lines just loaded.
  /// Returns how many were dropped from the back, so the caller can keep its window
  /// anchor pointing at the same content.
  /// Adds newer lines at the back and evicts from the front, which is the ordinary
  /// direction. Returns how many were dropped from the front so the caller can keep its
  /// window anchor on the same content.
  appendMany(lines) {
    if (!lines.length) return 0;
    const before = this.#dropped;
    for (const line of lines) {
      this.#items.push(line);
      this.#bytes += line.bytes;
    }
    this.#evict();
    return this.#dropped - before;
  }
  prependMany(lines) {
    if (!lines.length) return 0;
    // Compact first: unshift onto an array with a live dead prefix would leave the
    // head index pointing at the wrong element.
    if (this.#head > 0) {
      this.#items = this.#items.slice(this.#head);
      this.#head = 0;
    }
    this.#items.unshift(...lines);
    for (const line of lines) this.#bytes += line.bytes;
    return this.#evictFromBack();
  }
  #evict() {
    while ((this.#items.length - this.#head) > this.#maxLines || this.#bytes > LOG.MAX_BYTES) {
      const victim = this.#items[this.#head];
      if (!victim) break;
      this.#bytes -= victim.bytes;
      // Release the record so its strings are collectable before compaction.
      this.#items[this.#head] = null;
      this.#head++;
      this.#dropped++;
    }
    // Amortised compaction: only when the dead prefix is both large and a
    // significant share of the array.
    if (this.#head > 2048 && this.#head * 2 > this.#items.length) {
      this.#items = this.#items.slice(this.#head);
      this.#head = 0;
    }
  }
  /// Trims from the newest end. `#dropped` is deliberately NOT incremented: it counts
  /// how many lines have fallen off the FRONT, and the gutter no longer depends on it
  /// — but the window anchor still measures front-eviction, so conflating the two
  /// directions there would shift the view the wrong way.
  #evictFromBack() {
    let removed = 0;
    while ((this.#items.length - this.#head) > this.#maxLines || this.#bytes > LOG.MAX_BYTES) {
      const victim = this.#items.pop();
      if (!victim) break;
      this.#bytes -= victim.bytes;
      removed++;
    }
    return removed;
  }
}
