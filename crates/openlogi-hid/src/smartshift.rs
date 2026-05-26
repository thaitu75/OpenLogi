//! HID++ `SmartShift Enhanced` (feature `0x2111`) — wheel ratchet ↔
//! free-spin control with sensitivity threshold.
//!
//! `hidpp 0.2` ships a typed wrapper for the original `0x2110 SmartShift`
//! at function IDs `0` / `1`. The "Enhanced" variant `0x2111` (MX Master
//! 3 / 3S / 4 and most current MX-line devices) shifts the call table by
//! one slot — `0` is a capability query, `1` is the status read, `2` is
//! the status write. Using `0x2110`'s function IDs against a `0x2111`
//! device hits the wrong functions and the device silently keeps its
//! previous state.
//!
//! Mode encoding (consistent across 0x2110 / 0x2111):
//! - `1` = free-spin (no ratchet, infinite scroll)
//! - `2` = ratchet (clicky); when paired with sensitivity 1–50 the
//!   firmware also engages auto-switch ("SmartShift active"); `0xff`
//!   means "permanent ratchet, never auto-switch".

use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    feature::{CreatableFeature, Feature},
    nibble::U4,
    protocol::v20::{self, Hidpp20Error},
};

/// SmartShift mode values understood by the firmware. `Free` = free-spin,
/// `Ratchet` = clicky / smartshift-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartShiftMode {
    Free,
    Ratchet,
}

impl SmartShiftMode {
    /// Wire byte for the `setRatchetControlMode` request.
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            SmartShiftMode::Free => 1,
            SmartShiftMode::Ratchet => 2,
        }
    }

    /// Inverse of [`Self::as_byte`]. `None` for reserved values (0 / 3 /
    /// future). Callers fall back to whatever they consider sane.
    #[must_use]
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Free),
            2 => Some(Self::Ratchet),
            _ => None,
        }
    }

    /// The opposite mode — used by [`crate::write::toggle_smartshift`].
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Free => Self::Ratchet,
            Self::Ratchet => Self::Free,
        }
    }
}

/// Snapshot returned from [`SmartShiftFeatureV0::get_status`].
#[derive(Debug, Clone, Copy)]
pub struct SmartShiftStatus {
    pub mode: SmartShiftMode,
    /// Auto-switch threshold (0–255). Higher = harder to flip into free-
    /// spin while scrolling. Logitech defaults to ~32 on the MX line.
    pub sensitivity: u8,
}

/// `SmartShift` / `0x2111` feature, version 0+.
#[derive(Clone)]
pub struct SmartShiftFeatureV0 {
    chan: Arc<HidppChannel>,
    device_index: u8,
    feature_index: u8,
}

impl CreatableFeature for SmartShiftFeatureV0 {
    const ID: u16 = 0x2111;
    const STARTING_VERSION: u8 = 0;

    fn new(chan: Arc<HidppChannel>, device_index: u8, feature_index: u8) -> Self {
        Self {
            chan,
            device_index,
            feature_index,
        }
    }
}

impl Feature for SmartShiftFeatureV0 {}

/// `0x2111` function ID for `getStatus` — returns mode + current
/// sensitivity + default sensitivity. Different from `0x2110` which uses
/// function `0` for the same purpose.
const FUNCTION_GET_STATUS: u8 = 1;
/// `0x2111` function ID for `setStatus` — accepts mode + sensitivity +
/// defaultSensitivity. `0x2110` uses function `1` here.
const FUNCTION_SET_STATUS: u8 = 2;

impl SmartShiftFeatureV0 {
    /// Read the current mode + sensitivity. Reserved mode bytes fall back
    /// to [`SmartShiftMode::Ratchet`] because that's the "safe" / clicky
    /// behaviour most users expect.
    pub async fn get_status(&self) -> Result<SmartShiftStatus, Hidpp20Error> {
        let response = self
            .chan
            .send_v20(v20::Message::Short(
                v20::MessageHeader {
                    device_index: self.device_index,
                    feature_index: self.feature_index,
                    function_id: U4::from_lo(FUNCTION_GET_STATUS),
                    software_id: self.chan.get_sw_id(),
                },
                [0x00, 0x00, 0x00],
            ))
            .await?;
        let payload = response.extend_payload();
        let mode = SmartShiftMode::from_byte(payload[0]).unwrap_or(SmartShiftMode::Ratchet);
        Ok(SmartShiftStatus {
            mode,
            sensitivity: payload[1],
        })
    }

    /// Write a new mode + sensitivity. The third payload byte is
    /// `defaultSensitivity` per the spec; we pass `0` to mean "don't
    /// change the firmware default".
    pub async fn set_status(
        &self,
        mode: SmartShiftMode,
        sensitivity: u8,
    ) -> Result<(), Hidpp20Error> {
        let _ = self
            .chan
            .send_v20(v20::Message::Short(
                v20::MessageHeader {
                    device_index: self.device_index,
                    feature_index: self.feature_index,
                    function_id: U4::from_lo(FUNCTION_SET_STATUS),
                    software_id: self.chan.get_sw_id(),
                },
                [mode.as_byte(), sensitivity, 0],
            ))
            .await?;
        Ok(())
    }
}
