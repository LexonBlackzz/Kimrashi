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
    /// one track at a time through a small reused window with its own file handle, so
    /// raw bytes never pile up in memory (even for one huge track) and threads don't
    /// contend on page faults.
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
        let packed: Vec<PackedTrack> = chunks
            .into_par_iter()
            .map_init(
                || (File::open(path), vec![0u8; WINDOW], Scratch::default()),
                |(file, window, scratch), (offset, len)| {
                    let file = file.as_ref().map_err(|e| ParseError(e.to_string()))?;
                    let src = FileAt { file, offset, progress };
                    let packed = pack_track(&mut Input::streaming(window, src, len), vel_levels, scratch, &dedup);
                    scratch.trim();
                    progress.fetch_add(8, std::sync::atomic::Ordering::Relaxed);
                    packed
                },
            )
            .collect::<Result<_, _>>()?;
        Ok(Self::assemble(division, packed, dedup, file_bytes, parse_start.elapsed()))
    }

    /// Parse a MIDI file from a sequential stream (e.g. a decompressor). One thread
    /// reads track chunks while the thread pool packs them; a bounded queue keeps only
    /// a few raw chunks in memory at a time. A track over `POOLED_MAX` is packed by the
    /// reading thread itself, straight off the stream, so it never sits in memory whole.
    pub(crate) fn parse_stream(r: impl Read, vel_levels: u8) -> Result<Self, ParseError> {
        Self::parse_stream_with(r, vel_levels, POOLED_MAX)
    }

    fn parse_stream_with(mut r: impl Read, vel_levels: u8, pooled_max: usize) -> Result<Self, ParseError> {
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
                    .map_init(Scratch::default, |scratch, (i, mut raw)| {
                        let packed = pack_track(&mut Input::whole(&mut raw), vel_levels, scratch, dedup).map(|t| (i, t));
                        scratch.trim();
                        budget.release(raw.len() as u64);
                        packed
                    })
                    .collect();
                budget.close();
                packed
            });
            // Returns the tracks it packed itself (the big ones).
            let read_result = (|| -> Result<Vec<(usize, PackedTrack)>, ParseError> {
                let mut own = Vec::new();
                let (mut window, mut scratch) = (Vec::new(), Scratch::default());
                for i in 0..num_tracks as usize {
                    let mut h = [0u8; 8];
                    r.read_exact(&mut h).map_err(|_| "Truncated MIDI file")?;
                    if &h[..4] != b"MTrk" {
                        return Err("Invalid MTrk header".into());
                    }
                    let len = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
                    file_bytes += 8 + len as u64;
                    if len > pooled_max {
                        window.resize(WINDOW, 0);
                        let mut input = Input::streaming(&mut window, r.by_ref().take(len as u64), len);
                        own.push((i, pack_track(&mut input, vel_levels, &mut scratch, dedup)?));
                        scratch.trim();
                        continue;
                    }
                    if !budget.acquire(len as u64) {
                        break; // the packer stopped on an error; it reports it
                    }
                    let mut raw = vec![0u8; len];
                    r.read_exact(&mut raw).map_err(|_| "Truncated MIDI file")?;
                    if tx.send((i, raw)).is_err() {
                        break; // the packer stopped on an error; it reports it
                    }
                }
                Ok(own)
            })();
            drop(tx);
            (read_result, packer.join().unwrap_or_else(|_| Err("Packing thread panicked".into())))
        });
        let mut packed = packed?;
        packed.extend(read_result?);
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

    /// Build the final store and chunk table by appending the shards one after another,
    /// freeing each as it's copied: peak memory is the packed size plus one shard, not
    /// twice the packed size. Rewrites `tracks`' ids into table indices.
    fn finish<'a>(self, tracks: impl Iterator<Item = &'a mut Vec<u32>>) -> (Vec<u8>, Vec<Chunk>) {
        let mut shards: Vec<Shard> = self.shards.into_iter().map(|m| m.into_inner().unwrap()).collect();
        // Only needed while inserting.
        for s in &mut shards {
            s.index = Default::default();
        }
        let mut store = Vec::with_capacity(shards.iter().map(|s| s.bytes.len()).sum());
        let mut chunks = Vec::with_capacity(shards.iter().map(|s| s.chunks.len()).sum());
        // Table index of each shard's first chunk.
        let mut first = Vec::with_capacity(shards.len());
        for s in &mut shards {
            first.push(chunks.len() as u32);
            let base = store.len() as u64;
            store.extend_from_slice(&s.bytes);
            chunks.extend(s.chunks.iter().map(|c| Chunk { start: c.start + base, len: c.len }));
            *s = Shard::default();
        }
        for refs in tracks {
            for id in refs.iter_mut() {
                let (s, local) = ((*id & ((1 << Self::SHARD_BITS) - 1)) as usize, *id >> Self::SHARD_BITS);
                *id = first[s] + local;
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
    /// Events whose record can't be written yet (see `pack_track`).
    queue: std::collections::VecDeque<Pending>,
    /// The chunk being written.
    data: Vec<u8>,
}

impl Scratch {
    /// Drop a queue that a track with a stuck note blew up, instead of keeping that
    /// capacity on every thread for the rest of the load.
    fn trim(&mut self) {
        if self.queue.capacity() > 1 << 22 {
            self.queue = Default::default();
        }
    }
}

/// An event waiting for its record to be written. A note's record needs its duration,
/// so it waits for the matching note-off; everything behind it waits too, to keep order.
#[derive(Clone, Copy)]
struct Pending {
    tick: u32,
    /// Note-on: the tick of its note-off, `NEVER` until (unless) one arrives.
    end: u32,
    /// Note-on: sequence number of the next open note on the same channel+key.
    next: u32,
    status: u8,
    d1: u8,
    d2: u8,
}

const NEVER: u32 = u32::MAX;

/// Window size for reading a track (at least `Input::MIN_WINDOW`).
const WINDOW: usize = 1 << 20;

/// Streams with a track bigger than this pack it on the reading thread, straight off the
/// stream, instead of reading it into memory whole and handing it to the pool.
const POOLED_MAX: usize = 64 << 20;

/// One track's bytes, read through a window that refills from `src`, so the track never
/// has to be in memory whole.
struct Input<'a, R> {
    buf: &'a mut [u8],
    pos: usize,
    end: usize,
    /// Track bytes not yet read from `src`.
    left: usize,
    src: R,
}

impl<'a> Input<'a, std::io::Empty> {
    /// A track that is all in `buf` already.
    fn whole(buf: &'a mut [u8]) -> Self {
        let end = buf.len();
        Input { buf, pos: 0, end, left: 0, src: std::io::empty() }
    }
}

impl<'a, R: Read> Input<'a, R> {
    /// The biggest `window(n)` asked for.
    const MIN_WINDOW: usize = 16;

    /// A track of `len` bytes to come from `src`, read through `buf`.
    fn streaming(buf: &'a mut [u8], src: R, len: usize) -> Self {
        assert!(buf.len() >= Self::MIN_WINDOW);
        Input { buf, pos: 0, end: 0, left: len, src }
    }

    /// The unread bytes: at least `n` of them unless the track ends first.
    #[inline]
    fn window(&mut self, n: usize) -> Result<&[u8], ParseError> {
        if self.end - self.pos < n && self.left > 0 {
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
            let want = self.left.min(self.buf.len() - self.end);
            self.src.read_exact(&mut self.buf[self.end..self.end + want])?;
            self.end += want;
            self.left -= want;
        }
        Ok(&self.buf[self.pos..self.end])
    }

    /// Skip `n` bytes; false if the track ends first.
    fn skip(&mut self, n: usize) -> Result<bool, ParseError> {
        let have = self.end - self.pos;
        if n <= have {
            self.pos += n;
            return Ok(true);
        }
        let more = n - have;
        if more > self.left {
            return Ok(false);
        }
        self.pos = self.end;
        let skipped = std::io::copy(&mut (&mut self.src).take(more as u64), &mut std::io::sink())?;
        self.left -= more;
        Ok(skipped == more as u64)
    }
}

/// Reads a range of a file by position, counting bytes into `progress`.
struct FileAt<'a> {
    file: &'a File,
    offset: u64,
    progress: &'a std::sync::atomic::AtomicU64,
}

