import { INT, LIVE } from "./const.js";

// Filter predicates (pure, one concern each)
const matchesGroup = (proc, group) => !group || (proc.namespace ?? "") === group;
const matchesStatus = (proc, status) => !status || proc.status === status;
const matchesTerm = (proc, term) => !term || `${proc.name} ${proc.id} ${proc.status ?? ""}`.toLowerCase().includes(term);

// ── Sort extractors ──────────────────────────────────────────────────────────
// Each extractor returns a comparable value from a process record. Sortable
// columns map 1:1 to an extractor; non-sortable columns have none.
const UPTIME_NOW = () => ~~(Date.now() / 1000);
const SORT_EXTRACTORS = {
  name:     (p) => (p.name ?? "").toLowerCase(),
  id:       (p) => (p.id ?? "").toLowerCase(),
  pid:      (p) => Number(p.pid) || 0,
  uptime:   (p) => (LIVE.includes(p.status) && p.last_started_at) ? Math.max(0, UPTIME_NOW() - p.last_started_at) : -1,
  cpu:      (p) => Number(p.cpu_percent) || 0,
  ram:      (p) => Number(p.memory_bytes) || 0,
  restarts: (p) => Number(p.restart_count) || 0,
  health:   (p) => (p.health_status ?? "").toLowerCase(),
};

/// Stable multi-key sort. `keys` is an array of `{ key, dir }` where `dir`
/// is 1 (ascending) or -1 (descending). Null/empty values sort to the end.
const multiSort = (arr, keys) => {
  if (!keys.length) return arr;
  // toSorted is non-mutating and stable.
  return arr.toSorted((a, b) => {
    for (const { key, dir } of keys) {
      const extract = SORT_EXTRACTORS[key];
      if (!extract) continue;
      const va = extract(a), vb = extract(b);
      // "unavailable" (uptime === -1, health === "") sorts last regardless of direction.
      if (va < 0 && vb < 0) continue;
      if (va < 0) return 1;
      if (vb < 0) return -1;
      if (va < vb) return -dir;
      if (va > vb) return dir;
    }
    // Tie-break by name for deterministic order.
    const na = (a.name ?? "").toLowerCase(), nb = (b.name ?? "").toLowerCase();
    return na < nb ? -1 : na > nb ? 1 : 0;
  });
};

// State store
export class Store {
  #data = { procs: [], procsLoaded: false, search: "", group: "", status: "", interval: INT.DEF,
    advisories: {}, sort: [] };
  get(key) { return this.#data[key]; }
  set(key, val) { 
    if (key === "procs") this.#data.procsLoaded = true;
    this.#data[key] = val; 
  }
  find(name) { return this.#data.procs.find(proc => proc.name === name); }
  /// Advisories for one process, or an empty array. Never `undefined`, so a caller cannot
  /// accidentally treat "not loaded yet" as "none" by reading `.length` of nothing.
  advisoriesFor(name) { return this.#data.advisories[name] ?? []; }

  /// Toggle sort on a column key. Click cycles: asc → desc → remove.
  /// Shift+click appends a secondary sort instead of replacing.
  /// Returns the new sort state so the caller can update the UI.
  toggleSort(key, additive = false) {
    const sort = additive ? [...this.#data.sort] : [];
    const idx = sort.findIndex(s => s.key === key);
    if (idx >= 0) {
      const existing = sort[idx];
      if (existing.dir === 1) {
        // asc → desc
        sort[idx] = { key, dir: -1 };
      } else {
        // desc → remove
        sort.splice(idx, 1);
      }
    } else {
      // New sort key: asc first
      sort.push({ key, dir: 1 });
    }
    this.#data.sort = sort;
    return sort;
  }

  visible() {
    const term = this.#data.search.trim().toLowerCase();
    const { group, status, sort } = this.#data;
    const filtered = this.#data.procs.filter(proc => matchesGroup(proc, group) && matchesStatus(proc, status) && matchesTerm(proc, term));
    return multiSort(filtered, sort);
  }

  namespaces() { return [...new Set(this.#data.procs.map(proc => proc.namespace).filter(Boolean))].sort(); }
  counts() {
    return this.#data.procs.reduce((cnt, proc) => {
      cnt[proc.status] = (cnt[proc.status] ?? 0) + 1;
      return cnt;
    }, {});
  }
}
