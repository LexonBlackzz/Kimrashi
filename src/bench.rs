//! Headless playback for synth testing: play a range of a MIDI file through the
//! synth with no window, printing player and synth numbers each second and writing
//! the stats CSVs (plus SVMS's per-callback CSV when the synth is SVMS).
//!
//! `kimrashi --play <file> [--from m:ss] [--to m:ss] [--dll <KDMAPI dll>]
//!  [--batch] [--no-skip] [--limit <notes/s>] [--speed <x>]`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;

use crate::clock::PlaybackClock;
use crate::omnimidi::OmniMIDI;
use crate::output::OutputChoice;
use crate::player::{Player, PlayerCommand};
use crate::song::SongData;

struct Args {
    file: String,
    from_us: u64,
    to_us: Option<u64>,
    dll: Option<String>,
    batch: bool,
    skip: bool,
    limit: u32,
    speed: f64,
    /// Print what the range contains instead of playing it.
    analyze: bool,
}

pub fn parse_time(s: &str) -> Result<u64, String> {
    let secs = match s.split_once(':') {
        Some((m, s)) => m.parse::<f64>().map_err(|e| e.to_string())? * 60.0 + s.parse::<f64>().map_err(|e| e.to_string())?,
        None => s.parse::<f64>().map_err(|e| e.to_string())?,
    };
    Ok((secs * 1e6) as u64)
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args { file: String::new(), from_us: 0, to_us: None, dll: None, batch: false, skip: true, limit: 0, speed: 1.0, analyze: false };
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--play" => a.file = val()?,
            "--from" => a.from_us = parse_time(&val()?)?,
            "--to" => a.to_us = Some(parse_time(&val()?)?),
            "--dll" => a.dll = Some(val()?),
            "--batch" => a.batch = true,
            "--analyze" => a.analyze = true,
            "--no-skip" => a.skip = false,
            "--limit" => a.limit = val()?.parse().map_err(|e| format!("--limit: {e}"))?,
            "--speed" => a.speed = val()?.parse().map_err(|e| format!("--speed: {e}"))?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.file.is_empty() { Err("--play <file> is required".into()) } else { Ok(a) }
}

/// Run if the command line asks for headless playback; returns false otherwise.
pub fn run_from_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--play" || a == "--analyze") {
        return false;
    }
    if let Err(e) = parse(args.into_iter()).and_then(run) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    true
}

/// Per second of the range: note-ons, key range, note lengths, channels in use,
/// and the non-note events by kind.
fn analyze(a: &Args) -> Result<(), String> {
    use crate::parser::{Record, TrackReader};
    let song = SongData::load(&a.file, 127, &crate::parser::Progress::default()).map_err(|e| e.to_string())?;
    let tempo = &song.tempo;
    let end_us = a.to_us.unwrap_or(tempo.duration_us());
    let (t0, t1) = (tempo.time_to_tick(a.from_us), tempo.time_to_tick(end_us));
    let secs = ((end_us - a.from_us) / 1_000_000 + 1) as usize;
    #[derive(Default, Clone)]
    struct Sec { ons: u64, keys: Vec<u64>, len_us: f64, chans: u16, other: std::collections::BTreeMap<String, u64> }
    let mut per = vec![Sec { keys: vec![0; 128], ..Default::default() }; secs];
    for i in 0..song.midi.tracks.len() {
        let t = song.midi.track(i);
        let mut r = TrackReader::default();
        let mut cache = 0;
        while let Some((tick, rec)) = r.next(t) {
            if tick >= t1 { break; }
            if tick < t0 { continue; }
            let us = tempo.tick_to_time_with_cache(tick, &mut cache);
            let s = &mut per[(us.saturating_sub(a.from_us) / 1_000_000) as usize];
            match rec {
                Record::Note { ch, key, dur, .. } => {
                    s.ons += 1;
                    s.keys[key as usize & 127] += 1;
                    s.chans |= 1 << ch;
                    if let Some(d) = dur {
                        s.len_us += (tempo.tick_to_time_with_cache(tick + d, &mut cache) - us) as f64;
                    }
                }
                Record::Other { status, d1, .. } => {
                    let kind = match status & 0xF0 {
                        0xB0 => format!("cc{d1}"),
                        0xC0 => format!("prog{d1}"),
                        0xE0 => "bend".into(),
                        0xD0 => "chanpress".into(),
                        0xA0 => "polypress".into(),
                        _ => format!("{status:02x}"),
                    };
                    *s.other.entry(kind).or_default() += 1;
                }
            }
        }
    }
    for (i, s) in per.iter().enumerate() {
        let lo = s.keys.iter().position(|&n| n > 0).unwrap_or(0);
        let hi = s.keys.iter().rposition(|&n| n > 0).unwrap_or(0);
        let mean = s.keys.iter().enumerate().map(|(k, &n)| k as u64 * n).sum::<u64>() as f64 / s.ons.max(1) as f64;
        let other: Vec<String> = s.other.iter().map(|(k, n)| format!("{k}:{}", crate::si(*n as f64))).collect();
        println!(
            "{} ons {:>7} keys {lo:>3}-{hi:<3} mean {mean:5.1} avg len {:7.1} ms chans {:016b} {}",
            crate::format_timestamp(a.from_us + i as u64 * 1_000_000),
            crate::si(s.ons as f64),
            s.len_us / s.ons.max(1) as f64 / 1e3,
            s.chans,
            other.join(" "),
        );
    }
    Ok(())
}

