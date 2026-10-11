//! A built-in benchmark: everything it works on is generated here, in memory (so a disk
//! doesn't matter), and run through the real xz, scanning, parsing and playback code.
//! Each test is a fixed amount of work; its rate becomes points against a reference
//! machine (`REFERENCE`, 1000 points), and the score is the geometric mean of them.
//!
//! `kimrashi --benchmark`, or Settings > Benchmark in the app.

use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::time::Instant;

use crate::parser::{MidiFile, Plain, Progress};
use crate::song::{SongData, TempoMap};

/// Changes when the work or the scoring changes: scores only compare within a version.
pub const VERSION: u32 = 1;

/// How much work each test gets.
#[derive(Clone, Copy)]
pub struct Scale {
    pub name: &'static str,
    /// Times each test is run (the median counts).
    pub reps: usize,
    /// Output of the xz decode tests, MiB.
    pub xz_mib: usize,
    /// Size of the layer the scan test reads, MiB.
    pub scan_mib: usize,
    /// Notes in the one-thread parse test, and in the all-cores parse test (which is also
    /// what the playback test plays) as tracks of notes.
    pub parse_notes: u32,
    pub tracks: u32,
    pub track_notes: u32,
}

impl Scale {
    pub const FULL: Scale = Scale { name: "full", reps: 5, xz_mib: 512, scan_mib: 512, parse_notes: 48_000_000, tracks: 96, track_notes: 1_000_000 };
    /// Only for checking that the tests run (their rates mean nothing).
    #[cfg(test)]
    const SMOKE: Scale = Scale { name: "smoke", reps: 1, xz_mib: 8, scan_mib: 2, parse_notes: 20_000, tracks: 4, track_notes: 5_000 };
}

/// The tests, in order, and the rate (in the test's unit) that is worth 1000 points: the
/// median of five runs on an i5-13600KF at stock clocks, 32 GB.
pub const REFERENCE: [(&str, &str, f64); 6] = [
    ("xz decode, MIDI data", "MB/s", 76.05),
    ("xz decode, long matches", "MB/s", 1572.42),
    ("Layer scan", "MB/s", 7022.57),
    ("Parse, one thread", "Mnotes/s", 36.52),
    ("Parse, all cores", "Mnotes/s", 251.48),
    ("Playback engine", "Mnotes/s", 25.67),
];

pub struct TestResult {
    pub name: &'static str,
    pub unit: &'static str,
    pub rate: f64,
    pub points: f64,
}

pub struct Report {
    pub cpu: String,
    pub threads: usize,
    pub ram_gb: f64,
    pub tests: Vec<TestResult>,
    /// The geometric mean of the tests' points.
    pub score: f64,
    pub secs: f64,
}

impl Report {
    /// A line to paste somewhere.
    pub fn line(&self) -> String {
        format!(
            "Kimrashi benchmark v{VERSION}: {:.0} points | {} · {} threads · {:.0} GB | {}",
            self.score,
            self.cpu,
            self.threads,
            self.ram_gb,
            self.tests.iter().map(|t| format!("{} {:.0}", t.name, t.points)).collect::<Vec<_>>().join(", ")
        )
    }
}

/// A remark on a result.
pub fn nag(r: &Report) -> String {
    let scaling = r.tests[4].rate / r.tests[3].rate;
    if r.threads > 4 && scaling < r.threads as f64 * 0.25 {
        return format!("All cores did only {scaling:.1}x one core. Are the other {} on holiday?", r.threads - 1);
    }
    match r.score {
        s if s < 100.0 => "Is this a 2005 Celeron? Be honest.",
        s if s < 250.0 => "It loads. Eventually. Bring a snack.",
        s if s < 500.0 => "Have you tried closing Chrome?",
        s if s < 900.0 => "Not bad. Not great either.",
        s if s < 1100.0 => "Right about the reference machine.",
        s if s < 1500.0 => "Respectable. Someone has been tuning.",
        s if s < 3000.0 => "Okay, now you're just showing off.",
        s if s < 10_000.0 => "That's a server. Or a very good liar.",
        _ => "Over 10000? Absolutely cheating. We both know it.",
    }
    .to_owned()
}

// ---- the work, generated ----------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn vlv(out: &mut Vec<u8>, mut v: u32) {
    let mut bytes = [0u8; 5];
    bytes[0] = (v & 0x7F) as u8;
    let mut n = 1;
    v >>= 7;
    while v > 0 {
        bytes[n] = (v & 0x7F) as u8 | 0x80;
        v >>= 7;
        n += 1;
    }
    out.extend(bytes[..n].iter().rev());
}

