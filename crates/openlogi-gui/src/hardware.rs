//! Hardware-side actions invoked from both the GPUI thread (slider release)
//! and the OS-event hook thread (bound button press).
//!
//! Each call spawns a one-shot tokio runtime on a dedicated OS thread —
//! cheap at the cadence these fire at (≤ once per slider release / button
//! press) and avoids holding a long-lived async runtime alongside GPUI's
//! executor.

use tracing::{debug, warn};

use crate::components::dpi_panel::DpiTarget;

/// Spawn an OS thread that toggles SmartShift (free ↔ ratchet) on the
/// device at `target` via `openlogi_hid::toggle_smartshift`. Returns
/// immediately; failures (incl. devices that don't support `0x2111`) are
/// logged.
pub fn toggle_smartshift_in_background(target: Option<DpiTarget>) {
    let Some(target) = target else {
        debug!("no target device — SmartShift toggle skipped");
        return;
    };
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "tokio runtime init failed; SmartShift toggle skipped");
                return;
            }
        };
        let result = rt.block_on(openlogi_hid::toggle_smartshift(
            Some(&target.receiver_uid),
            target.slot,
        ));
        match result {
            Ok(mode) => debug!(slot = target.slot, ?mode, "SmartShift toggled"),
            Err(e) => warn!(error = ?e, "SmartShift toggle failed"),
        }
    });
}

/// Spawn an OS thread that writes `dpi` to the device at `target` via
/// `openlogi_hid::set_dpi`. Returns immediately; failures are logged.
///
/// `target == None` is a no-op (dev environment without a real device).
pub fn write_dpi_in_background(target: Option<DpiTarget>, dpi: u32) {
    let Some(target) = target else {
        debug!(dpi, "no target device — DPI write skipped");
        return;
    };
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "tokio runtime init failed; DPI write skipped");
                return;
            }
        };
        // DPI values are clamped to <= 6400 by every caller, so the cast is
        // lossless. The saturating fallback exists only for type-system
        // exhaustiveness.
        let dpi_u16 = u16::try_from(dpi).unwrap_or(u16::MAX);
        let result = rt.block_on(openlogi_hid::set_dpi(
            Some(&target.receiver_uid),
            target.slot,
            dpi_u16,
        ));
        match result {
            Ok(()) => debug!(slot = target.slot, dpi = dpi_u16, "DPI written to device"),
            Err(e) => warn!(error = ?e, "DPI write failed"),
        }
    });
}
