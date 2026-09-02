import { sel } from "../core/const.js";
import { Modal } from "./modal.js";
import { detailBody } from "./log-modal.js";
import { tableBtn } from "../shell/table.js";

// Detail modal
export class DetailModal extends Modal {
  #store;
  constructor(overlay, panel, store) {
    super(overlay, panel);
    this.#store = store;
    this.els = { title: sel("#detail-title"), body: sel("#detail-body"), actions: sel("#detail-actions") };
    sel("#detail-close").addEventListener("click", () => this.close(), this._signal());
  }
  show(name) {
    const proc = this.#store.find(name);
    if (!proc) return;
    this.els.title.textContent = `Process Detail — ${proc.name}`;
    this.els.body.innerHTML = detailBody(proc);
    this.#renderActions(proc);
    this.open();
  }
  // The full lifecycle set, so anything the narrow row drops is still reachable here.
  // These reuse tableBtn, so they carry the same data-action attributes and are dispatched
  // by the same handler — including confirmation for the destructive ones.
  #renderActions(proc) {
    const specs = [
      { label: proc.status === "running" ? "Restart" : "Start", act: "restart" },
      { label: "Stop", act: "stop", cls: "danger", run: 1 },
      { label: "Reload", act: "reload", run: 1 },
      { label: "Logs", act: "logs" },
    ];
    this.els.actions.replaceChildren(...specs.map(spec => tableBtn(proc, spec)));
  }
}
