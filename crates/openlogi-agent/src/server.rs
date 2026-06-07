//! tarpc IPC server: backs the [`Agent`] service with the orchestrator and the
//! agent-core device-I/O helpers, listening on the agent's Unix-domain socket.
//!
//! The agent owns all device I/O, so the GUI never opens a device — it routes
//! "apply now" / "read" commands here, and polls inventory/status.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use openlogi_agent_core::hardware;
use openlogi_agent_core::ipc::{Agent, AgentStatus, PROTOCOL_VERSION};
use openlogi_agent_core::orchestrator::{Orchestrator, SharedRuntime};
use openlogi_core::config::{Config, Lighting};
use openlogi_core::device::DeviceInventory;
use openlogi_hid::{DeviceRoute, DpiInfo, SmartShiftMode, SmartShiftStatus};
use openlogi_hook::Hook;
use tarpc::context::Context;
use tarpc::server::{BaseChannel, Channel as _};
use tarpc::tokio_serde::formats::Bincode;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Shared handle to the agent's state, cloned per connection (and per request).
#[derive(Clone)]
pub struct AgentServer {
    pub orchestrator: Arc<Mutex<Orchestrator>>,
    pub shared: SharedRuntime,
    pub hook_installed: Arc<AtomicBool>,
}

impl Agent for AgentServer {
    async fn protocol_version(self, _: Context) -> u32 {
        PROTOCOL_VERSION
    }

    async fn status(self, _: Context) -> AgentStatus {
        AgentStatus {
            accessibility_granted: Hook::has_accessibility(),
            hook_installed: self.hook_installed.load(Ordering::Relaxed),
            launch_at_login: self.orchestrator.lock().await.launch_at_login(),
            protocol_version: PROTOCOL_VERSION,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    async fn inventory(self, _: Context) -> Vec<DeviceInventory> {
        self.orchestrator.lock().await.inventory()
    }

    async fn reload_config(self, _: Context) {
        match Config::load_or_default() {
            Ok(config) => {
                let launch_at_login = config.app_settings.launch_at_login;
                self.orchestrator.lock().await.reload_config(config);
                // The GUI's launch-at-login toggle reaches us through this
                // reload, so re-reconcile the autostart from the new config.
                crate::launch_agent::reconcile(launch_at_login);
            }
            Err(e) => warn!(error = %e, "reload_config: parse failed; keeping current config"),
        }
    }

    async fn set_dpi(self, _: Context, route: DeviceRoute, dpi: u32) -> Result<(), String> {
        hardware::apply_dpi(&self.shared.capture_channel, &route, dpi)
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_lighting(
        self,
        _: Context,
        route: DeviceRoute,
        lighting: Lighting,
    ) -> Result<(), String> {
        hardware::apply_lighting(&route, &lighting)
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_smartshift(
        self,
        _: Context,
        route: DeviceRoute,
        mode: SmartShiftMode,
        auto_disengage: u8,
        tunable_torque: u8,
    ) -> Result<(), String> {
        hardware::apply_smartshift(
            &self.shared.capture_channel,
            &route,
            mode,
            auto_disengage,
            tunable_torque,
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn read_dpi(self, _: Context, route: DeviceRoute) -> Result<DpiInfo, String> {
        hardware::read_dpi(&route).await.map_err(|e| e.to_string())
    }

    async fn read_smartshift(
        self,
        _: Context,
        route: DeviceRoute,
    ) -> Result<SmartShiftStatus, String> {
        hardware::read_smartshift(&route)
            .await
            .map_err(|e| e.to_string())
    }

    async fn request_accessibility_prompt(self, _: Context) {
        Hook::prompt_accessibility();
    }
}

/// Bind the agent's Unix socket and serve [`Agent`] requests until the process
/// exits. A stale socket left by a prior crash is removed first — `main` holds
/// the single-instance lock (`agent.lock`), so no other live agent owns this
/// socket and any leftover file is from a dead instance.
pub async fn run(server: AgentServer, socket_path: PathBuf) {
    if let Some(parent) = socket_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(error = %e, "could not create IPC socket dir; IPC disabled");
        return;
    }
    let _ = std::fs::remove_file(&socket_path);

    let mut incoming =
        match tarpc::serde_transport::unix::listen(&socket_path, Bincode::default).await {
            Ok(incoming) => incoming,
            Err(e) => {
                warn!(error = %e, "could not bind IPC socket; IPC disabled");
                return;
            }
        };
    info!(socket = %socket_path.display(), "IPC server listening");

    while let Some(conn) = incoming.next().await {
        let transport = match conn {
            Ok(transport) => transport,
            Err(e) => {
                warn!(error = %e, "IPC accept failed");
                continue;
            }
        };
        let server = server.clone();
        let channel = BaseChannel::with_defaults(transport);
        tokio::spawn(
            channel
                .execute(server.serve())
                .for_each(|response| async move {
                    tokio::spawn(response);
                }),
        );
    }
}
