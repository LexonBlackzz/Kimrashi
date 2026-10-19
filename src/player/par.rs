//! The engine on several threads (see engine-parallel-design.md).
//!
//! The serial engine takes the song one tick at a time. This one takes a window of ticks
//! and works in stages, each on all the threads of a `Team` (the caller's included):
//!
//! 1. **Parse**: tracks are split in ranges, a chunk each. A chunk is a small serial engine
//!    for its tracks: it reads their records tick by tick, keeps every message in a list
//!    (`msgs`), and sends each note on to the partition its slot belongs to.
//! 2. **Decide**: each partition owns a set of slots: their skipper state and their pending
//!    note-offs. It plays the notes it got through the same skipper and note-off rules as
//!    the serial engine, marking the notes it sends in a bitmap and keeping its note-offs.
//! 3. **Emit** (the caller alone): walks the window's ticks; for each, the note-offs due, then
//!    the chunks' records (the marked notes and everything that isn't a note) in track
//!    order, then the note-offs of zero-length notes. The note-offs of one of these steps are
//!    by slot.
//!
//! That is the serial engine's output with one difference: within a tick, the order of the
//! note-offs of different keys, which is by slot here and by when they were scheduled there.
//! The skipper's state for a slot only depends on that slot's own events, so what is sent
//! is the same.

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize};
use std::sync::Mutex;

use super::*;

/// `NoteEv::dur` of a note that is never released.
const NO_DUR: u32 = u32::MAX;
/// Most ticks in a window (a tick is a 16 bit number inside one).
const MAX_WINDOW_TICKS: u32 = 1 << 16;
/// Most threads.
pub(super) const MAX_THREADS: usize = 32;

// --- A team of threads that spin while they wait, for work in short bursts. ---

struct TeamShared {
    /// Counts the jobs; a worker runs one when it differs from the last it saw.
    gen: AtomicU64,
    /// Workers done with the job.
    done: AtomicUsize,
    job: AtomicPtr<Job>,
    stop: AtomicBool,
    parked: Vec<AtomicBool>,
    panicked: AtomicBool,
}

struct Job {
    f: &'static (dyn Fn(usize) + Sync),
}

/// `n` threads, the one that calls `run` among them.
struct Team {
    n: usize,
    sh: Arc<TeamShared>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Team {
    fn new(n: usize) -> Self {
        use std::sync::atomic::Ordering::*;
        let sh = Arc::new(TeamShared {
            gen: AtomicU64::new(0),
            done: AtomicUsize::new(0),
            job: AtomicPtr::new(std::ptr::null_mut()),
            stop: AtomicBool::new(false),
            parked: (0..n).map(|_| AtomicBool::new(false)).collect(),
            panicked: AtomicBool::new(false),
        });
        let handles = (1..n)
            .map(|id| {
                let sh = sh.clone();
                thread::Builder::new()
                    .name(format!("engine-{id}"))
                    .spawn(move || {
                        let mut seen = 0;
                        loop {
                            // Spin, then yield, then sleep until woken.
                            let mut idle = 0u32;
                            let gen = loop {
                                let gen = sh.gen.load(Acquire);
                                if gen != seen {
                                    break gen;
                                }
                                idle += 1;
                                if idle < 4_000 {
                                    std::hint::spin_loop();
                                } else if idle < 6_000 {
                                    thread::yield_now();
                                } else {
                                    sh.parked[id].store(true, SeqCst);
                                    if sh.gen.load(SeqCst) == seen {
                                        thread::park_timeout(Duration::from_millis(2));
                                    }
                                    sh.parked[id].store(false, SeqCst);
                                }
                            };
                            if sh.stop.load(Acquire) {
                                return;
                            }
                            seen = gen;
                            let job = unsafe { &*sh.job.load(Acquire) };
                            if catch_unwind(AssertUnwindSafe(|| (job.f)(id))).is_err() {
                                sh.panicked.store(true, Release);
                            }
                            sh.done.fetch_add(1, AcqRel);
                        }
                    })
                    .expect("spawn engine thread")
            })
            .collect();
        Self { n, sh, handles }
    }

    /// Run `f(i)` on every thread `i` of the team, returning when all are done.
    fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        use std::sync::atomic::Ordering::*;
        if self.n == 1 {
            return f(0);
        }
        // The workers are all done with the job (counted in `done`) before this returns.
        let job = Job { f: unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(f) } };
        self.sh.done.store(0, Relaxed);
        self.sh.job.store(&job as *const Job as *mut Job, Release);
        self.sh.gen.fetch_add(1, SeqCst);
        for (id, parked) in self.sh.parked.iter().enumerate().skip(1) {
            if parked.load(SeqCst) {
                self.handles[id - 1].thread().unpark();
            }
        }
        let mine = catch_unwind(AssertUnwindSafe(|| f(0)));
        let mut spins = 0u32;
        while self.sh.done.load(Acquire) < self.n - 1 {
            spins += 1;
            if spins < 20_000 {
                std::hint::spin_loop();
            } else {
                thread::yield_now();
            }
        }
        self.sh.job.store(std::ptr::null_mut(), Release);
        if let Err(e) = mine {
            resume_unwind(e);
        }
        if self.sh.panicked.swap(false, AcqRel) {
            panic!("an engine thread panicked");
        }
    }
}

