# UI Guide

Oxmgr provides two interactive dashboards:
- **Terminal UI** (`oxmgr ui` or `oxmgr ui tui`): Full-featured TUI for terminal environments
- **Web Dashboard** (`oxmgr ui web`): Browser-based dashboard with real-time SSE streaming

---

# Terminal UI

`oxmgr ui` is the interactive terminal dashboard for fleet monitoring and quick actions.

## Start

```bash
oxmgr ui
oxmgr ui --interval-ms 500
```

Refresh interval is clamped to `200..5000 ms`.

## Key Controls

- `j` / `k` or `↑` / `↓`: move selection
- `/`: open search input for live filtering by name / namespace / command
- `f`: cycle process filter (`all` -> `running` -> `stopped` -> `unhealthy`)
- `o`: cycle sort (`id` -> `name` -> `cpu` -> `ram` -> `restarts`)
- `n`: open create-process modal
- `s`: stop selected service
- `d`: open delete confirmation for selected service
- `r`: reload selected service (best-effort no-downtime)
- `Shift+R`: restart selected service
- `l`: open fullscreen log viewer for selected service
- `p`: pull selected service from git and auto reload/restart on commit change
- `t`: show latest log line snapshot
- `g` or `Space`: refresh immediately
- `?`: open/close help overlay
- `Esc`: open quick menu
- `q`: quit

Delete confirmation uses `Enter` or `y` to confirm, and `Esc` or `n` to cancel.

Search input uses:

- type to filter immediately
- `Backspace`: delete one character
- `Delete` or `Ctrl+U`: clear query
- `Enter` or `Esc`: close the input while keeping the current filter text

## Log Viewer

Press `l` on a selected service to open the fullscreen log viewer.

- `j` / `k` or `↑` / `↓`: scroll
- `PageUp` / `PageDown`: fast scroll
- `Home` / `End`: jump to top/bottom
- `Tab`: switch between `stderr` and `stdout`
- `g` or `Space`: reload log files from disk
- `l` or `Esc`: close the viewer

## Mouse Controls

- Left click on a row: select service
- Mouse wheel: move selection
- Esc menu buttons are clickable (`Resume`, `Quit`)

## Panels

- Header: timestamp, refresh cadence, selected-service summary
- Fleet summary: visible/total plus running/restarting/stopped/unhealthy counters
- Left services pane: ID, name, status, PID, uptime, CPU, RAM, health
- Right sidebar (on selected process): full-height runtime/process/git details and compact bars
- Create modal: in-UI process creation flow
- Fullscreen log viewer: scrollable per-service stdout/stderr view

## Notes

- UI uses ANSI + UTF line drawing and progress bars.
- Rendering avoids last-column overflow artifacts by reserving one column.
- Dashboard redraw is event-driven to reduce unnecessary flicker.

---

# Web Dashboard

`oxmgr ui web` opens a browser-based dashboard with real-time updates via Server-Sent Events (SSE).

## Start

```bash
oxmgr ui web
oxmgr ui web --port 8080
oxmgr ui web --bind 0.0.0.0 --no-open
```

Or navigate directly to `http://127.0.0.1:46001` while the daemon is running.

## Features

- Real-time process list with status, CPU, RAM, uptime, and health
- Host panel with length-based bar gauges for CPU, RAM, and SWAP, plus CPU stats,
  per-core usage, storage and network, and top-consumer listings
- Live log streaming (stdout/stderr/error) via SSE
- Process control: stop, restart, reload from the browser
- Prometheus metrics at `/metrics`
- Responsive design for desktop sidebar, tablet banner, and mobile overlay

### Asset Serving Modes and Caching

The dashboard's JavaScript modules, stylesheet, icon and any other file placed
under the web directory are served as individual files at their own URLs — not
as one concatenated bundle. The browser loads the entry point and follows the
ESM import graph itself.

Two serving modes, same URL space:

*   **On-disk mode** — set `OXMGR_WEB_DIR` to a directory; every file under it
    is served at the path that locates it (subdirectories included). Content is
    read from disk on each request, so editing a file takes effect on reload
    with no daemon restart. Paths are resolved and contained within the
    directory before any read; traversal, encoded or symlinked escapes are
    refused. Unknown file extensions serve with a generic binary content type.
*   **Embedded mode** — leave `OXMGR_WEB_DIR` unset; the binary serves its own
    compiled-in copies at the same paths, so production images need no asset
    directory.

Caching contract: every asset response carries an ETag validator. A repeat
request presenting a matching validator is answered `304 Not Modified` without
a body. For embedded assets the validator derives from build-time content, so
a rebuild invalidates caches; for on-disk assets it reflects the file's
current state, so editing a file invalidates its previous copy immediately.
Responses carry `Vary: Accept-Encoding`; compression is negotiated per asset,
and SSE endpoints are never compressed.

Measured cost of per-module delivery versus the old single bundle (same gzip
level): +15.0% transferred bytes (79374 → 91290), and ~5 sequential HTTP/1.1
rounds instead of 1 on a cold load. Both figures are recorded as spec bounds
in `dashboard-module-boundaries` — a regression past them is a defect, and
enabling HTTP/2 would recover most of the round-trip cost.

### Design Tokens Scale

This dashboard uses a `rem`-based token system for typography and spacing, ensuring consistency and scalability across font sizes and viewports.
*   **Typography Scale:** 10 steps (e.g., `--text-3xs` 0.5625rem up to `--text-display` 3rem) replaces the previous literal `px` declarations.
*   **Spacing Scale:** 9 steps (e.g., `--space-3xs` 0.125rem up to `--space-3xl` 2rem) replaces the many literal `px` values used for padding and margin.
*   A few `px` values are intentionally retained for border widths, outline offsets, and 1px rules because those are pixel-sensitive.

