import { clamp, isPanelFullViewport } from "../core/const.js";
import { fmt } from "../format/fmt.js";

// Modal base
export class Modal {
  /// AbortController for every DOM listener this modal (and its subclasses)
  /// registers. One `destroy()` releases the whole set, so a subclass never
  /// tracks individual handler references.
  #life = new AbortController();
  /// Options for `addEventListener`/`bus.on` in subclasses, bound to #life.
  _signal() { return { signal: this.#life.signal }; }
  constructor(overlay, panel) {
    this.overlay = overlay;
    this.panel = panel;
    this.dragX = 0;
    this.dragY = 0;
    this.#initFullscreen();
    this.#initResize();
    this.#initDrag();
    this.#initKeyboardGeometry();
    // Must be last: it dispatches into `close()`, which subclasses override, so the
    // subclass's own fields have to be constructed before the listener can fire.
    this._bindNativeClose();
  }
  destroy() { this.#life.abort(); }
  // Header-drag movement. Applied as a translate offset rather than absolute
  // coordinates: the overlay's flexbox owns the resting position, so an offset
  // keeps layout authoritative and makes reset just "clear the offset".
  #initDrag() {
    const header = this.panel.querySelector(".log-head");
    if (!header) return;
    // Inline style would beat the stylesheet, so the cursor follows the same query that
    // gates the gesture: a header that cannot be dragged must not advertise that it can.
    const syncCursor = () => { header.style.cursor = isPanelFullViewport() ? "default" : "move"; };
    syncCursor();
    window.addEventListener("resize", syncCursor, this._signal());
    // `pointerdown`, not `mousedown`: these were the only two `mousedown` listeners in
    // the frontend and there were zero `pointerdown`/`touchstart`, so the panel could be
    // moved and resized with a mouse and by no other input. Pointer events cover mouse,
    // touch and pen from one code path.
    header.addEventListener("pointerdown", evt => {
      // Controls in the header keep working: a press on one is not a drag.
      if (evt.target.closest("button, input, select, textarea, a, .seg")) return;
      if (this.panel.classList.contains("fullscreen")) return;
      // Nothing to drag to when the panel already fills the viewport, and a stray offset
      // would push it off-screen with no backdrop left to grab it back.
      if (isPanelFullViewport()) return;
      // Secondary buttons must not drag: a right-click is a context menu.
      if (evt.button !== 0) return;
      evt.preventDefault();
      const startX = evt.clientX, startY = evt.clientY;
      const baseX = this.dragX, baseY = this.dragY;
      const rect = this.panel.getBoundingClientRect();
      // Clamp so a margin stays on screen: the header is the only grab
      // surface, so a fully off-screen panel is unreachable.
      const KEEP = 60;
      const minX = baseX - rect.left - rect.width + KEEP;
      const maxX = baseX + (window.innerWidth - rect.left - KEEP);
      const minY = baseY - rect.top;
      const maxY = baseY + (window.innerHeight - rect.top - KEEP);

      const onMove = (move) => {
        this.dragX = clamp(baseX + move.clientX - startX, minX, maxX);
        this.dragY = clamp(baseY + move.clientY - startY, minY, maxY);
        this.panel.style.transform = `translate(${this.dragX}px, ${this.dragY}px)`;
      };
      const onUp = () => {
        document.removeEventListener("pointermove", onMove);
        document.removeEventListener("pointerup", onUp);
        document.removeEventListener("pointercancel", onUp);
      };
      // These are gesture-scoped: the abort on destroy also releases them
      // mid-drag, leaving no orphaned document listeners.
      // `pointercancel` matters for touch specifically — the browser can take the
      // gesture away (scroll takeover, palm rejection) without ever sending
      // `pointerup`, which would leave the move listener attached for good.
      document.addEventListener("pointermove", onMove, this._signal());
      document.addEventListener("pointerup", onUp, this._signal());
      document.addEventListener("pointercancel", onUp, this._signal());
    }, this._signal());
  }
  #resetDrag() {
    this.dragX = 0; this.dragY = 0;
    this.panel.style.transform = "";
  }

  /// Keyboard equivalents for the pointer gestures (WCAG 2.2 SC 2.5.7 Dragging
  /// Movements): every move and resize must be reachable without a dragging movement.
  ///
  /// Bound on the dialog rather than the panel, because `showModal()` puts focus inside
  /// the dialog and keeps it there — so the dialog is the reliable place to catch keys
  /// however the operator navigated in.
  ///
  /// Alt is the modifier because unmodified arrows belong to the content: the log viewer
  /// scrolls with them and the search field uses them for caret movement. Alt+Arrow is
  /// not otherwise taken here.
  ///
  ///   Alt + arrows           move by a coarse step
  ///   Alt + Shift + arrows   move by a fine step
  ///   Alt + Enter            cycle through discrete sizes (the keyboard answer to eight
  ///                          directional handles, which do not map to a key set)
  ///   Alt + 0                reset position and size while the panel is open
  #initKeyboardGeometry() {
    const COARSE = 40, FINE = 8;
    // Fractions of the viewport, largest last so the cycle ends at fullscreen.
    const SIZES = [0.5, 0.7, 0.9];

    this.overlay.addEventListener("keydown", evt => {
      if (!evt.altKey) return;
      // Never steal a key from a text control: Alt+Arrow is a word-wise caret move on
      // some platforms, and the log search field is inside this dialog.
      if (evt.target.closest("input, textarea, select")) return;
      // Same gate as the pointer gestures: nothing to move or resize when the panel
      // already fills the viewport, and an offset there would push it off-screen.
      if (isPanelFullViewport()) return;

      const step = evt.shiftKey ? FINE : COARSE;
      let handled = true;

      switch (evt.key) {
        case "ArrowLeft":  this.dragX -= step; break;
        case "ArrowRight": this.dragX += step; break;
        case "ArrowUp":    this.dragY -= step; break;
        case "ArrowDown":  this.dragY += step; break;
        case "Enter":      this.#cycleSize(SIZES); break;
        case "0":          this.#resetGeometry(); break;
        default: handled = false;
      }
      if (!handled) return;
      evt.preventDefault();

      if (evt.key.startsWith("Arrow")) {
        // Clamp so the panel cannot be walked off-screen: the header is the only pointer
        // grab surface, so a fully off-screen panel would be unrecoverable by mouse.
        const rect = this.panel.getBoundingClientRect();
        const KEEP = 60;
        this.dragX = clamp(this.dragX, this.dragX - rect.left - rect.width + KEEP,
                           this.dragX + (window.innerWidth - rect.left - KEEP));
        this.dragY = clamp(this.dragY, this.dragY - rect.top,
                           this.dragY + (window.innerHeight - rect.top - KEEP));
        this.panel.style.transform = `translate(${this.dragX}px, ${this.dragY}px)`;
      }
      // The log viewer sizes its row window from the panel box, so a geometry change
      // has to tell it — the same hook the fullscreen toggle uses.
      this.onGeometryChange?.();
    }, this._signal());
  }