impl Drop for Team {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::*;
        self.sh.stop.store(true, Release);
        self.sh.gen.fetch_add(1, SeqCst);
        for h in &self.handles {
            h.thread().unpark();
        }
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

// --- The engine ---

/// A note-on for a partition.
#[derive(Clone, Copy)]
struct NoteEv {
    /// Ticks until the note ends, or `NO_DUR`.
    dur: u32,
    /// Its place in its chunk's records.
    seq: u32,
    /// Tick in the window << 11 | slot.
    at: u32,
}

/// A note-off to send: where it goes among a window's messages (tick in the window << 13
/// | step << 11 | slot) and the message.
#[derive(Clone, Copy)]
struct OffOut {
    key: u32,
    msg: u32,
}

/// A tick of a chunk and the first of its records.
#[derive(Clone, Copy)]
struct TickMark {
    rel: u32,
    seq: u32,
}

/// A tick's messages in `Chunk::out`.
#[derive(Clone, Copy)]
struct Seg {
    rel: u32,
    start: u32,
    end: u32,
}

/// A range of tracks and what was read from them in the window.
struct Chunk {
    first: usize,
    readers: Vec<TrackReader>,
    /// Tracks by the tick of their next record.
    heap: RadixHeap<u32>,
    due_buf: Vec<u32>,
    /// Notes by partition, in tick order.
    buckets: Vec<Vec<NoteEv>>,
    /// Every record of the window as a message (a SysEx as a marker into `sysex`), in tick
    /// and track order, and the ticks they are at.
    msgs: Vec<u32>,
    marks: Vec<TickMark>,
    sysex: Vec<SysexRef>,
    /// The records to send whatever happens: all but notes. (Partitions add the notes.)
    bits: Vec<u64>,
    /// The messages to send, by tick.
    out: Vec<u32>,
    segs: Vec<Seg>,
    /// Records read from each track lately (decaying), to balance the chunks by.
    load: Vec<u32>,
    decoded: u64,
    played: u64,
    repeats: u64,
    pub(super) busy: Duration,
}

/// One partition: the slots that hash to it.
struct Part {
    /// Only the slots of this partition are ever touched.
    skipper: Skipper,
    /// Its pending note-offs.
    heap: RadixHeap<PendingOff>,
    due_buf: Vec<PendingOff>,
    /// The notes it sent, by chunk (a bit per record).
    sent_bits: Vec<Vec<u64>>,
    /// The note-offs it sends, in order.
    offs: Vec<OffOut>,
    /// Change of polyphony and notes sent in the window.
    poly: i64,
    sent: u64,
    /// Tempo segment of the last tick (a lookup hint).
    seg: usize,
    pub(super) busy: Duration,
}

pub(super) struct Parallel {
    team: Team,
    chunks: Vec<Mutex<Chunk>>,
    parts: Vec<Mutex<Part>>,
    /// The partition of each slot: partitions have a range of slots each, so that their
    /// note-offs (sorted by slot) can be joined as they are.
    part_of: Box<[u8; SLOTS]>,
    /// Notes lately on each slot (decaying), to balance the partitions by.
    slot_load: Vec<u32>,
    /// Scratch for emitting.
    cursors: Vec<usize>,
    off_cursors: Vec<usize>,
    /// Time in each stage: parse, decide, collect, emit.
    pub(super) spent: [Duration; 4],
    windows: u32,
    /// Balance after every window whatever the loads are (for tests).
    pub(super) eager: bool,
}

/// Song time of a tick and the length of a tick there.
pub(super) type TimeOf<'a> = &'a (dyn Fn(u32, &mut usize) -> (u64, f32) + Sync);

fn off_key(rel: u32, step: u32, slot: usize) -> u32 {
    rel << 13 | step << 11 | slot as u32
}

impl Parallel {
    fn new(threads: usize, readers: &[TrackReader]) -> Self {
        let n = threads.clamp(1, MAX_THREADS);
        let part_of: Box<[u8; SLOTS]> = Box::new(std::array::from_fn(|s| (s * n / SLOTS) as u8));
        let bounds: Vec<usize> = (0..=n).map(|i| readers.len() * i / n).collect();
        let chunks = (0..n)
            .map(|c| {
                Mutex::new(Chunk {
                    first: bounds[c],
                    readers: readers[bounds[c]..bounds[c + 1]].to_vec(),
                    heap: RadixHeap::new(),
                    due_buf: Vec::new(),
                    buckets: vec![Vec::new(); n],
                    msgs: Vec::new(),
                    marks: Vec::new(),
                    sysex: Vec::new(),
                    bits: Vec::new(),
                    out: Vec::new(),
                    segs: Vec::new(),
                    load: vec![0; bounds[c + 1] - bounds[c]],
                    decoded: 0,
                    played: 0,
                    repeats: 0,
                    busy: Duration::ZERO,
                })
            })
            .collect();
        let parts = (0..n)
            .map(|_| {
                Mutex::new(Part {
                    skipper: Skipper::new(0, true),
                    heap: RadixHeap::new(),
                    due_buf: Vec::new(),
                    sent_bits: vec![Vec::new(); n],
                    offs: Vec::new(),
                    poly: 0,
                    sent: 0,
                    seg: 0,
                    busy: Duration::ZERO,
                })
            })
            .collect();
        Self {
            team: Team::new(n),
            chunks,
            parts,
            part_of,
            slot_load: vec![0; SLOTS],
            cursors: vec![0; n],
            off_cursors: vec![0; n],
            spent: Default::default(),
            windows: 0,
            eager: false,
        }
    }

