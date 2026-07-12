//! Inert telemetry stubs. This fork does not collect or transmit telemetry:
//! the `telemetry::event!` queue is never initialized and no reporting
//! endpoint is ever contacted. The `Telemetry` type is kept only so callers
//! that read ids or toggle-state keep compiling.

use crate::TelemetrySettings;
use anyhow::Result;
use clock::SystemClock;
use gpui::{App, Task};
use http_client::HttpClientWithUrl;
use parking_lot::Mutex;
use settings::{Settings, SettingsStore};
use std::sync::Arc;
use telemetry_events::AssistantEventData;
use worktree::{UpdatedEntriesSet, WorktreeId};

pub struct Telemetry {
    state: Arc<Mutex<TelemetryState>>,
}

struct TelemetryState {
    settings: TelemetrySettings,
    system_id: Option<Arc<str>>,       // Per system
    installation_id: Option<Arc<str>>, // Per app installation (different for dev, nightly, preview, and stable)
    metrics_id: Option<Arc<str>>,      // Per logged-in user
    is_staff: Option<bool>,
}

pub fn os_name() -> String {
    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        format!("Linux {}", gpui::guess_compositor())
    }
    #[cfg(target_os = "freebsd")]
    {
        format!("FreeBSD {}", gpui::guess_compositor())
    }

    #[cfg(target_os = "windows")]
    {
        "Windows".to_string()
    }
}

/// Note: This might do blocking IO! Only call from background threads
pub fn os_version() -> String {
    cfg_select! {
       feature = "test-support" => {
           // MacOS branch in particular is quite slow, hence we ought to "avoid" it in tests.
           "test binary".to_owned()
       }
       target_os = "macos" => {
           use regex::Regex;
           use std::sync::LazyLock;
           static MACOS_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
               Regex::new(r"(\s*\(Build [^)]*[0-9]\))").unwrap()
           });
           use objc2_foundation::NSProcessInfo;
           let process_info = NSProcessInfo::processInfo();
           let version_nsstring = process_info.operatingSystemVersionString();
           // "Version 15.6.1 (Build 24G90)" -> "15.6.1 (Build 24G90)"
           let version_string = version_nsstring.to_string().replace("Version ", "");
           // "15.6.1 (Build 24G90)" -> "15.6.1"
           // "26.0.0 (Build 25A5349a)" -> unchanged (Beta or Rapid Security Response; ends with letter)
           MACOS_VERSION_REGEX
               .replace_all(&version_string, "")
               .to_string()
       }
       any(target_os = "linux", target_os = "freebsd") => {
           use std::path::Path;

           let content = if let Ok(file) = std::fs::read_to_string(&Path::new("/etc/os-release")) {
               file
           } else if let Ok(file) = std::fs::read_to_string(&Path::new("/usr/lib/os-release")) {
               file
           } else if let Ok(file) = std::fs::read_to_string(&Path::new("/var/run/os-release")) {
               file
           } else {
               log::error!(
                   "Failed to load /etc/os-release, /usr/lib/os-release, or /var/run/os-release"
               );
               "".to_string()
           };
           util::parse_os_release(&content).unwrap_or_else(|| "unknown".to_string())
       }
       target_os = "windows" => {
           let mut info = unsafe { std::mem::zeroed() };
           let status = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) };
           if status.is_ok() {
               semver::Version::new(
                   info.dwMajorVersion as _,
                   info.dwMinorVersion as _,
                   info.dwBuildNumber as _,
               )
               .to_string()
           } else {
               "unknown".to_string()
           }
       }
    }
}

impl Telemetry {
    pub fn new(
        _clock: Arc<dyn SystemClock>,
        _client: Arc<HttpClientWithUrl>,
        cx: &mut App,
    ) -> Arc<Self> {
        let state = Arc::new(Mutex::new(TelemetryState {
            settings: *TelemetrySettings::get_global(cx),
            system_id: None,
            installation_id: None,
            metrics_id: None,
            is_staff: None,
        }));

        cx.observe_global::<SettingsStore>({
            let state = state.clone();

            move |cx| {
                let mut state = state.lock();
                state.settings = *TelemetrySettings::get_global(cx);
            }
        })
        .detach();

        Arc::new(Self { state })
    }

    pub fn start(
        self: &Arc<Self>,
        system_id: Option<String>,
        installation_id: Option<String>,
        _session_id: String,
        _cx: &App,
    ) {
        let mut state = self.state.lock();
        state.system_id = system_id.map(|id| id.into());
        state.installation_id = installation_id.map(|id| id.into());
    }

    pub fn metrics_enabled(self: &Arc<Self>) -> bool {
        self.state.lock().settings.metrics
    }

    pub fn diagnostics_enabled(self: &Arc<Self>) -> bool {
        self.state.lock().settings.diagnostics
    }

    pub fn set_authenticated_user_info(
        self: &Arc<Self>,
        metrics_id: Option<String>,
        is_staff: bool,
    ) {
        let mut state = self.state.lock();
        state.metrics_id = metrics_id.map(|id| id.into());
        state.is_staff = Some(is_staff);
    }

    pub fn report_assistant_event(self: &Arc<Self>, _event: AssistantEventData) {}

    pub fn log_edit_event(self: &Arc<Self>, _environment: &'static str, _is_via_ssh: bool) {}

    pub fn report_discovered_project_type_events(
        self: &Arc<Self>,
        _worktree_id: WorktreeId,
        _updated_entries_set: &UpdatedEntriesSet,
    ) {
    }

    /// Events forwarded from a remote server are dropped; this fork does not
    /// collect telemetry.
    pub fn report_remote_event(
        self: &Arc<Self>,
        _event_json: &str,
        _connection_type: &str,
        _os_name: String,
        _os_version: Option<String>,
        _architecture: String,
    ) -> Result<()> {
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn queued_events(self: &Arc<Self>) -> Vec<telemetry_events::FlexibleEvent> {
        Vec::new()
    }

    pub fn metrics_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().metrics_id.clone()
    }

    pub fn system_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().system_id.clone()
    }

    pub fn installation_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().installation_id.clone()
    }

    pub fn is_staff(self: &Arc<Self>) -> Option<bool> {
        self.state.lock().is_staff
    }

    pub fn flush_events(self: &Arc<Self>) -> Task<()> {
        Task::ready(())
    }
}