/// The body of a track of `notes` dense overlapping notes (16 channels, 88 keys, a few
/// velocities), always the same for a seed.
fn track_body(seed: u64, notes: u32) -> Vec<u8> {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut out = Vec::with_capacity(notes as usize * 8 + 8);
    let mut open: VecDeque<(u8, u8)> = VecDeque::new();
    for _ in 0..notes {
        let r = rng.next();
        let (ch, key, vel) = ((r & 15) as u8, 21 + ((r >> 8) % 88) as u8, 30 * (1 + ((r >> 20) % 4) as u8));
        vlv(&mut out, ((r >> 30) % 3) as u32);
        out.extend_from_slice(&[0x90 | ch, key, vel]);
        open.push_back((ch, key));
        if open.len() > 48 {
            let (c, k) = open.pop_front().unwrap();
            vlv(&mut out, ((r >> 40) % 3) as u32);
            out.extend_from_slice(&[0x80 | c, k, 0]);
        }
    }
    for (c, k) in open {
        out.extend_from_slice(&[1, 0x80 | c, k, 0]);
    }
    out.extend_from_slice(&[0, 0xFF, 0x2F, 0]);
    out
}

fn chunk(body: &[u8]) -> Vec<u8> {
    let mut c = b"MTrk".to_vec();
    c.extend_from_slice(&(body.len() as u32).to_be_bytes());
    c.extend_from_slice(body);
    c
}

/// A MIDI file of `tracks` tracks of `notes` notes each.
fn midi_file(tracks: u32, notes: u32) -> Vec<u8> {
    let mut f = b"MThd".to_vec();
    f.extend_from_slice(&[0, 0, 0, 6, 0, 1]);
    f.extend_from_slice(&(tracks as u16).to_be_bytes());
    f.extend_from_slice(&480u16.to_be_bytes());
    for t in 0..tracks {
        f.extend(chunk(&track_body(1000 + t as u64, notes)));
    }
    f
}

fn xz(data: &[u8], preset: u32) -> Vec<u8> {
    let mut w = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(preset)).unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

// ---- timing -------------------------------------------------------------------

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// The median rate over `reps` runs of `work`, which returns (amount done, in the unit
/// times 1e6) and is timed. It is run once first without being counted: a short run on a
/// cold core (or an E-core, or at a low clock) would read low.
fn rate(reps: usize, mut work: impl FnMut() -> f64) -> f64 {
    work();
    let mut rates: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            let amount = work();
            amount / t.elapsed().as_secs_f64() / 1e6
        })
        .collect();
    median(&mut rates)
}

fn drain(mut r: impl Read) -> f64 {
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        match r.read(&mut buf).unwrap() {
            0 => return n as f64,
            k => n += k as u64,
        }
    }
}

// ---- the system ---------------------------------------------------------------

fn cpu_name() -> String {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::__cpuid;
        if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
            let mut b = Vec::new();
            for leaf in 0x8000_0002..=0x8000_0004 {
                let r = __cpuid(leaf);
                for x in [r.eax, r.ebx, r.ecx, r.edx] {
                    b.extend_from_slice(&x.to_le_bytes());
                }
            }
            let name = String::from_utf8_lossy(&b).trim_matches(|c| c == '\0' || c == ' ').to_owned();
            return name.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    "unknown CPU".into()
}

#[cfg(windows)]
fn ram_gb() -> f64 {
    use winapi::um::sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            return m.ullTotalPhys as f64 / (1u64 << 30) as f64;
        }
    }
    0.0
}

#[cfg(not(windows))]
fn ram_gb() -> f64 {
    0.0
}

// ---- the run ------------------------------------------------------------------

/// The process at high priority while it lasts: otherwise Windows may leave a short
/// single-threaded test on a slow core (an E-core) for part of it.
struct HighPriority(u32);

#[cfg(windows)]
impl HighPriority {
    fn new() -> Self {
        use winapi::um::processthreadsapi::{GetCurrentProcess, GetPriorityClass, SetPriorityClass};
        use winapi::um::winbase::HIGH_PRIORITY_CLASS;
        unsafe {
            let p = GetCurrentProcess();
            let old = GetPriorityClass(p);
            SetPriorityClass(p, HIGH_PRIORITY_CLASS);
            Self(old)
        }
    }
}

#[cfg(windows)]
impl Drop for HighPriority {
    fn drop(&mut self) {
        use winapi::um::processthreadsapi::{GetCurrentProcess, SetPriorityClass};
        if self.0 != 0 {
            unsafe { SetPriorityClass(GetCurrentProcess(), self.0) };
        }
    }
}

#[cfg(not(windows))]
impl HighPriority {
    fn new() -> Self {
        Self(0)
    }
}

