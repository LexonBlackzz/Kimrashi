//! MIDI output: OmniMIDI's KDMAPI, or any Windows MIDI (WinMM) device such as the
//! Microsoft GS Wavetable Synth.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use winapi::um::mmeapi::{
    midiOutClose, midiOutGetDevCapsW, midiOutGetNumDevs, midiOutOpen, midiOutReset,
    midiOutShortMsg,
};
use winapi::um::mmsystem::{CALLBACK_NULL, HMIDIOUT, MIDIOUTCAPSW};

use crate::clock::PlaybackClock;
use crate::omnimidi::OmniMIDI;
use crate::svms;

#[derive(Clone, Debug, PartialEq)]
pub enum OutputChoice {
    Kdmapi,
    /// WinMM device id.
    WinMM(u32),
}

/// Available outputs with display names. KDMAPI is listed first when OmniMIDI is available.
pub fn list_outputs(kdmapi_available: bool) -> Vec<(OutputChoice, String)> {
    let mut outputs = Vec::new();
    if kdmapi_available {
        outputs.push((OutputChoice::Kdmapi, "KDMAPI (OmniMIDI.dll)".to_owned()));
    }
    for id in 0..unsafe { midiOutGetNumDevs() } {
        let mut caps: MIDIOUTCAPSW = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<MIDIOUTCAPSW>() as u32;
        if unsafe { midiOutGetDevCapsW(id as usize, &mut caps, size) } == 0 {
            let name = caps.szPname;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            outputs.push((OutputChoice::WinMM(id), String::from_utf16_lossy(&name[..len])));
        }
    }
    outputs
}

/// What an SVMS output needs from the player: the clock (to place callbacks in
/// the song), where to log callbacks, and whether to submit through SVMS's batch API.
pub struct SvmsSetup {
    pub clock: Arc<PlaybackClock>,
    pub log: Arc<Mutex<Option<PathBuf>>>,
    pub batch: bool,
}

/// The SVMS side of a KDMAPI output, when the DLL is SVMS.
pub struct Svms {
    link: Arc<svms::Link>,
    monitor: Option<svms::Monitor>,
    batch: Cell<bool>,
    scratch: RefCell<Vec<svms::ShortEvent>>,
}

/// An open output. Closes itself on drop.
pub enum Output {
    Closed,
    Kdmapi(Arc<OmniMIDI>, Option<Svms>),
    WinMM(HMIDIOUT),
}

impl Output {
    pub fn open(choice: &OutputChoice, omni: Option<&Arc<OmniMIDI>>, setup: &SvmsSetup) -> Self {
        match choice {
            OutputChoice::Kdmapi => match omni {
                Some(omni) if unsafe { omni.init_stream() } => {
                    let svms = omni.svms_get_interface.and_then(svms::Link::open).map(|link| {
                        let link = Arc::new(link);
                        Svms {
                            monitor: svms::Monitor::start(link.clone(), setup.clock.clone(), setup.log.clone()),
                            batch: Cell::new(setup.batch && link.can_batch()),
                            link,
                            scratch: RefCell::new(Vec::new()),
                        }
                    });
                    Output::Kdmapi(omni.clone(), svms)
                }
                _ => {
                    eprintln!("InitializeKDMAPIStream failed.");
                    Output::Closed
                }
            },
            OutputChoice::WinMM(id) => {
                let mut h: HMIDIOUT = std::ptr::null_mut();
                if unsafe { midiOutOpen(&mut h, *id, 0, 0, CALLBACK_NULL) } == 0 {
                    Output::WinMM(h)
                } else {
                    eprintln!("midiOutOpen({id}) failed.");
                    Output::Closed
                }
            }
        }
    }

    pub fn is_open(&self) -> bool {
        !matches!(self, Output::Closed)
    }

    #[inline]
    pub fn send(&self, msg: u32) {
        match self {
            Output::Closed => {}
            Output::Kdmapi(omni, _) => unsafe { omni.send_short(msg) },
            Output::WinMM(h) => unsafe {
                midiOutShortMsg(*h, msg);
            },
        }
    }
}

