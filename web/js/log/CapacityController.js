import { CAP, LOG, clamp } from "../core/const.js";

// Heap headroom, where the browser exposes it. `performance.memory` is
// Chromium-only and non-standard; `measureUserAgentSpecificMemory` needs
// cross-origin isolation the daemon does not send. So absence is common and must
// never be read as "there is headroom".
export const heap = {
  available() { return typeof performance !== "undefined" && !!performance.memory; },
  // Fraction of the heap limit still free, or null when unmeasurable.
  freeRatio() {
    if (!this.available()) return null;
    const { usedJSHeapSize: used, jsHeapSizeLimit: limit } = performance.memory;
    if (!(limit > 0)) return null;
    return Math.max(0, Math.min(1, 1 - used / limit));
  },
};

// Retention capacity controller.
//
// Driven by MEMORY, not frame timing. Frame timing was the original signal and it
// was the wrong one: measured on the running viewer, the rendered row count is
// identical at cap 9.000 and at cap 1.000 (67 rows both times), because the
// windowed renderer bounds the DOM by viewport height and not by buffer size.
// That is the whole point of the windowed renderer — and it means shrinking
// retention cannot improve frame timing. Using frames to drive retention therefore
// formed a feedback loop with no path to convergence: frames looked slow, capacity
// shrank, frames did not improve because retention was never the cause, so capacity
// shrank again, all the way to the floor.
//
// Retention's only real cost is memory, so memory is what governs it. Frame cost is
// already bounded elsewhere, by the per-frame work budget in ChunkRunner.
//
// One caveat, recorded rather than papered over: with an active filter, `#collect`
// walks backwards through records until it fills the window, and `#recount` scans
// the buffer — so a larger buffer does cost more per render *while filtering*. That
// scan is bounded by the cap, and the cap is bounded by memory, so it stays bounded;
// it is not, however, zero.
export class CapacityController {
  #cap; #goodRuns = 0; #lastStep = 0; #reason = "initial"; #enabled = true;
  constructor() {
    this.#cap = this.#seed();
  }
  // Device hints set only the starting point, so a strong machine does not climb
  // from the floor. They never override measured behaviour afterwards.
  #seed() {
    const gb = typeof navigator !== "undefined" ? Number(navigator.deviceMemory) : NaN;
    const cores = typeof navigator !== "undefined" ? Number(navigator.hardwareConcurrency) : NaN;
    let seed = LOG.MAX_LINES;
    if (Number.isFinite(gb) && gb >= 8 && Number.isFinite(cores) && cores >= 8) seed = 15000;
    else if (Number.isFinite(gb) && gb >= 4) seed = 8000;
    if (!heap.available()) seed = Math.min(seed, CAP.NO_MEMORY_CEILING);
    return clamp(seed, CAP.FLOOR, this.ceiling());
  }
  ceiling() { return heap.available() ? CAP.CEILING : CAP.NO_MEMORY_CEILING; }
  get capacity() { return this.#cap; }
  get reason() { return this.#reason; }
  get enabled() { return this.#enabled; }
  // Fixed capacity: adaptation off, honour the configured value.
  setFixed(lines) {
    this.#enabled = false;
    this.#cap = clamp(~~lines || LOG.MAX_LINES, CAP.FLOOR, CAP.CEILING);
    this.#reason = "fixed by configuration";
  }
  /// Evaluates the signals and returns the capacity to use. `now` is passed in so
  /// the settling period is testable without a clock.
  evaluate(now = performance.now()) {
    if (!this.#enabled) return this.#cap;
    if (now - this.#lastStep < CAP.SETTLE_MS) return this.#cap;

    const free = heap.freeRatio();

    // No memory signal: hold at the conservative ceiling. Absence of evidence is
    // not headroom, so we neither grow nor punish the viewer for a browser that
    // does not expose the heap.
    if (free === null) {
      this.#goodRuns = 0;
      const capped = Math.min(this.#cap, CAP.NO_MEMORY_CEILING);
      if (capped !== this.#cap) {
        this.#cap = capped;
        this.#lastStep = now;
      }
      this.#reason = "holding: heap not measurable, capped conservatively";
      return this.#cap;
    }

    // Tight heap: shrink now. Fast, because running out of memory is a present
    // problem while spare capacity is only a missed opportunity.
    if (free < CAP.HEAP_TIGHT) {
      this.#goodRuns = 0;
      const next = clamp(Math.round(this.#cap * CAP.SHRINK), CAP.FLOOR, this.ceiling());
      this.#reason = next === this.#cap ? "at floor" : "reduced: low memory headroom";
      if (next !== this.#cap) { this.#cap = next; this.#lastStep = now; }
      return this.#cap;
    }

    // Ample heap for a sustained run: grow one capped step.
    if (free > CAP.HEAP_HEADROOM) {
      this.#goodRuns++;
      if (this.#goodRuns >= CAP.GOOD_RUNS) {
        const next = clamp(Math.round(this.#cap * CAP.GROW), CAP.FLOOR, this.ceiling());
        this.#reason = next === this.#cap ? "at ceiling" : "raised: memory headroom available";
        if (next !== this.#cap) { this.#cap = next; this.#lastStep = now; this.#goodRuns = 0; }
      }
      return this.#cap;
    }

    // Between the two thresholds: hold. The gap is deliberate — it is what stops
    // the capacity oscillating around a boundary.
    this.#goodRuns = 0;
    this.#reason = "holding: memory headroom adequate";
    return this.#cap;
  }
}
