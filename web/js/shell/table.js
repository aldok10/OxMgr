import { EMPTY, ERROR_MAX_LEN, ERROR_STATUSES, UNKNOWN, sel } from "../core/const.js";
import { fmt } from "../format/fmt.js";
import { FINDING_RANK, advisoryMarker, currentWithTypical, findingBand, findingMarker, findingState } from "./stats.js";

/// Sortable column keys — the subset of COLUMNS that have sort extractors.
const SORTABLE_KEYS = new Set(["name", "id", "pid", "uptime", "cpu", "ram", "restarts", "health"]);

// Table helper functions (pure, no class dependency)
export const tableBtn = (proc, { label, act, cls, run }) => {
  const btn = document.createElement("button");
  btn.className = cls ? `small ${cls}` : "small";
  btn.textContent = label; btn.disabled = run && proc.status !== "running";
  btn.dataset.target = proc.name; btn.dataset.action = act;
  return btn;
};
// Cells carry the same priority class as their header so CSS can hide a whole column,
// and a `data-label` so the stacked layout at phone width can name each value — column
// headers are not visible there. The label comes from one place, so a renamed heading
// cannot disagree with it.
const COLUMNS = [
  { key: "status", label: "Status" },
  { key: "name", label: "Name" },
  { key: "id", label: "ID" },
  { key: "pid", label: "PID" },
  { key: "uptime", label: "Uptime" },
  { key: "cpu", label: "CPU%" },
  { key: "ram", label: "RAM" },
  { key: "disk", label: "Disk" },
  { key: "restarts", label: "Restarts" },
  { key: "health", label: "Health" },
  { key: "actions", label: "Actions" },
];
const colAttrs = (key) => {
  const column = COLUMNS.find(entry => entry.key === key);
  return `class="col-${key}" data-label="${fmt.esc(column ? column.label : key)}"`;
};

// Builds the per-row expansion control. No new column (§D7): the COLUMNS contract is
// the single source of truth for headers and phone-width data-labels, and a column
// would cost width on every row for data most rows do not have. The control lives in
// the name cell and costs nothing until used.
const expandBtn = (proc, expandedNow) => {
  if (!descendantControlWanted(proc)) return "";
  return `<button type="button" class="expand-btn" data-expand="${fmt.esc(proc.name)}"`
    + ` aria-expanded="${expandedNow ? "true" : "false"}"`
    + ` aria-label="${expandedNow ? "Hide" : "Show"} descendants of ${fmt.esc(proc.name)}">`
    + `<span class="expand-glyph" aria-hidden="true">${expandedNow ? "\u25be" : "\u25b8"}</span></button>`;
};

/// Cluster marker beside the process name (cluster-instance-visibility).
///
/// Same name-cell placement as the advisory and finding markers: no column of its
/// own, visible at every width including the phone card layout. Text, not colour
/// alone. The tooltip carries requested vs observed as FACTS — a shortfall between
/// them is stated, never graded (5.7): calling it degraded is the analytics path's
/// job, and during startup or after a worker death a gap is normal.
const clusterMarker = (proc) => {
  if (!proc.cluster_mode) return "";
  const c = proc.cluster;
  const lines = ["Cluster mode"];
  if (c?.requested) {
    lines.push(c.requested.derived
      ? "requested: derived by the runtime at startup"
      : `requested: ${c.requested.count}`);
  }
  if (c?.observed) {
    lines.push(c.observed.status === "ok"
      ? `observed workers: ${c.observed.workers}`
      : `observed workers: unavailable (${c.observed.reason ?? "unknown"})`);
  }
  return `<span class="cluster-flag" title="${fmt.esc(lines.join("\n"))}">cluster</span>`;
};

