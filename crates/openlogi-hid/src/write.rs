//! HID++ writes back to the device — currently just sensor DPI.
//!
//! Mirrors [`crate::inventory`]'s channel-opening dance, scoped to a
//! single receiver + slot. Each call re-enumerates and re-opens — fine
//! at the frequency this is invoked (once per slider release).

use std::sync::Arc;

use async_hid::HidBackend;
use futures_lite::StreamExt as _;
use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::CreatableFeature,
    receiver::{self, Receiver},
};
use thiserror::Error;
use tracing::debug;

use crate::adjustable_dpi::AdjustableDpiFeatureV0;
use crate::smartshift::{SmartShiftFeatureV0, SmartShiftMode, SmartShiftStatus};
use crate::transport::AsyncHidChannel;

/// Logitech HID vendor ID — kept in sync with [`crate::inventory`].
const LOGITECH_VID: u16 = 0x046d;
const HIDPP_USAGE_PAGE: u16 = 0xff00;
const HIDPP_LONG_USAGE_ID: u16 = 0x0002;

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("HID transport error")]
    Hid(#[from] async_hid::HidError),
    #[error("no matching Bolt receiver found")]
    ReceiverNotFound,
    #[error("device on slot {slot} did not respond to HID++")]
    DeviceUnreachable { slot: u8 },
    #[error("device does not expose HID++ feature {feature_hex:#06x}")]
    FeatureUnsupported { feature_hex: u16 },
    #[error("HID++ protocol error: {0}")]
    Hidpp(String),
}

/// Push a new DPI value to the sensor on `slot` of the receiver
/// identified by `receiver_uid`. Pass `None` to target the first Bolt
/// receiver found.
///
/// Re-enumerates each call — opening a HID++ channel is cheap enough
/// at slider-release cadence, and avoids the complexity of holding a
/// long-lived session over GPUI's runtime.
/// Snapshot of one HID++ feature exposed by a device: protocol ID +
/// version. Returned by [`dump_features`] for diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct FeatureEntry {
    pub id: u16,
    pub version: u8,
}

/// Enumerate every HID++ feature the device at `slot` reports — used by
/// `openlogi diag features` to confirm which DPI / SmartShift / etc.
/// feature IDs a given peripheral actually exposes (e.g. some mice use
/// `0x2202 ExtendedAdjustableDpi` instead of `0x2201 AdjustableDpi`).
pub async fn dump_features(
    receiver_uid: Option<&str>,
    slot: u8,
) -> Result<Vec<FeatureEntry>, WriteError> {
    use hidpp::feature::feature_set::v0::FeatureSetFeatureV0;
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        // The root feature exposes the FeatureSet (0x0001) at a fixed
        // address; we look it up directly rather than going through
        // `enumerate_features` so the iteration is observable.
        let feature_set_info = device
            .root()
            .get_feature(FeatureSetFeatureV0::ID)
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?
            .ok_or(WriteError::FeatureUnsupported {
                feature_hex: FeatureSetFeatureV0::ID,
            })?;
        let feature_set = device.add_feature::<FeatureSetFeatureV0>(feature_set_info.index);
        let count = feature_set
            .count()
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
        let mut entries = Vec::with_capacity(usize::from(count));
        for i in 0..=count {
            let info = feature_set
                .get_feature(i)
                .await
                .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
            entries.push(FeatureEntry {
                id: info.id,
                version: info.version,
            });
        }
        Ok(entries)
    })
    .await
}

/// Look up `F` on a device by HID++ feature ID, register it with
/// [`Device::add_feature`], and return the typed wrapper.
///
/// We bypass [`Device::enumerate_features`] because hidpp 0.2's central
/// registry has `versions: &[]` for the features OpenLogi cares about
/// (`0x2201 AdjustableDpi`, `0x2202 ExtendedAdjustableDpi`). Calling
/// `enumerate_features` ends up _not_ registering them, so a subsequent
/// `device.get_feature::<F>()` looking up our own TypeId returns `None`
/// even when the device announces the feature ID. The direct lookup via
/// `root().get_feature(id)` returns the assigned index unconditionally;
/// `add_feature` then attaches our wrapper to that index.
async fn open_feature<F: CreatableFeature + 'static>(
    device: &mut Device,
    _slot: u8,
) -> Result<Arc<F>, WriteError> {
    let info = device
        .root()
        .get_feature(F::ID)
        .await
        .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?
        .ok_or(WriteError::FeatureUnsupported { feature_hex: F::ID })?;
    Ok(device.add_feature::<F>(info.index))
}

