use std::fs::File;
use std::io::{Error as IoError, Read};
use std::path::Path;

use rayon::prelude::*;

// ------------------------------------------------------------------
// Error Type
// ------------------------------------------------------------------
#[derive(Debug)]
pub struct ParseError(String);

impl From<IoError> for ParseError {
    fn from(e: IoError) -> Self {
        ParseError(e.to_string())
    }
}
impl From<&str> for ParseError {
    fn from(s: &str) -> Self {
        ParseError(s.to_string())
    }
}
impl From<String> for ParseError {
    fn from(s: String) -> Self {
        ParseError(s)
    }
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MIDI Parse Error: {}", self.0)
    }
}

// ------------------------------------------------------------------
// Packed track format
// ------------------------------------------------------------------
//
// Each track is re-encoded into a compact byte stream, one record per note:
//
//   record  = vlv(delta << 2 | flags) [status] data
//   status  = only written when it differs from the previous record's (running status)
//   data    = 0x9n note:   key, [velocity], [vlv(duration + 1)]   (duration 0 = never released)
//             0xAn/Bn/En:  d1, d2
//             0xCn/Dn:     d1
//   flags   = bit 0: velocity present, bit 1: duration present (notes only).
//             When absent, the note reuses the track's previous velocity / duration
//             (running velocity / running duration).
//
// Each track's bytes are then cut into chunks (~256 bytes, always at a record boundary,
// at content-defined points so a repeated passage is cut the same way wherever it
// appears) and every distinct chunk is stored once: a track is a list of chunk ids.
// Merged black MIDIs repeat themselves a lot, so this shrinks them several times over
// at no decoding cost. Decoding state carries across chunks, so a chunk can be shared
// between places with different running velocity, duration or time.
//
// Note-offs (0x8n and 0x9n vel 0) are not stored: they're folded into the note's
// duration, and playback regenerates them. Meta events except tempo and SysEx are
// dropped (never sent during playback). Deltas are mostly < 32, so the flags ride
// along for free: a note repeating its track's velocity and length costs 2 bytes.

/// One decoded record of a packed track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Record {
    /// `dur` is None for a note that is never released.
    Note { ch: u8, key: u8, vel: u8, dur: Option<u32> },
    Other { status: u8, d1: u8, d2: u8 },
}

/// A packed track: its chunks, in order (see `MidiFile::track`).
#[derive(Clone, Copy)]
pub struct Track<'a> {
    refs: &'a [u32],
    chunks: &'a [Chunk],
    store: &'a [u8],
}

/// A distinct chunk's place in `MidiFile::store`.
#[derive(Clone, Copy, Debug)]
pub struct Chunk {
    start: u64,
    len: u32,
}

/// Read position in a packed track. Cheap to copy, so callers can peek ahead.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackReader {
    /// Position in `MidiFile::store`, and the end of the current chunk there. Within a
    /// chunk, reading is a plain walk through the store.
    pos: usize,
    end: usize,
    /// The next chunk to enter (index into the track's chunk list).
    next_chunk: u32,
    /// Absolute tick of the last record read.
    tick: u32,
    running: u8,
    /// Running velocity and encoded duration (duration + 1, 0 = never released).
    vel: u8,
    dur: u32,
}

impl TrackReader {
    /// Where the next record starts, the end of its chunk, and the next chunk to enter
    /// after that. Records never span chunks, and chunks are never empty.
    #[inline]
    fn locate(&self, t: Track) -> Option<(usize, usize, u32)> {
        if self.pos < self.end {
            return Some((self.pos, self.end, self.next_chunk));
        }
        let c = t.chunks[*t.refs.get(self.next_chunk as usize)? as usize];
        let start = c.start as usize;
        Some((start, start + c.len as usize, self.next_chunk + 1))
    }

    /// Tick of the next record, without consuming it.
    #[inline]
    pub fn peek_tick(&self, t: Track) -> Option<u32> {
        let (pos, _, _) = self.locate(t)?;
        let (v, _) = get_vlv(t.store, pos);
        Some(self.tick.wrapping_add((v >> 2) as u32))
    }

    /// Decode the next record and its absolute tick.
    #[inline]
    pub fn next(&mut self, t: Track) -> Option<(u32, Record)> {
        let (pos, end, next_chunk) = self.locate(t)?;
        self.end = end;
        self.next_chunk = next_chunk;
        let (v, p) = get_vlv(t.store, pos);
        self.tick = self.tick.wrapping_add((v >> 2) as u32);
        Some((self.tick, self.body(t.store, v, p)))
    }

