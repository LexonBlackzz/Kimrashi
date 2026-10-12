#![allow(non_camel_case_types)]

use libloading::Library;
use std::path::Path;

pub type BOOL = i32;
pub type DWORD = u32;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
#[allow(non_snake_case)] // KDMAPI's C ABI names
pub struct DebugInfo {
    pub RenderingTime: f32,
    pub ActiveVoices: [u32; 16],
    pub ASIOInputLatency: f64,
    pub ASIOOutputLatency: f64,
    pub HealthThreadTime: f64,
    pub ATThreadTime: f64,
    pub EPThreadTime: f64,
    pub CookedThreadTime: f64,
    pub CurrentSFList: u32,
    pub AudioLatency: f64,
    pub AudioBufferSize: u32,
}

type Fn_IsKDMAPIAvailable = unsafe extern "system" fn() -> BOOL;
type Fn_InitializeKDMAPIStream = unsafe extern "system" fn() -> BOOL;
type Fn_TerminateKDMAPIStream = unsafe extern "system" fn();
type Fn_GetDriverDebugInfo = unsafe extern "system" fn() -> *mut DebugInfo;
type Fn_SendDirectData = unsafe extern "system" fn(DWORD);
/// Optional extension (not in stock OmniMIDI): send `count` short messages in one call.
/// `void WINAPI SendDirectDataBatch(const DWORD* msgs, DWORD count);`
type Fn_SendDirectDataBatch = unsafe extern "system" fn(*const DWORD, DWORD);

pub struct OmniMIDI {
    _lib: Library,
    pub is_av: Option<Fn_IsKDMAPIAvailable>,
    pub init: Option<Fn_InitializeKDMAPIStream>,
    pub term: Option<Fn_TerminateKDMAPIStream>,
    #[allow(dead_code)] // optional KDMAPI diagnostics entry point
    pub getdbg: Option<Fn_GetDriverDebugInfo>,
    pub send_direct: Option<Fn_SendDirectData>,
    pub send_batch: Option<Fn_SendDirectDataBatch>,
    /// SVMS native API bootstrap (SuperVirtualMIDISynth only).
    pub svms_get_interface: Option<crate::svms::GetInterfaceFn>,
}

impl OmniMIDI {
    pub unsafe fn load<P: AsRef<Path>>(path: Option<P>) -> Result<Self, libloading::Error> {
        let lib = match path {
            Some(p) => Library::new(p.as_ref())?,
            None => Library::new("OmniMIDI.dll")?,
        };
        let is_av = lib
            .get::<Fn_IsKDMAPIAvailable>(b"IsKDMAPIAvailable")
            .ok()
            .map(|s| *s);
        let init = lib
            .get::<Fn_InitializeKDMAPIStream>(b"InitializeKDMAPIStream")
            .ok()
            .map(|s| *s);
        let term = lib
            .get::<Fn_TerminateKDMAPIStream>(b"TerminateKDMAPIStream")
            .ok()
            .map(|s| *s);
        let getdbg = lib
            .get::<Fn_GetDriverDebugInfo>(b"GetDriverDebugInfo")
            .ok()
            .map(|s| *s);
        let send_direct = lib
            .get::<Fn_SendDirectData>(b"SendDirectData")
            .ok()
            .map(|s| *s);
        let send_batch = lib
            .get::<Fn_SendDirectDataBatch>(b"SendDirectDataBatch")
            .ok()
            .map(|s| *s);
        let svms_get_interface = lib
            .get::<crate::svms::GetInterfaceFn>(b"SVMS_GetInterface")
            .ok()
            .map(|s| *s);
        Ok(Self {
            svms_get_interface,
            _lib: lib,
            is_av,
            init,
            term,
            getdbg,
            send_direct,
            send_batch,
        })
    }
    pub unsafe fn available(&self) -> bool {
        self.is_av.map(|f| f() != 0).unwrap_or(false)
    }
    pub unsafe fn init_stream(&self) -> bool {
        self.init.map(|f| f() != 0).unwrap_or(false)
    }
    pub unsafe fn term_stream(&self) {
        if let Some(f) = self.term {
            f();
        }
    }
    pub unsafe fn send_short(&self, w: DWORD) {
        if let Some(f) = self.send_direct {
            f(w);
        }
    }
    #[allow(dead_code)]
    pub unsafe fn get_debug(&self) -> Option<&'static DebugInfo> {
        if let Some(f) = self.getdbg {
            let p = f();
            if p.is_null() {
                None
            } else {
                Some(&*p)
            }
        } else {
            None
        }
    }
}

#[inline]
pub fn pack_short(status: u8, d1: u8, d2: u8) -> DWORD {
    (status as DWORD) | ((d1 as DWORD) << 8) | ((d2 as DWORD) << 16)
}
