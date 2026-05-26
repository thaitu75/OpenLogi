//! OpenLogi GPUI desktop window.
//!
//! Initial HID++ inventory is collected synchronously on startup (GPUI owns
//! the main thread, so we can't move it onto a tokio runtime). Live polling
//! lands when there's something to react to.

mod app;
mod app_watcher;
mod asset;
mod components;
mod data;
mod hardware;
mod inventory_watcher;
mod launch_agent;
mod mouse_model;
mod single_instance;
mod state;
mod theme;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Shared binding map threaded between `AppState` and the hook callback.
type BindingMap = Arc<RwLock<BTreeMap<ButtonId, Action>>>;

use anyhow::{Context as _, Result};
use gpui::{
    AppContext, BorrowAppContext as _, Bounds, SharedString, Size, Styled, TitlebarOptions,
    WindowBounds, WindowOptions, px,
};
use gpui_component::{ActiveTheme, Root};
use openlogi_core::binding::{self, Action, ButtonId};
use openlogi_core::config::Config;
use openlogi_core::device::{DeviceInventory, DeviceModelInfo};
use openlogi_hook::{EventDisposition, Hook, MouseEvent};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::app::AppView;
use crate::hardware::{toggle_smartshift_in_background, write_dpi_in_background};
use crate::state::{AppState, DpiCycleState};

