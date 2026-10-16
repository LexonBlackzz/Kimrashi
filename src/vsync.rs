//! Display refresh rate for the VSync label. VSync itself is the swap chain's present mode,
//! chosen at startup in main.rs.

/// Refresh rate reported by the compositor.
#[cfg(target_os = "windows")]
pub fn refresh_hz() -> Option<f32> {
    use std::ptr::null_mut;
    use winapi::um::dwmapi::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
    unsafe {
        let mut info: DWM_TIMING_INFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        let ok = DwmGetCompositionTimingInfo(null_mut(), &mut info) == 0;
        let r = info.rateRefresh;
        (ok && r.uiDenominator != 0).then(|| r.uiNumerator as f32 / r.uiDenominator as f32)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn refresh_hz() -> Option<f32> {
    None
}