impl Read for FileAt<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;
        #[cfg(windows)]
        let n = self.file.seek_read(buf, self.offset)?;
        #[cfg(unix)]
        let n = self.file.read_at(buf, self.offset)?;
        self.offset += n as u64;
        self.progress.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

/// Writes records and cuts them into chunks as it goes, so only the current chunk is held.
struct Packer<'a> {
    dedup: &'a Dedup,
    vel_levels: u8,
    /// The chunk being written.
    data: &'a mut Vec<u8>,
    /// Finished chunks (ids from `Dedup::insert`).
    refs: Vec<u32>,
    logical_bytes: u64,
    stats: PackStats,
    last_tick: u32,
    running: u8,
    running_vel: u8,
    running_dur: u32,
    /// Gear hash of the chunk so far.
    hash: u64,
}

impl Packer<'_> {
    /// Write the record for `p` (its duration, if it's a note, is final).
    fn emit(&mut self, p: &Pending) {
        let data = &mut *self.data;
        let delta = ((p.tick - self.last_tick) as u64) << 2;
        self.last_tick = p.tick;
        let start = data.len();
        if p.status & 0xF0 != 0x90 {
            put_vlv(data, delta);
            if p.status != self.running {
                data.push(p.status);
                self.running = p.status;
            }
            data.push(p.d1);
            if !matches!(p.status & 0xF0, 0xC0 | 0xD0) {
                data.push(p.d2);
            }
            self.stats.other_bytes += (data.len() - start) as u64;
            return self.cut_after(start);
        }

        let vel = quantize_velocity(p.d2, self.vel_levels);
        let dur = if p.end == NEVER { 0 } else { p.end - p.tick + 1 };
        let new_vel = vel != self.running_vel;
        let new_dur = dur != self.running_dur;
        put_vlv(data, delta | new_vel as u64 | (new_dur as u64) << 1);
        let long_delta = data.len() - start > 1;
        let new_status = p.status != self.running;
        if new_status {
            data.push(p.status);
            self.running = p.status;
        }
        data.push(p.d1);
        if new_vel {
            data.push(vel);
            self.running_vel = vel;
        }
        if new_dur {
            let before = data.len();
            put_vlv(data, dur as u64);
            self.stats.duration_bytes += (data.len() - before) as u64;
            self.running_dur = dur;
        }
        let s = &mut self.stats;
        s.notes += 1;
        s.new_velocity += new_vel as u64;
        s.new_duration += new_dur as u64;
        s.long_delta += long_delta as u64;
        s.status_byte += new_status as u64;
        s.two_byte += (data.len() - start == 2) as u64;
        self.cut_after(start);
    }

    /// Chunk boundaries: after a record, when the rolling hash of the bytes since the
    /// last cut hits the mask (~256 bytes on average), within [MIN, MAX] bytes.
    fn cut_after(&mut self, record_start: usize) {
        const MASK: u64 = (1 << 8) - 1;
        const MIN: usize = 64;
        const MAX: usize = 1024;
        for &b in &self.data[record_start..] {
            self.hash = (self.hash << 1).wrapping_add(GEAR[b as usize]);
        }
        let len = self.data.len();
        if (len >= MIN && self.hash & MASK == 0) || len >= MAX {
            self.flush();
        }
    }

    /// Store the current chunk, if any.
    fn flush(&mut self) {
        if !self.data.is_empty() {
            self.logical_bytes += self.data.len() as u64;
            self.refs.push(self.dedup.insert(self.data));
            self.data.clear();
        }
        self.hash = 0;
    }
}