/// Read the device's current DPI on sensor 0 — companion to [`set_dpi`].
/// Used by `openlogi diag dpi` and any future Settings → Diagnostics
/// surface that wants to display the current value without writing.
pub async fn get_dpi(receiver_uid: Option<&str>, slot: u8) -> Result<u16, WriteError> {
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature = open_feature::<AdjustableDpiFeatureV0>(&mut device, slot).await?;
        feature
            .get_sensor_dpi(0)
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))
    })
    .await
}

/// Read the device's current SmartShift mode + sensitivity — companion to
/// [`toggle_smartshift`].
pub async fn get_smartshift_status(
    receiver_uid: Option<&str>,
    slot: u8,
) -> Result<SmartShiftStatus, WriteError> {
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature = open_feature::<SmartShiftFeatureV0>(&mut device, slot).await?;
        feature
            .get_status()
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))
    })
    .await
}

pub async fn set_dpi(receiver_uid: Option<&str>, slot: u8, dpi: u16) -> Result<(), WriteError> {
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature = open_feature::<AdjustableDpiFeatureV0>(&mut device, slot).await?;
        feature
            .set_sensor_dpi(0, dpi)
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
        // PLAN.md "outstanding minor items": read back to confirm the
        // firmware accepted the value. A mismatch is a silent failure
        // mode that's otherwise invisible — devices in low-power states
        // or with unsupported DPI ranges can ACK the write yet keep the
        // old value. We log a warning but still return Ok because the
        // request reached the device.
        if let Ok(actual) = feature.get_sensor_dpi(0).await {
            if actual == dpi {
                debug!(slot, dpi, "wrote DPI (verified)");
            } else {
                tracing::warn!(
                    slot,
                    requested = dpi,
                    actual,
                    "DPI write accepted but device reports a different value — \
                     likely out of the device's supported range"
                );
            }
        } else {
            debug!(slot, dpi, "wrote DPI (read-back skipped)");
        }
        Ok(())
    })
    .await
}

/// Toggle SmartShift mode (free ↔ ratchet) on `slot`. Reads the current
/// mode first, then writes the opposite — keeps current sensitivity.
/// Returns the new mode written.
///
/// `FeatureUnsupported` when the device doesn't expose HID++ `0x2111`
/// (older Logi mice and most non-MX devices).
pub async fn toggle_smartshift(
    receiver_uid: Option<&str>,
    slot: u8,
) -> Result<SmartShiftMode, WriteError> {
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature = open_feature::<SmartShiftFeatureV0>(&mut device, slot).await?;
        let SmartShiftStatus { mode, sensitivity } = feature
            .get_status()
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
        let next = mode.flipped();
        feature
            .set_status(next, sensitivity)
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
        debug!(slot, ?next, "wrote SmartShift mode");
        Ok(next)
    })
    .await
}

/// Boilerplate-eater: enumerate HID candidates, find a matching Bolt
/// receiver, run `f` once with the opened HID++ channel.
async fn with_device<F, Fut, T>(
    receiver_uid: Option<&str>,
    _slot: u8,
    f: F,
) -> Result<T, WriteError>
where
    F: FnOnce(Arc<HidppChannel>) -> Fut,
    Fut: std::future::Future<Output = Result<T, WriteError>>,
{
    let backend = HidBackend::default();
    let candidates: Vec<async_hid::Device> = backend
        .enumerate()
        .await?
        .filter(|d| {
            d.vendor_id == LOGITECH_VID
                && d.usage_page == HIDPP_USAGE_PAGE
                && d.usage_id == HIDPP_LONG_USAGE_ID
        })
        .collect()
        .await;

    for dev in candidates {
        let info: async_hid::DeviceInfo = (*dev).clone();
        let (reader, writer) = dev.open().await?;
        let raw = AsyncHidChannel::new(reader, writer, info.clone());
        let channel = match HidppChannel::from_raw_channel(raw).await {
            Ok(c) => Arc::new(c),
            Err(e) => {
                debug!(name = %info.name, error = ?e, "not a HID++ channel");
                continue;
            }
        };
        let Some(Receiver::Bolt(bolt)) = receiver::detect(Arc::clone(&channel)) else {
            continue;
        };

        if let Some(want) = receiver_uid {
            match bolt.get_unique_id().await {
                Ok(uid) if uid.eq_ignore_ascii_case(want) => {}
                _ => continue,
            }
        }

        return f(channel).await;
    }

    Err(WriteError::ReceiverNotFound)
}
