use super::*;

/// Every valid expression must resolve to a restart strictly in the future.
///
/// Table-driven: the daily-2am, every-6-hours, and hourly expressions previously
/// had one copy-paste test each asserting only this shared contract.
#[test]
fn cron_valid_expressions_restart_in_the_future() {
    let expressions = ["0 0 2 * * *", "0 0 */6 * * *", "0 * * * * *"];
    let base_time = crate::process_manager::now_epoch_secs();

    for expr in expressions {
        let next = crate::process_manager::calculate_next_cron_restart(expr, Some(base_time))
            .unwrap_or_else(|err| panic!("{expr} should be valid: {err}"));
        assert!(
            next > base_time,
            "{expr}: next restart ({next}) should be in the future relative to {base_time}"
        );
    }
}

/// The hourly expression lands on the next hour, not a day out — the one assertion
/// that actually distinguishes it from the other valid expressions.
#[test]
fn cron_hourly_restart_is_within_the_hour() {
    let base_time = crate::process_manager::now_epoch_secs();
    let next = crate::process_manager::calculate_next_cron_restart("0 * * * * *", Some(base_time))
        .expect("hourly expression should be valid");
    let delay = next.saturating_sub(base_time);
    assert!(
        (1..=3600).contains(&delay),
        "hourly restart should land within the hour, got {delay}s"
    );
}

#[test]
fn cron_invalid_expression_fails() {
    let result =
        crate::process_manager::calculate_next_cron_restart("this is not valid at all", None);
    let err_msg = result.expect_err("invalid cron should fail").to_string();
    assert!(
        err_msg.contains("invalid cron expression"),
        "error should mention invalid cron, got: {err_msg}"
    );
}

/// The fixture defaults must stay cron-free so schedule tests start from a known state.
#[test]
fn fixture_process_has_no_cron_schedule() {
    let process = fixture_process();
    assert!(
        process.cron_restart.is_none(),
        "fixture should not set cron_restart"
    );
    assert!(
        process.next_cron_restart.is_none(),
        "fixture should not set next_cron_restart"
    );
}