const tableCells = (proc, expandedNow = false) => {
  // Read and write as one "r / w" value: two cells would not earn their width in a phone
  // card, and the pair is read together anyway. Absent rather than zero when the process is
  // stopped or the platform does not report it — 0 B reads as a measurement, not an absence.
  //
  // No network counterpart: sysinfo reports network per interface, not per PID, so any
  // figure here would silently be system-wide. Better absent than wrong.
  const diskKnown = proc.disk_read_bytes !== undefined && proc.status === "running";
  const view = {
    status: proc.status || UNKNOWN, pid: proc.pid ?? EMPTY,
    restarts: proc.restart_count ?? 0, health: proc.health_status || UNKNOWN,
    disk: diskKnown
      ? `${fmt.bytes(proc.disk_read_bytes)} / ${fmt.bytes(proc.disk_write_bytes)}`
      : EMPTY,
  };
  return `<td role="cell" ${colAttrs("status")}><span class="badge"><span class="dot ${fmt.esc(proc.status)}"></span>${fmt.esc(view.status)}</span></td>
    <td role="cell" class="name-cell col-name" data-label="Name">${expandBtn(proc, expandedNow)}<span class="cell-value">${fmt.esc(proc.name)}</span>${clusterMarker(proc)}${advisoryMarker(proc)}${findingMarker(proc)}</td>
    <td role="cell" class="num col-id" data-label="ID">${fmt.esc(proc.id)}</td>
    <td role="cell" class="num col-pid" data-label="PID">${fmt.esc(view.pid)}</td>
    <td role="cell" ${colAttrs("uptime")}>${fmt.esc(fmt.up(proc))}</td>
    <td role="cell" class="num col-cpu has-typical" data-label="CPU%">${currentWithTypical(proc, "cpu", value => `${fmt.pct(value)}%`)}</td>
    <td role="cell" class="num col-ram has-typical" data-label="RAM">${currentWithTypical(proc, "memory", value => fmt.bytes(value))}</td>
    <td role="cell" class="num col-disk" data-label="Disk">${fmt.esc(view.disk)}</td>
    <td role="cell" class="num col-restarts" data-label="Restarts">${fmt.esc(view.restarts)}</td>
    <td role="cell" ${colAttrs("health")}><span class="health ${fmt.esc(view.health)}">${fmt.esc(view.health)}</span></td>
    <td role="cell" class="actions col-actions" data-label="Actions"></td>`;
};
// Reduce a raw last_error into a single-line summary for the inline error row
const errorSummary = (lastError) => {
  // Prefer the "Last stderr:" portion when present
  const stderrMatch = lastError.match(/Last stderr:\n?([\s\S]*)/);
  const errorText = stderrMatch ? stderrMatch[1].trim() : lastError.split("\n")[0];
  const firstLine = errorText.split("\n")[0];
  return {
    text: firstLine.slice(0, ERROR_MAX_LEN),
    ellipsis: errorText.length > ERROR_MAX_LEN ? "\u2026" : "",
  };
};
const tableErrorRow = (proc) => {
  if (!proc.last_error || !ERROR_STATUSES.includes(proc.status)) return null;
  const row = document.createElement("tr");
  row.setAttribute("role", "row");
  row.className = "error-row";
  const cell = document.createElement("td");
  cell.setAttribute("role", "cell");
  cell.colSpan = 11;
  const { text, ellipsis } = errorSummary(proc.last_error);
  cell.innerHTML = `<span class="error-inline"><span class="error-icon">\u26a0</span> ${fmt.esc(text)}${ellipsis} <a href="#" class="error-detail-link" data-name="${fmt.esc(proc.name)}">View detail</a></span>`;
  row.appendChild(cell);
  return row;
};
// Width below which a row carries a reduced action set.
//
// Enlarging five buttons to 44px without reducing the count would be the worst outcome:
// 220px of targets plus gaps does not fit a 390px row beside anything else, and five
// large adjacent buttons where two are destructive is *more* dangerous than five small
// ones, because confidence rises with target size.
const NARROW_ACTIONS_MAX_WIDTH = 700;
const isNarrowViewport = () =>
  typeof window !== "undefined" && window.innerWidth <= NARROW_ACTIONS_MAX_WIDTH;

