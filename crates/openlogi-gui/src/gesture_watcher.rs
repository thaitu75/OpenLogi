//! Background gesture-button capture for the active device.
//!
//! Runs [`openlogi_hid::run_gesture_session`] on a dedicated thread for
//! whichever device the DPI / SmartShift path currently targets
//! ([`DpiCycleState::target`]), restarts it when the carousel selection
//! changes, and dispatches each captured [`GestureDirection`] through the
//! shared gesture binding map and the common action path
//! ([`crate::dispatch_action`]).
//!
//! Unlike the CGEventTap hook, this needs no macOS Accessibility permission —
//! gesture events arrive over HID++, and the bound action is synthesised the
//! same way regardless.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use openlogi_core::binding::{Action, GestureDirection};
use openlogi_hid::{GestureTarget, run_gesture_session};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::components::dpi_panel::DpiTarget;
use crate::state::DpiCycleState;

/// Shared gesture-direction binding map, mirrored from `AppState` (same shape
/// as the hook binding map, keyed by direction). The watcher reads it to map a
/// captured swipe to the user's bound action.
pub type GestureBindings = Arc<RwLock<BTreeMap<GestureDirection, Action>>>;

/// How often to re-read the active device target so a carousel switch re-points
/// gesture capture at the newly-selected device.
const TARGET_POLL: Duration = Duration::from_secs(1);

/// Spawn the gesture-capture manager thread. It owns a current-thread tokio
/// runtime that keeps a gesture session pointed at the active device and
/// dispatches each completed swipe through `bindings`.
pub fn spawn(bindings: GestureBindings, dpi_cycle: Arc<RwLock<DpiCycleState>>) {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "gesture watcher: could not build tokio runtime");
                return;
            }
        };
        runtime.block_on(manage(bindings, dpi_cycle));
    });
}

/// Keep one gesture session alive for the active device, restarting it on
/// device switch, and dispatch incoming directions. Runs for the lifetime of
/// the process.
async fn manage(bindings: GestureBindings, dpi_cycle: Arc<RwLock<DpiCycleState>>) {
    let (dir_tx, mut dir_rx) = mpsc::unbounded_channel::<GestureDirection>();
    let mut current: Option<DpiTarget> = None;
    let mut stop: Option<oneshot::Sender<()>> = None;
    let mut ticker = tokio::time::interval(TARGET_POLL);

    loop {
        tokio::select! {
            Some(direction) = dir_rx.recv() => {
                let action = bindings
                    .read()
                    .ok()
                    .and_then(|guard| guard.get(&direction).cloned());
                if let Some(action) = action {
                    debug!(?direction, action = %action.label(), "gesture → action");
                    crate::dispatch_action(&action, &dpi_cycle);
                } else {
                    debug!(?direction, "gesture with no binding — ignored");
                }
            }
            _ = ticker.tick() => {
                let target = dpi_cycle.read().ok().and_then(|guard| guard.target.clone());
                if target == current {
                    continue;
                }
                // Device switched (or first tick): stop the old session and
                // start one for the new target. Sending on the oneshot lets the
                // old session restore the control's default mapping.
                if let Some(stop) = stop.take() {
                    let _ = stop.send(());
                }
                current.clone_from(&target);
                if let Some(target) = target {
                    let (stop_tx, stop_rx) = oneshot::channel();
                    let sink = dir_tx.clone();
                    let gesture_target = GestureTarget {
                        receiver_uid: Some(target.receiver_uid),
                        slot: target.slot,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = run_gesture_session(gesture_target, sink, stop_rx).await {
                            debug!(error = %e, "gesture session ended");
                        }
                    });
                    stop = Some(stop_tx);
                }
            }
        }
    }
}
