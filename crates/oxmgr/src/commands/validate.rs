use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex::RegexSet;

use oxmgr_manager::ecosystem::EcosystemProcessSpec;

use super::import::load_import_specs_from_paths;

#[derive(Debug, Clone)]
struct OxfileValidationReport {
    app_count: usize,
    expanded_process_count: usize,
    unnamed_count: usize,
    /// Configuration risks, from the same rule set `doctor`, the API and the dashboard use.
    ///
    /// Warnings, never errors. `validate` says whether a file is well formed and its references
    /// resolve; whether the settings are wise is a separate judgement, and an advisory that failed
    /// validation would stop an operator deploying a configuration they deliberately chose.
    advisories: Vec<SpecAdvisory>,
    /// Settings that will not behave as written on THIS platform.
    ///
    /// Warnings, never errors: one config file is meant to be portable, so a setting that is
    /// meaningful on Linux must not make the file invalid on macOS. The operator is told what will
    /// differ and the configuration is accepted regardless.
    platform_notes: Vec<PlatformNote>,
}

/// One advisory, tagged with the app it belongs to.
///
/// The advisory itself carries no owner — it is derived from a config view, not from a named
/// process — so the app name is attached here. In a file with twenty apps an untagged advisory is
/// not actionable.
#[derive(Debug, Clone)]
struct SpecAdvisory {
    app: String,
    advisory: oxmgr_manager::advisories::Advisory,
}

/// One setting whose behaviour diverges on the current platform.
#[derive(Debug, Clone)]
struct PlatformNote {
    /// Which app, so a note is actionable in a file with twenty of them.
    app: String,
    /// The setting as written in the config, not the internal field name.
    setting: &'static str,
    /// What the capability does, from the support matrix.
    capability: &'static str,
    level: oxmgr_metrics::platform::SupportLevel,
    /// Why, in terms an operator can act on. Straight from the matrix, so `doctor`, `validate` and
    /// the matrix itself cannot disagree.
    reason: &'static str,
}

pub(crate) fn run(paths: &[PathBuf], env: Option<&str>, only: &[String]) -> Result<()> {
    validate_oxfile_command(paths, env, only)
}

fn validate_oxfile_command(paths: &[PathBuf], env: Option<&str>, only: &[String]) -> Result<()> {
    let mut specs = load_import_specs_from_paths(paths, env)?;
    if !only.is_empty() {
        specs.retain(|spec| {
            spec.name
                .as_ref()
                .map(|name| only.iter().any(|selected| selected == name))
                .unwrap_or(false)
        });
    }

    if specs.is_empty() {
        if only.is_empty() {
            anyhow::bail!("no apps resolved from {}", display_paths(paths));
        } else {
            anyhow::bail!(
                "no apps matched --only filter ({}) in {}",
                only.join(","),
                display_paths(paths)
            );
        }
    }

    let report = validate_resolved_specs(&specs)?;

    println!("Config validation: OK");
    println!("Paths: {}", display_paths(paths));
    println!("Format: {}", config_format_label(paths));
    println!("Profile: {}", env.unwrap_or("default"));
    println!("Apps: {}", report.app_count);
    println!("Expanded Processes: {}", report.expanded_process_count);
    if report.unnamed_count > 0 {
        println!(
            "Warning: {} app(s) have no name. Add `name` for deterministic `oxmgr apply`.",
            report.unnamed_count
        );
    }

    // Configuration risks, reported before anything is started — which is the only time an
    // operator can act on them cheaply.
    if !report.advisories.is_empty() {
        println!();
        println!("Configuration advisories ({}):", report.advisories.len());
        for entry in &report.advisories {
            println!(
                "  - {} [{}]: {}",
                entry.app,
                entry.advisory.severity.label(),
                entry.advisory.consequence
            );
        }
        println!(
            "Host-capacity checks are not run here: `validate` may not be running on the host \
             these processes will run on. Use `oxmgr doctor` on the target host for those."
        );
    }

    // Platform divergence, reported at configuration time rather than discovered at runtime. The
    // config is still valid — this is the difference between "your file is wrong" and "this setting
    // will not do what you expect here".
    if !report.platform_notes.is_empty() {
        let platform = oxmgr_metrics::platform::current()
            .map(|platform| platform.label())
            .unwrap_or("this platform");
        println!();
        println!(
            "Platform notes for {platform} ({} setting(s) behave differently here):",
            report.platform_notes.len()
        );
        for note in &report.platform_notes {
            println!(
                "  - {} `{}` [{}]: {}",
                note.app,
                note.setting,
                note.level.label(),
                note.reason
            );
            println!("      capability: {}", note.capability);
        }
        println!("The configuration is still valid and will be applied as written.");
    }

    Ok(())
}