    /// Move the chunk borders so that each has the same share of what is read (lately).
    fn rebalance(&mut self) {
        let n = self.chunks.len();
        let mut chunks: Vec<&mut Chunk> = self.chunks.iter_mut().map(|c| c.get_mut().unwrap()).collect();
        let sums: Vec<u64> = chunks.iter().map(|c| c.load.iter().map(|&l| l as u64).sum()).collect();
        let total: u64 = sums.iter().sum();
        let worst = sums.iter().copied().max().unwrap_or(0);
        for c in chunks.iter_mut() {
            c.load.iter_mut().for_each(|l| *l >>= 1);
        }
        if n == 1 || (!self.eager && (total < 100_000 || worst * n as u64 * 100 <= total * 115)) {
            return;
        }
        let tracks: usize = chunks.iter().map(|c| c.readers.len()).sum();
        let (mut readers, mut load, mut due) = (Vec::with_capacity(tracks), Vec::with_capacity(tracks), Vec::new());
        for c in chunks.iter_mut() {
            readers.append(&mut c.readers);
            load.append(&mut c.load);
            due.extend(std::mem::replace(&mut c.heap, RadixHeap::new()).buckets.into_iter().flatten());
        }
        // Each track weighs what was read from it, and a little for being there.
        let weight: u64 = load.iter().map(|&l| l as u64 + 1).sum();
        let mut bounds = vec![0];
        let mut acc = 0u64;
        for (i, &l) in load.iter().enumerate() {
            acc += l as u64 + 1;
            while bounds.len() < n && acc * n as u64 >= weight * bounds.len() as u64 {
                bounds.push(i + 1);
            }
        }
        bounds.resize(n, tracks);
        bounds.push(tracks);
        for (c, chunk) in chunks.iter_mut().enumerate() {
            chunk.first = bounds[c];
            chunk.readers = readers[bounds[c]..bounds[c + 1]].to_vec();
            chunk.load = load[bounds[c]..bounds[c + 1]].to_vec();
        }
        for (tick, track) in due {
            let c = bounds[1..n].partition_point(|&b| b <= track as usize);
            chunks[c].heap.push(tick, track);
        }
    }

