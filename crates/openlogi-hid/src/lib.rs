//! HID++ device discovery and inspection for OpenLogi.
//!
//! Wraps the `hidpp` crate over `async-hid` as the transport. Public
//! entry points:
//!
//! - [`enumerate`] — one-shot inventory of receivers + paired devices.
//! - [`set_dpi`] — write a new sensor DPI to a connected device.

mod transport;

pub mod adjustable_dpi;
pub mod inventory;
pub mod smartshift;
pub mod write;

pub use inventory::{InventoryError, enumerate};
pub use smartshift::{SmartShiftMode, SmartShiftStatus};
pub use write::{WriteError, set_dpi, toggle_smartshift};