fn validate_resolved_specs(specs: &[EcosystemProcessSpec]) -> Result<OxfileValidationReport> {
    if specs.is_empty() {
        anyhow::bail!("empty app list");
    }

    let mut named_apps = HashSet::new();
    let mut unnamed_count = 0_usize;
    for spec in specs {
        let tokens = shell_words::split(&spec.command)
            .with_context(|| format!("invalid command syntax: {}", spec.command))?;
        if tokens.is_empty() {
            anyhow::bail!("app command cannot be empty");
        }
        validate_cluster_settings(spec, &tokens)?;
        validate_watch_settings(spec)?;
        validate_readiness_settings(spec)?;

        if let Some(check) = &spec.health_check {
            let health_tokens = shell_words::split(&check.command)
                .with_context(|| format!("invalid health command syntax: {}", check.command))?;
            if health_tokens.is_empty() {
                anyhow::bail!("health command cannot be empty for app {:?}", spec.name);
            }
        }
        if let Some(pre_reload_cmd) = &spec.pre_reload_cmd {
            let pre_tokens = shell_words::split(pre_reload_cmd)
                .with_context(|| format!("invalid pre_reload_cmd syntax: {}", pre_reload_cmd))?;
            if pre_tokens.is_empty() {
                anyhow::bail!("pre_reload_cmd cannot be empty for app {:?}", spec.name);
            }
        }

        if let Some(name) = &spec.name {
            if !named_apps.insert(name.clone()) {
                anyhow::bail!("duplicate app name in oxfile: {}", name);
            }
        } else {
            unnamed_count = unnamed_count.saturating_add(1);
        }
    }

    for spec in specs {
        for dependency in &spec.depends_on {
            if !named_apps.contains(dependency) {
                anyhow::bail!(
                    "app {:?} depends_on unknown app '{}'",
                    spec.name,
                    dependency
                );
            }
        }
    }

    let mut expanded_names = HashSet::new();
    let mut expanded_process_count = 0_usize;
    for spec in specs {
        let instances = spec.instances.max(1) as usize;
        expanded_process_count = expanded_process_count.saturating_add(instances);

        let Some(base_name) = &spec.name else {
            continue;
        };

        if instances == 1 {
            if !expanded_names.insert(base_name.clone()) {
                anyhow::bail!("duplicate expanded process name: {}", base_name);
            }
            continue;
        }

        for idx in 0..instances {
            let expanded = format!("{base_name}-{idx}");
            if !expanded_names.insert(expanded.clone()) {
                anyhow::bail!("duplicate expanded process name: {}", expanded);
            }
        }
    }

    Ok(OxfileValidationReport {
        app_count: specs.len(),
        expanded_process_count,
        unnamed_count,
        platform_notes: platform_notes_for(specs),
        advisories: spec_advisories_for(specs),
    })
}

