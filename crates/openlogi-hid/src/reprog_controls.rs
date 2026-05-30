//! HID++ `ReprogControlsV4` (feature `0x1b04`) — temporary control diversion
//! and raw-XY reporting, the mechanism behind the MX-line thumb "gesture
//! button".
//!
//! `hidpp 0.2` ships no typed wrapper, so we re-implement the few functions
//! OpenLogi needs: `getCount` / `getCtrlIdInfo` (locate the gesture control and
//! confirm it can divert raw XY) and `setCidReporting` (turn diversion on or
//! off). While a control is diverted with raw-XY reporting, the device emits
//! two unsolicited events, decoded by [`decode_event`]:
//!
//! - function `0` `divertedButtonsEvent` — up to four currently-pressed CIDs.
//! - function `1` `rawXYEvent` — signed `dx`/`dy` while a raw-XY control is held.
//!
//! Wire formats cross-checked against Solaar's `hidpp20.py` and
//! `notifications.py`.

use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    nibble::U4,
    protocol::v20::{self, Hidpp20Error},
};

/// `ReprogControlsV4` HID++ feature ID.
pub const FEATURE_ID: u16 = 0x1b04;

/// Control ID of the MX-line thumb gesture button (`Mouse_Gesture_Button`,
/// Logitech "App_Switch_Gesture"). Cross-checked against Solaar
/// `special_keys.py`.
pub const GESTURE_BUTTON_CID: u16 = 0x00c3;

/// `getCount` function ID.
const FN_GET_COUNT: u8 = 0;
/// `getCtrlIdInfo` function ID.
const FN_GET_CTRL_ID_INFO: u8 = 1;
/// `setCidReporting` function ID.
const FN_SET_CID_REPORTING: u8 = 3;

/// `MappingFlag::DIVERTED` — the control reports presses as HID++ events.
const MAPPING_DIVERTED: u8 = 0x01;
/// `MappingFlag::RAW_XY_DIVERTED` — the control reports raw XY while held.
const MAPPING_RAW_XY_DIVERTED: u8 = 0x10;

/// `KeyFlag::DIVERTABLE` capability bit (from `getCtrlIdInfo`).
const KEYFLAG_DIVERTABLE: u16 = 0x20;
/// `KeyFlag::RAW_XY` capability bit (from `getCtrlIdInfo`).
const KEYFLAG_RAW_XY: u16 = 0x100;

/// Identity and capabilities of one reprogrammable control, as returned by
/// `getCtrlIdInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtrlIdInfo {
    /// Control ID — stable across firmware (e.g. [`GESTURE_BUTTON_CID`]).
    pub cid: u16,
    /// Task ID — the control's default on-device action.
    pub task_id: u16,
    /// `KeyFlag` capability bitfield (response bytes 4 and 8 combined).
    pub flags: u16,
}

impl CtrlIdInfo {
    /// Whether the control can be temporarily diverted to HID++ events.
    #[must_use]
    pub fn is_divertable(self) -> bool {
        self.flags & KEYFLAG_DIVERTABLE != 0
    }

    /// Whether the control can report raw XY movement while held — required to
    /// decode a swipe into a direction.
    #[must_use]
    pub fn supports_raw_xy(self) -> bool {
        self.flags & KEYFLAG_RAW_XY != 0
    }
}

/// An unsolicited `0x1b04` event decoded from a channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawControlEvent {
    /// `divertedButtonsEvent`: the (up to four) CIDs currently held down. A
    /// slot is `0` when fewer than four are pressed; an all-zero array means
    /// every diverted control was released.
    DivertedButtons([u16; 4]),
    /// `rawXYEvent`: signed movement deltas reported while a raw-XY control is
    /// held.
    RawXy {
        /// Horizontal delta (`+` = right, in the device's raw units).
        dx: i16,
        /// Vertical delta (`+` = down, in the device's raw units).
        dy: i16,
    },
}

