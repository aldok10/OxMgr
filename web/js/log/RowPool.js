import { LOG } from "../core/const.js";

// Bounded pool of reusable row elements. Rows carry no listeners of their own
// (interaction is delegated to the container), so recycling needs no teardown
// and a recycled row cannot retain a stale closure. The pool is capped because
// an unbounded pool is just a slower leak.
export class RowPool {
  #free = [];
  acquire() {
    const row = this.#free.pop();
    if (row) return row;
    const fresh = document.createElement("div");
    fresh.className = "log-row";
    const gutter = document.createElement("span");
    gutter.className = "log-gutter";
    const text = document.createElement("span");
    text.className = "log-text";
    fresh.append(gutter, text);
    return fresh;
  }
  release(row) {
    // Detach first. Clearing a row's content without removing it leaves an
    // empty `.log-row` in the document, which renders as a blank line — most
    // visible under a filter, where the row count drops sharply.
    row.remove();
    if (this.#free.length >= LOG.POOL_MAX) return;
    row.className = "log-row";
    row.lastChild.replaceChildren();
    this.#free.push(row);
  }
  releaseAll(rows) { for (const row of rows) this.release(row); }
}