/// Pack one MTrk payload in a single pass. Records are written in event order, but a
/// note's record needs its duration, which is only known at its note-off (FIFO per
/// channel+key). So events wait in a queue until nothing in front of them is waiting:
/// memory use follows how many notes are held at once, not the length of the track.
/// (A note that is never released holds up everything after it until the track ends.)
///
/// Sequencing events (`sequence`) and writing their records (`Packer`) are separate
/// stages. Running them on two threads for a big track was tried: about 1.7x slower,
/// not faster (batches handed over a channel; cause not found).
fn pack_track<R: Read>(
    input: &mut Input<R>,
    vel_levels: u8,
    scratch: &mut Scratch,
    dedup: &Dedup,
) -> Result<PackedTrack, ParseError> {
    let Scratch { queue, data } = scratch;
    queue.clear();
    data.clear();
    let mut out = Packer {
        dedup,
        vel_levels,
        data,
        refs: Vec::new(),
        logical_bytes: 0,
        stats: PackStats::default(),
        last_tick: 0,
        running: 0,
        running_vel: 0,
        running_dur: 0,
        hash: 0,
    };

    let seq = sequence(input, queue, |p| out.emit(p))?;
    out.flush();

    Ok(PackedTrack {
        stats: out.stats,
        refs: out.refs,
        logical_bytes: out.logical_bytes,
        tempos: seq.tempos,
        end_tick: seq.end_tick,
        note_count: seq.note_count,
        pitch_range: seq.pitch_range,
    })
}

