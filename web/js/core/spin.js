import { SPIN_MS } from "./const.js";

// Spinner with delay
export class Spin {
  #timers = new Map();
  show(elem, key) { clearTimeout(this.#timers.get(key)); this.#timers.set(key, setTimeout(() => elem.classList.add("on"), SPIN_MS)); }
  hide(elem, key) { clearTimeout(this.#timers.get(key)); this.#timers.delete(key); elem.classList.remove("on"); }
}
