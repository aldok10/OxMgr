import { sel } from "../core/const.js";

// Confirm modal
//
// Backed by a native <dialog> opened with showModal(), so dialog role, aria-modal,
// focus containment, focus restore, Escape and background inertness are browser
// behaviour rather than six things this class has to get right. See the markup comment
// in index.html for what the previous class-toggled div did not supply.
export class ConfirmModal {
  #overlay; #panel; #resolve = null;
  /// Releases this modal's DOM listeners in one call (see section 5 teardown).
  #life = new AbortController();
  _signal() { return { signal: this.#life.signal }; }
  destroy() { this.#life.abort(); }
  constructor(overlay) {
    this.#overlay = overlay;
    this.#panel = sel(".confirm-panel", overlay);
    this.els = { title: sel("#confirm-title"), message: sel("#confirm-message"), okBtn: sel("#confirm-ok") };
    sel("#confirm-cancel").addEventListener("click", () => this.#respond(false), this._signal());
    sel("#confirm-ok").addEventListener("click", () => this.#respond(true), this._signal());

    // `cancel` fires for Escape. Handling it here is what finally gives this dialog an
    // Escape path: the old global handler's #closeAll() never included this modal, so
    // Escape dismissed the log and detail overlays and left a "stop ALL processes"
    // prompt on screen.
    //
    // Escape resolves FALSE — a dismissal is a refusal, never an approval. Nothing about
    // pressing Escape says "yes, stop everything".
    this.#overlay.addEventListener("cancel", () => this.#respond(false), this._signal());

    // `close` covers any other route to closed (a form method=dialog, a stray close()
    // call) so the pending promise can never be abandoned. A confirm() whose promise
    // never settles leaves the caller's `.then` hanging forever.
    this.#overlay.addEventListener("close", () => {
      if (this.#resolve) { this.#resolve(false); this.#resolve = null; }
    }, this._signal());

    // Deliberately NO backdrop-click dismissal, unlike the log and detail dialogs.
    // An accidental press outside the panel must not answer a destructive prompt in
    // either direction. Escape and the explicit Cancel button are the exits, and both
    // are unambiguous.
  }
  #respond(result) {
    // Guard the close(): calling it on an already-closed dialog is harmless, but the
    // `close` listener above would then resolve a second time.
    if (this.#overlay.open) this.#overlay.close();
    if (this.#resolve) { this.#resolve(result); this.#resolve = null; }
  }
  confirm(action, target) {
    const isAll = target === "all";
    const actionLabel = action.charAt(0).toUpperCase() + action.slice(1);
    this.els.title.textContent = `${actionLabel} ${isAll ? "All Processes" : target}?`;
    this.els.message.textContent = isAll
      ? `This will ${action} ALL running processes. This action may cause service disruption.`
      : `Are you sure you want to ${action} "${target}"?`;
    this.els.okBtn.textContent = actionLabel;
    this.els.okBtn.className = ["stop", "restart"].includes(action) ? "small danger" : "small";

    // showModal(), not classList.add("open"): this is what moves focus into the dialog,
    // traps it there, makes everything behind it inert, and restores focus to the
    // invoker on close.
    //
    // Focus lands on Cancel rather than the default first-focusable, so the safe answer
    // is the one under the operator's fingers on a prompt that may stop every process.
    this.#overlay.showModal();
    sel("#confirm-cancel").focus();
    return new Promise(resolve => { this.#resolve = resolve; });
  }
}