// --- Descendant attribution (managed-process-child-visibility) ---
//
// The payload's `descendants` field has three shapes, and each one earns a different
// rendering:
//   ok + rows      → an expansion control; expanding lists the children and the
//                    subtree total, labelled as INCLUDING descendants so it can never
//                    be read as a replacement for the row's own figures.
//   ok + empty     → observed-and-none. NO control at all: an inert disclosure that
//                    opens onto nothing is worse than its absence.
//   unavailable    → an expansion control whose panel states WHY (sampling disabled,
//                    or no sample yet). "We do not know" must stay reachable, or it
//                    reads as "there are none".
const descendantControlWanted = (proc) => {
  const d = proc.descendants;
  if (!d) return false;
  if (d.status === "unavailable") return true;
  return d.status === "ok" && Array.isArray(d.descendants) && d.descendants.length > 0;
};

const descendantRowsHtml = (proc) => {
  const d = proc.descendants;
  if (!d || !descendantControlWanted(proc)) return null;
  const cell = document.createElement("td");
  cell.setAttribute("role", "cell");
  cell.colSpan = COLUMNS.length;

  if (d.status === "unavailable") {
    // Stated, not blank: an empty list here would claim "observed, none".
    cell.innerHTML = `<div class="descendant-block">`
      + `<p class="descendant-unavailable">Descendants unavailable: ${fmt.esc(d.reason ?? "unknown reason")}</p>`
      + `</div>`;
    return cell;
  }

  // §D4, stated in the UI on purpose: child figures come from the 30s host sampler
  // while the parent row's own figures refresh every couple of seconds. The gap is a
  // design decision (a second collection pass would tax the supervision path), not a
  // bug to be quietly reconciled later — so the age difference is printed every time.
  const freshness = `<p class="descendant-freshness">Child figures sampled `
    + `${fmt.esc(fmt.when(d.observed_at))} — up to one sampling cycle older than this `
    + `row's own figures.</p>`;

  const rows = d.descendants.map(child => {
    const depthLabel = child.depth > 1 ? ` (depth ${child.depth})` : "";
    return `<tr class="descendant-entry" role="row">`
      + `<td role="cell" data-label="Name">${fmt.esc(child.name)}${depthLabel}</td>`
      + `<td role="cell" class="num" data-label="PID">${fmt.esc(child.pid)}</td>`
      + `<td role="cell" class="num" data-label="CPU%">${fmt.esc(fmt.pct(child.cpu_percent))}%</td>`
      + `<td role="cell" class="num" data-label="RAM">${fmt.esc(fmt.bytes(child.memory_bytes))}</td>`
      + `</tr>`;
  }).join("");

  // Totals are descendants-only by contract; they ADD to the row's own figures.
  // Labelled so neither reading — "replaces" nor "includes only listed children"
  // (totals cover truncated descendants too) — is available to get wrong silently.
  const truncatedNote = d.truncated
    ? ` <span class="descendant-truncated">(list truncated; totals cover all observed)</span>`
    : "";
  const total = `<p class="descendant-total">Subtree total, incl. descendants: `
    + `<strong>${fmt.esc(fmt.pct(d.descendants_cpu_percent))}%</strong> CPU, `
    + `<strong>${fmt.esc(fmt.bytes(d.descendants_memory_bytes))}</strong> RAM`
    + `${truncatedNote}</p>`;

  cell.innerHTML = `<div class="descendant-block">${freshness}`
    + `<table class="descendant-table" role="table">`
    + `<thead><tr><th scope="col">Name</th><th scope="col">PID</th>`
    + `<th scope="col">CPU%</th><th scope="col">RAM</th></tr></thead>`
    + `<tbody role="rowgroup">${rows}</tbody></table>${total}</div>`;
  return cell;
};

