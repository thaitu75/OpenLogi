//! Best-effort host OS version string for the diagnostics report.

/// The OS product version (e.g. `"15.5"` on macOS), or `None` when unavailable.
#[must_use]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Option is the cross-platform contract; non-macOS arms return None"
)]
pub fn os_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        Some(if v.patchVersion == 0 {
            format!("{}.{}", v.majorVersion, v.minorVersion)
        } else {
            format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