fn main() -> Result<()> {
    init_tracing();

    // P2.3: refuse a second copy. If the lock is held we exit non-error so
    // the user's launcher (Dock click, Spotlight, `open -a OpenLogi`) doesn't
    // surface a scary crash dialog.
    let _guard = match single_instance::acquire() {
        Ok(g) => g,
        Err(single_instance::InstanceError::AlreadyRunning { path }) => {
            info!(
                path = %path.display(),
                "another OpenLogi instance is already running — exiting"
            );
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::from(e).context("single-instance check")),
    };

    // P2.2: keep the LaunchAgent in sync with the user's autostart preference.
    // Cheap (one fs read + maybe write), failures are logged inside.
    let early_config = Config::load_or_default().ok();
    if let Some(cfg) = early_config.as_ref() {
        launch_agent::reconcile(cfg.app_settings.launch_at_login);
    }

    let inventories = enumerate_blocking().context("HID enumeration failed")?;

    // Refresh / fetch device assets up front so the AssetCache the GUI
    // reads finds the right files on disk. Release builds normally skip
    // the sync because the .app ships pre-populated; debug builds always
    // run it. Either default is overridable via `OPENLOGI_SYNC=on/off`.
    let probe_cache = asset::AssetCache::new();
    if asset::sync::should_run(probe_cache.has_bundle_root()) {
        let server = std::env::var("OPENLOGI_ASSETS")
            .unwrap_or_else(|_| asset::sync::DEFAULT_BASE.to_string());
        let models = collect_models(&inventories);
        if let Err(e) = asset::sync::sync(&server, &models) {
            warn!(error = ?e, "asset sync raised — continuing with whatever's cached");
        }
    }
    drop(probe_cache);

    // Build the shared hook state from the on-disk config so the hook sees
    // saved bindings + DPI presets from the first event, before AppState is
    // initialised inside the GPUI thread. The Arcs are also handed into the
    // AppState global (see `cx.open_window` below) so that subsequent
    // `commit_binding` / `commit_dpi_presets` writes are visible to the hook
    // callback without GPUI thread involvement.
    let (hook_bindings, dpi_cycle, initial_config) = load_config_and_bindings(&inventories);

    // Start the OS hook. `_hook` is held alive for the duration of `run`;
    // both Arcs are captured by the callback closure.
    let _hook = start_hook(Arc::clone(&hook_bindings), Arc::clone(&dpi_cycle));

    // P1.6: poll for HID hot-plug / disconnect every 2s. Updates flow
    // through `inventory_rx` into AppState::refresh_inventories below.
    let mut inventory_rx = inventory_watcher::spawn(std::time::Duration::from_secs(2));

    // P1.4: poll for foreground-app changes every 1s. Empty channel on
    // non-macOS — the loop below falls through.
    let mut app_rx = app_watcher::spawn(std::time::Duration::from_secs(1));

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        cx.spawn(async move |cx| {
            let bounds = cx.update(|cx| Bounds::centered(None, Size::new(px(1100.), px(750.)), cx));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(Size::new(px(720.), px(520.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("OpenLogi")),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                ..WindowOptions::default()
            };

            #[allow(
                clippy::expect_used,
                reason = "failure to open the main window is fatal; nothing useful to recover to"
            )]
            cx.open_window(options, move |window, cx| {
                // Pre-set AppState with the hook-shared Arc BEFORE AppView::new
                // runs. AppView::new checks `has_global::<AppState>()` and
                // skips re-initialisation if the global is already present.
                // `with_runtime_shared` rebuilds the binding map and writes
                // back into the shared Arc, so the values match what the hook
                // was already reading via `load_config_and_bindings`.
                if !cx.has_global::<AppState>() {
                    let cache = asset::AssetCache::new();
                    cx.set_global(AppState::with_runtime_shared(
                        initial_config,
                        &inventories,
                        &cache,
                        hook_bindings,
                        dpi_cycle,
                    ));
                }
                let view = cx.new(|cx| AppView::new(&inventories, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("opening the main window should not fail");

            // Drain inventory + foreground-app updates for the lifetime of
            // the app. Each event rebuilds the relevant slice of AppState
            // and lets every observer (carousel, mouse model, DPI panel,
            // hook thread) pick up the change.
            //
            // `tokio::select!` is unavailable inside gpui's executor (it
            // needs the tokio reactor), so the two channels are polled with
            // a hand-rolled biased race built from `futures_lite`'s pollster.
            // The two streams produce events at human pace (≤ 1 Hz combined
            // in steady state), so any reasonable scheduling fairness is
            // good enough.
            loop {
                tokio::select! {
                    Some(new_inv) = inventory_rx.recv() => {
                        cx.update(|cx| {
                            let cache = asset::AssetCache::new();
                            cx.update_global::<AppState, _>(|state, _| {
                                state.refresh_inventories(&new_inv, &cache);
                            });
                        });
                    }
                    Some(bundle) = app_rx.recv() => {
                        cx.update(|cx| {
                            cx.update_global::<AppState, _>(|state, _| {
                                state.set_current_app(bundle);
                            });
                        });
                    }
                    else => break,
                }
            }
        })
        .detach();
    });

    // `_hook` drops here. On macOS `Hook::stop()` is called via `Drop`.
    // A `Drop` impl on `Hook` calling `stop` would be ideal but requires the
    // hook inner state to be `Option<HookInner>` — that's a P0.1 follow-up.
    // For now the background thread keeps running until the process exits,
    // which is fine for a single-window app that terminates cleanly.
    Ok(())
}

/// Load config from disk and build the initial hook-shared state for the
/// first paired device with HID++ model info — same selection rule as
/// [`AppState::with_runtime`]. Pre-populating both `Arc`s here means the
/// hook callback sees the right bindings *and* DPI presets from the very
/// first event, well before `AppState::with_runtime_shared` runs inside
/// the GPUI thread.
fn load_config_and_bindings(
    inventories: &[DeviceInventory],
) -> (BindingMap, Arc<RwLock<DpiCycleState>>, Config) {
    let config = match Config::load_or_default() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "could not load config.toml; using default bindings");
            Config::default()
        }
    };

    let (device_key, dpi_target) = inventories
        .iter()
        .find_map(|inv| {
            let receiver_uid = inv.receiver.unique_id.clone();
            inv.paired.iter().find_map(|p| {
                let model = p.model_info.as_ref()?;
                let key = model.config_key();
                let target =
                    receiver_uid
                        .as_ref()
                        .map(|uid| crate::components::dpi_panel::DpiTarget {
                            receiver_uid: uid.clone(),
                            slot: p.slot,
                        });
                Some((Some(key), target))
            })
        })
        .unwrap_or_default();

    let stored = device_key
        .as_deref()
        .map(|k| config.bindings_for(k))
        .unwrap_or_default();

    let mut bindings: BTreeMap<ButtonId, Action> = ButtonId::ALL
        .iter()
        .copied()
        .map(|b| (b, binding::default_binding(b)))
        .collect();
    for (k, v) in stored {
        bindings.insert(k, v);
    }
    let bindings_arc = Arc::new(RwLock::new(bindings));

    let dpi_presets = device_key
        .as_deref()
        .map(|k| config.dpi_presets(k))
        .unwrap_or_default();
    let dpi_cycle_arc = Arc::new(RwLock::new(DpiCycleState {
        presets: dpi_presets,
        index: 0,
        target: dpi_target,
    }));

    (bindings_arc, dpi_cycle_arc, config)
}