    /// Decode a record's status and data, starting at `p` after its delta+flags `v`.
    #[inline]
    fn body(&mut self, data: &[u8], v: u64, mut p: usize) -> Record {
        if data[p] & 0x80 != 0 {
            self.running = data[p];
            p += 1;
        }
        let status = self.running;
        let rec = match status & 0xF0 {
            0x90 => {
                let key = data[p];
                p += 1;
                if v & 1 != 0 {
                    self.vel = data[p];
                    p += 1;
                }
                if v & 2 != 0 {
                    let (dur, end) = get_vlv(data, p);
                    self.dur = dur as u32;
                    p = end;
                }
                Record::Note {
                    ch: status & 0x0F,
                    key,
                    vel: self.vel,
                    dur: self.dur.checked_sub(1),
                }
            }
            0xC0 | 0xD0 => {
                p += 1;
                Record::Other { status, d1: data[p - 1], d2: 0 }
            }
            _ => {
                p += 2;
                Record::Other { status, d1: data[p - 2], d2: data[p - 1] }
            }
        };
        self.pos = p;
        rec
    }
}

#[derive(Debug)]
pub struct MidiFile {
    pub division: u16,
    /// Each track's chunk ids, in order (see format above and `track`).
    pub tracks: Vec<Vec<u32>>,
    /// Distinct chunks: where each lives in `store`.
    pub chunks: Vec<Chunk>,
    /// Every distinct chunk's bytes, stored once.
    pub store: Vec<u8>,
    /// Size of the packed tracks before deduplication.
    pub logical_bytes: u64,
    /// (tick, microseconds per quarter) for every Set Tempo meta event, in track order.
    pub tempos: Vec<(u32, u32)>,
    /// Tick of the last event of any kind, across all tracks.
    pub end_tick: u32,
    pub note_count: u64,
    /// Lowest and highest key played, if there are any notes.
    pub pitch_range: Option<(u8, u8)>,
    /// Size of the (uncompressed) MIDI data.
    pub file_bytes: u64,
    /// Time spent parsing and packing the tracks.
    pub parse_time: std::time::Duration,
    /// When loaded from an archive: its format(s), outermost first ("7z › xz"), and
    /// size on disk.
    pub source: Option<(String, u64)>,
    /// How notes packed (see `PackStats`).
    pub pack_stats: PackStats,
}

/// What it took to pack the notes: a note is 2 bytes (delta+flags, key) unless it
/// needs any of these extras. Shown in the load info, to guide further compression.
#[derive(Clone, Copy, Debug, Default)]
pub struct PackStats {
    pub notes: u64,
    /// Notes that repeated their track's velocity and duration and fit a 1-byte delta.
    pub two_byte: u64,
    pub new_velocity: u64,
    pub new_duration: u64,
    /// Bytes spent on durations (when not repeated).
    pub duration_bytes: u64,
    /// Notes whose delta+flags needed more than 1 byte.
    pub long_delta: u64,
    /// Notes that needed a status byte (channel change, or after another event type).
    pub status_byte: u64,
    /// Bytes of non-note events (controllers, pitch bend, ...).
    pub other_bytes: u64,
}

impl PackStats {
    fn add(&mut self, o: &PackStats) {
        self.notes += o.notes;
        self.two_byte += o.two_byte;
        self.new_velocity += o.new_velocity;
        self.new_duration += o.new_duration;
        self.duration_bytes += o.duration_bytes;
        self.long_delta += o.long_delta;
        self.status_byte += o.status_byte;
        self.other_bytes += o.other_bytes;
    }
}

impl MidiFile {
    /// Memory used by the packed song: chunk bytes, chunk table and track chunk lists.
    pub fn packed_bytes(&self) -> u64 {
        let refs: usize = self.tracks.iter().map(|t| t.len()).sum();
        (self.store.len() + self.chunks.len() * std::mem::size_of::<Chunk>() + refs * 4) as u64
    }

    pub fn track(&self, i: usize) -> Track<'_> {
        Track {
            refs: &self.tracks[i],
            chunks: &self.chunks,
            store: &self.store,
        }
    }
}

// ------------------------------------------------------------------
// Public entry
// ------------------------------------------------------------------
impl MidiFile {
    /// Open and parse a MIDI file, packing tracks in parallel. Each worker thread reads
    /// one track at a time into a reused buffer with its own file handle, so raw bytes
    /// never pile up in memory and threads don't contend on page faults.
    /// `vel_levels` (1..=127) quantizes velocities: 127 keeps them exact, fewer levels
    /// make running velocity hit more often (1 = every note the same velocity).
    /// `progress` is increased by each track's byte size as it's packed (for a progress bar).
    pub fn open_and_parse<P: AsRef<Path>>(
        path: P,
        vel_levels: u8,
        progress: &std::sync::atomic::AtomicU64,
    ) -> Result<Self, ParseError> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let file_bytes = file.metadata()?.len();

