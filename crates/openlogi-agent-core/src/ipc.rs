//! tarpc service contract between the GUI (client) and the agent (server).
//!
//! tarpc generates the `AgentClient` and the `serve` glue from this trait. tarpc
//! is strict request/response — no server push — so the two streaming needs
//! become polling: the GUI polls [`Agent::inventory`]/[`Agent::status`] on a
//! timer, and button-learning long-polls [`Agent::next_capture`], which the
//! agent holds open until a capture arrives or the request deadline elapses.

use openlogi_core::config::Lighting;
use openlogi_core::device::DeviceInventory;
use openlogi_hid::{CapturedInput, DeviceRoute, DpiInfo, SmartShiftMode, SmartShiftStatus};
use serde::{Deserialize, Serialize};

/// Wire-protocol version. Bumped only on a breaking change to the types below —
/// independent of the crate version. The GUI checks it via
/// [`Agent::protocol_version`] on connect and refuses to drive a mismatch
/// (transient only: both binaries ship in one `.app` and update atomically).
pub const PROTOCOL_VERSION: u32 = 1;

/// Agent health the GUI surfaces: the Accessibility gate, whether the hook is
/// live, and the autostart toggle state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub accessibility_granted: bool,
    pub hook_installed: bool,
    pub launch_at_login: bool,
    pub protocol_version: u32,
    pub agent_version: String,
}

#[tarpc::service]
pub trait Agent {
    /// Wire-protocol version, for the connect handshake.
    async fn protocol_version() -> u32;
    /// Accessibility / hook / autostart state for the GUI gate + settings.
    async fn status() -> AgentStatus;
    /// Latest device inventory snapshot (the GUI polls this on a timer while a
    /// window is open).
    async fn inventory() -> Vec<DeviceInventory>;
    /// Re-read `config.toml` and rebuild the live binding/DPI maps. Called by
    /// the GUI after it saves a config change.
    async fn reload_config();
    /// Apply a DPI value to `route` now (slider preview / commit).
    async fn set_dpi(route: DeviceRoute, dpi: u32) -> Result<(), String>;
    /// Apply a lighting config to `route` now.
    async fn set_lighting(route: DeviceRoute, lighting: Lighting) -> Result<(), String>;
    /// Apply a full SmartShift config to `route` now.
    async fn set_smartshift(
        route: DeviceRoute,
        mode: SmartShiftMode,
        auto_disengage: u8,
        tunable_torque: u8,
    ) -> Result<(), String>;
    /// Read the current DPI + supported values from `route`.
    async fn read_dpi(route: DeviceRoute) -> Result<DpiInfo, String>;
    /// Read the current SmartShift config from `route`.
    async fn read_smartshift(route: DeviceRoute) -> Result<SmartShiftStatus, String>;
    /// Begin a button-learning capture on `route` (tees the agent's existing
    /// capture session to [`Agent::next_capture`]; does not open a 2nd channel).
    async fn start_capture(route: DeviceRoute, capture_thumbwheel: bool);
    /// Stop the button-learning capture stream.
    async fn stop_capture();
    /// Long-poll the next captured input: held until one arrives or the request
    /// deadline elapses (then `None`).
    async fn next_capture() -> Option<CapturedInput>;
    /// Override the active app for per-app binding overlays. The agent also
    /// tracks the frontmost app itself; this is an explicit/testing override.
    async fn set_active_app(bundle_id: Option<String>);
    /// Prompt for Accessibility from the agent, so the system dialog names the
    /// agent — the actually-trusted binary — rather than the GUI.
    async fn request_accessibility_prompt();
    /// Install or remove the agent's launchd autostart.
    async fn set_launch_at_login(enabled: bool);
}