    /// Move the partitions' slot ranges so that each gets the same share of the notes (lately).
    fn rebalance_slots(&mut self, master: &mut Skipper) {
        let n = self.parts.len();
        let mut parts: Vec<&mut Part> = self.parts.iter_mut().map(|p| p.get_mut().unwrap()).collect();
        let mut load = std::mem::take(&mut self.slot_load);
        for (s, l) in load.iter_mut().enumerate() {
            for p in parts.iter_mut() {
                let d = std::mem::take(&mut p.skipper.demand[s]);
                master.demand[s] += d;
                *l = l.saturating_add(d);
            }
        }
        let mut sums = vec![0u64; n];
        for (s, &l) in load.iter().enumerate() {
            sums[self.part_of[s] as usize] += l as u64;
        }
        let total: u64 = sums.iter().sum();
        let worst = sums.iter().copied().max().unwrap_or(0);
        if n == 1 || (!self.eager && (total < 100_000 || worst * n as u64 * 100 <= total * 115)) {
            // Not worth moving; forget half of what was seen.
            load.iter_mut().for_each(|l| *l >>= 1);
            self.slot_load = load;
            return;
        }
        // Each slot weighs its notes, and a little for being there.
        let weight: u64 = load.iter().map(|&l| l as u64 + 1).sum();
        let (mut part, mut acc) = (0usize, 0u64);
        let old = self.part_of.clone();
        for (s, &l) in load.iter().enumerate() {
            while part + 1 < n && acc * n as u64 >= weight * (part as u64 + 1) {
                part += 1;
            }
            self.part_of[s] = part as u8;
            acc += l as u64 + 1;
        }
        for s in 0..SLOTS {
            let (a, b) = (old[s] as usize, self.part_of[s] as usize);
            if a != b {
                let from = &parts[a].skipper;
                let moved = (from.last_on_us[s], from.sounding[s], from.hold_until[s], from.demand[s]);
                let to = &mut parts[b].skipper;
                (to.last_on_us[s], to.sounding[s], to.hold_until[s], to.demand[s]) = moved;
            }
        }
        let offs: Vec<_> = parts
            .iter_mut()
            .flat_map(|p| std::mem::replace(&mut p.heap, RadixHeap::new()).buckets.into_iter().flatten())
            .collect();
        for (tick, (ch, key)) in offs {
            parts[self.part_of[Skipper::slot(ch, key)] as usize].heap.push(tick, (ch, key));
        }
        load.iter_mut().for_each(|l| *l >>= 1);
        self.slot_load = load;
    }

    /// The chunk that has `track`.
    fn chunk_of(&self, track: usize) -> usize {
        (0..self.chunks.len()).rev().find(|&c| self.chunks[c].lock().unwrap().first <= track).unwrap_or(0)
    }