### Accessibility and Announcements

This dashboard uses ARIA live regions for critical status announcements, ensuring assistive technology users receive up-to-date information about UI changes.
*   **Live Regions:** Two regions (`role="status"` and `role="alert"`) are used for announcements. They are visually hidden (`.sr-only`) but remain available to screen readers.
*   **Status Announcements:** All action outcomes (start, stop, restart, delete) and daemon connectivity changes are announced explicitly.
*   **Stream Silence:** Per-tick data (e.g., CPU/RAM updates) is not announced, avoiding excessive screen reader chatter and keeping the experience smooth.
*   **Semantic Dialogs:** All critical overlays (confirm, log, detail) use the native `<dialog>` element, ensuring correct modal behavior, focus containment, and dismissal via the Escape key.

### UI Contribution Guide

When developing the dashboard UI, follow these two primary rules:
1.  **Never announce per-tick figures** (e.g., CPU/RAM updates). Rapidly changing data makes the dashboard unreadable for screen readers.
2.  **Never declare text sizes in `px`**. Use the available token system to ensure scalability and accessibility.

### Process Descendants and Cluster Shape

A managed process can reveal the child processes attributed to it, and a cluster
process additionally reports its requested-versus-observed worker shape.

**Attribution boundary.** Descendants are resolved by walking parent pids
transitively from each managed process's pid, so children AND grandchildren land
under the process that ultimately spawned them. A child's own children count
toward its ancestor's subtree; a child's disk I/O does not — per-process I/O
remains bounded by `process-io-metrics` and is never rolled up.

**Two clocks, on purpose.** The parent row's own CPU/RAM refresh every couple of
seconds; child figures come from the host-wide sampler, which completes one
observation every 30 seconds. Every descendants panel prints its sample time.
The duty-cycle arithmetic behind this: sampling every process tree every 2 s
would multiply the sampler's cost by 15 for data that changes least urgently;
the 30 s cadence keeps the daemon lean while the freshness marker keeps the
figure honest. Do not "fix" the two clocks into one — if child staleness ever
proves unacceptable, the honest change is a shorter sampler interval with its
duty cost stated, not a hidden second scan.

Because of that cadence, the host-consumers panel and a per-process expansion
can show different sets at the same moment: they are views of samples taken
seconds apart, not an inconsistency.

**Three states, deliberately distinct.** Expanding a row shows either the
children (possibly none — observed-and-none is stated in words), or
`unavailable` with its reason: sampling disabled (`OXMGR_HOST_CONSUMERS`), or
the first sample not yet completed since daemon start. Unavailable is never
rendered as an empty list or as zero.

**Cluster shape.** A cluster-mode process carries a `cluster` chip and reports:

- **requested** — the configured worker count. When no count was configured,
  the Node bootstrap derived one from CPU availability at start; the daemon
  cannot know that number, so it reports *derived* rather than borrowing the
  observed count (which would make every shortfall invisible).
- **observed** — workers the last sample attributed to this bootstrap's pid,
  unavailable under the same conditions as descendants.

The two figures legitimately differ during startup and after a worker death;
they are shown side by side so a gap is visible, and nothing attaches a verdict
to it. Workers carry pid and name only — per-worker identity such as
`NODE_APP_INSTANCE` is not read (process environments are more sensitive than
the command lines the dashboard already withholds). Expanded instances
(`instances: N`) stay separate managed processes; an instance that is itself a
cluster reports its own worker count. There is no per-worker start/stop/restart.

## Authentication

Configure Basic Auth via `[http_server]` in `oxfile.toml`:

```toml
[http_server]
port = "0.0.0.0:46001"
username = "admin"
password = "changeme"
interval_ms = 1000  # refresh interval in ms (200-10000)
label = "PRODUCTION"  # environment label in header
label_color = "#ef4444"  # label color (CSS value)
```

Or via environment variables (take precedence over oxfile):

```bash
export OXMGR_DASHBOARD_USER="admin"
export OXMGR_DASHBOARD_PASS="s3cret"
export OXMGR_DASHBOARD_LABEL="PRODUCTION"
export OXMGR_DASHBOARD_LABEL_COLOR="#ef4444"
```

Common label colors:
- Production: `#ef4444` (red)
- Staging: `#f97316` (orange)
- Development: `#eab308` (yellow)
- Local: `#22c55e` (green)

Password formats:
- Plain text: `s3cret`
- SHA256 (supervisord-compatible): `{SHA256}<base64-hash>`
- SHA512: `{SHA512}<base64-hash>`

Generate a SHA256 hash:

```bash
echo -n 'yourpassword' | openssl dgst -sha256 -binary | base64
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | GET | Web dashboard HTML |
| `/api/processes` | GET | JSON list of all processes |
| `/api/processes/:name` | GET | Single process details |
| `/api/processes/:name/logs` | GET | Log tail (`?stream=stdout\|stderr\|error`) |
| `/api/processes/:name/stop` | POST | Stop a process |
| `/api/processes/:name/restart` | POST | Restart a process |
| `/api/processes/:name/reload` | POST | Reload a process |
| `/api/stop-all` | POST | Stop all processes |
| `/api/events` | GET | SSE stream of real-time updates |
| `/metrics` | GET | Prometheus metrics |
| `/health` | GET | Health check endpoint |

## Docker

The Docker image exposes the web dashboard on port 46001:

```bash
docker compose up --build
# open http://localhost:46001
# credentials: admin / oxmgr-demo
```

See [docker-compose.yaml](../docker-compose.yaml) for configuration options.