/// Configuration risk advisories for these specs, before anything is started.
///
/// Calls the same `advisories::evaluate` as `doctor` and the API. The capacity-relative class is
/// deliberately NOT evaluated here: `validate` runs on a developer's laptop against a config bound
/// for a server, and comparing a limit to the wrong machine's memory would be worse than withholding
/// it. Passing `None` makes the withholding explicit in the report rather than implied by absence.
fn spec_advisories_for(specs: &[EcosystemProcessSpec]) -> Vec<SpecAdvisory> {
    use oxmgr_manager::advisories::{ProcessConfig, evaluate};

    let mut found = Vec::new();
    for spec in specs {
        let app = spec.name.clone().unwrap_or_else(|| spec.command.clone());
        let report = evaluate(&ProcessConfig::from(spec), None);
        for advisory in report.advisories {
            found.push(SpecAdvisory {
                app: app.clone(),
                advisory,
            });
        }
    }
    found
}

/// Settings in these specs that will not behave as written on the current platform.
///
/// Every verdict and every reason comes from `src/platform.rs`, never from a string written here:
/// `doctor`, `validate` and the matrix must not be able to disagree about what a platform does. A
/// capability that is `Supported` produces nothing, so this is silent on a platform where every
/// configured setting works.
///
/// Only settings a config file can actually set are checked. The matrix also declares divergences in
/// log rotation, state replacement and service installation, but no `oxfile` setting turns those on
/// or off, so warning about them here would be noise an operator cannot act on.
fn platform_notes_for(specs: &[EcosystemProcessSpec]) -> Vec<PlatformNote> {
    use oxmgr_metrics::platform::{Capability, SupportLevel, current_support, declaration};

    let mut notes = Vec::new();
    // No matrix for this target: nothing can be claimed either way, and inventing a verdict would
    // be worse than silence.
    if oxmgr_metrics::platform::current().is_none() {
        return notes;
    }

    for spec in specs {
        let app = spec.name.clone().unwrap_or_else(|| spec.command.clone());

        // (setting as written in the config, the capability it depends on, whether it is set)
        let checks: [(&'static str, Capability, bool); 3] = [
            ("reuse_port", Capability::ReusePortHint, spec.reuse_port),
            (
                "stop_signal",
                Capability::CustomStopSignal,
                spec.stop_signal.is_some(),
            ),
            (
                "resource_limits.cgroup_enforce",
                Capability::CgroupResourceLimits,
                spec.resource_limits
                    .as_ref()
                    .is_some_and(|limits| limits.cgroup_enforce),
            ),
        ];

        for (setting, capability, configured) in checks {
            if !configured {
                continue;
            }
            let Some(support) = current_support(capability) else {
                continue;
            };
            if support.level == SupportLevel::Supported {
                continue;
            }
            notes.push(PlatformNote {
                app: app.clone(),
                setting,
                capability: declaration(capability).summary,
                level: support.level,
                // The matrix guarantees a reason for anything not fully supported; the fallback
                // exists so a future entry that forgets one degrades to a vague note rather than a
                // panic in a validation command.
                reason: support
                    .reason
                    .unwrap_or("behaviour differs on this platform; see the support matrix"),
            });
        }
    }

    notes
}

fn validate_watch_settings(spec: &EcosystemProcessSpec) -> Result<()> {
    if !spec.watch {
        if !spec.watch_paths.is_empty()
            || !spec.ignore_watch.is_empty()
            || spec.watch_delay_secs > 0
        {
            anyhow::bail!(
                "app {:?} configures watch paths/ignore/delay but watch is disabled",
                spec.name
            );
        }
        return Ok(());
    }

    if spec.watch_paths.is_empty() && spec.cwd.is_none() {
        anyhow::bail!(
            "app {:?} enables watch but does not set cwd or explicit watch paths",
            spec.name
        );
    }

    if spec.cwd.is_none() && spec.watch_paths.iter().any(|path| !path.is_absolute()) {
        anyhow::bail!(
            "app {:?} uses relative watch paths but does not set cwd",
            spec.name
        );
    }

    if !spec.ignore_watch.is_empty() {
        RegexSet::new(&spec.ignore_watch)
            .with_context(|| format!("invalid ignore_watch regex for app {:?}", spec.name))?;
    }

    Ok(())
}

fn validate_readiness_settings(spec: &EcosystemProcessSpec) -> Result<()> {
    if spec.wait_ready && spec.health_check.is_none() {
        anyhow::bail!(
            "app {:?} enables wait_ready but does not define a health check",
            spec.name
        );
    }
    if spec.ready_timeout_secs == 0 {
        anyhow::bail!("app {:?} has ready_timeout_secs = 0", spec.name);
    }
    Ok(())
}

fn validate_cluster_settings(spec: &EcosystemProcessSpec, command_tokens: &[String]) -> Result<()> {
    if !spec.cluster_mode {
        if spec.cluster_instances.is_some() {
            anyhow::bail!(
                "app {:?} sets cluster_instances but cluster_mode is disabled",
                spec.name
            );
        }
        return Ok(());
    }

    if !is_node_command_token(&command_tokens[0]) {
        anyhow::bail!(
            "app {:?} enables cluster_mode but command is not Node.js: {}",
            spec.name,
            command_tokens[0]
        );
    }
    if command_tokens.len() < 2 {
        anyhow::bail!(
            "app {:?} enables cluster_mode but command has no script argument",
            spec.name
        );
    }
    if command_tokens[1].starts_with('-') {
        anyhow::bail!(
            "app {:?} enables cluster_mode with unsupported Node flags before script path",
            spec.name
        );
    }

    Ok(())
}

fn is_node_command_token(token: &str) -> bool {
    let executable = std::path::Path::new(token)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(token)
        .to_ascii_lowercase();
    matches!(
        executable.as_str(),
        "node" | "node.exe" | "nodejs" | "nodejs.exe"
    )
}

fn single_config_format_label(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("toml") => "oxfile.toml",
        Some("js") | Some("cjs") | Some("mjs") | Some("json") | Some("json5") => "ecosystem config",
        _ => "config",
    }
}