        let mut hdr = [0u8; 14];
        let _ = read_at(&file, 0, &mut hdr);
        if &hdr[..4] != b"MThd" {
            // Not a plain MIDI: maybe an archive or compressed file containing one.
            return crate::archive::open(path, vel_levels, progress);
        }
        let (num_tracks, division) = parse_header(&hdr)?;

        // Step 1: locate the track chunks (offset, length).
        let mut chunks: Vec<(u64, usize)> = Vec::with_capacity(num_tracks as usize);
        let mut pos = 14u64;
        for _ in 0..num_tracks {
            let mut h = [0u8; 8];
            if read_at(&file, pos, &mut h).is_err() {
                return Err("Truncated MIDI file".into());
            }
            if &h[..4] != b"MTrk" {
                return Err("Invalid MTrk header".into());
            }
            let len = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as u64;
            if pos + 8 + len > file_bytes {
                return Err("Truncated MIDI file".into());
            }
            chunks.push((pos + 8, len as usize));
            pos += 8 + len;
        }

        // Step 2: read and pack each track in parallel.
        let parse_start = std::time::Instant::now();
        let dedup = Dedup::new();
        let budget = ByteBudget::new();
        let packed: Vec<PackedTrack> = chunks
            .into_par_iter()
            .map_init(
                || (File::open(path), Vec::new(), Scratch::default()),
                |(file, raw, scratch), (offset, len)| {
                    let file = file.as_ref().map_err(|e| ParseError(e.to_string()))?;
                    budget.acquire(len as u64); // never closed here: always succeeds
                    raw.resize(len, 0);
                    let read = read_at(file, offset, raw);
                    let packed = read.map_err(ParseError::from).and_then(|_| pack_track(raw, vel_levels, scratch, &dedup));
                    scratch.trim();
                    // Big tracks would otherwise keep this thread's buffer at their size.
                    if raw.len() > 16 << 20 {
                        *raw = Vec::new();
                    }
                    budget.release(len as u64);
                    progress.fetch_add(len as u64 + 8, std::sync::atomic::Ordering::Relaxed);
                    packed
                },
            )
            .collect::<Result<_, _>>()?;
        Ok(Self::assemble(division, packed, dedup, file_bytes, parse_start.elapsed()))
    }

    /// Parse a MIDI file from a sequential stream (e.g. a decompressor). One thread
    /// reads track chunks while the thread pool packs them; a bounded queue keeps only
    /// a few raw chunks in memory at a time.
    pub(crate) fn parse_stream(mut r: impl Read, vel_levels: u8) -> Result<Self, ParseError> {
        let mut hdr = [0u8; 14];
        if r.read_exact(&mut hdr).is_err() || &hdr[..4] != b"MThd" {
            return Err("Not a MIDI file (invalid MThd header)".into());
        }
        let (num_tracks, division) = parse_header(&hdr)?;

        let parse_start = std::time::Instant::now();
        let mut file_bytes = 14u64;
        let dedup = Dedup::new();
        let budget = ByteBudget::new();
        let (tx, rx) = crossbeam_channel::bounded::<(usize, Vec<u8>)>(rayon::current_num_threads() * 2);
        let (read_result, packed) = std::thread::scope(|s| {
            let (dedup, budget) = (&dedup, &budget);
            let packer = s.spawn(move || -> Result<Vec<(usize, PackedTrack)>, ParseError> {
                let packed = rx
                    .into_iter()
                    .par_bridge()
                    .map_init(Scratch::default, |scratch, (i, raw)| {
                        let packed = pack_track(&raw, vel_levels, scratch, dedup).map(|t| (i, t));
                        scratch.trim();
                        budget.release(raw.len() as u64);
                        packed
                    })
                    .collect();
                budget.close();
                packed
            });
            let read_result = (|| -> Result<(), ParseError> {
                for i in 0..num_tracks as usize {
                    let mut h = [0u8; 8];
                    r.read_exact(&mut h).map_err(|_| "Truncated MIDI file")?;
                    if &h[..4] != b"MTrk" {
                        return Err("Invalid MTrk header".into());
                    }
                    let len = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
                    if !budget.acquire(len as u64) {
                        break; // the packer stopped on an error; it reports it
                    }
                    let mut raw = vec![0u8; len];
                    r.read_exact(&mut raw).map_err(|_| "Truncated MIDI file")?;
                    file_bytes += 8 + len as u64;
                    if tx.send((i, raw)).is_err() {
                        break; // the packer stopped on an error; it reports it
                    }
                }
                Ok(())
            })();
            drop(tx);
            (read_result, packer.join().unwrap_or_else(|_| Err("Packing thread panicked".into())))
        });
        let mut packed = packed?;
        read_result?;
        packed.sort_unstable_by_key(|(i, _)| *i);
        Ok(Self::assemble(
            division,
            packed.into_iter().map(|(_, t)| t),
            dedup,
            file_bytes,
            parse_start.elapsed(),
        ))
    }

    fn assemble(
        division: u16,
        packed: impl IntoIterator<Item = PackedTrack>,
        dedup: Dedup,
        file_bytes: u64,
        parse_time: std::time::Duration,
    ) -> Self {
        let mut packed: Vec<PackedTrack> = packed.into_iter().collect();
        let (store, chunks) = dedup.finish(packed.iter_mut().map(|t| &mut t.refs));
        let mut file = MidiFile {
            division,
            tracks: Vec::new(),
            chunks,
            store,
            logical_bytes: 0,
            tempos: Vec::new(),
            end_tick: 0,
            note_count: 0,
            pitch_range: None,
            file_bytes,
            parse_time,
            source: None,
            pack_stats: PackStats::default(),
        };
        for t in packed {
            file.end_tick = file.end_tick.max(t.end_tick);
            file.note_count += t.note_count;
            file.pitch_range = match (file.pitch_range, t.pitch_range) {
                (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
                (a, b) => a.or(b),
            };
            file.pack_stats.add(&t.stats);
            file.tempos.extend(t.tempos);
            file.logical_bytes += t.logical_bytes;
            file.tracks.push(t.refs);
        }
        file
    }
}