    /// The next tick with anything due: a track's records or a pending note-off.
    pub(super) fn next_tick(&self) -> Option<u32> {
        let tracks = self.chunks.iter().filter_map(|c| c.lock().unwrap().heap.peek_min());
        let offs = self.parts.iter().filter_map(|p| p.lock().unwrap().heap.peek_min());
        tracks.chain(offs).min()
    }
}

impl Chunk {
    /// Read the records of every track in the window `first..end`.
    fn parse(&mut self, song: &SongData, first: u32, end: u32, part_of: &[u8; SLOTS]) {
        for b in &mut self.buckets {
            b.clear();
        }
        self.msgs.clear();
        self.marks.clear();
        self.sysex.clear();
        self.bits.clear();
        (self.decoded, self.played, self.repeats) = (0, 0, 0);
        let started = Instant::now();
        let mut seq = 0u32;
        while let Some(t) = self.heap.min_key_below(end) {
            self.heap.take_min(&mut self.due_buf);
            // Same order as the file within a tick.
            self.due_buf.sort_unstable();
            let rel = t - first;
            self.marks.push(TickMark { rel, seq });
            for &i in &self.due_buf {
                let data = song.midi.track(i as usize);
                let copies = song.midi.track_copies[i as usize];
                let local = i as usize - self.first;
                let before = seq;
                let r = &mut self.readers[local];
                while let Some(rec) = r.next_at(data, t) {
                    match rec {
                        Record::Note { ch, key, vel, dur } => {
                            self.decoded += 1;
                            self.played += copies;
                            self.repeats += copies - 1;
                            let slot = Skipper::slot(ch, key);
                            self.msgs.push(pack_short(0x90 | ch, key, vel));
                            self.buckets[part_of[slot] as usize].push(NoteEv {
                                dur: dur.unwrap_or(NO_DUR),
                                seq,
                                at: rel << 11 | slot as u32,
                            });
                        }
                        Record::Other { status, d1, d2 } => {
                            self.msgs.push(pack_short(status, d1, d2));
                            set_bit(&mut self.bits, seq);
                        }
                        Record::Sysex { status, at, len } => {
                            self.msgs.push(sysex_marker(self.sysex.len()));
                            self.sysex.push(SysexRef { status, at, len });
                            set_bit(&mut self.bits, seq);
                        }
                    }
                    seq += 1;
                }
                self.load[local] += seq - before;
                if let Some(next) = r.peek_tick(data) {
                    self.heap.push(next, i);
                }
            }
            self.due_buf.clear();
        }
        self.bits.resize((seq as usize).div_ceil(64), 0);
        self.busy += started.elapsed();
    }
}

impl Chunk {
    /// Make the messages to send: the records not skipped, tick by tick.
    fn collect(&mut self, parts: &[&Part], c: usize) {
        let started = Instant::now();
        self.out.clear();
        self.segs.clear();
        for (i, m) in self.marks.iter().enumerate() {
            let to = self.marks.get(i + 1).map_or(self.msgs.len() as u32, |m| m.seq);
            let start = self.out.len() as u32;
            let (first_w, last_w) = ((m.seq >> 6) as usize, ((to + 63) >> 6) as usize);
            for w in first_w..last_w {
                let mut word = self.bits[w];
                for p in parts {
                    word |= p.sent_bits[c][w];
                }
                if w == first_w {
                    word &= !0u64 << (m.seq & 63);
                }
                if w == (to >> 6) as usize {
                    word &= (1u64 << (to & 63)) - 1;
                }
                while word != 0 {
                    self.out.push(self.msgs[(w << 6) + word.trailing_zeros() as usize]);
                    word &= word - 1;
                }
            }
            self.segs.push(Seg { rel: m.rel, start, end: self.out.len() as u32 });
        }
        self.busy += started.elapsed();
    }
}

fn set_bit(bits: &mut Vec<u64>, i: u32) {
    let w = (i >> 6) as usize;
    if bits.len() <= w {
        bits.resize(w + 1, 0);
    }
    bits[w] |= 1 << (i & 63);
}

impl Part {
    /// Send the note-offs due below `limit` (in order of tick); `events_at` is the tick whose
    /// notes were just played, if any (its note-offs are of zero-length notes).
    #[inline]
    fn offs_below(&mut self, limit: u32, events_at: u32, first: u32) {
        let mut due = std::mem::take(&mut self.due_buf);
        while let Some(d) = self.heap.min_key_below(limit) {
            self.heap.take_min(&mut due);
            let step = if d == events_at { 2 } else { 0 };
            let start = self.offs.len();
            for (ch, key) in due.drain(..) {
                let slot = Skipper::slot(ch, key);
                match self.skipper.note_off(slot, d) {
                    OffAction::Send => {
                        self.poly -= 1;
                        self.offs.push(OffOut { key: off_key(d - first, step, slot), msg: pack_short(0x80 | ch, key, 0) });
                    }
                    OffAction::Postpone(t2) => self.heap.push(t2, (ch, key)),
                    OffAction::Hold => {}
                }
            }
            self.offs[start..].sort_unstable_by_key(|o| o.key);
        }
        self.due_buf = due;
    }

