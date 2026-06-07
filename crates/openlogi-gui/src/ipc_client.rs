//! Client side of the agent IPC.
//!
//! The agent owns all device I/O, so the GUI never opens a device — it connects
//! to the agent's Unix socket and (a) polls inventory + status on a timer to
//! drive the device list and the Accessibility gate, and (b) forwards "apply
//! now" / "read" device commands. Both run on one dedicated OS thread with a
//! tokio runtime (the GPUI thread owns no async runtime), mirroring the old
//! watcher pattern: results cross back over `mpsc` to the GPUI loop.
//!
//! The single client connection is re-established automatically if the agent
//! restarts (launchd `KeepAlive`), so the GUI recovers without user action.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use openlogi_agent_core::ipc::{AgentClient, AgentStatus};
use openlogi_core::config::Lighting;
use openlogi_core::device::DeviceInventory;
use openlogi_hid::{DeviceRoute, DpiInfo, SmartShiftMode, SmartShiftStatus, WriteError};
use tarpc::client;
use tarpc::context;
use tarpc::tokio_serde::formats::Bincode;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Minimum gap between agent-launch attempts while the socket is unreachable.
/// Long enough that a missing or crash-looping binary can't be respawned in a
/// tight loop, short enough that a quit / crashed agent is recovered promptly.
const SPAWN_RETRY_PERIOD: Duration = Duration::from_secs(30);

/// A poll snapshot pushed to the GPUI loop every `poll_period`.
pub struct PollUpdate {
    pub inventory: Vec<DeviceInventory>,
    pub status: AgentStatus,
}

/// A device command sent from the GPUI thread to the client thread. Reads carry
/// a `oneshot` for the reply; "apply now" writes are fire-and-forget (the GUI
/// updates its display optimistically and the client logs any device failure).
pub enum Command {
    SetDpi(DeviceRoute, u32),
    SetLighting(DeviceRoute, Lighting),
    SetSmartShift(DeviceRoute, SmartShiftMode, u8, u8),
    ReadDpi(DeviceRoute, oneshot::Sender<Result<DpiInfo, WriteError>>),
    ReadSmartShift(
        DeviceRoute,
        oneshot::Sender<Result<SmartShiftStatus, WriteError>>,
    ),
    ReloadConfig,
    /// Ask the agent to fire the macOS Accessibility prompt. The agent owns the
    /// CGEventTap, so the system dialog must name (and authorize) the *agent*
    /// binary, not the GUI — prompting locally would grant the wrong process.
    RequestAccessibilityPrompt,
}

/// Handle the GUI holds to talk to the agent: a stream of poll snapshots and a
/// sender for device commands.
pub struct IpcClient {
    pub updates: mpsc::UnboundedReceiver<PollUpdate>,
    pub commands: mpsc::UnboundedSender<Command>,
}

/// Spawn the IPC client thread. Returns immediately; the thread connects (and
/// reconnects) on its own.
#[must_use]
pub fn spawn(poll_period: Duration) -> IpcClient {
    let (update_tx, updates) = mpsc::unbounded_channel();
    let (commands, mut cmd_rx) = mpsc::unbounded_channel::<Command>();

    let spawn_result = std::thread::Builder::new()
        .name("openlogi-ipc-client".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!(error = %e, "tokio runtime init failed; IPC client exiting");
                    return;
                }
            };
            rt.block_on(async move {
                let mut client: Option<AgentClient> = None;
                // The agent is normally started by launchd, but the GUI launches
                // it if the socket is down — invaluable in dev (one `cargo run` of
                // the GUI brings the whole system up) and a prod fallback. Retry
                // while the socket stays down, but rate-limited (see
                // SPAWN_RETRY_PERIOD) so a missing / failing binary can't become a
                // tight respawn loop, while a crashed / quit agent is still
                // recovered without restarting the GUI.
                let mut last_spawn_attempt: Option<Instant> = None;
                let mut interval = tokio::time::interval(poll_period);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if poll(&mut client, &update_tx).await.is_err() {
                                client = None; // drop a dead connection; reconnect next tick
                            }
                            if client.is_none()
                                && last_spawn_attempt
                                    .is_none_or(|t| t.elapsed() >= SPAWN_RETRY_PERIOD)
                            {
                                spawn_agent();
                                last_spawn_attempt = Some(Instant::now());
                            }
                        }
                        cmd = cmd_rx.recv() => {
                            let Some(cmd) = cmd else { break }; // GUI dropped the sender → shut down
                            if handle(&mut client, cmd).await.is_err() {
                                client = None;
                            }
                        }
                    }
                }
            });
        });
    if let Err(e) = spawn_result {
        warn!(error = %e, "could not spawn IPC client thread — agent state unavailable");
    }

    IpcClient { updates, commands }
}

/// Launch the agent once when the socket is unreachable. Detached `spawn` so it
/// outlives the GUI (the agent is the always-on process); logs and moves on if
/// the binary can't be found / started — the user may start it via launchd or by
/// hand, and the poll loop keeps retrying the connection regardless.
fn spawn_agent() {
    let Some(path) = agent_binary_path() else {
        warn!(
            "agent not reachable and its binary wasn't found next to the GUI — \
             start it via launchd or by hand"
        );
        return;
    };
    match std::process::Command::new(&path).spawn() {
        Ok(_) => info!(path = %path.display(), "agent not running — launched it"),
        Err(e) => warn!(error = %e, path = %path.display(), "could not launch the agent"),
    }
}

