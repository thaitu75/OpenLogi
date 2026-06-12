//! Implements the different HID++ wireless receivers.
//!
//! Because of the lack of public documentation about the different receivers
//! and their capabilities, and because I currently only own a single Bolt
//! receiver, this module is largely incomplete. I would be more than happy for
//! anyone who owns a different receiver, with Unifying having the highest
//! priority, and who is willing to actively support its implementation by
//! providing information and testing.
//!
//! Receivers can generally only be differentiated by their USB vendor and
//! product IDs, so the [`detect`] function does nothing more than matching
//! those values to the sets of known vendor and product ID pairs of the
//! different receivers.

use std::sync::Arc;

use thiserror::Error;

use crate::{channel::HidppChannel, protocol::v10::Hidpp10Error};

/// Removes a HID++ message listener when the last receiver clone is dropped.
///
/// Storing this inside an `Arc` on the receiver struct prevents the
/// `#[derive(Clone)]` copy from sharing the raw `u32` handle: cloning a
/// receiver increments the Arc refcount, and `remove_msg_listener` is called
/// exactly once — when the last clone's Arc refcount reaches zero.
pub(super) struct ListenerDropGuard {
    pub(super) chan: Arc<HidppChannel>,
    pub(super) hdl: u32,
}

impl Drop for ListenerDropGuard {
    fn drop(&mut self) {
        self.chan.remove_msg_listener(self.hdl);
    }
}

pub mod bolt;
pub mod unifying;

/// The index to use when communicating with the receiver on any HID++ channel.
pub const RECEIVER_DEVICE_INDEX: u8 = 0xff;

/// Tries to detect the receiver present on a HID++ channel.
pub fn detect(chan: Arc<HidppChannel>) -> Option<Receiver> {
    let vpid_pair = &(chan.vendor_id, chan.product_id);

    if bolt::VPID_PAIRS.contains(vpid_pair) {
        return bolt::Receiver::new(chan).ok().map(Receiver::Bolt);
    }

    if unifying::VPID_PAIRS.contains(vpid_pair) {
        return unifying::Receiver::new(chan).ok().map(Receiver::Unifying);
    }
    None
}

/// Represents a HID++ wireless receiver.
#[derive(Clone)]
#[non_exhaustive]
pub enum Receiver {
    Bolt(bolt::Receiver),
    Unifying(unifying::Receiver),
}

impl Receiver {
    /// Provides a human-readable name for the receiver.
    pub fn name(&self) -> String {
        match self {
            Self::Bolt(_) => "Logi Bolt Receiver",
            Self::Unifying(_) => "Unifying Receiver",
        }
        .to_string()
    }

    /// Provides a string that uniquely identifies the specific receiver.
    ///
    /// This MAY be the serial number, but it may also be any other value that
    /// is defined as unique.
    pub async fn get_unique_id(&self) -> Result<String, ReceiverError> {
        match self {
            Self::Bolt(bolt) => bolt.get_unique_id().await,
            Self::Unifying(unifying) => unifying.get_unique_id().await,
        }
    }
}

/// Represents an error returned by a receiver.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReceiverError {
    /// Indicates that no supported receiver could be identified on a HID++
    /// channel.
    #[error("no (supported) receiver could be found")]
    UnknownReceiver,

    /// Indicates that a HID++1.0 register access resulted in an error.
    #[error("a HID++1.0 error occurred")]
    Protocol(#[from] Hidpp10Error),
}