const tableRow = (proc, expandedNow = false) => {
  const row = document.createElement("tr");
  // Role restated because the stacked layout's `display: block` would otherwise strip it.
  row.setAttribute("role", "row");
  row.className = "clickable"; row.dataset.name = proc.name; row.innerHTML = tableCells(proc, expandedNow);
  const restartBtn = tableBtn(proc, { label: proc.status === "running" ? "Restart" : "Start", act: "restart" });
  // Narrow: the primary lifecycle action plus Logs and Detail — what an operator reaches
  // for while glancing at a phone. Stop and Reload move into the detail panel, which is
  // where a considered decision belongs anyway. They keep their confirmation either way.
  const specs = isNarrowViewport()
    ? Table.ACTS.filter(spec => spec.act === "logs" || spec.act === "detail")
    : Table.ACTS;
  sel(".actions", row).append(restartBtn, ...specs.map(spec => tableBtn(proc, spec)));
  return row;
};
const tableGroupRow = (name, count) => {
  const row = document.createElement("tr"); row.className = "group-row";
  row.setAttribute("role", "row");
  const cell = document.createElement("td"); cell.colSpan = 11;
  cell.setAttribute("role", "cell");
  
  // Check for namespace-scoped findings (e.g. restart storm)
  const scope = name === "default" ? "namespace:" : `namespace:${name}`;
  const groupFindings = findingState.get(scope) ?? [];
  const active = groupFindings.filter(item => item.status === "active");
  let marker = "";
  if (active.length > 0) {
    const worst = active.reduce((acc, item) => {
      const band = findingBand(item.confidence?.score);
      return (FINDING_RANK[band] ?? 0) > (FINDING_RANK[acc] ?? 0) ? band : acc;
    }, "info");
    const detail = active.slice(0, 3).map(item => {
      const key = item.key ?? {};
      const score = Number(item.confidence?.score);
      const pct = Number.isFinite(score) ? ` (${Math.round(score * 100)}% confidence)` : "";
      return `• ${key.detector ?? "?"} on ${key.metric ?? "?"}${pct}`;
    }).join("\n\n");
    marker = `<span class="finding-flag ${fmt.esc(worst)}" title="Namespace findings:\n\n${fmt.esc(detail)}">~</span>`;
  }

  cell.innerHTML = `<span class="group-label">${fmt.esc(name)}</span><span class="muted"> (${count})</span>${marker}`;
  row.appendChild(cell); return row;
};
// Table view
export class Table {
  #tbody; #empty; #store; #bus;
  // Which rows are expanded, by process name. Collapsed by default (§D7); the set
  // survives re-renders so a row does not fold itself up every 2s tick. A process
  // that disappears from the listing simply stops matching — no cleanup needed.
  #expanded = new Set();
  static ACTS = [{ label: "Stop", act: "stop", cls: "danger", run: 1 }, { label: "Reload", act: "reload", run: 1 }, { label: "Logs", act: "logs" }, { label: "Detail", act: "detail" }];
  constructor(tbody, empty, store, bus) { this.#tbody = tbody; this.#empty = empty; this.#store = store; this.#bus = bus; }
  /// Flips one row's expansion and re-renders. Called from the app's delegated tbody
  /// click handler, which is already covered by the App's AbortController — the
  /// expansion adds NO listener of its own, so there is nothing new to leak (4.9).
  toggle(name) {
    this.#expanded.has(name) ? this.#expanded.delete(name) : this.#expanded.add(name);
    this.render();
  }
  render() {
    const visible = this.#store.visible();
    this.#empty.classList.toggle("is-shown", visible.length === 0);
    const groups = new Map();
    visible.forEach(proc => { const key = proc.namespace || "default"; (groups.get(key) ?? groups.set(key, []).get(key)).push(proc); });
    const frag = document.createDocumentFragment();
    groups.forEach((members, name) => {
      frag.appendChild(tableGroupRow(name, members.length));
      members.forEach(proc => {
        const expandedNow = this.#expanded.has(proc.name);
        frag.appendChild(tableRow(proc, expandedNow));
        if (expandedNow) {
          const cell = descendantRowsHtml(proc);
          if (cell) {
            const detailRow = document.createElement("tr");
            detailRow.setAttribute("role", "row");
            detailRow.className = "descendant-row";
            detailRow.appendChild(cell);
            frag.appendChild(detailRow);
          }
        }
        const errRow = tableErrorRow(proc);
        if (errRow) frag.appendChild(errRow);
      });
    });
    this.#tbody.replaceChildren(frag);
  }
}
