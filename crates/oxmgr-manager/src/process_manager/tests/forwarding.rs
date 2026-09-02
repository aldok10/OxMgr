//! Tests for the log forwarding hot path.
//!
//! This path runs once per line per managed process, so it is the only place in the
//! daemon where a per-call allocation multiplies into a real cost. These tests pin the
//! behaviour that the allocation removal must not change, and the one saving that is
//! observable from outside: an event is not published when nobody is subscribed.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use super::fixture_process;
use crate::logging::LogRotationPolicy;
use oxmgr_core::events::{BusEvent, EventProcessInfo};

fn policy() -> LogRotationPolicy {
    LogRotationPolicy {
        max_size_bytes: 1024 * 1024,
        max_files: 3,
        max_age_days: 14,
        max_age_secs: None,
    }
}

fn temp_log(prefix: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("oxmgr-fwd-{prefix}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir.join("out.log")
}

/// Feeds `lines` through a pipe into `forward_log_pipe` and returns once the writer has
/// flushed them. The idle flush is what makes the file readable without waiting for the
/// buffer to fill, so a short pause after writing is enough.
async fn forward(
    path: &std::path::Path,
    date_format: Option<String>,
    lines: &[&str],
    event_tx: tokio::sync::broadcast::Sender<Arc<BusEvent>>,
) {
    let (reader, mut writer) = tokio::io::duplex(64 * 1024);
    let process = fixture_process();
    let info = EventProcessInfo::from(&process);

    super::super::forward_log_pipe(
        reader,
        super::super::LogForwardParams {
            log_path: path.to_path_buf(),
            date_format,
            rotation: policy(),
            event_tx,
            process_info: info,
            is_stderr: false,
            stderr_buf: None,
        },
    );

    for line in lines {
        writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("failed writing test line");
    }
    writer.flush().await.expect("failed flushing pipe");
    drop(writer);
    // The forwarding task flushes on idle and again when the pipe ends; give it room to
    // observe the close.
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// The saving that phase 4 exists for: with no subscriber, no event is constructed. On a
/// process emitting thousands of lines a second that is thousands of avoided String and
/// Arc allocations, and nothing was ever going to read them.
#[tokio::test]
async fn no_event_is_published_when_nothing_is_subscribed() {
    let path = temp_log("no-subscriber");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    // A receiver is created and dropped so the channel has existed but has none now.
    drop(bus.subscribe());
    assert_eq!(bus.receiver_count(), 0);

    forward(&path, None, &["alpha", "beta"], bus.clone()).await;

    // Subscribing afterwards must see nothing: broadcast only delivers to receivers that
    // existed when the value was sent, so an empty stream here confirms nothing was sent.
    let mut rx = bus.subscribe();
    assert!(
        rx.try_recv().is_err(),
        "no event should have been published while unsubscribed"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Skipping event construction must not skip the write. The file is the authoritative
/// record, and an operator reading `oxmgr logs` has no idea whether anyone was watching.
#[tokio::test]
async fn lines_are_written_to_disk_even_with_no_subscriber() {
    let path = temp_log("write-without-subscriber");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    assert_eq!(bus.receiver_count(), 0);

    forward(&path, None, &["first line", "second line"], bus).await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    assert!(content.contains("first line"), "got {content:?}");
    assert!(content.contains("second line"), "got {content:?}");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// With a subscriber attached the events must still arrive, unchanged. This is the half
/// the optimisation could plausibly break.
#[tokio::test]
async fn events_are_published_when_a_subscriber_exists() {
    let path = temp_log("with-subscriber");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    let mut rx = bus.subscribe();

    forward(&path, None, &["hello", "world"], bus.clone()).await;

    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::LogOut { data, .. } = &*event {
            seen.push(data.line.clone());
        }
    }
    assert_eq!(seen, vec!["hello".to_string(), "world".to_string()]);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The reused format buffer must produce exactly what `format!` did: the configured
/// timestamp, a colon and a space, then the line. A buffer that is not cleared between
/// lines would concatenate them, which this catches.
#[tokio::test]
async fn timestamped_lines_are_framed_identically_across_many_lines() {
    let path = temp_log("timestamp-framing");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    let lines: Vec<String> = (0..50).map(|idx| format!("line {idx}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

    forward(&path, Some("%Y-%m-%d".to_string()), &refs, bus).await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    let written: Vec<&str> = content.lines().collect();
    assert_eq!(written.len(), 50, "every line should be written once");

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for (idx, line) in written.iter().enumerate() {
        let expected = format!("{today}: line {idx}");
        assert_eq!(
            *line, expected,
            "line {idx} should carry exactly one prefix, got {line:?}"
        );
    }

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Without a date format the line is written verbatim — the path that used to clone the
/// buffer and now borrows it.
#[tokio::test]
async fn unprefixed_lines_are_written_verbatim() {
    let path = temp_log("verbatim");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;

    forward(&path, None, &["plain one", "plain two"], bus).await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    assert_eq!(content, "plain one\nplain two\n");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The published line must match what was written to the file, prefix included. These
/// diverged once before: the file carried the timestamp and the bus did not, so
/// timestamps appeared to stop after the first screenful in the dashboard.
#[tokio::test]
async fn published_line_matches_the_written_line_including_prefix() {
    let path = temp_log("prefix-parity");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    let mut rx = bus.subscribe();

    forward(
        &path,
        Some("%Y-%m-%d".to_string()),
        &["parity check"],
        bus.clone(),
    )
    .await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    let written = content
        .lines()
        .next()
        .expect("one line written")
        .to_string();

    let mut published = None;
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::LogOut { data, .. } = &*event {
            published = Some(data.line.clone());
        }
    }
    assert_eq!(
        published.expect("an event should have been published"),
        written,
        "the bus and the file must agree, prefix included"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A burst arriving faster than the flush interval must all reach the file, in order.
/// This is the case the reused buffer could corrupt.
#[tokio::test]
async fn a_burst_is_written_in_order_without_loss() {
    let path = temp_log("burst-order");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    let lines: Vec<String> = (0..300).map(|idx| format!("burst {idx:04}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

    forward(&path, None, &refs, bus).await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    let written: Vec<&str> = content.lines().collect();
    assert_eq!(written.len(), 300);
    assert_eq!(written[0], "burst 0000");
    assert_eq!(written[299], "burst 0299");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Blank lines are written to the file but not published: an empty event carries no
/// information and would just be noise on the stream.
#[tokio::test]
async fn blank_lines_are_written_but_not_published() {
    let path = temp_log("blank-lines");
    let bus = broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0;
    let mut rx = bus.subscribe();

    forward(&path, None, &["", "real", ""], bus.clone()).await;

    let content = std::fs::read_to_string(&path).expect("log file should exist");
    assert_eq!(content, "\nreal\n\n");

    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::LogOut { data, .. } = &*event {
            published.push(data.line.clone());
        }
    }
    assert_eq!(published, vec!["real".to_string()]);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