/// Validate an MThd header; returns (track count, ticks per quarter).
fn parse_header(hdr: &[u8; 14]) -> Result<(u16, u16), ParseError> {
    if hdr[4..8] != [0, 0, 0, 6] {
        return Err("Invalid MIDI header length (not 6)".into());
    }
    let num_tracks = u16::from_be_bytes([hdr[10], hdr[11]]);
    let division = u16::from_be_bytes([hdr[12], hdr[13]]);
    if (division & 0x8000) != 0 {
        return Err("SMTPE time division not supported".into());
    }
    if division == 0 {
        return Err("Invalid time division (0 ticks per quarter)".into());
    }
    Ok((num_tracks, division))
}

// ------------------------------------------------------------------
// Per-track packing
// ------------------------------------------------------------------

struct PackedTrack {
    stats: PackStats,
    /// Chunk ids from `Dedup::insert` (remapped by `assemble`).
    refs: Vec<u32>,
    logical_bytes: u64,
    tempos: Vec<(u32, u32)>,
    end_tick: u32,
    note_count: u64,
    pitch_range: Option<(u8, u8)>,
}

enum RawEvent {
    Channel { status: u8, d1: u8, d2: u8 },
    Tempo(u32),
}

/// Caps raw track bytes held in memory at once while loading (a single larger track
/// is still let through on its own).
struct ByteBudget {
    /// Bytes in use, and whether the consumer has stopped (then nobody waits).
    used: std::sync::Mutex<(u64, bool)>,
    freed: std::sync::Condvar,
}

impl ByteBudget {
    const LIMIT: u64 = 256 << 20;

    fn new() -> Self {
        Self { used: Default::default(), freed: Default::default() }
    }

    /// Wait until `n` more bytes fit; false if the budget was closed meanwhile.
    fn acquire(&self, n: u64) -> bool {
        let mut used = self.used.lock().unwrap();
        while !used.1 && used.0 > 0 && used.0 + n > Self::LIMIT {
            used = self.freed.wait(used).unwrap();
        }
        used.0 += n;
        !used.1
    }

    fn release(&self, n: u64) {
        self.used.lock().unwrap().0 -= n;
        self.freed.notify_all();
    }

    /// The consumer is done (e.g. stopped on an error): wake and fail any waiter.
    fn close(&self) {
        self.used.lock().unwrap().1 = true;
        self.freed.notify_all();
    }
}

/// Stores each distinct chunk once while tracks are packed in parallel. Sharded by
/// hash, so threads rarely wait on each other; a hash match is only shared after the
/// bytes compare equal, so a collision just stores a duplicate.
struct Dedup {
    shards: Vec<std::sync::Mutex<Shard>>,
}

#[derive(Default)]
struct Shard {
    index: std::collections::HashMap<u64, u32>,
    chunks: Vec<Chunk>,
    bytes: Vec<u8>,
}

impl Dedup {
    const SHARD_BITS: u32 = 6;

    fn new() -> Self {
        Self {
            shards: (0..1 << Self::SHARD_BITS).map(|_| Default::default()).collect(),
        }
    }

