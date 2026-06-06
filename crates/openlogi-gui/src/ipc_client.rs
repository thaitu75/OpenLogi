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

use std::time::Duration;

use openlogi_agent_core::ipc::{AgentClient, AgentStatus};
use openlogi_core::config::Lighting;
use openlogi_core::device::DeviceInventory;
use openlogi_hid::{DeviceRoute, DpiInfo, SmartShiftMode, SmartShiftStatus};
use tarpc::client;
use tarpc::context;
use tarpc::tokio_serde::formats::Bincode;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// A poll snapshot pushed to the GPUI loop every `poll_period`.
pub struct PollUpdate {
    pub inventory: Vec<DeviceInventory>,
    pub status: AgentStatus,
}

/// A device command sent from the GPUI thread to the client thread. Each
/// request-shaped variant carries a `oneshot` for the reply; the fire-and-forget
/// variants carry none.
pub enum Command {
    SetDpi(DeviceRoute, u32, oneshot::Sender<Result<(), String>>),
    SetLighting(DeviceRoute, Lighting, oneshot::Sender<Result<(), String>>),
    SetSmartShift(
        DeviceRoute,
        SmartShiftMode,
        u8,
        u8,
        oneshot::Sender<Result<(), String>>,
    ),
    ReadDpi(DeviceRoute, oneshot::Sender<Result<DpiInfo, String>>),
    ReadSmartShift(
        DeviceRoute,
        oneshot::Sender<Result<SmartShiftStatus, String>>,
    ),
    ReloadConfig,
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
                let mut interval = tokio::time::interval(poll_period);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if poll(&mut client, &update_tx).await.is_err() {
                                client = None; // drop a dead connection; reconnect next tick
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
        Command::SetDpi(route, dpi, reply) => {
            let r = client.set_dpi(ctx, route, dpi).await;
            let _ = reply.send(rpc_result(r)?);
        }
        Command::SetLighting(route, lighting, reply) => {
            let r = client.set_lighting(ctx, route, lighting).await;
            let _ = reply.send(rpc_result(r)?);
        }
        Command::SetSmartShift(route, mode, auto, torque, reply) => {
            let r = client.set_smartshift(ctx, route, mode, auto, torque).await;
            let _ = reply.send(rpc_result(r)?);
        }
        Command::ReadDpi(route, reply) => {
            let r = client.read_dpi(ctx, route).await;
            let _ = reply.send(rpc_result(r)?);
        }
        Command::ReadSmartShift(route, reply) => {
            let r = client.read_smartshift(ctx, route).await;
            let _ = reply.send(rpc_result(r)?);
        }
        Command::ReloadConfig => {
            client.reload_config(ctx).await.map_err(|_| ())?;
        }
        Command::RequestAccessibilityPrompt => {
            client
                .request_accessibility_prompt(ctx)
                .await
                .map_err(|_| ())?;
        }
    }
    Ok(())
}

/// Unwrap a tarpc transport result: `Err` (connection dropped) propagates so the
/// caller reconnects; the inner application `Result` is returned for the reply.
fn rpc_result<T>(r: Result<T, tarpc::client::RpcError>) -> Result<T, ()> {
    r.map_err(|_| ())
}

/// Reply to a request-shaped command that the agent is unreachable.
#[allow(
    clippy::match_same_arms,
    reason = "the read arms send the same disconnect error to differently-typed reply channels, so they can't be merged with the write arms"
)]
fn reply_disconnected(cmd: Command) {
    const MSG: &str = "background agent not running";
    match cmd {
        Command::SetDpi(_, _, reply)
        | Command::SetLighting(_, _, reply)
        | Command::SetSmartShift(_, _, _, _, reply) => {
            let _ = reply.send(Err(MSG.to_string()));
        }
        Command::ReadDpi(_, reply) => {
            let _ = reply.send(Err(MSG.to_string()));
        }
        Command::ReadSmartShift(_, reply) => {
            let _ = reply.send(Err(MSG.to_string()));
        }
        Command::ReloadConfig | Command::RequestAccessibilityPrompt => {}
    }
}
