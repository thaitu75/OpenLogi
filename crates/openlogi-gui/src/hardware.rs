//! Hardware-side actions invoked from both the GPUI thread (slider release)
//! and the OS-event hook thread (bound button press).
//!
//! Each call spawns a one-shot tokio runtime on a dedicated OS thread —
//! cheap at the cadence these fire at (≤ once per slider release / button
//! press) and avoids holding a long-lived async runtime alongside GPUI's
//! executor.
//!
//! When the HID++ capture session already has the target device open, these
//! reuse that channel ([`openlogi_hid::CaptureChannel`]) instead of
//! re-enumerating and opening a fresh one — the dominant cost of a write. The
//! transient open is kept as a fallback for callers (e.g. the CGEventTap hook)
//! firing while no session is connected.

use openlogi_hid::{CaptureChannel, SharedChannel};
use tracing::{debug, warn};

/// Clone out the capture session's channel when it points at `target`. `None`
/// when no slot is supplied (e.g. the DPI slider, where a transient open is
/// fine) or the open channel targets a different device.
fn reusable_channel(capture: Option<&CaptureChannel>, target: &DpiTarget) -> Option<SharedChannel> {
    capture?
        .read()
        .ok()
        .and_then(|slot| (*slot).clone())
        .filter(|chan| chan.matches(Some(&target.receiver_uid), target.slot))
}

/// Identifies which physical device hardware-side writes should target.
/// `receiver_uid` is the Bolt receiver's unique id (so writes route
/// correctly when more than one receiver is plugged in); `slot` is the
/// device's pairing slot on that receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpiTarget {
    pub receiver_uid: String,
    pub slot: u8,
}

/// Spawn an OS thread that toggles SmartShift (free ↔ ratchet) on the
/// device at `target` via `openlogi_hid::toggle_smartshift`. Returns
/// immediately; failures (incl. devices that don't support `0x2111`) are
/// logged.
pub fn toggle_smartshift_in_background(
    capture: Option<&CaptureChannel>,
    target: Option<DpiTarget>,
) {
    let Some(target) = target else {
        debug!("no target device — SmartShift toggle skipped");
        return;
    };
    let shared = reusable_channel(capture, &target);
    let reused = shared.is_some();
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
        let result = rt.block_on(async {
            match &shared {
                Some(shared) => openlogi_hid::toggle_smartshift_on(shared).await,
                None => {
                    openlogi_hid::toggle_smartshift(Some(&target.receiver_uid), target.slot).await
                }
            }
        });
        match result {
            Ok(mode) => debug!(slot = target.slot, ?mode, reused, "SmartShift toggled"),
            Err(e) => warn!(error = ?e, "SmartShift toggle failed"),
        }
    });
}

/// Spawn an OS thread that writes `dpi` to the device at `target` via
/// `openlogi_hid::set_dpi`. Returns immediately; failures are logged.
///
/// `target == None` is a no-op (dev environment without a real device).
pub fn write_dpi_in_background(
    capture: Option<&CaptureChannel>,
    target: Option<DpiTarget>,
    dpi: u32,
) {
    let Some(target) = target else {
        debug!(dpi, "no target device — DPI write skipped");
        return;
    };
    let shared = reusable_channel(capture, &target);
    let reused = shared.is_some();
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
        let result = rt.block_on(async {
            match &shared {
                Some(shared) => openlogi_hid::set_dpi_on(shared, dpi_u16).await,
                None => {
                    openlogi_hid::set_dpi(Some(&target.receiver_uid), target.slot, dpi_u16).await
                }
            }
        });
        match result {
            Ok(()) => debug!(
                slot = target.slot,
                dpi = dpi_u16,
                reused,
                "DPI written to device"
            ),
            Err(e) => warn!(error = ?e, "DPI write failed"),
        }
    });
}