    /// Play the notes the chunks gave this partition (`p`), and the note-offs due before `end`.
    fn run(&mut self, chunks: &[&Chunk], p: usize, first: u32, end: u32, time: TimeOf) {
        let started = Instant::now();
        self.offs.clear();
        for (bits, c) in self.sent_bits.iter_mut().zip(chunks) {
            bits.clear();
            bits.resize(c.bits.len(), 0);
        }
        let mut heads: [&[NoteEv]; MAX_THREADS] = [&[]; MAX_THREADS];
        for (head, c) in heads.iter_mut().zip(chunks) {
            *head = &c.buckets[p];
        }
        let heads = &mut heads[..chunks.len()];
        let interval = self.skipper.interval_us;
        let mut events_at = u32::MAX;
        loop {
            let Some(rel) = heads.iter().filter_map(|h| h.first()).map(|e| e.at >> 11).min() else { break };
            let t = first + rel;
            self.offs_below(t + 1, events_at, first);
            events_at = t;
            let (tick_us, us_per_tick) = time(t, &mut self.seg);
            for (c, head) in heads.iter_mut().enumerate() {
                let n = head.iter().take_while(|e| e.at >> 11 == rel).count();
                let (now, rest) = head.split_at(n);
                *head = rest;
                for e in now {
                    let slot = (e.at & 0x7FF) as usize;
                    let off_at = (e.dur != NO_DUR).then(|| t + e.dur);
                    let dur_us = match off_at {
                        Some(_) if interval > 0 => (e.dur as f32 * us_per_tick) as u64,
                        _ => 0,
                    };
                    if self.skipper.note_on(slot, tick_us, off_at, dur_us) {
                        self.sent += 1;
                        self.poly += 1;
                        if let Some(off_at) = off_at {
                            self.heap.push(off_at, ((slot >> 7) as u8, (slot & 0x7F) as u8));
                        }
                        self.sent_bits[c][(e.seq >> 6) as usize] |= 1 << (e.seq & 63);
                    }
                }
            }
            self.offs_below(t + 1, events_at, first);
        }
        self.offs_below(end, events_at, first);
        self.busy += started.elapsed();
    }
}

impl Engine {
    /// Hand the notes' slots and the tracks over to `threads` partitions and chunks. (Does
    /// nothing if it already has.)
    pub(super) fn enter_parallel(&mut self, threads: usize) {
        if self.par.is_some() {
            return;
        }
        let par = Parallel::new(threads, &self.cur.readers);
        {
            let mut parts: Vec<_> = par.parts.iter().map(|p| p.lock().unwrap()).collect();
            for s in 0..SLOTS {
                let w = &mut parts[par.part_of[s] as usize].skipper;
                w.last_on_us[s] = self.skipper.last_on_us[s];
                w.sounding[s] = self.skipper.sounding[s];
                w.hold_until[s] = self.skipper.hold_until[s];
            }
            let old = std::mem::replace(&mut self.cur.due, RadixHeap::new());
            for (k, d) in old.buckets.into_iter().flatten() {
                match d {
                    Due::Track(i) => par.chunks[par.chunk_of(i as usize)].lock().unwrap().heap.push(k, i),
                    Due::Off((ch, key)) => parts[par.part_of[Skipper::slot(ch, key)] as usize].heap.push(k, (ch, key)),
                }
            }
        }
        self.par = Some(par);
    }

