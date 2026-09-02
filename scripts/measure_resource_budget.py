#!/usr/bin/env python3
"""Measures oxmgr's resource budgets so a regression is a failing check, not a
user complaint.

The repository already benchmarks oxmgr against pm2 (`benchmark_oxmgr_vs_pm2.py`).
That answers "is oxmgr competitive". This answers a different question: "is oxmgr
still lightweight compared to its own last release", which nothing currently
checks.

Four figures, chosen because each answers something an operator or maintainer
actually asks, and because more metrics would mean more noise for less signal:

  binary_bytes      the single-binary promise, and the figure most likely to
                    creep since the dashboard assets are inlined
  idle_rss_bytes    the cost of merely running, at a fixed process count
  loaded_rss_bytes  RSS under log-heavy load, where allocation behaviour shows
  idle_cpu_percent  a supervisor should be invisible when nothing is happening

Plus per-cycle maintenance cost as a distribution rather than a mean, because it
is shared with future analysis work and a mean would hide the tail that matters.

Output is JSON on stdout so CI can diff it against a recorded baseline. Nothing
here fails a build on its own: comparison is `compare_resource_budget.py`'s job,
and until the runner's variance is known this is report-only by design.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# The load generator emits mixed-format lines with periodic bursts: plain, ANSI,
# JSON, logfmt, stack traces and over-long payloads. Uniform output would not
# stress the log path the way a real service does, and the bursts are what expose
# batching problems.
WORKLOAD = r"""#!/bin/sh
N=0
ESC=$(printf '\033')
while true; do
  N=$((N + 1))
  TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  case $((N % 6)) in
  0) echo "$TS INFO  [worker] handled GET /api/orders in ${N}ms" ;;
  1) echo "${ESC}[32mINFO${ESC}[0m ${ESC}[36m[http]${ESC}[0m GET /health 200" ;;
  2) echo "{\"ts\":\"$TS\",\"level\":\"info\",\"msg\":\"done\",\"n\":$N,\"user\":{\"id\":$N}}" ;;
  3) echo "ts=$TS level=warn component=cache hit=false n=$N" ;;
  4) echo "$TS ERROR [db] connection refused" >&2 ;;
  5) echo "$TS TRACE [payload] body=$(head -c 300 /dev/urandom | base64 | tr -d '\n')" ;;
  esac
  if [ $((N % 40)) -eq 0 ]; then
    i=0
    while [ $i -lt 60 ]; do
      echo "$TS DEBUG [burst] flood line $i of 60"
      i=$((i + 1))
    done
  fi
  sleep 0.08