/// What `sequence` learns about a track besides its records.
struct Sequenced {
    tempos: Vec<(u32, u32)>,
    end_tick: u32,
    note_count: u64,
    pitch_range: Option<(u8, u8)>,
}

/// Pair note-ons with their note-offs and call `emit` for each record to write, in
/// order, once its duration is known.
fn sequence<R: Read>(
    input: &mut Input<R>,
    queue: &mut std::collections::VecDeque<Pending>,
    mut emit: impl FnMut(&Pending),
) -> Result<Sequenced, ParseError> {
    const NONE: u32 = u32::MAX;
    // Open notes per channel+key: a FIFO as a linked list through `Pending::next`
    // (head/tail sequence numbers; the queue's front is `base`).
    let mut head = [NONE; 16 * 128];
    let mut tail = [NONE; 16 * 128];
    let mut base = 0u32;
    let mut tempos = Vec::new();
    let mut pitch_range: Option<(u8, u8)> = None;
    let mut note_count = 0u64;
    let end_tick = walk_raw_track(input, |tick, ev| {
        let RawEvent::Channel { status, d1, d2 } = ev else {
            if let RawEvent::Tempo(t) = ev {
                tempos.push((tick, t));
            }
            return;
        };
        let slot = (status as usize & 0x0F) * 128 + (d1 as usize & 0x7F);
        let mut p = Pending { tick, end: 0, next: NONE, status, d1, d2 };
        match status & 0xF0 {
            0x90 if d2 > 0 => {
                let seq = base.wrapping_add(queue.len() as u32);
                p.end = NEVER;
                match tail[slot] {
                    NONE => head[slot] = seq,
                    t => queue[t.wrapping_sub(base) as usize].next = seq,
                }
                tail[slot] = seq;
                queue.push_back(p);
                note_count += 1;
                pitch_range = Some(match pitch_range {
                    Some((lo, hi)) => (lo.min(d1), hi.max(d1)),
                    None => (d1, d1),
                });
            }
            0x80 | 0x90 => {
                // Note-offs are folded into durations; one with nothing open is ignored.
                let seq = head[slot];
                if seq != NONE {
                    let note = &mut queue[seq.wrapping_sub(base) as usize];
                    note.end = tick;
                    head[slot] = note.next;
                    if note.next == NONE {
                        tail[slot] = NONE;
                    }
                }
            }
            _ => queue.push_back(p),
        }
        while let Some(front) = queue.front() {
            if front.end == NEVER {
                break; // an open note
            }
            emit(front);
            queue.pop_front();
            base = base.wrapping_add(1);
        }
    })?;
    // Notes still open were never released.
    for p in queue.drain(..) {
        emit(&p);
    }
    Ok(Sequenced { tempos, end_tick, note_count, pitch_range })
}

