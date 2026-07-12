use client::Client;
use gpui::{App, AppContext as _};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use util::ResultExt;

mod hang_detection;

pub fn init(client: Arc<Client>, cx: &mut App) {
    hang_detection::start(client, cx);
    start_memory_usage_logging(cx);
}

const MEMORY_USAGE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const MEMORY_USAGE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10 * 60);
const MEMORY_USAGE_MINIMUM_LOGGED_DELTA: u64 = 64 * 1024 * 1024;

/// Periodically logs this process' memory usage, so that gradual memory growth can be
///
/// Logs on a fixed heartbeat, and additionally whenever resident memory changed
/// significantly since the last logged value, so that bursts of growth are timestamped
/// against the surrounding log entries.
fn start_memory_usage_logging(cx: &App) {
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        let Some(pid) = sysinfo::get_current_pid().log_err() else {
            return;
        };
        let refresh_kind = ProcessRefreshKind::nothing().with_memory();
        let mut system = System::new();
        let mut last_logged_resident: Option<u64> = None;
        let mut last_logged_at = Instant::now();
        loop {
            let refreshed = system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                refresh_kind,
            );
            if refreshed == 1
                && let Some(process) = system.process(pid)
            {
                let resident = process.memory();
                let significant_change = last_logged_resident.is_none_or(|last| {
                    resident.abs_diff(last) >= (last / 10).max(MEMORY_USAGE_MINIMUM_LOGGED_DELTA)
                });
                if significant_change || last_logged_at.elapsed() >= MEMORY_USAGE_HEARTBEAT_INTERVAL
                {
                    const MIB: u64 = 1024 * 1024;
                    let delta = match last_logged_resident {
                        Some(last) => {
                            format!(" ({:+} MiB)", (resident as i64 - last as i64) / MIB as i64)
                        }
                        None => String::new(),
                    };
                    log::info!(
                        "memory usage: resident {} MiB{delta}, virtual {} MiB",
                        resident / MIB,
                        process.virtual_memory() / MIB,
                    );
                    last_logged_resident = Some(resident);
                    last_logged_at = Instant::now();
                }
            }
            executor.timer(MEMORY_USAGE_POLL_INTERVAL).await;
        }
    })
    .detach();
}
