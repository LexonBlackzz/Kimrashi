//! In-app VSync. The swap chain presents uncapped (so VSync can be toggled at runtime),
//! and each frame waits for the vertical blank of the monitor the window is on.

#[cfg(target_os = "windows")]
pub use windows::VBlank;

#[cfg(target_os = "windows")]
mod windows {
    use std::ptr::null_mut;
    use winapi::shared::dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC,
    };
    use winapi::shared::windef::RECT;
    use winapi::um::dwmapi::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
    use winapi::Interface;

    pub struct VBlank {
        /// Desktop rect and output of every attached monitor.
        outputs: Vec<(RECT, *mut IDXGIOutput)>,
    }

    impl VBlank {
        pub fn new() -> Self {
            Self {
                outputs: unsafe { enum_outputs() },
            }
        }

        /// Block until the next vertical blank of the monitor containing (x, y)
        /// (physical desktop pixels). Falls back to the first monitor.
        pub fn wait(&mut self, x: i32, y: i32) {
            let contains = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
            let output = self
                .outputs
                .iter()
                .find(|(r, _)| contains(r))
                .or(self.outputs.first())
                .map(|&(_, o)| o);
            let ok = output.is_some_and(|o| unsafe { (*o).WaitForVBlank() } >= 0);
            if !ok {
                // Monitors changed (or none found); re-enumerate for next frame.
                *self = Self::new();
            }
        }

        /// Refresh rate reported by the compositor.
        pub fn refresh_hz() -> Option<f32> {
            unsafe {
                let mut info: DWM_TIMING_INFO = std::mem::zeroed();
                info.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
                let ok = DwmGetCompositionTimingInfo(null_mut(), &mut info) == 0;
                let r = info.rateRefresh;
                (ok && r.uiDenominator != 0).then(|| r.uiNumerator as f32 / r.uiDenominator as f32)
            }
        }
    }

    impl Drop for VBlank {
        fn drop(&mut self) {
            for &(_, o) in &self.outputs {
                unsafe { (*o).Release() };
            }
        }
    }

    unsafe fn enum_outputs() -> Vec<(RECT, *mut IDXGIOutput)> {
        let mut outputs = Vec::new();
        let mut factory: *mut IDXGIFactory1 = null_mut();
        if CreateDXGIFactory1(&IDXGIFactory1::uuidof(), &mut factory as *mut _ as *mut _) < 0 {
            return outputs;
        }
        let mut a = 0;
        loop {
            let mut adapter: *mut IDXGIAdapter1 = null_mut();
            if (*factory).EnumAdapters1(a, &mut adapter) < 0 {
                break;
            }
            let mut o = 0;
            loop {
                let mut output: *mut IDXGIOutput = null_mut();
                if (*adapter).EnumOutputs(o, &mut output) < 0 {
                    break;
                }
                let mut desc: DXGI_OUTPUT_DESC = std::mem::zeroed();
                if (*output).GetDesc(&mut desc) >= 0 && desc.AttachedToDesktop != 0 {
                    outputs.push((desc.DesktopCoordinates, output));
                } else {
                    (*output).Release();
                }
                o += 1;
            }
            (*adapter).Release();
            a += 1;
        }
        (*factory).Release();
        outputs
    }
}

#[cfg(not(target_os = "windows"))]
pub struct VBlank;

#[cfg(not(target_os = "windows"))]
impl VBlank {
    pub fn new() -> Self {
        Self
    }
    pub fn wait(&mut self, _x: i32, _y: i32) {}
    pub fn refresh_hz() -> Option<f32> {
        None
    }
}