done
"""


class HarnessError(RuntimeError):
    pass


def log(message: str) -> None:
    # stderr so stdout stays a clean JSON document.
    print(f"[budget] {message}", file=sys.stderr, flush=True)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run(argv: list[str], timeout: float = 60, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, **(env or {})},
        check=False,
    )
    if result.returncode != 0:
        raise HarnessError(f"{argv[0]} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout


def build_release(cargo: str) -> Path:
    log("building release binary")
    run([cargo, "build", "--release"], timeout=900)
    binary = REPO_ROOT / "target" / "release" / "oxmgr"
    if not binary.is_file():
        raise HarnessError(f"release binary not found at {binary}")
    return binary


def read_ps(pid: int, field: str) -> float:
    """Reads one `ps` field, or raises if the process is gone."""
    result = subprocess.run(
        ["ps", "-o", f"{field}=", "-p", str(pid)],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    text = result.stdout.strip()
    if not text:
        raise HarnessError(f"ps reported no {field} for pid {pid}")
    return float(text)


def rss_bytes(pid: int) -> int:
    # ps reports RSS in kilobytes.
    return int(read_ps(pid, "rss") * 1024)


def quantile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = fraction * (len(ordered) - 1)
    low = int(position)
    high = min(low + 1, len(ordered) - 1)
    weight = position - low
    return ordered[low] * (1 - weight) + ordered[high] * weight


def distribution(values: list[float]) -> dict[str, float]:
    """A distribution, not a mean: the tail is what an operator feels."""
    if not values:
        return {}
    return {
        "samples": len(values),
        "min": round(min(values), 3),
        "p50": round(quantile(values, 0.50), 3),
        "p95": round(quantile(values, 0.95), 3),
        "max": round(max(values), 3),
    }


class Daemon:
    """A throwaway oxmgr daemon in its own OXMGR_HOME, on its own ports."""

    def __init__(self, binary: Path, home: Path) -> None:
        self.binary = binary
        self.home = home
        self.ipc_port = free_port()
        self.api_port = free_port()
        self.process: subprocess.Popen[bytes] | None = None
        self.env = {
            "OXMGR_HOME": str(home),
            "OXMGR_DAEMON_ADDR": f"127.0.0.1:{self.ipc_port}",
            "OXMGR_API_ADDR": f"127.0.0.1:{self.api_port}",
            # A small rotation size keeps the log path exercised rather than just
            # appending to one ever-growing file.
            "OXMGR_LOG_MAX_SIZE_MB": "1",
            "OXMGR_LOG_MAX_FILES": "3",
        }

    def start(self) -> None:
        home = Path(self.home)
        home.mkdir(parents=True, exist_ok=True)
        self.process = subprocess.Popen(
            [str(self.binary), "daemon", "run"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={**os.environ, **self.env},
            start_new_session=True,
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", self.api_port), timeout=0.5):
                    log(f"daemon up (pid {self.process.pid}, api {self.api_port})")
                    return
            except OSError:
                time.sleep(0.2)
        raise HarnessError("daemon did not become reachable within 30s")

    def cli(self, *args: str) -> str:
        return run([str(self.binary), *args], env=self.env)

    @property
    def pid(self) -> int:
        if self.process is None:
            raise HarnessError("daemon not started")
        return self.process.pid

    def stop(self) -> None:
        if self.process is None:
            return
        try:
            self.cli("daemon", "stop")
        except Exception:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
            except OSError:
                pass
        self.process = None


def sample_process(pid: int, seconds: float, interval: float) -> tuple[list[int], list[float]]:
    """Samples RSS and CPU together so both describe the same period."""
    rss: list[int] = []
    cpu: list[float] = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            rss.append(rss_bytes(pid))
            cpu.append(read_ps(pid, "pcpu"))
        except HarnessError:
            break
        time.sleep(interval)
    return rss, cpu


def measure(binary: Path, *, processes: int, settle: float, sample: float) -> dict:
    """Runs the full measurement and returns the report."""
    report: dict = {
        "binary_bytes": binary.stat().st_size,
        "platform": f"{sys.platform}-{os.uname().machine}",
    }

    with tempfile.TemporaryDirectory(prefix="oxmgr-budget-") as tmp:
        tmp_path = Path(tmp)
        workload = tmp_path / "workload.sh"
        workload.write_text(WORKLOAD)
        workload.chmod(0o755)

        daemon = Daemon(binary, tmp_path / "home")
        try:
            daemon.start()

            # Idle: the daemon running with nothing managed.
            #
            # Sampled AFTER the settle period on purpose, and worth knowing what it does
            # and does not mean. Measured trajectory of one daemon: 6.6 MB at t=1s,
            # 10.68 MB from t=2s onward, then falling to 8.0 MB once a workload started.
            # The peak is startup allocation the allocator has no reason to release while
            # nothing is happening, so this figure describes "freshly booted and unused",
            # not a floor that load will rise above. Idle exceeding loaded is expected
            # here rather than a contradiction.
            log(f"sampling idle for {sample}s")
            time.sleep(settle)
            idle_rss, idle_cpu = sample_process(daemon.pid, sample, 0.5)
            if not idle_rss:
                raise HarnessError("no idle samples collected")
            # p95, not max: a single allocator spike must not decide the figure. Using
            # max made idle RSS swing 6.5-10.7 MB run to run on identical code, which is
            # noise masquerading as a measurement.
            report["idle_rss_bytes"] = int(quantile([float(v) for v in idle_rss], 0.95))
            report["idle_cpu_percent"] = round(quantile(idle_cpu, 0.50), 2)
            # Kept so the spread is inspectable rather than hidden behind one number.
            report["idle_rss_series"] = idle_rss

            # Loaded: managed processes emitting mixed-format lines with bursts.
            log(f"starting {processes} workload processes")
            for index in range(processes):
                daemon.cli(
                    "start",
                    str(workload),
                    "--name",
                    f"load{index}",
                    "--restart",
                    "always",
                )
            time.sleep(settle)
            log(f"sampling under load for {sample}s")
            loaded_rss, loaded_cpu = sample_process(daemon.pid, sample, 0.5)
            if not loaded_rss:
                raise HarnessError("no loaded samples collected")
            report["loaded_rss_bytes"] = int(quantile([float(v) for v in loaded_rss], 0.95))
            report["loaded_cpu_percent"] = round(quantile(loaded_cpu, 0.50), 2)
            report["load_processes"] = processes

            # RSS over the loaded window, kept as a series so the growth check can
            # ask whether it plateaus rather than only how high it got.
            report["loaded_rss_series"] = loaded_rss
            report["loaded_rss_distribution"] = distribution([float(v) for v in loaded_rss])
        finally:
            daemon.stop()

    return report


def main() -> int:
    parser = argparse.ArgumentParser(description="Measure oxmgr resource budgets.")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--skip-build", action="store_true", help="use the existing release binary")
    parser.add_argument("--processes", type=int, default=3, help="managed processes under load")
    parser.add_argument("--settle", type=float, default=6.0, help="seconds to settle before sampling")
    parser.add_argument("--sample", type=float, default=15.0, help="seconds to sample each phase")
    parser.add_argument("--out", type=Path, help="write the report here as well as stdout")
    args = parser.parse_args()

    if sys.platform.startswith("win"):
        log("this harness relies on ps and POSIX signals; unsupported on Windows")
        return 2
    if shutil.which("ps") is None:
        log("ps not found")
        return 2

    binary = (
        REPO_ROOT / "target" / "release" / "oxmgr"
        if args.skip_build
        else build_release(args.cargo)
    )
    if not binary.is_file():
        log(f"release binary missing at {binary}; drop --skip-build")
        return 2

    report = measure(
        binary,
        processes=args.processes,
        settle=args.settle,
        sample=args.sample,
    )
    text = json.dumps(report, indent=2, sort_keys=True)
    print(text)
    if args.out:
        args.out.write_text(text + "\n")
        log(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except HarnessError as error:
        log(f"error: {error}")
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