/// Walk a track, calling `f` for channel events and tempo changes.
/// Returns the tick of the last event.
fn walk_raw_track<R: Read>(
    input: &mut Input<R>,
    mut f: impl FnMut(u32, RawEvent),
) -> Result<u32, ParseError> {
    let mut abs_tick: u32 = 0;
    let mut running_status: u8 = 0;

    loop {
        // Room for any event's fixed part: delta (4), status, type, length (4), tempo (3).
        let w = input.window(16)?;
        if w.is_empty() {
            return Ok(abs_tick);
        }
        let len = w.len();

        // 1) delta (VLV)
        let (delta, mut cursor) = read_vlv_from_buf(w)?;
        abs_tick = abs_tick.wrapping_add(delta);

        if cursor >= len {
            return Err("Unexpected end of track while reading status/data".into());
        }

        // 2) status / running status (only channel messages set running status)
        let mut status = w[cursor];
        if status & 0x80 == 0 {
            status = running_status;
        } else {
            if status < 0xF0 {
                running_status = status;
            }
            cursor += 1;
        }

        // A payload to skip once the event's fixed part is consumed, and the error if
        // the track ends inside it.
        let mut skip = (0usize, "");
        match status {
            0x80..=0xBF | 0xE0..=0xEF => {
                if cursor + 2 > len {
                    return Err("Unexpected end of track in 2-byte MIDI event".into());
                }
                let (d1, d2) = (w[cursor] & 0x7F, w[cursor + 1] & 0x7F);
                f(abs_tick, RawEvent::Channel { status, d1, d2 });
                cursor += 2;
            }

            0xC0..=0xDF => {
                if cursor + 1 > len {
                    return Err("Unexpected end of track in 1-byte MIDI event".into());
                }
                let d1 = w[cursor] & 0x7F;
                f(abs_tick, RawEvent::Channel { status, d1, d2: 0 });
                cursor += 1;
            }

            // SysEx: skipped (never sent during playback)
            0xF0 | 0xF7 => {
                let (sy_len, sy_len_len) = read_vlv_from_buf(&w[cursor..])?;
                cursor += sy_len_len;
                skip = (sy_len as usize, "Unexpected end of track in sysex");
            }

            // Meta: only Set Tempo (0x51) is kept
            0xFF => {
                if cursor >= len {
                    return Err("Unexpected end of track in meta type".into());
                }
                let meta_type = w[cursor];
                cursor += 1;
                let (m_len, m_len_len) = read_vlv_from_buf(&w[cursor..])?;
                cursor += m_len_len;
                if meta_type == 0x51 && m_len >= 3 {
                    let d = w.get(cursor..cursor + 3).ok_or("Unexpected end of track in meta data")?;
                    f(abs_tick, RawEvent::Tempo(u32::from_be_bytes([0, d[0], d[1], d[2]])));
                }
                skip = (m_len as usize, "Unexpected end of track in meta data");
            }

            _ => {
                return Err(format!("Unknown MIDI status byte: 0x{:X}", status).into());
            }
        }
        input.pos += cursor;
        if !input.skip(skip.0)? {
            return Err(skip.1.into());
        }
    }
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

    /// Pack a raw track read through a window of `window` bytes (at least 16).
    fn pack_raw(raw: &[u8], window: usize, scratch: &mut Scratch, dedup: &Dedup) -> PackedTrack {
        let mut buf = vec![0u8; window];
        pack_track(&mut Input::streaming(&mut buf, raw, raw.len()), 127, scratch, dedup).unwrap()
    }

    /// Pack one raw track into a (one-track) MidiFile.
    fn pack_one(raw: &[u8]) -> MidiFile {
        let dedup = Dedup::new();
        let t = pack_raw(raw, 1 << 10, &mut Scratch::default(), &dedup);
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
            .map(|raw| pack_raw(raw, 1 << 10, &mut scratch, &dedup))
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

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// What a raw track should pack to.
    struct Expected {
        records: Vec<(u32, Record)>,
        tempos: Vec<(u32, u32)>,
        end_tick: u32,
    }

    /// A random raw track (overlapping notes on a few keys, controllers, running status,
    /// tempo, long meta and sysex payloads) and what packing it should give, with the
    /// note-offs paired to note-ons the slow, obvious way.
    fn random_track(rng: &mut Rng, events: usize) -> (Vec<u8>, Expected) {
        let mut raw = vec![];
        let mut want = Expected { records: vec![], tempos: vec![], end_tick: 0 };
        let mut open: std::collections::HashMap<(u8, u8), std::collections::VecDeque<usize>> = Default::default();
        let (mut tick, mut prev_status) = (0u32, 0u8);
        for _ in 0..events {
            let delta = match rng.below(10) {
                0 => rng.below(300_000),
                1..=4 => 0,
                _ => rng.below(8),
            } as u32;
            tick += delta;
            put_vlv(&mut raw, delta as u64);
            let (ch, key) = (rng.below(3) as u8, 60 + rng.below(4) as u8);
            let omit = rng.below(2) == 0; // running status, when the status repeats
            let mut channel = |raw: &mut Vec<u8>, status: u8, data: &[u8]| {
                if status != prev_status || !omit {
                    raw.push(status);
                }
                prev_status = status;
                raw.extend_from_slice(data);
            };
            match rng.below(15) {
                0..=5 => {
                    let vel = 1 + rng.below(127) as u8;
                    channel(&mut raw, 0x90 | ch, &[key, vel]);
                    want.records.push((tick, Record::Note { ch, key, vel, dur: None }));
                    open.entry((ch, key)).or_default().push_back(want.records.len() - 1);
                }
                6..=8 => {
                    if rng.below(2) == 0 {
                        channel(&mut raw, 0x80 | ch, &[key, rng.below(128) as u8]);
                    } else {
                        channel(&mut raw, 0x90 | ch, &[key, 0]);
                    }
                    if let Some(i) = open.get_mut(&(ch, key)).and_then(|q| q.pop_front()) {
                        let Record::Note { ch, key, vel, .. } = want.records[i].1 else { unreachable!() };
                        let dur = Some(tick - want.records[i].0);
                        want.records[i].1 = Record::Note { ch, key, vel, dur };
                    }
                }
                9 => {
                    let (d1, d2) = (rng.below(128) as u8, rng.below(128) as u8);
                    channel(&mut raw, 0xB0 | ch, &[d1, d2]);
                    want.records.push((tick, Record::Other { status: 0xB0 | ch, d1, d2 }));
                }
                10 => {
                    let d1 = rng.below(128) as u8;
                    channel(&mut raw, 0xC0 | ch, &[d1]);
                    want.records.push((tick, Record::Other { status: 0xC0 | ch, d1, d2: 0 }));
                }
                11 => {
                    let (d1, d2) = (rng.below(128) as u8, rng.below(128) as u8);
                    channel(&mut raw, 0xE0 | ch, &[d1, d2]);
                    want.records.push((tick, Record::Other { status: 0xE0 | ch, d1, d2 }));
                }
                12 => {
                    let t = 1 + rng.below(0xFF_FFFF) as u32;
                    raw.extend_from_slice(&[0xFF, 0x51, 3, (t >> 16) as u8, (t >> 8) as u8, t as u8]);
                    want.tempos.push((tick, t));
                    prev_status = 0;
                }
                _ => {
                    // other meta or sysex, sometimes longer than the read window
                    let n = if rng.below(4) == 0 { 100 + rng.below(200) } else { rng.below(20) } as usize;
                    raw.extend_from_slice(if rng.below(2) == 0 { &[0xFF, 0x01] } else { &[0xF0] });
                    put_vlv(&mut raw, n as u64);
                    raw.extend(std::iter::repeat(0x55).take(n));
                    prev_status = 0;
                }
            }
        }
        want.end_tick = tick;
        (raw, want)
    }

    fn decode(m: &MidiFile, i: usize) -> Vec<(u32, Record)> {
        let (mut r, mut out) = (TrackReader::default(), vec![]);
        while let Some(x) = r.next(m.track(i)) {
            out.push(x);
        }
        out
    }

    #[test]
    fn packing_matches_a_naive_model() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for seed in 0..300 {
            let events = if seed % 50 == 0 { 5000 } else { 1 + rng.below(200) as usize };
            let (raw, want) = random_track(&mut rng, events);
            // a tiny window forces refills in the middle of events and payloads
            for window in [16, 17, 64, 1 << 20] {
                let dedup = Dedup::new();
                let t = pack_raw(&raw, window, &mut Scratch::default(), &dedup);
                let m = MidiFile::assemble(96, [t], dedup, 0, Default::default());
                assert_eq!(decode(&m, 0), want.records, "seed {seed}, window {window}");
                assert_eq!((&m.tempos, m.end_tick), (&want.tempos, want.end_tick));
                let notes = want.records.iter().filter(|r| matches!(r.1, Record::Note { .. })).count();
                assert_eq!(m.note_count, notes as u64);
            }
            // a track already in memory
            let (dedup, mut raw) = (Dedup::new(), raw);
            let t = pack_track(&mut Input::whole(&mut raw), 127, &mut Scratch::default(), &dedup).unwrap();
            let m = MidiFile::assemble(96, [t], dedup, 0, Default::default());
            assert_eq!(decode(&m, 0), want.records, "seed {seed}, whole");
        }
    }

    #[test]
    fn file_stream_and_big_tracks_load_the_same() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut file = b"MThd".to_vec();
        file.extend_from_slice(&[0, 0, 0, 6, 0, 1, 0, 3, 0, 96]);
        let mut wants = vec![];
        for events in [2000, 3, 800] {
            let (raw, want) = random_track(&mut rng, events);
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(raw.len() as u32).to_be_bytes());
            file.extend_from_slice(&raw);
            wants.push(want.records);
        }
        let path = std::env::temp_dir().join(format!("fmp_parser_test_{}.mid", std::process::id()));
        std::fs::write(&path, &file).unwrap();
        let progress = std::sync::atomic::AtomicU64::new(0);
        let from_file = MidiFile::open_and_parse(&path, 127, &progress).unwrap();
        assert!(progress.load(std::sync::atomic::Ordering::Relaxed) >= file.len() as u64 - 14 - 3 * 8);
        let _ = std::fs::remove_file(&path);
        // all tracks pooled; only the biggest packed on the reading thread; all of them
        for pooled_max in [usize::MAX, 1000, 0] {
            let m = MidiFile::parse_stream_with(&file[..], 127, pooled_max).unwrap();
            for (i, want) in wants.iter().enumerate() {
                assert_eq!(&decode(&m, i), want, "pooled_max {pooled_max}, track {i}");
            }
            assert_eq!((m.note_count, m.end_tick, m.file_bytes), (from_file.note_count, from_file.end_tick, file.len() as u64));
        }
        for (i, want) in wants.iter().enumerate() {
            assert_eq!(&decode(&from_file, i), want, "file, track {i}");
        }
    }
}
