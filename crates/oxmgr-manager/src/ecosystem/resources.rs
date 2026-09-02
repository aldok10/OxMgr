//! Lint-level cleanup: display-path casts are harmless indices/lengths.

use anyhow::{Context, Result};
use serde_json::Value;

use oxmgr_metrics::process::ResourceLimits;
use oxmgr_store::hash::sha256_hex;

use super::ResolvedSettings;

pub(super) fn resource_limits_from(
    max_memory_restart: Option<Value>,
    max_memory_mb: Option<u64>,
    max_cpu_percent: Option<u64>,
    cgroup_enforce: Option<bool>,
    deny_gpu: Option<bool>,
) -> Result<Option<ResourceLimits>> {
    let mut limits = ResourceLimits::default();

    if let Some(memory_mb) = max_memory_mb
        && memory_mb > 0
    {
        limits.max_memory_mb = Some(memory_mb);
    }
    if let Some(cpu_percent) = max_cpu_percent
        && cpu_percent > 0
    {
        limits.max_cpu_percent = Some(cpu_percent);
    }
    if let Some(memory_restart) = max_memory_restart {
        let parsed = parse_memory_limit_mb_value(&memory_restart)?;
        if parsed > 0 {
            limits.max_memory_mb = Some(parsed);
        }
    }
    limits.cgroup_enforce = cgroup_enforce.unwrap_or(false);
    limits.deny_gpu = deny_gpu.unwrap_or(false);

    Ok(normalize_resource_limits(limits))
}

pub(super) fn parse_memory_limit_mb_value(value: &Value) -> Result<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .context("max_memory_restart numeric value must be a positive integer"),
        Value::String(text) => parse_memory_limit_mb_str(text),
        _ => anyhow::bail!(
            "max_memory_restart must be a string like '256M' or a numeric value in MB"
        ),
    }
}

pub(super) fn set_memory_limit_mb(settings: &mut ResolvedSettings, value_mb: u64) {
    if value_mb == 0 {
        return;
    }
    let mut limits = settings.resource_limits.clone().unwrap_or_default();
    limits.max_memory_mb = Some(value_mb);
    settings.resource_limits = normalize_resource_limits(limits);
}

pub(super) fn set_cpu_limit_percent(settings: &mut ResolvedSettings, value_percent: u64) {
    if value_percent == 0 {
        return;
    }
    let mut limits = settings.resource_limits.clone().unwrap_or_default();
    limits.max_cpu_percent = Some(value_percent);
    settings.resource_limits = normalize_resource_limits(limits);
}

pub(super) fn set_cgroup_enforce(settings: &mut ResolvedSettings, enabled: bool) {
    let mut limits = settings.resource_limits.clone().unwrap_or_default();
    limits.cgroup_enforce = enabled;
    settings.resource_limits = normalize_resource_limits(limits);
}

pub(super) fn set_deny_gpu(settings: &mut ResolvedSettings, deny: bool) {
    let mut limits = settings.resource_limits.clone().unwrap_or_default();
    limits.deny_gpu = deny;
    settings.resource_limits = normalize_resource_limits(limits);
}

pub(super) fn normalize_pull_secret_hash(secret: Option<String>) -> Result<Option<String>> {
    let Some(secret) = secret else {
        return Ok(None);
    };
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        anyhow::bail!("pull_secret cannot be empty");
    }
    if trimmed.len() > 512 {
        anyhow::bail!("pull_secret exceeds maximum length 512");
    }

    Ok(Some(sha256_hex(trimmed.as_bytes())))
}

fn parse_memory_limit_mb_str(input: &str) -> Result<u64> {
    let normalized = input.trim().to_ascii_uppercase();
    if normalized.is_empty() {
        anyhow::bail!("max_memory_restart cannot be empty");
    }

    let split_idx = normalized
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(normalized.len());
    let (number_part, unit_part) = normalized.split_at(split_idx);
    if number_part.is_empty() {
        anyhow::bail!("max_memory_restart is missing numeric value");
    }

    let (whole, frac_thousandths) = match number_part.split_once('.') {
        Some((w, f)) => {
            let whole: u64 = w
                .parse()
                .with_context(|| format!("invalid max_memory_restart numeric value: {input}"))?;
            let frac = f
                .chars()
                .take(3)
                .fold(0u32, |acc, c| acc * 10 + c.to_digit(10).unwrap_or(0));
            let padded = match f.len() {
                1 => frac * 100,
                2 => frac * 10,
                _ => frac,
            };
            (whole, padded)
        }
        None => (
            number_part
                .parse()
                .with_context(|| format!("invalid max_memory_restart numeric value: {input}"))?,
            0,
        ),
    };
    if whole == 0 && frac_thousandths == 0 {
        anyhow::bail!("max_memory_restart must be greater than zero");
    }

    // Integer arithmetic: parse the numeric part as thousandths to avoid any f64→int cast.
    let (whole, frac_thousandths) = match number_part.split_once('.') {
        Some((w, f)) => {
            let frac = f
                .chars()
                .take(3)
                .fold(0u32, |acc, c| acc * 10 + c.to_digit(10).unwrap_or(0));
            // Pad right: "5"→500, "51"→510, "512"→512
            let padded = match f.len() {
                1 => frac * 100,
                2 => frac * 10,
                _ => frac,
            };
            (w.parse::<u64>().unwrap_or(0), padded)
        }
        None => (number_part.parse::<u64>().unwrap_or(0), 0),
    };
    let unit_thousandths = whole
        .saturating_mul(1000)
        .saturating_add(u64::from(frac_thousandths));
    let mb = match unit_part.trim() {
        "" | "M" | "MB" => unit_thousandths.div_ceil(1000), // ceiling
        "G" | "GB" => unit_thousandths.saturating_mul(1024).div_ceil(1000),
        "K" | "KB" => {
            let mb_thousandths = unit_thousandths / 1024;
            mb_thousandths.div_ceil(1000)
        }
        "B" => {
            let mb_thousandths = unit_thousandths / (1024 * 1024);
            mb_thousandths.div_ceil(1000)
        }
        _ => anyhow::bail!("unsupported max_memory_restart unit: {}", unit_part.trim()),
    };
    Ok(mb.max(1))
}

fn normalize_resource_limits(mut limits: ResourceLimits) -> Option<ResourceLimits> {
    if matches!(limits.max_memory_mb, Some(0)) {
        limits.max_memory_mb = None;
    }
    if matches!(limits.max_cpu_percent, Some(v) if v == 0) {
        limits.max_cpu_percent = None;
    }
    if limits.max_memory_mb.is_none()
        && limits.max_cpu_percent.is_none()
        && !limits.cgroup_enforce
        && !limits.deny_gpu
    {
        None
    } else {
        Some(limits)
    }
}
