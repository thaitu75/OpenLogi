//! OS-level mouse-event hook for OpenLogi.
//!
//! On macOS the hook is implemented with `CGEventTap` (the same primitive used
//! by Logi Options+ and external-reference). Linux and Windows return
//! [`HookError::Unsupported`] from [`Hook::start`] — stubs that let the
//! workspace compile on all platforms without feature-gating callers.
//!
//! # Usage
//!
//! ```no_run
//! use openlogi_hook::{Hook, MouseEvent, EventDisposition};
//!
//! if !Hook::has_accessibility() {
//!     eprintln!("grant Accessibility access first");
//!     return;
//! }
//!
//! let hook = Hook::start(|event| {
//!     println!("{event:?}");
//!     EventDisposition::PassThrough
//! }).unwrap();
//!
//! // … later, on shutdown:
//! hook.stop();
//! ```

pub use openlogi_core::binding::ButtonId;

/// An event captured at the OS layer.
#[derive(Clone, Debug)]
pub enum MouseEvent {
    /// A mouse button was pressed or released.
    Button {
        /// Which button.
        id: ButtonId,
        /// `true` = button down; `false` = button up.
        pressed: bool,
    },
    /// A scroll-wheel tick (or continuous momentum scroll).
    Scroll {
        /// Positive = right, negative = left.
        delta_x: f32,
        /// Positive = down, negative = up.
        delta_y: f32,
    },
}

/// What the hook callback wants the OS to do with the captured event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDisposition {
    /// Let the event reach its original target unchanged.
    PassThrough,
    /// Drop the event; the target application never sees it.
    Suppress,
}

/// Errors that [`Hook::start`] and related functions can produce.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// This platform has no hook implementation yet (Linux, Windows).
    #[error("mouse event hook is not supported on this platform")]
    Unsupported,
    /// macOS Accessibility permission has not been granted to this process.
    #[error(
        "macOS Accessibility permission is required to capture mouse events; \
         grant it in System Settings → Privacy & Security → Accessibility"
    )]
    AccessibilityDenied,
    /// `CGEventTapCreate` returned null, or the run loop source could not be
    /// created. The inner string carries the context.
    #[error("CGEventTap setup failed: {0}")]
    MacOsTap(String),
}

/// A running OS-level mouse hook. Call [`Hook::stop`] to tear down.
///
/// Internally on macOS, a dedicated `std::thread` runs a `CFRunLoop` that
/// drains the `CGEventTap` queue. `stop` signals that run loop and joins the
/// thread so the process exits cleanly. Dropping a `Hook` without calling
/// `stop` has the same effect via `Drop`.
pub struct Hook {
    #[cfg(target_os = "macos")]
    inner: Option<macos::HookInner>,
    /// Prevents construction outside this crate on non-macOS platforms.
    #[cfg(not(target_os = "macos"))]
    _priv: std::convert::Infallible,
}

impl Drop for Hook {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(inner) = self.inner.take() {
            macos::stop(inner);
        }
        #[cfg(not(target_os = "macos"))]
        // Unreachable: `_priv: Infallible` prevents construction.
        {}
    }
}

impl Hook {
    /// Install the mouse hook and start delivering events to `cb`.
    ///
    /// The callback runs on a private background thread (not the GPUI thread)
    /// for every mouse button or scroll event at the OS HID tap. It must
    /// return [`EventDisposition`] quickly — blocking it stalls input delivery
    /// system-wide.
    ///
    /// On macOS, returns [`HookError::AccessibilityDenied`] when the process
    /// has not been granted Accessibility permission. On Linux and Windows,
    /// returns [`HookError::Unsupported`].
    pub fn start(
        cb: impl Fn(MouseEvent) -> EventDisposition + Send + Sync + 'static,
    ) -> Result<Self, HookError> {
        #[cfg(target_os = "macos")]
        {
            macos::start(cb).map(|inner| Self { inner: Some(inner) })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cb;
            Err(HookError::Unsupported)
        }
    }

    /// Stop the hook and release OS resources.
    ///
    /// Signals the background run loop to exit and blocks until the thread
    /// joins. Calling this explicitly is preferred over relying on `Drop` when
    /// errors in cleanup should be visible. `Drop` calls this automatically.
    pub fn stop(mut self) {
        #[cfg(target_os = "macos")]
        if let Some(inner) = self.inner.take() {
            macos::stop(inner);
        }
        #[cfg(not(target_os = "macos"))]
        match self._priv {}
    }