    /// Id of `chunk` (stored if new): its index in its shard, and the shard.
    fn insert(&self, chunk: &[u8]) -> u32 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        chunk.hash(&mut h);
        let hash = h.finish();
        let s = (hash & ((1 << Self::SHARD_BITS) - 1)) as usize;
        let id = |local: u32| local << Self::SHARD_BITS | s as u32;
        let mut shard = self.shards[s].lock().unwrap();
        if let Some(&local) = shard.index.get(&hash) {
            let c = shard.chunks[local as usize];
            if &shard.bytes[c.start as usize..c.start as usize + c.len as usize] == chunk {
                return id(local);
            }
        }
        let local = shard.chunks.len() as u32;
        assert!(local < 1 << (32 - Self::SHARD_BITS), "too many distinct chunks");
        let start = shard.bytes.len() as u64;
        shard.bytes.extend_from_slice(chunk);
        shard.chunks.push(Chunk { start, len: chunk.len() as u32 });
        shard.index.entry(hash).or_insert(local);
        id(local)
    }

    /// Build the final store and chunk table, laying chunks out in order of first use
    /// so each track's chunks sit together (a track is read front to back, and memory
    /// that is read in order is fast). Rewrites `tracks`' ids into table indices.
    fn finish<'a>(self, tracks: impl Iterator<Item = &'a mut Vec<u32>>) -> (Vec<u8>, Vec<Chunk>) {
        let shards: Vec<Shard> = self.shards.into_iter().map(|m| m.into_inner().unwrap()).collect();
        let mut store = Vec::with_capacity(shards.iter().map(|s| s.bytes.len()).sum());
        let mut chunks = Vec::with_capacity(shards.iter().map(|s| s.chunks.len()).sum());
        let mut placed: Vec<Vec<u32>> = shards.iter().map(|s| vec![u32::MAX; s.chunks.len()]).collect();
        for refs in tracks {
            for id in refs.iter_mut() {
                let (s, local) = ((*id & ((1 << Self::SHARD_BITS) - 1)) as usize, (*id >> Self::SHARD_BITS) as usize);
                let slot = &mut placed[s][local];
                if *slot == u32::MAX {
                    let c = shards[s].chunks[local];
                    *slot = chunks.len() as u32;
                    chunks.push(Chunk { start: store.len() as u64, len: c.len });
                    store.extend_from_slice(&shards[s].bytes[c.start as usize..][..c.len as usize]);
                }
                *id = *slot;
            }
        }
        (store, chunks)
    }
}

/// Random table for the gear rolling hash that picks chunk boundaries.
const GEAR: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut i = 0;
    while i < 256 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t[i] = x;
        i += 1;
    }
    t
};

/// Per-thread buffers reused from track to track, so packing doesn't hammer the
/// allocator (which scales badly across threads).
#[derive(Default)]
struct Scratch {
    /// End tick per note-on ordinal (NONE = never released).
    note_ends: Vec<u32>,
    /// Next note-on ordinal in the same channel+key FIFO.
    next: Vec<u32>,
    data: Vec<u8>,
    /// End offset of each chunk in `data`.
    cuts: Vec<usize>,
}

impl Scratch {
    /// Drop buffers a huge track blew up (~10 bytes per note), instead of keeping that
    /// capacity on every thread for the rest of the load.
    fn trim(&mut self) {
        if self.note_ends.capacity() > 1 << 22 {
            *self = Scratch::default();
        }
    }
}