fn config_format_label(paths: &[PathBuf]) -> &'static str {
    if paths.len() != 1 {
        return "multiple configs";
    }

    single_config_format_label(&paths[0])
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{validate_oxfile_command, validate_resolved_specs};
    use oxmgr_manager::ecosystem::EcosystemProcessSpec;
    use oxmgr_metrics::process::{HealthCheck, RestartPolicy};
    use std::collections::HashMap;
    #[test]
    fn validate_resolved_specs_accepts_valid_definitions() {
        let specs = vec![
            fixture_spec("db", "docker compose up db", vec![], 1),
            fixture_spec("api", "node server.js", vec!["db".to_string()], 2),
        ];

        let report = validate_resolved_specs(&specs).expect("validation should pass");
        assert_eq!(report.app_count, 2);
        assert_eq!(report.expanded_process_count, 3);
        assert_eq!(report.unnamed_count, 0);
    }

    #[test]
    fn config_advisories_are_reported_without_failing_validation() {
        // The drift task 3.5 exists to prevent: `validate` must use the SAME rules as `doctor`, and
        // an advisory must not make a well-formed file invalid. An operator who deliberately chose a
        // risky setting still needs to be able to deploy it.
        let mut spec = fixture_spec("risky", "/bin/sleep 600", vec![], 1);
        spec.restart_policy = RestartPolicy::Always;
        spec.restart_delay_secs = 0;
        spec.crash_restart_limit = 0;

        let report =
            validate_resolved_specs(&[spec]).expect("an advisory is not a validation error");
        assert_eq!(report.app_count, 1, "the configuration is still valid");

        assert!(
            !report.advisories.is_empty(),
            "a disabled circuit breaker with always-restart must be advised on"
        );
        for entry in &report.advisories {
            assert_eq!(
                entry.app, "risky",
                "an advisory must name the app that set it"
            );
            assert!(
                entry.advisory.consequence.len() > 40,
                "an advisory must state its consequence: {}",
                entry.advisory.consequence
            );
        }
    }

    #[test]
    fn capacity_relative_advisories_are_withheld_during_validation() {
        // `validate` runs on a developer's laptop against a config bound for a server. Comparing a
        // memory limit against the wrong machine's capacity would be worse than withholding it, so
        // capacity is deliberately passed as `None`.
        let mut spec = fixture_spec("big", "/bin/sleep 600", vec![], 1);
        // A limit far above any plausible host: the capacity rule would certainly fire if it ran.
        spec.resource_limits = Some(oxmgr_metrics::process::ResourceLimits {
            max_memory_mb: Some(1024 * 1024),
            ..oxmgr_metrics::process::ResourceLimits::default()
        });

        let report = validate_resolved_specs(&[spec]).expect("valid");
        for entry in &report.advisories {
            assert!(
                !entry.advisory.rule.is_capacity_relative(),
                "a capacity-relative rule must not be evaluated without the target host: {}",
                entry.advisory.id
            );
        }
    }

    #[test]
    fn a_sound_spec_produces_no_config_advisories() {
        let mut spec = fixture_spec("sound", "/bin/sleep 600", vec![], 1);
        spec.restart_policy = RestartPolicy::OnFailure;
        spec.restart_delay_secs = 2;
        spec.crash_restart_limit = 3;
        spec.resource_limits = None;
        spec.watch = false;

        let report = validate_resolved_specs(&[spec]).expect("valid");
        assert!(
            report.advisories.is_empty(),
            "a sound configuration must be quiet: {:?}",
            report.advisories
        );
    }

    #[test]
    fn a_platform_divergent_setting_warns_but_the_config_is_still_accepted() {
        // The whole point of task 2.2: one config file stays portable. A setting that is meaningful
        // on Linux must not make the file invalid on macOS, so this asserts acceptance first.
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.reuse_port = true;
        spec.resource_limits = Some(oxmgr_metrics::process::ResourceLimits {
            max_memory_mb: Some(512),
            cgroup_enforce: true,
            ..oxmgr_metrics::process::ResourceLimits::default()
        });

        let report = validate_resolved_specs(&[spec]).expect("a divergent setting is not an error");
        assert_eq!(report.app_count, 1, "the configuration is still applied");

        let Some(platform) = oxmgr_metrics::platform::current() else {
            // Unreleased target: nothing can be claimed, so nothing is reported.
            assert!(report.platform_notes.is_empty());
            return;
        };

        // Every note must name the app, the setting as written, and a reason from the matrix —
        // never a string invented here, so `doctor` and `validate` cannot disagree.
        for note in &report.platform_notes {
            assert_eq!(note.app, "api");
            assert!(
                !note.reason.is_empty() && note.reason != "unsupported",
                "a note must state why, not repeat the level: {}",
                note.reason
            );
            assert_ne!(
                note.level,
                oxmgr_metrics::platform::SupportLevel::Supported,
                "a supported capability must produce no note"
            );
        }

        // On macOS specifically, cgroup enforcement is unavailable, so that setting must be named.
        if platform == oxmgr_metrics::platform::Platform::MacOs {
            assert!(
                report
                    .platform_notes
                    .iter()
                    .any(|note| note.setting == "resource_limits.cgroup_enforce"),
                "macOS has no cgroups, so cgroup_enforce must be reported: {:?}",
                report.platform_notes
            );
        }
    }

    #[test]
    fn a_setting_that_is_not_configured_produces_no_platform_note() {
        // Silence is the default. A note for a setting the operator never set would be noise, and
        // an operator who learns to skim the notes will miss the one that matters.
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.reuse_port = false;
        spec.stop_signal = None;
        spec.resource_limits = None;

        let report = validate_resolved_specs(&[spec]).expect("valid");
        assert!(
            report.platform_notes.is_empty(),
            "nothing platform-specific was configured: {:?}",
            report.platform_notes
        );
    }

    #[test]
    fn platform_notes_name_the_app_they_belong_to() {
        // In a file with twenty apps, "cgroup_enforce diverges here" is not actionable without
        // knowing which app set it.
        let mut quiet = fixture_spec("quiet", "node quiet.js", vec![], 1);
        quiet.reuse_port = false;
        quiet.stop_signal = None;
        quiet.resource_limits = None;

        let mut loud = fixture_spec("loud", "node loud.js", vec![], 1);
        loud.reuse_port = false;
        loud.stop_signal = None;
        loud.resource_limits = Some(oxmgr_metrics::process::ResourceLimits {
            cgroup_enforce: true,
            ..oxmgr_metrics::process::ResourceLimits::default()
        });

        let report = validate_resolved_specs(&[quiet, loud]).expect("valid");
        if oxmgr_metrics::platform::current().is_none() {
            return;
        }
        for note in &report.platform_notes {
            assert_eq!(
                note.app, "loud",
                "a note was attributed to an app that did not configure it"
            );
        }
    }

    #[test]
    fn validate_resolved_specs_rejects_unknown_dependency() {
        let specs = vec![fixture_spec(
            "api",
            "node server.js",
            vec!["missing-db".to_string()],
            1,
        )];

        let error = validate_resolved_specs(&specs).expect_err("validation should fail");
        assert!(
            error.to_string().contains("depends_on unknown app"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_duplicate_names() {
        let specs = vec![
            fixture_spec("api", "node server.js", vec![], 1),
            fixture_spec("api", "node worker.js", vec![], 1),
        ];

        let error = validate_resolved_specs(&specs).expect_err("validation should fail");
        assert!(
            error.to_string().contains("duplicate app name"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_invalid_command_syntax() {
        let specs = vec![fixture_spec("api", "node \"unterminated", vec![], 1)];

        let error = validate_resolved_specs(&specs).expect_err("validation should fail");
        assert!(
            error.to_string().contains("invalid command syntax"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_empty_app_list() {
        let error = validate_resolved_specs(&[]).expect_err("validation should fail");
        assert_eq!(error.to_string(), "empty app list");
    }

    #[test]
    fn validate_resolved_specs_rejects_invalid_health_command_syntax() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.health_check = Some(HealthCheck {
            command: "curl \"unterminated".to_string(),
            interval_secs: 30,
            timeout_secs: 5,
            max_failures: 3,
        });

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error.to_string().contains("invalid health command syntax"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_wait_ready_without_health_check() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.health_check = None;
        spec.wait_ready = true;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error.to_string().contains("wait_ready"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_watch_without_cwd_or_paths() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.watch = true;
        spec.watch_paths.clear();
        spec.cwd = None;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("does not set cwd or explicit watch paths"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_relative_watch_paths_without_cwd() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.watch = true;
        spec.cwd = None;
        spec.watch_paths = vec![std::path::PathBuf::from("src")];

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("uses relative watch paths but does not set cwd"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_watch_tuning_when_watch_disabled() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.watch = false;
        spec.ignore_watch = vec!["node_modules".to_string()];

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("configures watch paths/ignore/delay but watch is disabled"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_invalid_ignore_watch_regex() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.watch = true;
        spec.cwd = Some(std::env::temp_dir());
        spec.ignore_watch = vec!["(".to_string()];

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error.to_string().contains("invalid ignore_watch regex"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_zero_ready_timeout() {
        let mut spec = fixture_spec("api", "node server.js", vec![], 1);
        spec.ready_timeout_secs = 0;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error.to_string().contains("ready_timeout_secs = 0"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_cluster_mode_for_non_node_command() {
        let mut spec = fixture_spec("api", "python app.py", vec![], 1);
        spec.cluster_mode = true;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("cluster_mode but command is not Node.js"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_cluster_mode_without_script_argument() {
        let mut spec = fixture_spec("api", "node", vec![], 1);
        spec.cluster_mode = true;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("cluster_mode but command has no script argument"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_cluster_mode_with_node_flags_before_script() {
        let mut spec = fixture_spec(
            "api",
            "node --require ts-node/register server.js",
            vec![],
            1,
        );
        spec.cluster_mode = true;

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported Node flags before script path"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_rejects_cluster_instances_without_cluster_mode() {
        let mut spec = fixture_spec("api", "node app.js", vec![], 1);
        spec.cluster_instances = Some(2);

        let error = validate_resolved_specs(&[spec]).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("cluster_instances but cluster_mode is disabled"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_accepts_absolute_node_binary_in_cluster_mode() {
        let mut spec = fixture_spec("api", "/usr/local/bin/node server.js", vec![], 1);
        spec.cluster_mode = true;
        spec.cluster_instances = Some(2);

        let report = validate_resolved_specs(&[spec]).expect("validation should pass");
        assert_eq!(report.app_count, 1);
        assert_eq!(report.expanded_process_count, 1);
    }

    #[test]
    fn validate_resolved_specs_rejects_duplicate_expanded_names() {
        let specs = vec![
            fixture_spec("api", "node server.js", vec![], 2),
            fixture_spec("api-0", "node sidecar.js", vec![], 1),
        ];

        let error = validate_resolved_specs(&specs).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("duplicate expanded process name"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn validate_resolved_specs_counts_unnamed_apps() {
        let unnamed = EcosystemProcessSpec {
            command: "echo unnamed".to_string(),
            name: None,
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::Never,
            max_restarts: 0,
            crash_restart_limit: 3,
            cwd: None,
            env: HashMap::new(),
            health_check: None,
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout_secs: 5,
            restart_delay_secs: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            namespace: None,
            resource_limits: None,
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            start_order: 0,
            depends_on: Vec::new(),
            instances: 1,
            instance_var: None,
            wait_ready: false,
            ready_timeout_secs: 30,
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
        };
        let named = fixture_spec("api", "node server.js", vec![], 1);

        let report = validate_resolved_specs(&[unnamed, named]).expect("validation should succeed");
        assert_eq!(report.app_count, 2);
        assert_eq!(report.expanded_process_count, 2);
        assert_eq!(report.unnamed_count, 1);
    }

    #[test]
    fn validate_command_accepts_ecosystem_json_path() {
        let path = temp_file("validate-ecosystem", "json");
        std::fs::write(
            &path,
            r#"{
  "apps": [
    { "name": "api", "script": "server.js" }
  ]
}"#,
        )
        .expect("failed to write ecosystem fixture");

        validate_oxfile_command(std::slice::from_ref(&path), None, &[])
            .expect("validate should accept ecosystem json");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_command_accepts_ecosystem_js_path() {
        let path = temp_file("validate-ecosystem-js", "js");
        let url = oxmgr_core::constants::DEFAULT_HEALTH_URL;
        let payload = format!(
            r#"
module.exports = {{
  apps: [
    {{
      name: "api",
      cmd: "node server.js",
      cwd: "/srv/api",
      watch: ["src"],
      ignore_watch: ["node_modules"],
      watch_delay: 1000,
      health_cmd: "curl -fsS {url}/health",
      wait_ready: true,
      listen_timeout: 5000
    }}
  ]
}};
"#,
            url = url
        );
        std::fs::write(&path, payload).expect("failed to write ecosystem fixture");

        validate_oxfile_command(std::slice::from_ref(&path), None, &[])
            .expect("validate should accept ecosystem js");

        let _ = std::fs::remove_file(path);
    }

    fn fixture_spec(
        name: &str,
        command: &str,
        depends_on: Vec<String>,
        instances: u32,
    ) -> EcosystemProcessSpec {
        EcosystemProcessSpec {
            command: command.to_string(),
            name: Some(name.to_string()),
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::OnFailure,
            max_restarts: 10,
            crash_restart_limit: 3,
            cwd: None,
            env: HashMap::new(),
            health_check: Some(HealthCheck {
                command: "echo ok".to_string(),
                interval_secs: 30,
                timeout_secs: 5,
                max_failures: 3,
            }),
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout_secs: 5,
            restart_delay_secs: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            namespace: None,
            resource_limits: None,
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            start_order: 0,
            depends_on,
            instances,
            instance_var: Some("INSTANCE_ID".to_string()),
            wait_ready: false,
            ready_timeout_secs: 30,
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
        }
    }

    fn temp_file(prefix: &str, extension: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nonce}.{extension}"))
    }
}