    /// Returns `true` when the process has the macOS Accessibility entitlement
    /// required to install an active `CGEventTap`.
    ///
    /// On Linux and Windows this always returns `true`; those platforms handle
    /// permissions at a higher layer.
    #[must_use]
    pub fn has_accessibility() -> bool {
        #[cfg(target_os = "macos")]
        {
            macos::has_accessibility()
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Arc, mpsc};
    use std::thread;

    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventTapProxy, CGEventType, CallbackResult, EventField,
    };
    use tracing::{debug, error};

    use crate::{ButtonId, EventDisposition, HookError, MouseEvent};

    /// Everything `Hook` needs to control the background thread.
    pub(crate) struct HookInner {
        thread: thread::JoinHandle<()>,
        run_loop: CFRunLoop,
    }

    // SAFETY: CFRunLoop is a Core Foundation ref-counted object. The CF
    // documentation states that CFRunLoop objects can be passed between
    // threads; only CFRunLoopRun must be called on the owning thread.
    unsafe impl Send for HookInner {}

    // Raw FFI for `AXIsProcessTrustedWithOptions` from the Accessibility
    // framework. Passing `NULL` queries trust state without prompting.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
    }

    /// Check whether this process has been granted Accessibility access.
    pub(crate) fn has_accessibility() -> bool {
        // SAFETY: NULL is documented as a valid argument; it queries the current
        // trust state without raising a permission dialog.
        unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
    }

    /// Translate a raw OS button number to a [`ButtonId`].
    ///
    /// Logi's convention: button 0 = left, 1 = right, 2 = middle, 3 = back,
    /// 4 = forward. Numbers ≥5 don't map to a `ButtonId` we track.
    fn button_number_to_id(n: i64) -> Option<ButtonId> {
        match n {
            0 => Some(ButtonId::LeftClick),
            1 => Some(ButtonId::RightClick),
            2 => Some(ButtonId::MiddleClick),
            3 => Some(ButtonId::Back),
            4 => Some(ButtonId::Forward),
            _ => None,
        }
    }

    /// Convert a `CGEvent` to our [`MouseEvent`] vocabulary. Returns `None`
    /// for event types we don't translate (e.g. move events, unknown buttons).
    fn translate(etype: CGEventType, event: &CGEvent) -> Option<MouseEvent> {
        match etype {
            CGEventType::LeftMouseDown => Some(MouseEvent::Button {
                id: ButtonId::LeftClick,
                pressed: true,
            }),
            CGEventType::LeftMouseUp => Some(MouseEvent::Button {
                id: ButtonId::LeftClick,
                pressed: false,
            }),
            CGEventType::RightMouseDown => Some(MouseEvent::Button {
                id: ButtonId::RightClick,
                pressed: true,
            }),
            CGEventType::RightMouseUp => Some(MouseEvent::Button {
                id: ButtonId::RightClick,
                pressed: false,
            }),
            CGEventType::OtherMouseDown => {
                let n = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
                button_number_to_id(n).map(|id| MouseEvent::Button { id, pressed: true })
            }
            CGEventType::OtherMouseUp => {
                let n = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
                button_number_to_id(n).map(|id| MouseEvent::Button { id, pressed: false })
            }
            CGEventType::ScrollWheel => {
                // axis 1 = vertical scroll; axis 2 = horizontal scroll.
                let dy = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let dx = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "scroll deltas are small fractional values that fit comfortably in f32"
                )]
                Some(MouseEvent::Scroll {
                    delta_x: dx as f32,
                    delta_y: dy as f32,
                })
            }
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                error!(
                    "CGEventTap disabled by OS (type={etype:?}); \
                     hook will stop receiving events until re-enabled"
                );
                None
            }
            _ => None,
        }
    }

    /// Create the event tap and run loop on a dedicated thread.
    pub(crate) fn start(
        cb: impl Fn(MouseEvent) -> EventDisposition + Send + Sync + 'static,
    ) -> Result<HookInner, HookError> {
        if !has_accessibility() {
            return Err(HookError::AccessibilityDenied);
        }

        // Wrap in Arc so the closure handed to CGEventTap::new captures it by
        // clone rather than by move — avoids a second Box allocation.
        let cb: Arc<dyn Fn(MouseEvent) -> EventDisposition + Send + Sync> = Arc::new(cb);

        let (rl_tx, rl_rx) = mpsc::channel::<CFRunLoop>();

        let thread = thread::Builder::new()
            .name("openlogi-hook".into())
            .spawn(move || thread_main(cb, rl_tx))
            .map_err(|e| HookError::MacOsTap(e.to_string()))?;

        // Block until the background thread confirms the run loop is live, or
        // reports failure by dropping its sender.
        let run_loop = rl_rx.recv().map_err(|_| {
            HookError::MacOsTap(
                "background thread exited before the run loop started; \
                 CGEventTapCreate likely returned null"
                    .into(),
            )
        })?;

        Ok(HookInner { thread, run_loop })
    }

    /// Body of the background hook thread.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "rl_tx must be owned: dropping it signals the parent's recv() to return Err on failure paths"
    )]
    fn thread_main(
        cb: Arc<dyn Fn(MouseEvent) -> EventDisposition + Send + Sync>,
        rl_tx: mpsc::Sender<CFRunLoop>,
    ) {
        let event_types = vec![
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
        ];

        let tap_result = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            event_types,
            move |_proxy: CGEventTapProxy, etype: CGEventType, event: &CGEvent| {
                let Some(mouse_event) = translate(etype, event) else {
                    return CallbackResult::Keep;
                };
                match cb(mouse_event) {
                    EventDisposition::PassThrough => CallbackResult::Keep,
                    EventDisposition::Suppress => CallbackResult::Drop,
                }
            },
        );

        let Ok(tap) = tap_result else {
            error!("CGEventTapCreate returned null — Accessibility may have been revoked");
            // Dropping rl_tx causes rl_rx.recv() on the parent to return Err,
            // which we surface as MacOsTap.
            return;
        };

        let Ok(loop_source) = tap.mach_port().create_runloop_source(0) else {
            error!("CFRunLoopSourceCreate failed for event tap");
            return;
        };

        let run_loop = CFRunLoop::get_current();

        // SAFETY: kCFRunLoopCommonModes is a static CF string constant that
        // lives for the duration of the process.
        unsafe {
            run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
        }
        tap.enable();

        if rl_tx.send(run_loop).is_err() {
            debug!("hook parent dropped before run loop was ready; stopping");
            return;
        }

        // Blocks until `CFRunLoop::stop()` is called from another thread.
        CFRunLoop::run_current();
    }

    /// Signal the run loop to stop and join the background thread.
    pub(crate) fn stop(inner: HookInner) {
        inner.run_loop.stop();
        if let Err(e) = inner.thread.join() {
            error!("hook thread panicked on shutdown: {e:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// All `HookError` variants produce non-empty display messages.
    #[test]
    fn hook_error_display() {
        let errors: &[HookError] = &[
            HookError::Unsupported,
            HookError::AccessibilityDenied,
            HookError::MacOsTap("test reason".into()),
        ];
        for e in errors {
            assert!(!e.to_string().is_empty(), "empty display for {e:?}");
        }
    }

    /// `MouseEvent` is `Clone + Debug` — both variants exercise without panic.
    #[test]
    fn mouse_event_clone_and_debug() {
        let events = [
            MouseEvent::Button {
                id: ButtonId::Back,
                pressed: true,
            },
            MouseEvent::Scroll {
                delta_x: 1.0,
                delta_y: -1.5,
            },
        ];
        for e in &events {
            let cloned = e.clone();
            let _ = format!("{e:?}");
            let _ = format!("{cloned:?}");
        }
    }

    /// `EventDisposition` implements `PartialEq` correctly.
    #[test]
    fn event_disposition_equality() {
        assert_eq!(EventDisposition::PassThrough, EventDisposition::PassThrough);
        assert_eq!(EventDisposition::Suppress, EventDisposition::Suppress);
        assert_ne!(EventDisposition::PassThrough, EventDisposition::Suppress);
    }

    /// On non-macOS targets, `Hook::start` returns `Unsupported`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_start_returns_unsupported() {
        let result = Hook::start(|_| EventDisposition::PassThrough);
        assert!(matches!(result, Err(HookError::Unsupported)));
    }

    /// On non-macOS targets, `Hook::has_accessibility` is always `true`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_has_accessibility_is_true() {
        assert!(Hook::has_accessibility());
    }
}