    /// Take everything back (after which the serial `process_tick` carries on).
    pub(super) fn leave_parallel(&mut self) {
        let Some(par) = self.par.take() else { return };
        let mut parts: Vec<_> = par.parts.iter().map(|p| p.lock().unwrap()).collect();
        for s in 0..SLOTS {
            let w = &parts[par.part_of[s] as usize].skipper;
            self.skipper.last_on_us[s] = w.last_on_us[s];
            self.skipper.sounding[s] = w.sounding[s];
            self.skipper.hold_until[s] = w.hold_until[s];
        }
        for part in &mut parts {
            self.skipper.skipped += std::mem::take(&mut part.skipper.skipped);
            for (s, n) in part.skipper.demand.iter_mut().enumerate() {
                self.skipper.demand[s] += std::mem::take(n);
            }
            for (k, off) in std::mem::replace(&mut part.heap, RadixHeap::new()).buckets.into_iter().flatten() {
                self.cur.due.push(k, Due::Off(off));
            }
        }
        for chunk in &par.chunks {
            let mut chunk = chunk.lock().unwrap();
            let first = chunk.first;
            for (i, r) in chunk.readers.iter().enumerate() {
                self.cur.readers[first + i] = *r;
            }
            for (k, i) in std::mem::replace(&mut chunk.heap, RadixHeap::new()).buckets.into_iter().flatten() {
                self.cur.due.push(k, Due::Track(i));
            }
        }
    }

    /// Seconds each chunk and partition has been busy.
    pub(super) fn busy(&self) -> (Vec<f64>, Vec<f64>) {
        let par = self.par.as_ref().unwrap();
        (
            par.chunks.iter().map(|c| c.lock().unwrap().busy.as_secs_f64()).collect(),
            par.parts.iter().map(|c| c.lock().unwrap().busy.as_secs_f64()).collect(),
        )
    }

    /// `Skipper::regulate`, with the demand the partitions have counted.
    pub(super) fn regulate(&mut self, lag_us: u64, now: Instant, costs: Costs) {
        if let Some(par) = &mut self.par {
            if now.saturating_duration_since(self.skipper.window_start).as_secs_f32() >= Skipper::STEP {
                for part in &par.parts {
                    for (s, n) in part.lock().unwrap().skipper.demand.iter_mut().enumerate() {
                        self.skipper.demand[s] += *n;
                        par.slot_load[s] = par.slot_load[s].saturating_add(std::mem::take(n));
                    }
                }
            }
        }
        self.skipper.regulate(lag_us, now, costs);
    }