impl Output {
    /// Send many messages: one call through SVMS's batch API (when enabled) or the
    /// synth's `SendDirectDataBatch` export, otherwise one call per message.
    #[inline]
    pub fn send_batch(&self, msgs: &[u32]) {
        match self {
            Output::Kdmapi(_, Some(s)) if s.batch.get() => {
                let mut events = s.scratch.borrow_mut();
                events.clear();
                events.extend(msgs.iter().map(|&m| svms::short_event(m)));
                s.link.send_batch(&events);
            }
            Output::Kdmapi(omni, _) if omni.send_batch.is_some() => {
                for chunk in msgs.chunks(u32::MAX as usize) {
                    unsafe { (omni.send_batch.unwrap())(chunk.as_ptr(), chunk.len() as u32) };
                }
            }
            _ => msgs.iter().for_each(|&m| self.send(m)),
        }
    }

    /// Whether `send_batch` uses a batch entry point.
    pub fn batched(&self) -> bool {
        match self {
            Output::Kdmapi(omni, svms) => {
                svms.as_ref().is_some_and(|s| s.batch.get()) || omni.send_batch.is_some()
            }
            _ => false,
        }
    }

    /// Submit through SVMS's batch API (if this is SVMS and it supports batches).
    pub fn set_svms_batch(&self, on: bool) {
        if let Output::Kdmapi(_, Some(s)) = self {
            s.batch.set(on && s.link.can_batch());
        }
    }

    /// The synth's own numbers over the last second (SVMS only).
    pub fn svms_stats(&self) -> Option<svms::Stats> {
        match self {
            Output::Kdmapi(_, Some(s)) => s.monitor.as_ref()?.stats.lock().unwrap().clone(),
            _ => None,
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        match self {
            Output::Closed => {}
            Output::Kdmapi(omni, svms) => {
                // Close the SVMS session (and its monitor) before the stream.
                *svms = None;
                unsafe { omni.term_stream() }
            }
            Output::WinMM(h) => unsafe {
                midiOutReset(*h);
                midiOutClose(*h);
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a real SVMS: `SVMS_DLL=path\to\OmniMIDI.dll cargo test -- --ignored`.
    /// Plays a few velocity-1 notes through both submit paths.
    #[test]
    #[ignore]
    fn svms_monitor_sees_callbacks() {
        let dll = std::env::var("SVMS_DLL").expect("set SVMS_DLL");
        let omni = Arc::new(unsafe { OmniMIDI::load(Some(&dll)) }.unwrap());
        let log = std::env::temp_dir().join("kimrashi_svms_test.csv");
        let _ = std::fs::remove_file(&log);
        let clock = Arc::new(PlaybackClock::new());
        clock.set_running(0);
        let setup = SvmsSetup { clock, log: Arc::new(Mutex::new(Some(log.clone()))), batch: true };
        let out = Output::open(&OutputChoice::Kdmapi, Some(&omni), &setup);
        assert!(matches!(&out, Output::Kdmapi(_, Some(s)) if s.batch.get()), "not SVMS, or no batch API");
        for i in 0..150u32 {
            out.set_svms_batch(i % 2 == 0);
            let key = 40 + (i % 40) as u8;
            out.send_batch(&[crate::omnimidi::pack_short(0x90, key, 1), crate::omnimidi::pack_short(0x80, key, 0)]);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let s = out.svms_stats().expect("no SVMS stats after 1.5 s");
        drop(out);
        assert!(s.callbacks > 0 && s.sample_rate > 0 && s.load_avg > 0.0, "{s:?}");
        let csv = std::fs::read_to_string(&log).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert!(rows.len() >= s.callbacks as usize, "{} rows", rows.len());
        let ons: u64 = rows.iter().map(|r| r.split(',').nth(20).unwrap().parse::<u64>().unwrap()).sum();
        assert!(ons > 0, "no note-ons seen");
        println!("{s:#?}\n{} rows, {ons} note-ons", rows.len());
    }
}