/// Pack one raw MTrk payload. Two passes over the raw bytes: the first pairs each
/// note-on with its note-off (FIFO per channel+key) to learn durations, the second
/// writes the records.
fn pack_track(
    buf: &[u8],
    vel_levels: u8,
    scratch: &mut Scratch,
    dedup: &Dedup,
) -> Result<PackedTrack, ParseError> {
    const NONE: u32 = u32::MAX;
    const NEVER: u32 = NONE;
    let Scratch { note_ends, next, data, cuts } = scratch;
    note_ends.clear();
    next.clear();
    data.clear();
    cuts.clear();

    // Pass 1: note end ticks, indexed by note-on ordinal.
    // Open notes per channel+key: FIFO as a linked list through `next` (head/tail ordinals).
    let mut head = [NONE; 16 * 128];
    let mut tail = [NONE; 16 * 128];
    let mut tempos = Vec::new();
    let mut pitch_range: Option<(u8, u8)> = None;
    let end_tick = walk_raw_track(buf, |tick, ev| match ev {
        RawEvent::Channel { status, d1, d2 } => {
            let slot = (status as usize & 0x0F) * 128 + (d1 as usize & 0x7F);
            match status & 0xF0 {
                0x90 if d2 > 0 => {
                    let ord = note_ends.len() as u32;
                    note_ends.push(NEVER);
                    next.push(NONE);
                    match tail[slot] {
                        NONE => head[slot] = ord,
                        t => next[t as usize] = ord,
                    }
                    tail[slot] = ord;
                    pitch_range = Some(match pitch_range {
                        Some((lo, hi)) => (lo.min(d1), hi.max(d1)),
                        None => (d1, d1),
                    });
                }
                0x80 | 0x90 => {
                    let ord = head[slot];
                    if ord != NONE {
                        note_ends[ord as usize] = tick;
                        head[slot] = next[ord as usize];
                        if head[slot] == NONE {
                            tail[slot] = NONE;
                        }
                    }
                }
                _ => {}
            }
        }
        RawEvent::Tempo(t) => tempos.push((tick, t)),
    })?;

    // Pass 2: write records.
    let mut last_tick = 0u32;
    let mut running = 0u8;
    let (mut running_vel, mut running_dur) = (0u8, 0u32);
    let mut ord = 0usize;
    let mut stats = PackStats::default();
    // Chunk boundaries: after a record, when the rolling hash of the bytes since the
    // last cut hits the mask (~256 bytes on average), within [MIN, MAX] bytes.
    const MASK: u64 = (1 << 8) - 1;
    const MIN: usize = 64;
    const MAX: usize = 1024;
    let (mut hash, mut chunk_start) = (0u64, 0usize);
    let mut cut_after = |data: &mut Vec<u8>, record_start: usize, cuts: &mut Vec<usize>| {
        for &b in &data[record_start..] {
            hash = (hash << 1).wrapping_add(GEAR[b as usize]);
        }
        let len = data.len() - chunk_start;
        if (len >= MIN && hash & MASK == 0) || len >= MAX {
            cuts.push(data.len());
            chunk_start = data.len();
            hash = 0;
        }
    };
    walk_raw_track(buf, |tick, ev| {
        let RawEvent::Channel { status, d1, d2 } = ev else {
            return;
        };
        let is_note_on = status & 0xF0 == 0x90 && d2 > 0;
        if status & 0xF0 == 0x80 || (status & 0xF0 == 0x90 && !is_note_on) {
            return; // note-offs are folded into durations
        }
        let delta = ((tick - last_tick) as u64) << 2;
        last_tick = tick;
        if !is_note_on {
            let start = data.len();
            put_vlv(data, delta);
            if status != running {
                data.push(status);
                running = status;
            }
            data.push(d1);
            if !matches!(status & 0xF0, 0xC0 | 0xD0) {
                data.push(d2);
            }
            stats.other_bytes += (data.len() - start) as u64;
            cut_after(data, start, cuts);
            return;
        }

        let vel = quantize_velocity(d2, vel_levels);
        let end = note_ends[ord];
        ord += 1;
        let dur = if end == NEVER { 0 } else { end - tick + 1 };
        let new_vel = vel != running_vel;
        let new_dur = dur != running_dur;
        let start = data.len();
        put_vlv(data, delta | new_vel as u64 | (new_dur as u64) << 1);
        let long_delta = data.len() - start > 1;
        let new_status = status != running;
        if new_status {
            data.push(status);
            running = status;
        }
        data.push(d1);
        if new_vel {
            data.push(vel);
            running_vel = vel;
        }
        if new_dur {
            let before = data.len();
            put_vlv(data, dur as u64);
            stats.duration_bytes += (data.len() - before) as u64;
            running_dur = dur;
        }
        stats.notes += 1;
        stats.new_velocity += new_vel as u64;
        stats.new_duration += new_dur as u64;
        stats.long_delta += long_delta as u64;
        stats.status_byte += new_status as u64;
        stats.two_byte += (data.len() - start == 2) as u64;
        cut_after(data, start, cuts);
    })?;
    if cuts.last().copied().unwrap_or(0) < data.len() {
        cuts.push(data.len());
    }

    let mut refs = Vec::with_capacity(cuts.len());
    let mut start = 0;
    for &end in cuts.iter() {
        refs.push(dedup.insert(&data[start..end]));
        start = end;
    }

    Ok(PackedTrack {
        stats,
        refs,
        logical_bytes: data.len() as u64,
        tempos,
        end_tick,
        note_count: note_ends.len() as u64,
        pitch_range,
    })
}