/// Resolve the agent executable relative to the running GUI: a sibling in the
/// cargo target dir (dev), else the embedded `OpenLogiAgent.app` login-item
/// helper (packaged build).
fn agent_binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let sibling = dir.join("openlogi-agent");
    if sibling.exists() {
        return Some(sibling);
    }
    // Packaged: …/OpenLogi.app/Contents/MacOS/openlogi-gui → the helper at
    // …/OpenLogi.app/Contents/Library/LoginItems/OpenLogiAgent.app/Contents/MacOS/openlogi-agent
    let helper = dir
        .parent()?
        .join("Library/LoginItems/OpenLogiAgent.app/Contents/MacOS/openlogi-agent");
    helper.exists().then_some(helper)
}

/// Ensure a live client, connecting on demand. `Err` means the agent is
/// unreachable right now (it may be starting up / restarting).
async fn ensure(client: &mut Option<AgentClient>) -> std::io::Result<&AgentClient> {
    if client.is_none() {
        let path = openlogi_core::paths::agent_socket_path()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let transport = tarpc::serde_transport::unix::connect(&path, Bincode::default).await?;
        *client = Some(AgentClient::new(client::Config::default(), transport).spawn());
        debug!("connected to agent IPC socket");
    }
    // `client` is `Some` here (just set, or already was); the `None` arm is
    // unreachable but keeps this `expect`-free.
    match client.as_ref() {
        Some(client) => Ok(client),
        None => Err(std::io::Error::other("IPC client unexpectedly absent")),
    }
}

/// Poll inventory + status and push a snapshot. `Err` on a dropped connection.
async fn poll(
    client: &mut Option<AgentClient>,
    update_tx: &mpsc::UnboundedSender<PollUpdate>,
) -> Result<(), ()> {
    let Ok(client) = ensure(client).await else {
        return Ok(()); // agent not up yet; try again next tick (keep `client` None)
    };
    let inventory = client.inventory(context::current()).await.map_err(|_| ())?;
    let status = client.status(context::current()).await.map_err(|_| ())?;
    let _ = update_tx.send(PollUpdate { inventory, status });
    Ok(())
}

/// Run one device command. `Err` signals a dropped connection so the caller
/// reconnects; the command's own failure is reported back over its oneshot.
async fn handle(client: &mut Option<AgentClient>, cmd: Command) -> Result<(), ()> {
    // keep `client` None on connect failure; that's not a dropped live connection
    let Ok(client) = ensure(client).await else {
        reply_disconnected(cmd);
        return Ok(());
    };
    let ctx = context::current();
    match cmd {
        Command::SetDpi(route, dpi) => log_apply(client.set_dpi(ctx, route, dpi).await)?,
        Command::SetLighting(route, lighting) => {
            log_apply(client.set_lighting(ctx, route, lighting).await)?;
        }
        Command::SetSmartShift(route, mode, auto, torque) => {
            log_apply(client.set_smartshift(ctx, route, mode, auto, torque).await)?;
        }
        Command::ReadDpi(route, reply) => {
            let _ = reply.send(rpc_result(client.read_dpi(ctx, route).await)?);
        }
        Command::ReadSmartShift(route, reply) => {
            let _ = reply.send(rpc_result(client.read_smartshift(ctx, route).await)?);
        }
        Command::ReloadConfig => client.reload_config(ctx).await.map_err(|_| ())?,
        Command::RequestAccessibilityPrompt => client
            .request_accessibility_prompt(ctx)
            .await
            .map_err(|_| ())?,
    }
    Ok(())
}

/// A fire-and-forget "apply now": `Err(())` (transport drop) propagates so the
/// caller reconnects; a device-side failure is logged, not surfaced.
fn log_apply(r: Result<Result<(), WriteError>, tarpc::client::RpcError>) -> Result<(), ()> {
    match r {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            warn!(error = %e, "agent rejected device command");
            Ok(())
        }
        Err(_) => Err(()),
    }
}

/// Unwrap a tarpc transport result: `Err(())` (connection dropped) propagates so
/// the caller reconnects; the inner application `Result` is returned for the reply.
fn rpc_result<T>(r: Result<T, tarpc::client::RpcError>) -> Result<T, ()> {
    r.map_err(|_| ())
}

/// Reply to a read command that the agent is unreachable; writes are
/// fire-and-forget so they have nothing to reply to.
#[allow(
    clippy::match_same_arms,
    reason = "the two read arms send the same disconnect error to differently-typed reply channels, so they can't be merged"
)]
fn reply_disconnected(cmd: Command) {
    // Transient (Hidpp), not a permanent feature error: the agent is just
    // restarting, so the panel should keep retrying, not latch "unsupported".
    let unreachable = || WriteError::Hidpp("background agent not running".to_string());
    match cmd {
        Command::ReadDpi(_, reply) => {
            let _ = reply.send(Err(unreachable()));
        }
        Command::ReadSmartShift(_, reply) => {
            let _ = reply.send(Err(unreachable()));
        }
        _ => {}
    }
}
