//! Measures the full-host process refresh cost that sizes [`ConsumerSampler`]'s cadence
//! (`src/host_consumers.rs:18-24`). Re-run this after toolchain or sysinfo upgrades and
//! beside any change to the sampler's refresh kind; the recorded figure in the module
//! docs must match what this prints.
//!
//! ```text
//! cargo run --example measure_refresh_cost --release
//! ```
//!
//! The refresh kind mirrors `ConsumerSampler::sample` exactly (`with_cpu`, `with_memory`,
//! `with_exe(OnlyIfNotSet)`, `with_user(OnlyIfNotSet)`, command lines withheld): a figure
//! measured with a different kind is a claim about a different operation.

use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const ITERATIONS: usize = 50;

fn main() {
    // `System::new()` rather than `new_all()`: the sampler needs only the process table.
    let mut system = System::new();
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet);

    // Warm-up: first refresh populates caches (exe paths, user ids) and is not
    // representative of the steady-state cost the cadence decision depends on.
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);

    let mut samples: Vec<Duration> = Vec::with_capacity(ITERATIONS);
    let mut process_count = 0usize;
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        samples.push(start.elapsed());
        process_count = system.processes().len();
    }
    samples.sort();

    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = samples[samples.len() - 1];
    println!("refresh_processes(All) over {ITERATIONS} iterations, {process_count} processes:");
    println!("  p50 {:.2} ms", p50.as_secs_f64() * 1e3);
    println!("  min {:.2} ms", min.as_secs_f64() * 1e3);
    println!("  max {:.2} ms", max.as_secs_f64() * 1e3);

    // Marginal cost of also reading environment variables (`managed-process-child-visibility`
    // task 1.4): decides whether per-worker cluster identity is reported or reported-absent.
    let mut system_env = System::new();
    let kind_env = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_environ(UpdateKind::OnlyIfNotSet);
    system_env.refresh_processes_specifics(ProcessesToUpdate::All, true, kind_env);

    let mut samples_env: Vec<Duration> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        system_env.refresh_processes_specifics(ProcessesToUpdate::All, true, kind_env);
        samples_env.push(start.elapsed());
    }
    samples_env.sort();

    let non_empty = system_env
        .processes()
        .values()
        .filter(|p| !p.environ().is_empty())
        .count();
    let p50e = samples_env[samples_env.len() / 2];
    println!("with_environ(OnlyIfNotSet) added, {process_count} processes ({non_empty} with non-empty env):");
    println!("  p50 {:.2} ms", p50e.as_secs_f64() * 1e3);
    println!(
        "  marginal vs base p50: {:.2} ms",
        (p50e.as_secs_f64() - p50.as_secs_f64()) * 1e3
    );
}