impl RawControlEvent {
    /// Whether `cid` is among the controls reported as currently pressed.
    #[must_use]
    pub fn is_pressed(&self, cid: u16) -> bool {
        matches!(self, RawControlEvent::DivertedButtons(cids) if cids.contains(&cid))
    }
}

/// Decode a channel message into a [`RawControlEvent`] when it is an unsolicited
/// `0x1b04` event for `(device_index, feature_index)`.
///
/// Returns `None` for request responses (`software_id != 0`), messages from a
/// different device or feature, and unrecognised event functions.
#[must_use]
pub fn decode_event(
    msg: &v20::Message,
    device_index: u8,
    feature_index: u8,
) -> Option<RawControlEvent> {
    let header = msg.header();
    if header.device_index != device_index
        || header.feature_index != feature_index
        || header.software_id.to_lo() != 0
    {
        return None;
    }
    let p = msg.extend_payload();
    match header.function_id.to_lo() {
        0 => Some(RawControlEvent::DivertedButtons([
            u16::from_be_bytes([p[0], p[1]]),
            u16::from_be_bytes([p[2], p[3]]),
            u16::from_be_bytes([p[4], p[5]]),
            u16::from_be_bytes([p[6], p[7]]),
        ])),
        1 => Some(RawControlEvent::RawXy {
            dx: i16::from_be_bytes([p[0], p[1]]),
            dy: i16::from_be_bytes([p[2], p[3]]),
        }),
        _ => None,
    }
}

/// Build the `setCidReporting` change bitfield.
///
/// Each touched flag sets its value bit to the requested state plus the
/// adjacent "valid"/change bit, so the device updates only the flags we name
/// and leaves the rest intact (Solaar `_setCidReporting`). Enabling both
/// diversion and raw-XY yields `0x33`; clearing both yields `0x22`.
#[must_use]
fn reporting_bfield(diverted: bool, raw_xy: bool) -> u8 {
    let mut bfield = 0u8;
    for (flag, on) in [
        (MAPPING_DIVERTED, diverted),
        (MAPPING_RAW_XY_DIVERTED, raw_xy),
    ] {
        if on {
            bfield |= flag;
        }
        bfield |= flag << 1;
    }
    bfield
}

/// `ReprogControlsV4` accessor bound to one device + resolved feature index.
///
/// Construct with the feature index obtained from the device's root feature
/// (`get_feature(`[`FEATURE_ID`]`)`), then call the functions below. Cheap to
/// clone (an `Arc` plus two indices).
#[derive(Clone)]
pub struct ReprogControlsV4 {
    chan: Arc<HidppChannel>,
    device_index: u8,
    feature_index: u8,
}

impl ReprogControlsV4 {
    /// Bind the feature to `(device_index, feature_index)` on `chan`.
    #[must_use]
    pub fn new(chan: Arc<HidppChannel>, device_index: u8, feature_index: u8) -> Self {
        Self {
            chan,
            device_index,
            feature_index,
        }
    }

    /// The feature index this accessor talks to — used to match unsolicited
    /// events in [`decode_event`].
    #[must_use]
    pub fn feature_index(&self) -> u8 {
        self.feature_index
    }

    /// The device index this accessor talks to.
    #[must_use]
    pub fn device_index(&self) -> u8 {
        self.device_index
    }

    /// Send a feature function call carrying a full long-message payload.
    async fn call(&self, function_id: u8, params: [u8; 16]) -> Result<[u8; 16], Hidpp20Error> {
        let response = self
            .chan
            .send_v20(v20::Message::Long(
                v20::MessageHeader {
                    device_index: self.device_index,
                    feature_index: self.feature_index,
                    function_id: U4::from_lo(function_id),
                    software_id: self.chan.get_sw_id(),
                },
                params,
            ))
            .await?;
        Ok(response.extend_payload())
    }

    /// Number of reprogrammable controls the device exposes.
    pub async fn get_count(&self) -> Result<u8, Hidpp20Error> {
        Ok(self.call(FN_GET_COUNT, [0; 16]).await?[0])
    }

