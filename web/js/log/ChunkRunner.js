// Drives a generator across frames: resumes it while frame budget remains,
// then defers. Holding one generator (not a queue of them) is deliberate —
// a new bulk job supersedes the old one, which is the semantics every caller
// here wants. Dropping the reference abandons the work; nothing else to undo.
// Share of a frame a chunk may consume. Derived from the frame interval the
// display actually reports, not a fixed 8ms: that was half of a 16.7ms frame
// and is the WHOLE of a 120Hz frame (measured 8.3ms), leaving nothing for
// style, layout and paint. Seeded conservatively for 60Hz and narrowed once
// real intervals are observed.
const FRAME_BUDGET_SHARE = 0.5;   // half a frame, leaving the rest for paint
const FRAME_MIN_BUDGET_MS = 2;    // never so small that no progress is made
const frameClock = {
  intervalMs: 16.7,               // conservative 60Hz seed
  budgetMs: 8,
  ready: false,
  samples: [],
  // Fed from ChunkRunner's frames: a rolling median is robust against the odd
  // stalled frame in a way a mean is not. Its purpose is to size the per-frame work
  // budget to the display actually in use — nothing more. Retention capacity is
  // governed by memory, for the reason recorded on CapacityController.
  observe(deltaMs) {
    if (!(deltaMs > 0) || deltaMs > 250) return; // ignore tab-switch gaps
    this.samples.push(deltaMs);
    if (this.samples.length > 60) this.samples.shift();
    if (this.samples.length < 12) return;
    const sorted = [...this.samples].sort((a, b) => a - b);
    this.intervalMs = sorted[sorted.length >> 1];
    this.budgetMs = Math.max(FRAME_MIN_BUDGET_MS, this.intervalMs * FRAME_BUDGET_SHARE);
    this.ready = true;
  },
};
export class ChunkRunner {
  #gen = null; #raf = 0; #onDone = null; #lastFrame = 0;
  // Runs `gen` to completion across frames. Any previous job is abandoned.
  run(gen, onDone = null) {
    this.cancel();
    this.#gen = gen;
    this.#onDone = onDone;
    this.#schedule();
  }
  cancel() {
    if (this.#raf) cancelAnimationFrame(this.#raf);
    this.#raf = 0;
    // Let the generator release whatever it holds instead of waiting for GC
    // to notice a suspended frame.
    this.#gen?.return?.();
    this.#gen = null;
    this.#onDone = null;
  }
  get running() { return this.#gen !== null; }
  #schedule() {
    if (!this.#gen || this.#raf) return;
    this.#raf = requestAnimationFrame(now => {
      this.#raf = 0;
      // Feed the clock from the interval between our own frames, so the budget
      // tracks the display this dashboard is actually on.
      if (this.#lastFrame) frameClock.observe(now - this.#lastFrame);
      this.#lastFrame = now;
      this.#pump();
    });
  }
  #pump() {
    const gen = this.#gen;
    if (!gen) return;
    const deadline = performance.now() + frameClock.budgetMs;
    try {
      // Each `next()` does one bounded chunk. We keep pulling while there is
      // budget so a fast machine finishes sooner without ever blocking long.
      do {
        if (gen.next().done) {
          const done = this.#onDone;
          this.#gen = null; this.#onDone = null;
          done?.();
          return;
        }
      } while (performance.now() < deadline);
    } catch (err) {
      this.#gen = null; this.#onDone = null;
      throw err;
    }
    this.#schedule();
  }
}