    /// Play every tick below `end_tick` that has anything due: the messages go to `batch`
    /// and, after each tick's, its tick and where in `batch` its messages end go to `ends`;
    /// SysEx goes to `self.sysex`.
    pub(super) fn process_window(
        &mut self,
        song: &SongData,
        end_tick: u32,
        time: TimeOf,
        batch: &mut Vec<u32>,
        ends: &mut Vec<(u32, u32)>,
    ) {
        let par = self.par.as_mut().expect("process_window needs enter_parallel");
        let Some(first) = par.next_tick() else { return };
        let end = end_tick.min(first.saturating_add(MAX_WINDOW_TICKS)).max(first + 1);
        let interval = self.skipper.interval_us;
        let mut clock = Instant::now();
        let mut lap = |par: &mut Parallel, i: usize| {
            par.spent[i] += clock.elapsed();
            clock = Instant::now();
        };

        // Stages 1 and 2.
        {
            let Parallel { team, chunks, part_of, .. } = &mut *par;
            let part_of: &[u8; SLOTS] = part_of;
            team.run(&|c| chunks[c].lock().unwrap().parse(song, first, end, part_of));
        }
        lap(par, 0);
        {
            let Parallel { team, chunks, parts, .. } = &mut *par;
            let views: Vec<&Chunk> = chunks.iter_mut().map(|c| &*c.get_mut().unwrap()).collect();
            team.run(&|p| {
                let mut part = parts[p].lock().unwrap();
                part.skipper.interval_us = interval;
                part.run(&views, p, first, end, time);
            });
        }
        lap(par, 1);

        // Stage 3.
        {
            let Parallel { team, chunks, parts, .. } = &mut *par;
            let views: Vec<&Part> = parts.iter_mut().map(|p| &*p.get_mut().unwrap()).collect();
            team.run(&|c| chunks[c].lock().unwrap().collect(&views, c));
        }
        lap(par, 2);
        let chunks: Vec<&mut Chunk> = par.chunks.iter_mut().map(|c| c.get_mut().unwrap()).collect();
        let mut parts: Vec<&mut Part> = par.parts.iter_mut().map(|p| p.get_mut().unwrap()).collect();
        let mut poly = 0;
        for chunk in chunks.iter() {
            self.decoded += chunk.decoded;
            self.notes_played += chunk.played;
            self.notes_this_second += chunk.played;
            self.repeats_skipped += chunk.repeats;
        }
        for part in parts.iter_mut() {
            self.sent += std::mem::take(&mut part.sent);
            poly += std::mem::take(&mut part.poly);
            self.skipper.skipped += std::mem::take(&mut part.skipper.skipped);
        }
        par.cursors.iter_mut().for_each(|c| *c = 0);
        par.off_cursors.iter_mut().for_each(|c| *c = 0);
        loop {
            // The next step: a tick's note-offs due (0), its records (1), or its note-offs of
            // zero-length notes (2).
            let mut next = u64::MAX;
            for (part, &i) in parts.iter().zip(&par.off_cursors) {
                if let Some(o) = part.offs.get(i) {
                    next = next.min((o.key >> 11) as u64);
                }
            }
            for (chunk, &i) in chunks.iter().zip(&par.cursors) {
                if let Some(s) = chunk.segs.get(i) {
                    next = next.min((s.rel as u64) << 2 | 1);
                }
            }
            if next == u64::MAX {
                break;
            }
            let (rel, step) = ((next >> 2) as u32, next & 3);
            if step == 1 {
                for (chunk, i) in chunks.iter().zip(par.cursors.iter_mut()) {
                    let Some(s) = chunk.segs.get(*i).filter(|s| s.rel == rel) else { continue };
                    *i += 1;
                    let msgs = &chunk.out[s.start as usize..s.end as usize];
                    if chunk.sysex.is_empty() {
                        batch.extend_from_slice(msgs);
                    } else {
                        for &msg in msgs {
                            batch.push(if msg as u8 == 0xF0 {
                                self.sysex.push(chunk.sysex[(msg >> 8) as usize]);
                                sysex_marker(self.sysex.len() - 1)
                            } else {
                                msg
                            });
                        }
                    }
                }
            } else {
                // Each partition has a range of slots and its note-offs are by slot, so they
                // follow one another in order.
                for (part, i) in parts.iter().zip(par.off_cursors.iter_mut()) {
                    while let Some(o) = part.offs.get(*i).filter(|o| (o.key >> 11) as u64 == next) {
                        batch.push(o.msg);
                        *i += 1;
                    }
                }
            }
            let (tick, len) = (first + rel, batch.len() as u32);
            if let Some(last) = ends.last_mut().filter(|l| l.0 == tick) {
                last.1 = len;
            } else if ends.last().map_or(0, |e| e.1) != len {
                ends.push((tick, len));
            }
        }
        self.polyphony = (self.polyphony as i64 + poly).max(0) as u32;
        self.peak_polyphony = self.peak_polyphony.max(self.polyphony);
        lap(par, 3);
        par.windows += 1;
        if par.windows % if par.eager { 1 } else { 32 } == 0 {
            par.rebalance();
            par.rebalance_slots(&mut self.skipper);
        }
    }
}