    /// Identity + capabilities of the control at `index` (`0..get_count`).
    pub async fn get_ctrl_id_info(&self, index: u8) -> Result<CtrlIdInfo, Hidpp20Error> {
        let mut params = [0u8; 16];
        params[0] = index;
        let p = self.call(FN_GET_CTRL_ID_INFO, params).await?;
        Ok(CtrlIdInfo {
            cid: u16::from_be_bytes([p[0], p[1]]),
            task_id: u16::from_be_bytes([p[2], p[3]]),
            flags: u16::from(p[4]) | (u16::from(p[8]) << 8),
        })
    }

    /// Scan the control table for the control with `cid`. `None` if the device
    /// doesn't expose it.
    pub async fn find_control(&self, cid: u16) -> Result<Option<CtrlIdInfo>, Hidpp20Error> {
        let count = self.get_count().await?;
        for index in 0..count {
            let info = self.get_ctrl_id_info(index).await?;
            if info.cid == cid {
                return Ok(Some(info));
            }
        }
        Ok(None)
    }

    /// Set (or clear) temporary diversion and raw-XY reporting for `cid`.
    ///
    /// `remap` is left at `0` (keep the control's current mapping). After
    /// enabling, the device emits [`RawControlEvent`]s on this feature index;
    /// clear both flags on teardown to hand the control back to the firmware.
    pub async fn set_cid_reporting(
        &self,
        cid: u16,
        diverted: bool,
        raw_xy: bool,
    ) -> Result<(), Hidpp20Error> {
        let [cid_hi, cid_lo] = cid.to_be_bytes();
        let mut params = [0u8; 16];
        params[0] = cid_hi;
        params[1] = cid_lo;
        params[2] = reporting_bfield(diverted, raw_xy);
        self.call(FN_SET_CID_REPORTING, params).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bfield_enable_and_clear() {
        // DIVERTED value 0x01 + valid 0x02, RAW_XY value 0x10 + valid 0x20.
        assert_eq!(reporting_bfield(true, true), 0x33);
        assert_eq!(reporting_bfield(false, false), 0x22);
        assert_eq!(reporting_bfield(true, false), 0x23);
        assert_eq!(reporting_bfield(false, true), 0x32);
    }

    fn event(function_id: u8, software_id: u8, payload: [u8; 16]) -> v20::Message {
        v20::Message::Long(
            v20::MessageHeader {
                device_index: 2,
                feature_index: 7,
                function_id: U4::from_lo(function_id),
                software_id: U4::from_lo(software_id),
            },
            payload,
        )
    }

    #[test]
    fn decodes_diverted_buttons() {
        let mut p = [0u8; 16];
        p[0..2].copy_from_slice(&GESTURE_BUTTON_CID.to_be_bytes());
        let decoded = decode_event(&event(0, 0, p), 2, 7);
        assert_eq!(
            decoded,
            Some(RawControlEvent::DivertedButtons([
                GESTURE_BUTTON_CID,
                0,
                0,
                0
            ]))
        );
        assert!(decoded.is_some_and(|e| e.is_pressed(GESTURE_BUTTON_CID)));
    }

    #[test]
    fn decodes_signed_raw_xy() {
        let mut p = [0u8; 16];
        p[0..2].copy_from_slice(&(-5i16).to_be_bytes());
        p[2..4].copy_from_slice(&12i16.to_be_bytes());
        assert_eq!(
            decode_event(&event(1, 0, p), 2, 7),
            Some(RawControlEvent::RawXy { dx: -5, dy: 12 })
        );
    }

    #[test]
    fn ignores_responses_and_foreign_messages() {
        let p = [0u8; 16];
        // software_id != 0 marks a request response, not an event.
        assert_eq!(decode_event(&event(0, 5, p), 2, 7), None);
        // Right device + feature, but an unknown event function.
        assert_eq!(decode_event(&event(2, 0, p), 2, 7), None);
        // Wrong feature index.
        assert_eq!(decode_event(&event(0, 0, p), 2, 9), None);
    }
}
