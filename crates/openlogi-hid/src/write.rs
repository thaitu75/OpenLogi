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
pub async fn set_dpi(receiver_uid: Option<&str>, slot: u8, dpi: u16) -> Result<(), WriteError> {
    with_device(receiver_uid, slot, |channel| async move {
        let mut device = Device::new(Arc::clone(&channel), slot)
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        device
            .enumerate_features()
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature = device.get_feature::<AdjustableDpiFeatureV0>().ok_or(
            WriteError::FeatureUnsupported {
                feature_hex: AdjustableDpiFeatureV0::ID,
            },
        )?;
        feature
            .set_sensor_dpi(0, dpi)
            .await
            .map_err(|e| WriteError::Hidpp(format!("{e:?}")))?;
        debug!(slot, dpi, "wrote DPI");
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
        device
            .enumerate_features()
            .await
            .map_err(|_| WriteError::DeviceUnreachable { slot })?;
        let feature =
            device
                .get_feature::<SmartShiftFeatureV0>()
                .ok_or(WriteError::FeatureUnsupported {
                    feature_hex: SmartShiftFeatureV0::ID,
                })?;
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