/// Attempt to start the OS hook. Returns `None` if Accessibility is not
/// granted or on an unsupported platform — the app continues without crashing.
fn start_hook(bindings: BindingMap, dpi_cycle: Arc<RwLock<DpiCycleState>>) -> Option<Hook> {
    if !Hook::has_accessibility() {
        warn!(
            "Accessibility not granted — events will not be captured. \
             Open System Settings → Privacy & Security → Accessibility."
        );
        return None;
    }

    let result = Hook::start(move |event| {
        match event {
            MouseEvent::Button { id, pressed } => {
                // OpenLogi "owns" the side buttons: they're suppressed so the
                // OS default (browser back/forward) never fires, and we
                // synthesize the bound action ourselves on press. Primary
                // clicks pass through to keep the OS default behaviour even
                // though `default_binding` lists actions for them — rebinding
                // Left/Middle requires the gesture-button work in P1.5.
                let owned = matches!(id, ButtonId::Back | ButtonId::Forward);
                if !owned {
                    return EventDisposition::PassThrough;
                }
                if pressed {
                    let action = bindings.read().ok().and_then(|g| g.get(&id).cloned());
                    if let Some(action) = action {
                        info!(button = %id, action = %action.label(), "button → executing bound action");
                        dispatch_action(&action, &dpi_cycle);
                    } else {
                        info!(button = %id, "button pressed with no binding — suppressed");
                    }
                }
                // Suppress both press and release so foreground apps never see
                // an orphan event pair.
                EventDisposition::Suppress
            }
            MouseEvent::Scroll { .. } => {
                // Scroll events have no ButtonId binding yet; pass through.
                // P1.2 (scroll inversion) will revisit.
                EventDisposition::PassThrough
            }
        }
    });

    match result {
        Ok(hook) => {
            info!("OS mouse hook installed");
            Some(hook)
        }
        Err(e) => {
            warn!(error = %e, "could not install OS mouse hook — events will not be captured");
            None
        }
    }
}

/// Route a bound action either to OS-level event synthesis
/// ([`Action::execute`]) or to one of OpenLogi's hardware-side handlers
/// (currently just DPI cycling).
///
/// `dpi_cycle` is held across a write lock long enough to advance the index
/// and snapshot the new DPI + target; the actual HID write spawns its own
/// thread via [`write_dpi_in_background`] to keep the hook callback
/// non-blocking.
fn dispatch_action(action: &Action, dpi_cycle: &Arc<RwLock<DpiCycleState>>) {
    let next = match action {
        Action::CycleDpiPresets => match dpi_cycle.write() {
            Ok(mut guard) => guard.cycle(),
            Err(e) => {
                warn!(error = %e, "dpi_cycle lock poisoned — cycle skipped");
                None
            }
        },
        Action::SetDpiPreset(i) => match dpi_cycle.write() {
            Ok(mut guard) => guard.set(usize::from(*i)),
            Err(e) => {
                warn!(error = %e, "dpi_cycle lock poisoned — set skipped");
                None
            }
        },
        Action::ToggleSmartShift => {
            // P1.1: SmartShift uses the same device target as DPI. Read
            // the target from the shared cycle state instead of duplicating
            // a SmartShiftState mirror.
            let target = dpi_cycle.read().ok().and_then(|g| g.target.clone());
            info!("SmartShift toggle → flipping wheel mode");
            toggle_smartshift_in_background(target);
            return;
        }
        other => {
            other.execute();
            None
        }
    };
    if let Some((dpi, target)) = next {
        info!(dpi, "DPI action → writing to device");
        write_dpi_in_background(target, dpi);
    } else if matches!(action, Action::CycleDpiPresets | Action::SetDpiPreset(_)) {
        info!(
            action = %action.label(),
            "no DPI presets configured for active device — press ignored"
        );
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_env("OPENLOGI_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn enumerate_blocking() -> Result<Vec<DeviceInventory>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime init")?;
    rt.block_on(openlogi_hid::enumerate())
        .context("openlogi_hid::enumerate")
}

/// Flatten every paired device's HID++ model snapshot — that's what the
/// asset sync feeds into the registry lookup.
fn collect_models(inventories: &[DeviceInventory]) -> Vec<DeviceModelInfo> {
    inventories
        .iter()
        .flat_map(|inv| inv.paired.iter())
        .filter_map(|p| p.model_info)
        .collect()
}