/// Walk a raw MTrk payload, calling `f` for channel events and tempo changes.
/// Returns the tick of the last event.
fn walk_raw_track(buf: &[u8], mut f: impl FnMut(u32, RawEvent)) -> Result<u32, ParseError> {
    let mut cursor: usize = 0;
    let len = buf.len();
    let mut abs_tick: u32 = 0;
    let mut running_status: u8 = 0;

    while cursor < len {
        // 1) delta (VLV)
        let (delta, delta_len) = read_vlv_from_buf(&buf[cursor..])?;
        cursor += delta_len;
        abs_tick = abs_tick.wrapping_add(delta);

        if cursor >= len {
            return Err("Unexpected end of track while reading status/data".into());
        }

        // 2) status / running status (only channel messages set running status)
        let mut status = buf[cursor];
        if status & 0x80 == 0 {
            status = running_status;
        } else {
            if status < 0xF0 {
                running_status = status;
            }
            cursor += 1;
        }

        match status {
            0x80..=0xBF | 0xE0..=0xEF => {
                if cursor + 2 > len {
                    return Err("Unexpected end of track in 2-byte MIDI event".into());
                }
                let (d1, d2) = (buf[cursor] & 0x7F, buf[cursor + 1] & 0x7F);
                f(abs_tick, RawEvent::Channel { status, d1, d2 });
                cursor += 2;
            }

            0xC0..=0xDF => {
                if cursor + 1 > len {
                    return Err("Unexpected end of track in 1-byte MIDI event".into());
                }
                let d1 = buf[cursor] & 0x7F;
                f(abs_tick, RawEvent::Channel { status, d1, d2: 0 });
                cursor += 1;
            }

            // SysEx: skipped (never sent during playback)
            0xF0 | 0xF7 => {
                let (sy_len, sy_len_len) = read_vlv_from_buf(&buf[cursor..])?;
                cursor += sy_len_len + sy_len as usize;
                if cursor > len {
                    return Err("Unexpected end of track in sysex".into());
                }
            }

            // Meta: only Set Tempo (0x51) is kept
            0xFF => {
                if cursor >= len {
                    return Err("Unexpected end of track in meta type".into());
                }
                let meta_type = buf[cursor];
                cursor += 1;
                let (m_len, m_len_len) = read_vlv_from_buf(&buf[cursor..])?;
                cursor += m_len_len;
                let data = buf
                    .get(cursor..cursor + m_len as usize)
                    .ok_or("Unexpected end of track in meta data")?;
                if meta_type == 0x51 && data.len() >= 3 {
                    f(abs_tick, RawEvent::Tempo(u32::from_be_bytes([0, data[0], data[1], data[2]])));
                }
                cursor += data.len();
            }

            _ => {
                return Err(format!("Unknown MIDI status byte: 0x{:X}", status).into());
            }
        }
    }

    Ok(abs_tick)
}

// ------------------------------------------------------------------
// Buffer helpers (no I/O)
// ------------------------------------------------------------------

