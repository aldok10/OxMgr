import { EVENT_NAMES } from "./const.js";

// EventBus
export class Bus {
  #subs = new Map();
  on(evt, func, { signal } = {}) {
    if (!EVENT_NAMES.has(evt)) throw new ReferenceError(`bus.on: "${evt}" is not in the EVENTS catalogue`);
    (this.#subs.get(evt) ?? this.#subs.set(evt, new Set()).get(evt)).add(func);
    if (signal) signal.addEventListener("abort", () => this.off(evt, func), { once: true });
  }
  off(evt, func) { this.#subs.get(evt)?.delete(func); }
  emit(evt, data) {
    if (!EVENT_NAMES.has(evt)) throw new ReferenceError(`bus.emit: "${evt}" is not in the EVENTS catalogue`);
    this.#subs.get(evt) && [...this.#subs.get(evt)].forEach(func => func(data));
  }
  // Live subscription count, for teardown verification (growth across
  // create/destroy cycles must be zero).
  count() {
    let n = 0;
    for (const set of this.#subs.values()) n += set.size;
    return n;
  }
}