/// Run the tests; `step(i, name)` is called before each (of `REFERENCE.len()`).
pub fn run(scale: Scale, mut step: impl FnMut(usize, &str)) -> Report {
    let started = Instant::now();
    let _priority = HighPriority::new();
    let mut rates = Vec::new();

    // xz decode of MIDI-like data: the same compressed chunk, many times over.
    step(0, REFERENCE[0].0);
    let chunk_bytes = chunk(&track_body(1, scale.track_notes));
    let one = xz(&chunk_bytes, 6);
    let copies = (scale.xz_mib << 20) / chunk_bytes.len().max(1) + 1;
    let layer = one.repeat(copies);
    rates.push(rate(scale.reps, || drain(liblzma::read::XzDecoder::new_multi_decoder(Cursor::new(&layer)))));
    drop(layer);

    // xz decode of long matches: what a huge repeated file is made of.
    step(1, REFERENCE[1].0);
    let pattern = track_body(2, 8_000);
    let data: Vec<u8> = pattern.iter().copied().cycle().take(scale.xz_mib << 20).collect();
    let packed = xz(&data, 1);
    drop(data);
    rates.push(rate(scale.reps, || drain(liblzma::read::XzDecoder::new_multi_decoder(Cursor::new(&packed)))));
    drop(packed);

    // The scan that finds repeated streams in a layer.
    step(2, REFERENCE[2].0);
    let streams: Vec<Vec<u8>> = (0..16).map(|s| xz(&chunk(&track_body(100 + s, 12_000)), 1)).collect();
    let cycle = streams.concat();
    let layer = cycle.repeat(((scale.scan_mib << 20) / cycle.len()).max(2));
    rates.push(rate(scale.reps, || (0..4).map(|_| crate::archive::bench_scan(&layer) as f64).sum()));
    drop(layer);

    // Packing one track on one thread.
    step(3, REFERENCE[3].0);
    let body = track_body(7, scale.parse_notes);
    rates.push(rate(scale.reps, || {
        let mut raw = body.clone();
        crate::parser::bench_pack(&mut raw) as f64
    }));
    drop(body);

    // Loading a whole file: its tracks are packed on all cores.
    step(4, REFERENCE[4].0);
    let file = midi_file(scale.tracks, scale.track_notes);
    let mut song = None;
    rates.push(rate(scale.reps, || {
        let m = MidiFile::parse_stream(Box::new(Plain(Cursor::new(&file))), 127, &Progress::default()).unwrap();
        let notes = m.note_count as f64;
        song = Some(m);
        notes
    }));
    drop(file);

    // The playback engine, offline: what the player does with notes, without a synth.
    step(5, REFERENCE[5].0);
    let midi = std::sync::Arc::new(song.unwrap());
    let tempo = std::sync::Arc::new(TempoMap::from_midi(&midi));
    let song = SongData { path: String::new(), midi, tempo, load_time: Default::default() };
    rates.push(rate(scale.reps, || {
        crate::player::bench_engine(&song);
        song.midi.note_count as f64
    }));

    let tests: Vec<TestResult> = REFERENCE
        .iter()
        .zip(&rates)
        .map(|(&(name, unit, reference), &rate)| TestResult { name, unit, rate, points: 1000.0 * rate / reference })
        .collect();
    let score = (tests.iter().map(|t| t.points.max(1e-9).ln()).sum::<f64>() / tests.len() as f64).exp();
    Report {
        cpu: cpu_name(),
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        ram_gb: ram_gb(),
        tests,
        score,
        secs: started.elapsed().as_secs_f64(),
    }
}

/// `--benchmark`: run and print; false if the command line doesn't ask for it.
pub fn run_from_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--benchmark") {
        return false;
    }
    println!("Kimrashi benchmark v{VERSION}: close other programs for a fair score.");
    let r = run(Scale::FULL, |i, name| println!("[{}/{}] {name} ...", i + 1, REFERENCE.len()));
    println!("\n{} · {} threads · {:.0} GB RAM", r.cpu, r.threads, r.ram_gb);
    for t in &r.tests {
        println!("  {:<26} {:>9.2} {:<9} {:>7.0} points", t.name, t.rate, t.unit, t.points);
    }
    println!("\nScore: {:.0} points  ({:.0} s)\n\n{}", r.score, r.secs, r.line());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_work_is_the_same_every_time_and_parses() {
        assert_eq!(track_body(5, 300), track_body(5, 300));
        assert_ne!(track_body(5, 300), track_body(6, 300));
        let m = MidiFile::parse_stream(Box::new(Plain(Cursor::new(midi_file(3, 400)))), 127, &Progress::default()).unwrap();
        assert_eq!(m.note_count, 1200);
    }

    #[test]
    fn nags_follow_the_score() {
        let mut r = Report { cpu: String::new(), threads: 4, ram_gb: 0.0, tests: vec![], score: 50.0, secs: 0.0 };
        let t = |rate| TestResult { name: "", unit: "", rate, points: 0.0 };
        r.tests = vec![t(1.0), t(1.0), t(1.0), t(1.0), t(4.0), t(1.0)];
        for (score, word) in [(50.0, "Celeron"), (700.0, "Not bad"), (1000.0, "reference"), (12_000.0, "cheating")] {
            r.score = score;
            assert!(nag(&r).contains(word), "{score}: {}", nag(&r));
        }
    }

    #[test]
    fn every_test_runs() {
        let mut seen = vec![];
        let r = run(Scale::SMOKE, |i, _| seen.push(i));
        assert_eq!(seen, [0, 1, 2, 3, 4, 5]);
        assert_eq!(r.tests.len(), REFERENCE.len());
        assert!(r.tests.iter().all(|t| t.rate.is_finite() && t.rate > 0.0), "{:?}", r.tests.iter().map(|t| t.rate).collect::<Vec<_>>());
        assert!(r.score > 0.0 && r.line().contains("points"));
    }
}