/// Fill `buf` from `file` at `offset` (positioned read; doesn't touch the file cursor).
fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = file.seek_read(buf, offset)?;
        #[cfg(unix)]
        let n = file.read_at(buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Read a VLV from an in-memory slice. Returns (value, bytes_read).
fn read_vlv_from_buf(buf: &[u8]) -> Result<(u32, usize), ParseError> {
    let mut value: u32 = 0;
    for (i, &b) in buf.iter().enumerate().take(4) {
        value = (value << 7) | (b & 0x7F) as u32;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err("Invalid VLV (more than 4 bytes)".into())
}

/// Map a velocity (1..=127) onto `levels` evenly sized buckets, each represented by
/// its loudest velocity. 127 levels (or more) keeps velocities exact.
fn quantize_velocity(vel: u8, levels: u8) -> u8 {
    if levels >= 127 || vel == 0 {
        return vel;
    }
    let (v, n) = (vel as u32, levels.max(1) as u32);
    let bucket = (v - 1) * n / 127;
    (((bucket + 1) * 127 - 1) / n + 1) as u8
}

/// Write a MIDI-style VLV (big-endian 7-bit groups, up to 10 bytes for a u64).
fn put_vlv(out: &mut Vec<u8>, v: u64) {
    let mut shift = 63;
    while shift > 0 && v >> shift == 0 {
        shift -= 7;
    }
    while shift > 0 {
        out.push(((v >> shift) & 0x7F) as u8 | 0x80);
        shift -= 7;
    }
    out.push((v & 0x7F) as u8);
}

/// Read a VLV written by `put_vlv`. Returns (value, position after it).
#[inline]
fn get_vlv(data: &[u8], mut pos: usize) -> (u64, usize) {
    let mut v = 0u64;
    loop {
        let b = data[pos];
        pos += 1;
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return (v, pos);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_decodes_track() {
        let raw: &[u8] = &[
            0x00, 0x90, 60, 100, // C4 on @0
            0x0A, 60, 90, //        running status: second C4 on @10 (overlaps)
            0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo @10
            0x0A, 0x80, 60, 64, //  off @20 -> ends the first C4 (FIFO)
            0x00, 0xB3, 7, 99, //   CC on channel 3 @20
            0x00, 0xC3, 5, //       program change @20
            0x0A, 0x90, 60, 0, //   on-vel-0 off @30 -> ends the second C4
            0x81, 0x00, 0x91, 67, 1, // G4 on ch 1 @158, never released
            0x00, 0xFF, 0x2F, 0x00, // end of track @158
        ];
        let m = pack_one(raw);
        assert_eq!(m.tempos, [(10, 500_000)]);
        assert_eq!((m.end_tick, m.note_count, m.pitch_range), (158, 3, Some((60, 67))));

        let mut r = TrackReader::default();
        let mut got = vec![];
        while let Some(tick) = r.peek_tick(m.track(0)) {
            let (t2, rec) = r.next(m.track(0)).unwrap();
            assert_eq!(tick, t2);
            got.push((t2, rec));
        }
        use Record::*;
        assert_eq!(
            got,
            [
                (0, Note { ch: 0, key: 60, vel: 100, dur: Some(20) }),
                (10, Note { ch: 0, key: 60, vel: 90, dur: Some(20) }),
                (20, Other { status: 0xB3, d1: 7, d2: 99 }),
                (20, Other { status: 0xC3, d1: 5, d2: 0 }),
                (158, Note { ch: 1, key: 67, vel: 1, dur: None }),
            ]
        );
        // 5 records in 21 bytes, vs 38 raw (the second C4 reuses the running duration)
        assert_eq!((m.logical_bytes, raw.len()), (21, 38));
    }

    #[test]
    fn vlv_roundtrip() {
        for v in [0, 1, 127, 128, 16_383, 16_384, 1 << 28, u32::MAX as u64, u64::MAX] {
            let mut b = vec![];
            put_vlv(&mut b, v);
            assert_eq!(get_vlv(&b, 0), (v, b.len()));
        }
    }

    #[test]
    fn running_velocity_and_duration() {
        // 4 notes, same velocity and length, 1 tick apart: after the first, 2 bytes each.
        let mut raw = vec![0x00, 0x90, 60, 100, 0x10, 60, 0];
        for k in 61..64 {
            raw.extend_from_slice(&[0x00, k, 100, 0x10, k, 0]);
        }
        let m = pack_one(&raw);
        // first note: delta+flags, status, key, vel, dur = 5 bytes; then key + delta = 2
        assert_eq!(m.logical_bytes, 5 + 3 * 2);
        let mut r = TrackReader::default();
        let mut keys = vec![];
        while let Some((_, rec)) = r.next(m.track(0)) {
            assert!(matches!(rec, Record::Note { vel: 100, dur: Some(16), .. }));
            if let Record::Note { key, .. } = rec {
                keys.push(key);
            }
        }
        assert_eq!(keys, [60, 61, 62, 63]);
    }

    #[test]
    fn velocity_levels() {
        for v in 1..=127 {
            assert_eq!(quantize_velocity(v, 127), v);
            assert_eq!(quantize_velocity(v, 1), 127);
            let q = quantize_velocity(v, 8);
            assert!(q >= v && q - v < 16, "{v} -> {q}");
        }
        assert_eq!((quantize_velocity(64, 2), quantize_velocity(65, 2)), (64, 127));
    }

    /// Pack one raw track into a (one-track) MidiFile.
    fn pack_one(raw: &[u8]) -> MidiFile {
        let dedup = Dedup::new();
        let t = pack_track(raw, 127, &mut Scratch::default(), &dedup).unwrap();
        MidiFile::assemble(96, [t], dedup, 0, Default::default())
    }

    #[test]
    fn repeated_passages_are_stored_once() {
        // 3 tracks: the same 2000-note pattern twice, and once more with other velocities
        // (same bytes after the first note's explicit velocity, so mostly shared too).
        let track = |vel: u8| {
            let mut raw = vec![];
            for i in 0..2000u32 {
                let key = 30 + (i * 7 % 60) as u8;
                raw.extend_from_slice(&[0x00, 0x90, key, vel, 0x05, 0x80, key, 0]);
            }
            raw
        };
        let dedup = Dedup::new();
        let mut scratch = Scratch::default();
        let tracks: Vec<_> = [track(100), track(100), track(90)]
            .iter()
            .map(|raw| pack_track(raw, 127, &mut scratch, &dedup).unwrap())
            .collect();
        let m = MidiFile::assemble(96, tracks, dedup, 0, Default::default());
        assert!((m.store.len() as u64) < m.logical_bytes / 2, "{} of {}", m.store.len(), m.logical_bytes);
        // every track still decodes to the same notes
        for i in 0..3 {
            let mut r = TrackReader::default();
            let mut n = 0;
            while let Some((tick, rec)) = r.next(m.track(i)) {
                let Record::Note { key, dur, .. } = rec else { panic!() };
                assert_eq!((tick, key, dur), (n * 5, 30 + (n * 7 % 60) as u8, Some(5)));
                n += 1;
            }
            assert_eq!(n, 2000);
        }
    }
}
