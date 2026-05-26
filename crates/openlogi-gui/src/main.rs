//! OpenLogi GPUI desktop window.
//!
//! Initial HID++ inventory is collected synchronously on startup (GPUI owns
//! the main thread, so we can't move it onto a tokio runtime). Live polling
//! lands when there's something to react to.

mod app;
mod asset;
mod components;
mod data;
mod mouse_model;
mod state;
mod theme;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Shared binding map threaded between `AppState` and the hook callback.
type BindingMap = Arc<RwLock<BTreeMap<ButtonId, Action>>>;

use anyhow::{Context as _, Result};
use gpui::{
    AppContext, Bounds, SharedString, Size, Styled, TitlebarOptions, WindowBounds, WindowOptions,
    px,
};
use gpui_component::{ActiveTheme, Root};
use openlogi_core::binding::{self, Action, ButtonId};
use openlogi_core::config::Config;
use openlogi_core::device::{DeviceInventory, DeviceModelInfo};
use openlogi_hook::{EventDisposition, Hook, MouseEvent};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::app::AppView;
use crate::state::AppState;

fn main() -> Result<()> {
    init_tracing();

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

    // Build the shared hook binding map from the on-disk config so the hook
    // sees saved bindings from the first event, before AppState is initialised
    // inside the GPUI thread. The Arc is also handed into the AppState global
    // (see `cx.open_window` below) so that `commit_binding` writes are
    // immediately visible to the hook callback.
    let (hook_bindings, initial_config) = load_config_and_bindings(&inventories);

    // Start the OS hook. `_hook` is held alive for the duration of `run`;
    // the binding Arc is captured by the callback closure.
    let _hook = start_hook(Arc::clone(&hook_bindings));

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
                    ));
                }
                let view = cx.new(|cx| AppView::new(&inventories, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("opening the main window should not fail");
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

/// Load config from disk and build the initial hook binding map for the
/// first paired device with HID++ model info — same selection rule as
/// [`AppState::with_runtime`]. Pre-populating the `Arc` here means the hook
/// callback sees the right bindings from the very first event, well before
/// `AppState::with_runtime_shared` runs inside the GPUI thread.
fn load_config_and_bindings(inventories: &[DeviceInventory]) -> (BindingMap, Config) {
    let config = match Config::load_or_default() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "could not load config.toml; using default bindings");
            Config::default()
        }
    };

    let device_key = inventories
        .iter()
        .flat_map(|inv| inv.paired.iter())
        .find_map(|p| p.model_info.as_ref().map(DeviceModelInfo::config_key));

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

    let arc = Arc::new(RwLock::new(bindings));
    (arc, config)
}

/// Attempt to start the OS hook. Returns `None` if Accessibility is not
/// granted or on an unsupported platform — the app continues without crashing.
fn start_hook(bindings: BindingMap) -> Option<Hook> {
    if !Hook::has_accessibility() {
        warn!(
            "Accessibility not granted — events will not be captured. \
             Open System Settings → Privacy & Security → Accessibility."
        );
        return None;
    }

    let result = Hook::start(move |event| {
        match &event {
            MouseEvent::Button { id, pressed: true } => {
                let action = bindings.read().ok().and_then(|g| g.get(id).cloned());
                if let Some(ref action) = action {
                    info!(button = %id, action = action.label(), "button pressed → action matched");
                }
            }
            MouseEvent::Button { id, pressed: false } => {
                info!(button = %id, "button released");
            }
            MouseEvent::Scroll { .. } => {
                // Scroll events have no ButtonId binding; pass through silently.
            }
        }
        // P0.2 will implement Action::execute; for now everything passes through.
        EventDisposition::PassThrough
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
