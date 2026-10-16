#![allow(non_camel_case_types)]

use libloading::Library;
use std::path::Path;
use winapi::um::mmsystem::MIDIHDR;

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

/// KDMAPI's long (SysEx) messages: `UINT Fn(MIDIHDR*, UINT header_size)`.
type Fn_LongData = unsafe extern "system" fn(*mut MIDIHDR, u32) -> u32;

pub struct OmniMIDI {
    _lib: Library,
    pub is_av: Option<Fn_IsKDMAPIAvailable>,
    pub init: Option<Fn_InitializeKDMAPIStream>,
    pub term: Option<Fn_TerminateKDMAPIStream>,
    #[allow(dead_code)] // optional KDMAPI diagnostics entry point
    pub getdbg: Option<Fn_GetDriverDebugInfo>,
    pub send_direct: Option<Fn_SendDirectData>,
    pub send_batch: Option<Fn_SendDirectDataBatch>,
    prepare_long: Option<Fn_LongData>,
    send_long: Option<Fn_LongData>,
    unprepare_long: Option<Fn_LongData>,
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
        let prepare_long = lib.get::<Fn_LongData>(b"PrepareLongData").ok().map(|s| *s);
        let send_long = lib.get::<Fn_LongData>(b"SendDirectLongData").ok().map(|s| *s);
        let unprepare_long = lib.get::<Fn_LongData>(b"UnprepareLongData").ok().map(|s| *s);
        let svms_get_interface = lib
            .get::<crate::svms::GetInterfaceFn>(b"SVMS_GetInterface")
            .ok()
            .map(|s| *s);
        Ok(Self {
            svms_get_interface,
            prepare_long,
            send_long,
            unprepare_long,
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
    /// Whether the synth takes long messages (SysEx, and batches of events as a byte stream).
    pub fn has_long(&self) -> bool {
        self.send_long.is_some()
    }

    /// Send one SysEx (or other long) message, status byte included.
    pub unsafe fn send_long_msg(&self, bytes: &[u8]) {
        let Some(send) = self.send_long else { return };
        let mut hdr: MIDIHDR = std::mem::zeroed();
        hdr.lpData = bytes.as_ptr() as *mut _; // the driver copies it
        hdr.dwBufferLength = bytes.len() as u32;
        hdr.dwBytesRecorded = bytes.len() as u32;
        let size = std::mem::size_of::<MIDIHDR>() as u32;
        if let Some(prepare) = self.prepare_long {
            prepare(&mut hdr, size);
        }
        send(&mut hdr, size);
        if let Some(unprepare) = self.unprepare_long {
            unprepare(&mut hdr, size);
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