fn run(a: Args) -> Result<(), String> {
    if a.analyze {
        return analyze(&a);
    }
    let omni = unsafe { OmniMIDI::load(a.dll.as_deref()) }.map_err(|e| format!("loading the KDMAPI dll: {e}"))?;
    if !unsafe { omni.available() } {
        return Err("KDMAPI not available".into());
    }
    let omni = Arc::new(omni);
    let progress = crate::parser::Progress::default();
    let song = SongData::load(&a.file, 127, &progress).map_err(|e| e.to_string())?;
    let end_us = a.to_us.unwrap_or(song.tempo.duration_us()).min(song.tempo.duration_us());
    eprintln!("loaded {} in {:.2} s; playing {:.1}..{:.1} s", a.file, song.load_time.as_secs_f64(), a.from_us as f64 / 1e6, end_us as f64 / 1e6);

    let (cmd_tx, cmd_rx) = unbounded();
    let (tel_tx, tel_rx) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(PlaybackClock::new());
    clock.set_speed(a.speed);
    Player::new(Some(omni), OutputChoice::Kdmapi, cmd_rx, tel_tx, stop.clone(), clock.clone());
    for cmd in [
        PlayerCommand::SetSkipping(a.skip),
        PlayerCommand::SetSynthLimit(a.limit),
        PlayerCommand::SetSvmsBatch(a.batch),
        PlayerCommand::SetStatsLog(true),
        PlayerCommand::Play(song.clone()),
        PlayerCommand::Seek(a.from_us),
    ] {
        let _ = cmd_tx.send(cmd);
    }

    println!(
        "{:>7} {:>6} {:>6} {:>7} {:>7} {:>6} | {:>5} {:>5} {:>7} {:>7} {:>6} {:>7} {:>9} {:>6} {:>4}",
        "song", "NPS", "skip", "msg/s", "ns/msg", "lag", "load", "max", "events", "late", "skipms", "voices", "backlog", "block", "path"
    );
    let mut last_second = u64::MAX;
    let mut log_path = None;
    loop {
        let Ok(t) = tel_rx.recv_timeout(Duration::from_secs(10)) else {
            return Err("player stopped responding".into());
        };
        log_path = t.log_path.clone().or(log_path);
        if t.playback_time_us < a.from_us {
            continue;
        }
        let second = t.playback_time_us / 1_000_000;
        if second != last_second {
            last_second = second;
            let s = t.svms.clone().unwrap_or_default();
            println!(
                "{:>7} {:>6} {:>6} {:>7} {:>7.0} {:>6.1} | {:>4.0}% {:>4.0}% {:>7} {:>7} {:>6.0} {:>7} {:>9} {:>6.1} {:>4}",
                crate::format_timestamp(t.playback_time_us),
                crate::si(t.nps as f64),
                crate::si(t.skipped_nps as f64),
                crate::si(t.stats.msgs_per_sec as f64),
                t.stats.synth_ns_per_msg,
                t.lag_us as f64 / 1e3,
                s.load_avg,
                s.load_max,
                crate::si(s.events_per_s),
                crate::si(s.late_per_s),
                s.skipped_ms,
                crate::si(s.active_voices as f64),
                crate::si((s.scheduled_backlog + s.ingress_backlog + s.compiled_backlog) as f64),
                s.blocked_ms_per_s,
                crate::svms::paths(s.paths),
            );
        }
        if t.playback_time_us >= end_us || (!t.is_playing && t.playback_time_us > a.from_us) {
            break;
        }
    }
    let _ = cmd_tx.send(PlayerCommand::Stop);
    std::thread::sleep(Duration::from_millis(300)); // let the output close (and its logs flush)
    if let Some(p) = log_path {
        eprintln!("stats: {p}\nsvms:  {}", p.replace(".csv", "_svms.csv"));
    }
    Ok(())
}