  /// Steps to the next discrete size. Starts from whichever entry is closest to the
  /// current width, so the cycle stays predictable after a pointer resize.
  #cycleSize(sizes) {
    this.panel.classList.remove("fullscreen");
    const current = this.panel.getBoundingClientRect().width / window.innerWidth;
    let idx = 0;
    let best = Infinity;
    sizes.forEach((frac, i) => {
      const d = Math.abs(frac - current);
      if (d < best) { best = d; idx = i; }
    });
    const next = sizes[(idx + 1) % sizes.length];
    this.panel.style.width = `${Math.round(window.innerWidth * next)}px`;
    this.panel.style.height = `${Math.round(window.innerHeight * next)}px`;
  }

  /// Returns the panel to its default position and size WITHOUT closing it.
  /// `#resetDrag()` already did this on close; an operator who has moved a panel
  /// somewhere awkward needs it while the panel is still open.
  #resetGeometry() {
    this.#resetDrag();
    this.panel.classList.remove("fullscreen");
    this.panel.style.width = "";
    this.panel.style.height = "";
  }
  #initFullscreen() {
    const header = this.panel.querySelector(".log-head");
    if (header) {
      header.addEventListener("dblclick", evt => {
        // Same control exclusion as dragging: double-clicking inside the
        // search field selects a word, it does not maximise the panel.
        if (evt.target.closest("button, input, select, textarea, a, .seg")) return;
        this.panel.classList.toggle("fullscreen");
        this.onGeometryChange?.();
      }, this._signal());
    }
  }
  #initResize() {
    const handles = ['n', 's', 'e', 'w', 'nw', 'ne', 'sw', 'se'];
    handles.forEach(dir => {
      const handle = document.createElement('div');
      handle.className = `resize-handle resize-handle-${dir}`;
      handle.dataset.resize = dir;
      this.panel.appendChild(handle);
    });
    this.panel.addEventListener('pointerdown', this.#onResizeStart.bind(this), this._signal());
  }
  #onResizeStart(evt) {
    const handle = evt.target.closest('.resize-handle');
    if (!handle || this.panel.classList.contains('fullscreen')) return;
    if (evt.button !== 0) return;
    evt.preventDefault();
    const dir = handle.dataset.resize;
    const rect = this.panel.getBoundingClientRect();
    const startX = evt.clientX, startY = evt.clientY;
    const startW = rect.width, startH = rect.height;
    const minW = 320, minH = 200;
    const maxW = window.innerWidth * 0.98, maxH = window.innerHeight * 0.98;

    const onMove = (e) => {
      const dx = e.clientX - startX, dy = e.clientY - startY;
      let newW = startW, newH = startH;
      if (dir.includes('e')) newW = clamp(startW + dx, minW, maxW);
      if (dir.includes('w')) newW = clamp(startW - dx, minW, maxW);
      if (dir.includes('s')) newH = clamp(startH + dy, minH, maxH);
      if (dir.includes('n')) newH = clamp(startH - dy, minH, maxH);
      this.panel.style.width = `${newW}px`;
      this.panel.style.height = `${newH}px`;
    };
    const onUp = () => {
      document.removeEventListener('pointermove', onMove);
      document.removeEventListener('pointerup', onUp);
      document.removeEventListener('pointercancel', onUp);
    };
    document.addEventListener('pointermove', onMove, this._signal());
    document.addEventListener('pointerup', onUp, this._signal());
    document.addEventListener('pointercancel', onUp, this._signal());
  }
  /// Guards against `close()` re-entering itself. The native `close` event handler
  /// below calls `close()`, and `close()` itself calls `overlay.close()` which fires
  /// that event — so without this, closing via the Close button runs a subclass's
  /// teardown twice.
  #closing = false;

  /// Routes a NATIVE close (Escape, or the browser closing the dialog) into the
  /// subclass's `close()`.
  ///
  /// This is load-bearing and easy to miss. `showModal()` gives Escape for free, but
  /// Escape closes the ELEMENT — it does not call `LogModal.close()`, which is where
  /// `#api.stopLog()`, `#resizeObs.disconnect()` and `#view.destroy()` live. Convert to
  /// `<dialog>`, delete the old global Escape handler, and Escape would silently leak an
  /// SSE subscription, a ResizeObserver and a virtualized view on every dismissal.
  ///
  /// Registered by the base class so every present and future subclass inherits it,
  /// rather than each one remembering to wire its own.
  _bindNativeClose() {
    this.overlay.addEventListener("close", () => this.close(), this._signal());
  }
  open() {
    // showModal(), not a class toggle: this is what moves focus in, contains it,
    // makes the background inert, restores focus on close, and enables Escape.
    // Guarded because calling showModal() on an already-open dialog throws.
    if (!this.overlay.open) this.overlay.showModal();
  }
  close() {
    if (this.#closing) return;
    this.#closing = true;
    try {
      this.#closeInner();
    } finally {
      this.#closing = false;
    }
  }
  #closeInner() {
    if (this.overlay.open) this.overlay.close();
    this.panel.classList.remove("fullscreen");
    // A reopened panel appears at its default position and size: leaving a
    // drag offset or an inline size behind makes the next open depend on
    // whatever the last session happened to do.
    this.#resetDrag();
    this.panel.style.width = "";
    this.panel.style.height = "";
  }
}
// Files tab: listing of the active log file and its rotated archives.
export const fileRow = (entry, name) => {
  const row = document.createElement("div");
  row.className = "log-file";
  const badge = entry.index === 0 ? `<span class="lf-badge active">active</span>` : `<span class="lf-badge">#${entry.index}</span>`;
  const query = `stream=${encodeURIComponent(entry.stream)}${entry.index ? `&index=${entry.index}` : ""}`;
  row.innerHTML = `${badge}
    <span class="lf-name">${fmt.esc(entry.filename)}</span>
    <span class="spacer"></span>
    <span class="lf-meta">${fmt.esc(fmt.bytes(entry.size))} · ${fmt.esc(fmt.when(entry.modified_at))}</span>
    <span class="lf-actions">
      <a class="small" href="/logs/${encodeURIComponent(name)}?${query}" target="_blank" rel="noopener"><button class="small">View</button></a>
      <a class="small" href="/api/processes/${encodeURIComponent(name)}/logs/download?${query}"><button class="small">Download</button></a>
    </span>`;
  return row;
};
