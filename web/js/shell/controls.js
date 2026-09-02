import { fmt } from "../format/fmt.js";

// Group select
export class GroupSel {
  #elem; #store;
  constructor(elem, store) { this.#elem = elem; this.#store = store; }
  render() {
    const namespaces = this.#store.namespaces(), selected = this.#elem.value;
    const options = namespaces.map(nsp => `<option value="${fmt.esc(nsp)}">${fmt.esc(nsp)}</option>`).join("");
    this.#elem.innerHTML = `<option value="">All groups</option>${options}`;
    this.#elem.value = namespaces.includes(selected) ? selected : "";
    this.#store.set("group", this.#elem.value);
  }
}
// Main controller
/// The Auto / Light / Dark control.
///
/// Applying a theme is one attribute on `<html>`: every themed value is a `light-dark()` pair
/// resolved against `color-scheme`, so narrowing that property re-resolves the whole palette.
/// There is no class to add per element and no second stylesheet.
///
/// The stored choice is applied by the inline script in the document head, not here — this
/// class only reflects and changes it. Doing the initial apply from this file would paint once
/// in the OS theme and then re-paint in the chosen one.
export class ThemeSwitch {
  static KEY = "oxmgr-theme";
  static VALUES = ["auto", "light", "dark"];

  #root = document.documentElement;
  /// Releases this component's listeners in one call (section 5 teardown).
  #life = new AbortController();
  _signal() { return { signal: this.#life.signal }; }
  destroy() { this.#life.abort(); }

  constructor(container) {
    // Absent on the standalone log page, which shares this script but not the header.
    if (!container) return;
    this.#sync(container);
    container.addEventListener("change", (evt) => {
      const value = evt.target?.value;
      if (!ThemeSwitch.VALUES.includes(value)) return;
      this.#apply(value);
      this.#store(value);
    }, this._signal());
  }

  /// Checks the radio matching the stored choice, so the control shows the real state on load
  /// rather than always showing its markup default.
  #sync(container) {
    const current = this.#read();
    const input = container.querySelector(`input[value="${current}"]`);
    if (input) input.checked = true;
  }

  #apply(value) {
    if (value === "auto") {
      // Removing the attribute IS auto: the base `color-scheme: light dark` then follows the
      // OS. Setting `data-theme="auto"` would need a third CSS rule that does the same thing.
      delete this.#root.dataset.theme;
    } else {
      this.#root.dataset.theme = value;
    }
  }

  #read() {
    try {
      const stored = localStorage.getItem(ThemeSwitch.KEY);
      return ThemeSwitch.VALUES.includes(stored) ? stored : "auto";
    } catch {
      return "auto";
    }
  }

  #store(value) {
    try {
      // "auto" is removed rather than stored, so the key's absence and the auto choice are the
      // same state. Storing the string would leave two representations of one thing.
      if (value === "auto") localStorage.removeItem(ThemeSwitch.KEY);
      else localStorage.setItem(ThemeSwitch.KEY, value);
    } catch {
      // Private mode and blocked-storage policies throw. The theme still applies for this
      // session; only persistence is lost, which is the right thing to degrade.
    }
  }
}
