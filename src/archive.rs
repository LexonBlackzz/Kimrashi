//! Loading MIDI files from inside archives and compressed files, nested up to
//! `MAX_DEPTH` layers deep (e.g. a .mid.xz inside a .7z, or a .tar.zst).
//!
//! After each layer the output's first bytes say what it is: a MIDI is parsed, another
//! layer is unwrapped. Stream formats (gz, xz, zst, tar) and entries of zip/7z files
//! are decompressed straight into the parser, so the uncompressed MIDI never exists in
//! full. Two cases go through a temporary file instead: rar (the unrar library can only
//! extract to disk), and a zip/7z/rar found *inside* another layer (those need random
//! access). If a layer holds several candidates, the largest is loaded.
//!
//! An xz layer whose streams repeat (see `scan_xz`) is not decoded copy by copy: if the
//! streams that made a MIDI track come back in a row, the track is counted, not read.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::parser::{MidiFile, ParseError, Plain, Progress, Source};

/// Deepest nesting followed (guards against archive bombs).
const MAX_DEPTH: usize = 4;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Midi,
    Gz,
    Xz,
    Zstd,
    Tar,
    Zip,
    SevenZ,
    Rar,
}

impl Kind {
    /// What `head` (a stream's first bytes, up to 512) contains.
    fn sniff(head: &[u8]) -> Option<Kind> {
        Some(match head {
            [b'M', b'T', b'h', b'd', ..] => Kind::Midi,
            [0x1F, 0x8B, ..] => Kind::Gz,
            [0xFD, b'7', b'z', b'X', b'Z', 0, ..] => Kind::Xz,
            [0x28, 0xB5, 0x2F, 0xFD, ..] => Kind::Zstd,
            [b'P', b'K', 3, 4, ..] => Kind::Zip,
            [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, ..] => Kind::SevenZ,
            [b'R', b'a', b'r', b'!', 0x1A, 0x07, ..] => Kind::Rar,
            h if h.get(257..262) == Some(b"ustar") => Kind::Tar,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Midi => "mid",
            Kind::Gz => "gz",
            Kind::Xz => "xz",
            Kind::Zstd => "zst",
            Kind::Tar => "tar",
            Kind::Zip => "zip",
            Kind::SevenZ => "7z",
            Kind::Rar => "rar",
        }
    }
}

/// Counts bytes read from the file on disk, for the load progress bar.
struct Counting<'a, R> {
    inner: R,
    progress: &'a Progress,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.add(n as u64)?;
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A temporary file, deleted on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new() -> std::io::Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join("kimrashi");
        std::fs::create_dir_all(&dir)?;
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Ok(Self(dir.join(format!("{}_{n}.tmp", std::process::id()))))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A reader as the bytes of a MIDI file that has no repeats.
fn plain<'a, R: Read + 'a>(r: R) -> Box<dyn Source + 'a> {
    Box::new(Plain(r))
}

/// `head` (bytes already taken from `rest`), then `rest`.
struct Prefixed<'a> {
    head: Cursor<Vec<u8>>,
    rest: Box<dyn Source + 'a>,
}

impl Read for Prefixed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.head.read(buf)? {
            0 => self.rest.read(buf),
            n => Ok(n),
        }
    }
}

impl Source for Prefixed<'_> {
    // `rest` counts bytes from the start of the whole stream, head included.
    fn extra_copies(&mut self, start: u64, end: u64) -> u64 {
        self.rest.extra_copies(start, end)
    }

    fn late_copies(&mut self) -> Vec<(u64, u64)> {
        self.rest.late_copies()
    }
}

fn err(e: impl std::fmt::Display) -> ParseError {
    e.to_string().into()
}

/// Whether an archive entry is worth opening: a MIDI, possibly compressed or archived.
fn is_candidate(name: &str) -> bool {
    let mut name = name.to_ascii_lowercase();
    while let Some(stripped) = [".gz", ".xz", ".zst", ".zstd"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
    {
        name = stripped.to_owned();
    }
    [".mid", ".midi", ".smf", ".kar", ".zip", ".7z", ".rar", ".tar", ".tgz", ".txz"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Pick the largest candidate among (id, name, size) entries.
fn pick<T>(entries: impl IntoIterator<Item = (T, String, u64)>) -> Result<T, ParseError> {
    entries
        .into_iter()
        .filter(|(_, name, _)| is_candidate(name))
        .max_by_key(|&(_, _, size)| size)
        .map(|(id, _, _)| id)
        .ok_or_else(|| "No MIDI file (.mid/.midi) in the archive".into())
}

/// Block count and largest uncompressed block size of an .xz file, from the index of
/// its last stream (None if it can't be read; callers then just stream).
fn xz_blocks(path: &Path) -> Option<(u64, u64)> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut footer = [0u8; 12];
    f.seek(SeekFrom::Start(len.checked_sub(12)?)).ok()?;
    f.read_exact(&mut footer).ok()?;
    if &footer[10..] != b"YZ" {
        return None;
    }
    let index_len = (u32::from_le_bytes(footer[4..8].try_into().ok()?) as u64 + 1) * 4;
    f.seek(SeekFrom::Start(len.checked_sub(12 + index_len)?)).ok()?;
    let mut index = vec![0u8; index_len as usize];
    f.read_exact(&mut index).ok()?;
    // Index: 0x00, record count, then (unpadded size, uncompressed size) per block,
    // as little-endian base-128 numbers.
    let mut pos = 1;
    let mut num = || {
        let mut v = 0u64;
        for shift in (0..63).step_by(7) {
            let b = *index.get(pos)?;
            pos += 1;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    if index.first() != Some(&0) {
        return None;
    }
    let count = num()?;
    let mut largest = 0;
    for _ in 0..count {
        num()?;
        largest = largest.max(num()?);
    }
    Some((count, largest))
}

fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    File::open(path)?.take(512).read_to_end(&mut head)?;
    Ok(head)
}

// ------------------------------------------------------------------
// xz files made of streams that repeat
// ------------------------------------------------------------------
//
// An .xz file can be several streams back to back. If a MIDI's tracks are one track
// copied thousands of times, each copy compressed on its own (a track may be many streams
// long: a 4 GB track in 256 MB streams), the same streams come back in the same order, and
// what they decode to is the same. So each distinct stream is kept once, and when the
// MIDI parser finishes a track that is exactly some whole streams, the next time those
// streams come back in a row they are not decoded or parsed: the track is counted once
// more (see `Source::late_copies`). The order of the streams is never changed.
//
// A short layer is scanned whole first (which is what lets a layer of xz made of repeats
// of xz streams be seen through, see `xz_runs`). A long one (a TB of repeats) can't be: its
// first part is scanned, then the rest is scanned as the parser reads it.

const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
/// A stream longer than this is not compared with others (and a layer whose first stream
/// is longer is read as usual).
const XZ_STREAM_MAX: usize = 64 << 20;
/// Most bytes of distinct streams held while looking for repeats.
const XZ_STORE_MAX: usize = 256 << 20;
/// Bytes of streams scanned before the parser gets any; the rest of a longer layer is
/// scanned as it is read.
const XZ_SCAN_MAX: u64 = 512 << 20;
/// Copies of a track that must come back identical before the rest, if they are exactly
/// whole copies by size, are taken to be the same (see `RepeatedXz::trust_the_rest`).
const TRUST_AFTER: u64 = 8;
/// Most bytes of a layer held to read the size of the layer inside it from its indexes.
const XZ_INDEX_BUF: u64 = 1 << 30;
/// Longest track (in streams) and most distinct tracks looked for among later streams.
const TRACK_STREAMS_MAX: usize = 1 << 12;
const TRACKS_MAX: usize = 64;

/// Finds a stream among the distinct ones. Mostly it is the one before or the next in a
/// cycle (a plain compare); the rest are found by hash.
#[derive(Default)]
struct RunIndex {
    seen: HashMap<u64, usize>,
    last: usize,
}

impl RunIndex {
    fn hash(piece: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        piece.hash(&mut h);
        h.finish()
    }

    fn find(&mut self, store: &[Vec<u8>], piece: &[u8]) -> Option<usize> {
        let guess = [self.last, (self.last + 1) % store.len().max(1)];
        let hit = guess
            .into_iter()
            .find(|&i| store.get(i).is_some_and(|s| s == piece))
            .or_else(|| self.seen.get(&Self::hash(piece)).copied().filter(|&i| store[i] == piece));
        if let Some(i) = hit {
            self.last = i;
        }
        hit
    }

    fn add(&mut self, piece: &[u8], at: usize) {
        self.seen.insert(Self::hash(piece), at);
        self.last = at;
    }
}

/// What `scan_xz` found.
enum XzScan<'a> {
    /// Some stream repeats: the layer, decoded stream by stream.
    Repeats(RepeatedXz<'a>),
    /// Nothing to gain: the bytes as they were (to decode as usual).
    Plain(Box<dyn Read + 'a>),
}

/// The bytes of the streams `order` names (each with its count in a row), then `then`.
struct Expanded<R> {
    store: Vec<Vec<u8>>,
    order: Vec<(usize, u64)>,
    run: usize,
    copy: u64,
    pos: usize,
    then: R,
}

impl<R: Read> Read for Expanded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while let Some(&(id, count)) = self.order.get(self.run) {
            let bytes = &self.store[id];
            if self.pos < bytes.len() {
                let n = buf.len().min(bytes.len() - self.pos);
                buf[..n].copy_from_slice(&bytes[self.pos..][..n]);
                self.pos += n;
                return Ok(n);
            }
            self.pos = 0;
            self.copy += 1;
            if self.copy >= count {
                self.copy = 0;
                self.run += 1;
            }
        }
        self.then.read(buf)
    }
}

/// Positions of `XZ_MAGIC` in `bytes[from..]`.
fn magics(bytes: &[u8], from: usize) -> Vec<usize> {
    let rest = bytes.get(from..).unwrap_or_default();
    memchr::memmem::find_iter(rest, &XZ_MAGIC).map(|i| from + i).collect()
}

/// A finished stream's bytes, up to its footer (a stream may be followed by padding).
fn stream_end(piece: &[u8]) -> Option<usize> {
    let end = piece.iter().rposition(|&b| b != 0)? + 1;
    (end >= 32 && piece[end - 2..end] == *b"YZ").then_some(end)
}

enum Piece {
    Stream(Vec<u8>),
    /// The layer ended after whole streams.
    End,
    /// What follows isn't a whole xz stream (or is too long to hold): `into_rest`.
    Rest,
}

/// A layer read as xz streams back to back, one at a time.
struct Pull<'a> {
    r: Box<dyn Read + 'a>,
    /// Told how much is read (and a cancel stops the read).
    progress: Option<&'a Progress>,
    /// Bytes read past the streams handed out; starts with an xz magic.
    cur: Vec<u8>,
    block: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
    /// Bytes of the streams handed out so far.
    yielded: u64,
    eof: bool,
    rest: bool,
}

impl<'a> Pull<'a> {
    fn new(r: Box<dyn Read + 'a>, progress: Option<&'a Progress>) -> Self {
        Self { r, progress, cur: Vec::new(), block: vec![0u8; 1 << 20], ready: VecDeque::new(), yielded: 0, eof: false, rest: false }
    }

    fn next(&mut self) -> std::io::Result<Piece> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                self.yielded += p.len() as u64;
                return Ok(Piece::Stream(p));
            }
            if self.rest {
                return Ok(Piece::Rest);
            }
            if self.eof && self.cur.is_empty() {
                return Ok(Piece::End);
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let n = self.r.read(&mut self.block)?;
        if let Some(p) = self.progress {
            p.add(0)?; // fails if cancelled: the file itself isn't being read any more
            p.scan_done.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let from = self.cur.len().saturating_sub(XZ_MAGIC.len() - 1).max(1);
        self.cur.extend_from_slice(&self.block[..n]);
        self.eof = n == 0;
        if self.cur.is_empty() || (self.cur.len() < XZ_MAGIC.len() && !self.eof) {
            return Ok(()); // a short read
        }
        if self.cur.len() < XZ_MAGIC.len() || self.cur[..XZ_MAGIC.len()] != XZ_MAGIC {
            self.rest = true; // not an xz stream (or no more of them)
            return Ok(());
        }
        // Everything before the last magic found is whole streams.
        let starts = magics(&self.cur, from);
        let mut done = 0;
        for w in 0..starts.len() + usize::from(self.eof) {
            let end = starts.get(w).copied().unwrap_or(self.cur.len());
            let piece = &self.cur[done..end];
            if stream_end(piece).is_none() {
                if end < self.cur.len() {
                    continue; // a magic inside the stream's data, not the next stream
                }
                self.rest = true; // not a stream, or cut off
                break;
            }
            self.ready.push_back(piece.to_vec());
            done = end;
        }
        self.cur.drain(..done);
        if self.cur.len() > XZ_STREAM_MAX {
            self.rest = true;
        }
        Ok(())
    }

    /// The bytes not handed out as streams.
    fn into_rest(self) -> Box<dyn Read + 'a> {
        let mut head: Vec<u8> = self.ready.into_iter().flatten().collect();
        head.extend_from_slice(&self.cur);
        Box::new(Cursor::new(head).chain(self.r))
    }
}

/// Read `r` as xz streams back to back, keeping each distinct stream once.
/// (`total`: how many bytes the layer is, if known.)
fn scan_xz<'a, R: Read + 'a>(r: R, progress: Option<&'a Progress>, total: Option<u64>) -> std::io::Result<XzScan<'a>> {
    scan_xz_within(r, XZ_SCAN_MAX, progress, total)
}

fn scan_xz_within<'a, R: Read + 'a>(
    r: R,
    budget: u64,
    progress: Option<&'a Progress>,
    total: Option<u64>,
) -> std::io::Result<XzScan<'a>> {
    let mut pull = Pull::new(Box::new(r), progress);
    let mut store: Vec<Vec<u8>> = Vec::new();
    // The streams in order, as (which, how many times in a row).
    let mut order: Vec<(usize, u64)> = Vec::new();
    let mut index = RunIndex::default();
    let (mut stored, mut scanned) = (0usize, 0u64);
    let plain = |store, order, then| XzScan::Plain(Box::new(Expanded { store, order, run: 0, copy: 0, pos: 0, then }));
    loop {
        match pull.next()? {
            Piece::Stream(piece) => {
                scanned += piece.len() as u64;
                let id = index.find(&store, &piece).unwrap_or_else(|| {
                    stored += piece.len();
                    index.add(&piece, store.len());
                    store.push(piece);
                    store.len() - 1
                });
                match order.last_mut() {
                    Some((last, count)) if *last == id => *count += 1,
                    _ => order.push((id, 1)),
                }
                if stored > XZ_STORE_MAX {
                    return Ok(plain(store, order, pull.into_rest()));
                }
                if scanned > budget {
                    // A long layer: the rest is scanned as the parser reads it.
                    let mut repeated = RepeatedXz::new(store, order, Some((pull, index)));
                    repeated.layer_total = total;
                    return Ok(XzScan::Repeats(repeated));
                }
            }
            Piece::End => {
                if store.len() == order.len() && order.iter().all(|o| o.1 == 1) {
                    return Ok(plain(store, order, Box::new(std::io::empty())));
                }
                return Ok(XzScan::Repeats(RepeatedXz::new(store, order, None)));
            }
            Piece::Rest => return Ok(plain(store, order, pull.into_rest())),
        }
    }
}

/// A track that was exactly these streams, and how many more times they came back.
struct Reg {
    ids: Vec<usize>,
    /// Where the track began in the decoded bytes.
    start: u64,
    extra: u64,
}

/// The decoded bytes of a layer of xz streams, leaving out the repeats of tracks the
/// parser has seen whole.
struct RepeatedXz<'a> {
    store: Vec<Vec<u8>>,
    /// The scanned streams in order (which, how many times in a row), and how far
    /// `next_id` is through them.
    order: Vec<(usize, u64)>,
    at: usize,
    given: u64,
    /// The rest of the layer, scanned as it is needed.
    more: Option<(Pull<'a>, RunIndex)>,
    /// Bytes of distinct streams kept.
    stored: usize,
    /// Streams read ahead, to see whether a known track follows.
    pending: VecDeque<usize>,
    decoder: Option<Box<dyn Read>>,
    /// Decoded bytes handed out so far.
    pos: u64,
    /// A byte read ahead to see whether the decoder was at its end.
    peeked: Option<u8>,
    /// Where each stream handed out since the last track ended began, and which it was.
    log: Vec<(u64, usize)>,
    tracks: Vec<Reg>,
    /// What the layer turned out to hold after its last stream.
    tail: Option<Box<dyn Read + 'a>>,
    /// How many bytes the layer is, if known.
    layer_total: Option<u64>,
}

impl<'a> RepeatedXz<'a> {
    fn new(store: Vec<Vec<u8>>, order: Vec<(usize, u64)>, more: Option<(Pull<'a>, RunIndex)>) -> Self {
        let stored = store.iter().map(|s| s.len()).sum();
        Self {
            store,
            order,
            at: 0,
            given: 0,
            more,
            stored,
            pending: VecDeque::new(),
            decoder: None,
            pos: 0,
            peeked: None,
            log: Vec::new(),
            tracks: Vec::new(),
            tail: None,
            layer_total: None,
        }
    }

    /// Streams with the number of times each comes in a row (as `xz_runs` hands them).
    fn from_runs(runs: Vec<(Vec<u8>, u64)>) -> Self {
        let (mut store, mut order, mut index) = (Vec::new(), Vec::new(), RunIndex::default());
        for (bytes, count) in runs {
            let id = index.find(&store, &bytes).unwrap_or_else(|| {
                index.add(&bytes, store.len());
                store.push(bytes);
                store.len() - 1
            });
            order.push((id, count));
        }
        Self::new(store, order, None)
    }

    fn decode(bytes: &[u8]) -> Box<dyn Read> {
        let end = stream_end(bytes).unwrap_or(bytes.len());
        Box::new(liblzma::read::XzDecoder::new_multi_decoder(Cursor::new(bytes[..end].to_vec())))
    }

    /// The next stream of the layer, or None at its end.
    fn next_id(&mut self) -> std::io::Result<Option<usize>> {
        while let Some(&(id, count)) = self.order.get(self.at) {
            if self.given < count {
                self.given += 1;
                return Ok(Some(id));
            }
            self.at += 1;
            self.given = 0;
        }
        let Some((pull, index)) = &mut self.more else { return Ok(None) };
        match pull.next()? {
            Piece::Stream(piece) => {
                if let Some(id) = index.find(&self.store, &piece) {
                    return Ok(Some(id));
                }
                if self.stored + piece.len() > XZ_STORE_MAX {
                    // Too much to keep: the rest is read as it comes.
                    let (pull, _) = self.more.take().unwrap();
                    self.tail = Some(Box::new(liblzma::read::XzDecoder::new_multi_decoder(Cursor::new(piece).chain(pull.into_rest()))));
                    return Ok(None);
                }
                self.stored += piece.len();
                index.add(&piece, self.store.len());
                self.store.push(piece);
                Ok(Some(self.store.len() - 1))
            }
            Piece::End => {
                self.more = None;
                Ok(None)
            }
            Piece::Rest => {
                let (pull, _) = self.more.take().unwrap();
                self.tail = Some(Box::new(liblzma::read::XzDecoder::new_multi_decoder(pull.into_rest())));
                Ok(None)
            }
        }
    }

    /// The next stream that has to be read: the tracks that came back whole before it are
    /// counted, not returned. None at the end of the streams.
    fn next_piece(&mut self) -> std::io::Result<Option<usize>> {
        loop {
            let want = self.tracks.iter().map(|t| t.ids.len()).max().unwrap_or(1);
            while self.pending.len() < want {
                match self.next_id()? {
                    Some(id) => self.pending.push_back(id),
                    None => break,
                }
            }
            let pending = &self.pending;
            let known = self.tracks.iter().position(|t| pending.len() >= t.ids.len() && pending.iter().zip(&t.ids).all(|(a, b)| a == b));
            let Some(at) = known else { return Ok(self.pending.pop_front()) };
            self.tracks[at].extra += 1;
            self.pending.drain(..self.tracks[at].ids.len());
            self.trust_the_rest(at);
        }
    }

    /// A track has come back several times, identical. If what is left of the layer is
    /// exactly a whole number of copies of it by size, take them to be copies (without
    /// reading them): a file that isn't would have to differ without changing a size.
    /// (Setting `verify_all` reads them all, which for a TB of repeats takes a while.)
    fn trust_the_rest(&mut self, at: usize) {
        let Some((pull, _)) = &self.more else { return };
        let (Some(total), Some(p)) = (self.layer_total, pull.progress) else { return };
        let all_taken = self.at >= self.order.len() || (self.at + 1 == self.order.len() && self.given == self.order[self.at].1);
        let t = &self.tracks[at];
        if t.extra < TRUST_AFTER || !self.pending.is_empty() || !all_taken || p.verify_all.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let unit: u64 = t.ids.iter().map(|&i| self.store[i].len() as u64).sum();
        let left = total.saturating_sub(pull.yielded);
        if unit == 0 || left % unit != 0 {
            return; // not whole copies: read on
        }
        let n = left / unit;
        p.trusted_tracks.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        self.tracks[at].extra += n;
        self.more = None;
    }
}

impl Read for RepeatedXz<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(b) = self.peeked.take() {
            buf[0] = b;
            self.pos += 1;
            return Ok(1);
        }
        loop {
            if self.decoder.is_none() {
                let Some(id) = self.next_piece()? else {
                    let Some(tail) = &mut self.tail else { return Ok(0) };
                    let n = tail.read(buf)?;
                    self.pos += n as u64;
                    return Ok(n);
                };
                self.log.push((self.pos, id));
                self.decoder = Some(Self::decode(&self.store[id]));
            }
            let n = self.decoder.as_mut().unwrap().read(buf)?;
            if n > 0 {
                self.pos += n as u64;
                return Ok(n);
            }
            self.decoder = None; // this stream is done: the next
        }
    }
}

impl Source for RepeatedXz<'_> {
    // When every stream decodes to exactly one xz stream (a layer of xz made of repeats of
    // xz streams), the layer above is those streams, each repeated as often as it is:
    // known without producing a byte of the repeats.
    fn xz_runs(&mut self) -> Option<Vec<(Vec<u8>, u64)>> {
        if self.more.is_some() || self.tail.is_some() || self.decoder.is_some() || self.peeked.is_some() {
            return None;
        }
        if (self.at, self.given, self.pos) != (0, 0, 0) || !self.pending.is_empty() {
            return None;
        }
        let mut out: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut stored = 0;
        for &(id, count) in &self.order {
            let mut decoder = Self::decode(&self.store[id]);
            let mut magic = [0u8; 6];
            decoder.read_exact(&mut magic).ok().filter(|_| magic == XZ_MAGIC)?;
            let mut bytes = magic.to_vec();
            decoder.take(XZ_STREAM_MAX as u64 + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() > XZ_STREAM_MAX || stream_end(&bytes).is_none() || !magics(&bytes, 1).is_empty() {
                return None;
            }
            stored += bytes.len();
            if stored > XZ_STORE_MAX {
                return None;
            }
            match out.last_mut() {
                Some((last, n)) if *last == bytes => *n += count,
                _ => out.push((bytes, count)),
            }
        }
        Some(out)
    }

    fn extra_copies(&mut self, start: u64, end: u64) -> u64 {
        // A track that is exactly whole streams: it began with one and ends with the one
        // being read. Those streams, back in a row later, are another copy.
        if self.pos == end && self.peeked.is_none() {
            if let Ok(first) = self.log.binary_search_by_key(&start, |e| e.0) {
                let at_end = match &mut self.decoder {
                    Some(decoder) => {
                        let mut next = [0u8; 1];
                        match decoder.read(&mut next) {
                            Ok(0) => true,
                            Ok(_) => {
                                self.peeked = Some(next[0]); // more follows: not one track
                                false
                            }
                            Err(_) => false,
                        }
                    }
                    None => true,
                };
                let ids: Vec<usize> = self.log[first..].iter().map(|e| e.1).collect();
                if at_end && ids.len() <= TRACK_STREAMS_MAX && self.tracks.len() < TRACKS_MAX && !self.tracks.iter().any(|t| t.ids == ids) {
                    if let Some(p) = self.more.as_ref().and_then(|m| m.0.progress) {
                        if self.tracks.is_empty() {
                            let unit = ids.iter().map(|&i| self.store[i].len() as u64).sum();
                            p.scan_unit.store(unit, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    self.tracks.push(Reg { ids, start, extra: 0 });
                }
            }
        }
        self.log.retain(|e| e.0 >= end);
        0
    }

    fn late_copies(&mut self) -> Vec<(u64, u64)> {
        self.tracks.iter().filter(|t| t.extra > 0).map(|t| (t.start, t.extra)).collect()
    }
}

/// The variable-length integer at `b[i..]` of an xz index, and where it ends.
fn xz_vli(b: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for shift in (0..63).step_by(7) {
        let c = *b.get(i)?;
        i += 1;
        v |= u64::from(c & 0x7F) << shift;
        if c & 0x80 == 0 {
            return Some((v, i));
        }
    }
    None
}

/// How many bytes the xz streams in `buf` decode to, from their indexes (nothing is
/// decoded).
fn xz_total(buf: &[u8]) -> Option<u64> {
    let (mut end, mut total) = (buf.len(), 0u64);
    loop {
        while end >= 4 && buf[end - 4..end] == [0; 4] {
            end -= 4; // stream padding
        }
        if end == 0 {
            return Some(total);
        }
        if end < 32 || buf[end - 2..end] != *b"YZ" {
            return None;
        }
        let index_len = (u32::from_le_bytes(buf[end - 8..end - 4].try_into().ok()?) as usize + 1) * 4;
        let index = buf.get(end.checked_sub(12 + index_len)?..end - 12)?;
        if index[0] != 0 {
            return None;
        }
        let (blocks, mut i) = xz_vli(index, 1)?;
        let mut blocks_len = 0usize;
        for _ in 0..blocks {
            let (unpadded, j) = xz_vli(index, i)?;
            let (size, k) = xz_vli(index, j)?;
            (blocks_len, total, i) = (blocks_len + (unpadded as usize + 3) / 4 * 4, total + size, k);
        }
        end = end.checked_sub(12 + blocks_len + index_len + 12)?;
    }
}

/// `r` with the size of what it decodes to (as xz), if it can be read into memory to look:
/// that tells how long a layer is before a byte of it is read.
fn with_xz_total<'a>(mut r: Box<dyn Read + 'a>) -> std::io::Result<(Box<dyn Read + 'a>, Option<u64>)> {
    let mut buf = Vec::new();
    (&mut r).take(XZ_INDEX_BUF + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > XZ_INDEX_BUF {
        return Ok((Box::new(Cursor::new(buf).chain(r)), None));
    }
    let total = xz_total(&buf);
    Ok((Box::new(Cursor::new(buf)), total))
}

/// Load the MIDI inside the archive or compressed file at `path`.
pub fn open(path: &Path, vel_levels: u8, progress: &Progress) -> Result<MidiFile, ParseError> {
    let size = std::fs::metadata(path)?.len();
    let mut ctx = Ctx { vel_levels, progress, layers: Vec::new(), layer_total: None };
    let mut midi = ctx.file(path, 0)?;
    midi.source = Some((ctx.layers.join(" › "), size));
    Ok(midi)
}

struct Ctx<'a> {
    vel_levels: u8,
    progress: &'a Progress,
    /// The formats unwrapped so far, outermost first.
    layers: Vec<&'static str>,
    /// How many bytes the layer about to be read is, when the layer before it said (see
    /// `with_xz_total`).
    layer_total: Option<u64>,
}

impl<'a> Ctx<'a> {
    fn layer(&mut self, name: &'static str) {
        self.layers.push(name);
        if let Ok(mut s) = self.progress.layers.lock() {
            *s = self.layers.join(" › ");
        }
    }

    fn reader(&self, path: &Path) -> Result<Counting<'a, BufReader<File>>, ParseError> {
        Ok(Counting {
            inner: BufReader::with_capacity(1 << 20, File::open(path)?),
            progress: self.progress,
        })
    }

    /// A layer on disk. Seekable formats are handled here; the rest stream.
    fn file(&mut self, path: &Path, depth: usize) -> Result<MidiFile, ParseError> {
        if depth > MAX_DEPTH {
            return Err("Archive nested too deeply".into());
        }
        let kind = Kind::sniff(&read_head(path)?)
            .ok_or("Not a MIDI file or a supported archive (zip, 7z, rar, xz, gz, zst, tar)")?;
        match kind {
            Kind::Midi => MidiFile::open_and_parse(path, self.vel_levels, self.progress),
            Kind::Xz => {
                self.layer("xz");
                // A file that is one stream repeated many times is read specially.
                if let XzScan::Repeats(repeated) = scan_xz(self.reader(path)?, Some(self.progress), None)? {
                    return self.stream(Box::new(repeated), depth + 1);
                }
                // On disk, xz blocks can be decoded on several threads, but each worker
                // holds a whole decoded block: only do that when blocks are small. A file
                // written as one big block (common for "ultra" settings) would otherwise
                // need its entire uncompressed size in memory.
                const BUDGET: u64 = 1 << 30;
                let cores = std::thread::available_parallelism().map_or(4, |n| n.get() as u64);
                match xz_blocks(path) {
                    Some((blocks, largest)) if blocks > 1 && largest * 2 <= BUDGET => {
                        let workers = (BUDGET / (largest * 2)).clamp(1, cores) as u32;
                        let xz = lzma_rust2::XzReaderMt::new(self.reader(path)?, true, workers)?;
                        self.stream(plain(xz), depth + 1)
                    }
                    _ => {
                        let xz = liblzma::read::XzDecoder::new_multi_decoder(self.reader(path)?);
                        self.stream(plain(xz), depth + 1)
                    }
                }
            }
            Kind::Gz | Kind::Zstd | Kind::Tar => {
                let r = self.reader(path)?;
                self.stream(plain(r), depth)
            }
            Kind::Zip => {
                self.layer("zip");
                let mut zip = zip::ZipArchive::new(self.reader(path)?).map_err(err)?;
                let mut entries = Vec::new();
                for i in 0..zip.len() {
                    let f = zip.by_index_raw(i).map_err(err)?;
                    if f.is_file() {
                        entries.push((i, f.name().map_err(err)?.into_owned(), f.size()));
                    }
                }
                let index = pick(entries)?;
                let entry = zip.by_index(index).map_err(err)?;
                self.stream(plain(entry), depth + 1)
            }
            Kind::SevenZ => {
                self.layer("7z");
                let mut archive = sevenz_rust2::ArchiveReader::new(
                    self.reader(path)?,
                    sevenz_rust2::Password::empty(),
                )
                .map_err(err)?;
                // The multi-threaded LZMA2 decoder holds each independent unit's whole
                // output in memory, and a stream with few resets is one unit: a 7z of
                // tightly compressed MIDI then needs gigabytes (tens of GB for the 10
                // billion note one). One thread decodes through a dictionary-sized window.
                archive.set_thread_count(1);
                let target = pick(
                    archive
                        .archive()
                        .files
                        .iter()
                        .filter(|f| !f.is_directory())
                        .map(|f| (f.name().to_owned(), f.name().to_owned(), f.size)),
                )?;
                let solid = archive.archive().is_solid;
                let mut result = None;
                archive
                    .for_each_entries(|entry, reader| {
                        if entry.name() == target {
                            result = Some(self.stream(plain(reader), depth + 1));
                            return Ok(false); // done
                        }
                        // Solid archives must decode what comes before: the next entry
                        // starts where this one's data ends, read or not.
                        if solid {
                            std::io::copy(reader, &mut std::io::sink())?;
                        }
                        Ok(true)
                    })
                    .map_err(err)?;
                result.unwrap_or_else(|| Err("MIDI file not found while extracting".into()))
            }
            Kind::Rar => {
                self.layer("rar");
                let mut entries = Vec::new();
                for header in unrar::Archive::new(path).open_for_listing().map_err(err)? {
                    let header = header.map_err(err)?;
                    if header.is_file() {
                        let name = header.filename.to_string_lossy().into_owned();
                        entries.push((header.filename.clone(), name, header.unpacked_size));
                    }
                }
                let target: PathBuf = pick(entries)?;
                let out = TempFile::new()?;
                let mut archive = unrar::Archive::new(path).open_for_processing().map_err(err)?;
                while let Some(header) = archive.read_header().map_err(err)? {
                    if header.entry().filename == target {
                        header.extract_to(&out.0).map_err(err)?;
                        return self.file(&out.0, depth + 1);
                    }
                    archive = header.skip().map_err(err)?;
                }
                Err("MIDI file not found while extracting".into())
            }
        }
    }

    /// A layer arriving as a stream: peek at its first bytes and continue accordingly.
    fn stream(&mut self, mut r: Box<dyn Source + '_>, depth: usize) -> Result<MidiFile, ParseError> {
        if depth > MAX_DEPTH {
            return Err("Archive nested too deeply".into());
        }
        let total = self.layer_total.take();
        if let Some(runs) = r.xz_runs() {
            self.layer("xz");
            return self.stream(Box::new(RepeatedXz::from_runs(runs)), depth + 1);
        }
        let mut head = Vec::with_capacity(512);
        (&mut r).take(512).read_to_end(&mut head)?;
        let kind = Kind::sniff(&head).ok_or("Not a MIDI file or a supported archive")?;
        let mut r: Box<dyn Source + '_> = Box::new(Prefixed { head: Cursor::new(head), rest: r });
        match kind {
            Kind::Midi => MidiFile::parse_stream(r, self.vel_levels, self.progress),
            Kind::Gz => {
                self.layer("gz");
                self.stream(plain(flate2::read::MultiGzDecoder::new(r)), depth + 1)
            }
            Kind::Xz => {
                self.layer("xz");
                match scan_xz(r, Some(self.progress), total)? {
                    XzScan::Repeats(repeated) => self.stream(Box::new(repeated), depth + 1),
                    XzScan::Plain(bytes) => {
                        let (bytes, inner) = with_xz_total(bytes)?;
                        self.layer_total = inner;
                        self.stream(plain(liblzma::read::XzDecoder::new_multi_decoder(bytes)), depth + 1)
                    }
                }
            }
            Kind::Zstd => {
                self.layer("zst");
                let mut zst = zstd::stream::read::Decoder::new(r)?;
                // Allow the large windows `zstd --long` / `--ultra` produce (up to 2 GB).
                zst.window_log_max(31)?;
                self.stream(plain(zst), depth + 1)
            }
            Kind::Tar => {
                self.layer("tar");
                // Sequential, so the first candidate is used (sizes aren't known ahead).
                let mut tar = tar::Archive::new(r);
                for entry in tar.entries()? {
                    let entry = entry?;
                    let name = entry.path()?.to_string_lossy().into_owned();
                    if entry.header().entry_type().is_file() && is_candidate(&name) {
                        return self.stream(plain(entry), depth + 1);
                    }
                }
                Err("No MIDI file (.mid/.midi) in the archive".into())
            }
            Kind::Zip | Kind::SevenZ | Kind::Rar => {
                // These need random access: spool this layer to a temporary file.
                let tmp = TempFile::new()?;
                std::io::copy(&mut r, &mut File::create(&tmp.0)?)?;
                self.file(&tmp.0, depth + 1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn archives_load_like_the_plain_file() {
        // Two tracks: a C major chord, then a tempo change and a held note.
        let mut midi = b"MThd".to_vec();
        midi.extend_from_slice(&[0, 0, 0, 6, 0, 1, 0, 2, 0, 96]);
        for trk in [
            &[0, 0x90, 60, 100, 0, 64, 90, 0, 67, 80, 0x60, 0x80, 60, 0, 0, 64, 0, 0, 67, 0][..],
            &[0, 0xFF, 0x51, 3, 0x07, 0xA1, 0x20, 0x10, 0x91, 40, 70][..],
        ] {
            midi.extend_from_slice(b"MTrk");
            midi.extend_from_slice(&(trk.len() as u32).to_be_bytes());
            midi.extend_from_slice(trk);
        }
        let dir = std::env::temp_dir().join(format!("fmp_archive_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let load = |name: &str| MidiFile::open_and_parse(dir.join(name), 127, &Progress::default());

        std::fs::write(dir.join("plain.mid"), &midi).unwrap();
        std::fs::write(dir.join("song.gz"), gz(&midi)).unwrap();
        std::fs::write(dir.join("song.zst"), zstd::encode_all(&midi[..], 3).unwrap()).unwrap();

        // zip: a decoy text file, a smaller MIDI, and the song compressed inside the zip
        let mut zip = zip::ZipWriter::new(File::create(dir.join("song.zip")).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.txt", opts).unwrap();
        zip.write_all(b"not a midi").unwrap();
        zip.start_file("small.mid", opts).unwrap();
        zip.write_all(&midi[..14]).unwrap();
        zip.start_file("song.mid.gz", opts).unwrap();
        zip.write_all(&gz(&midi)).unwrap(); // largest candidate, and another layer
        zip.finish().unwrap();

        // 7z, solid (the smaller MIDI first must be skipped over) and not
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};
        let mut sz = ArchiveWriter::create(dir.join("solid.7z")).unwrap();
        sz.push_archive_entries(
            vec![ArchiveEntry::new_file("a_small.mid"), ArchiveEntry::new_file("b_song.mid")],
            vec![SourceReader::from(&midi[..14]), SourceReader::from(&midi[..])],
        )
        .unwrap();
        sz.finish().unwrap();
        let mut sz = ArchiveWriter::create(dir.join("separate.7z")).unwrap();
        sz.push_archive_entry(ArchiveEntry::new_file("a_small.mid"), Some(&midi[..14])).unwrap();
        sz.push_archive_entry(ArchiveEntry::new_file("b_song.mid"), Some(&midi[..])).unwrap();
        sz.finish().unwrap();

        // tar.gz: a tar holding the song, gzipped
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(midi.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "song.mid", &midi[..]).unwrap();
        std::fs::write(dir.join("song.tar.gz"), gz(&tar.into_inner().unwrap())).unwrap();
        std::fs::write(dir.join("text.txt"), b"hello").unwrap();

        let plain = load("plain.mid").unwrap();
        assert_eq!(plain.note_count, 4);
        for (name, layers) in [
            ("song.gz", "gz"),
            ("song.zst", "zst"),
            ("song.zip", "zip › gz"),
            ("solid.7z", "7z"),
            ("separate.7z", "7z"),
            ("song.tar.gz", "gz › tar"),
        ] {
            let m = load(name).unwrap();
            assert_eq!(m.tracks, plain.tracks, "{name}");
            assert_eq!(
                (m.note_count, m.end_tick, &m.tempos),
                (plain.note_count, plain.end_tick, &plain.tempos)
            );
            assert_eq!(m.source.map(|s| s.0).as_deref(), Some(layers));
        }
        assert!(load("text.txt").is_err());

        // A cancelled load fails at its next read, from a plain file or an archive.
        for name in ["plain.mid", "song.gz", "song.zip", "solid.7z"] {
            let progress = Progress::default();
            progress.cancel();
            let e = MidiFile::open_and_parse(dir.join(name), 127, &progress).err().expect(name);
            assert!(e.to_string().contains("cancelled"), "{name}: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    fn xz(data: &[u8]) -> Vec<u8> {
        let mut w = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap()
    }

    /// An MTrk chunk of `notes` notes starting at `key0`.
    fn track(key0: u8, notes: u32) -> Vec<u8> {
        let mut body = vec![];
        for i in 0..notes {
            body.extend_from_slice(&[0x01, 0x90, key0 + (i % 40) as u8, 100, 0x05, 0x80, key0 + (i % 40) as u8, 0]);
        }
        body.extend_from_slice(&[0, 0xFF, 0x2F, 0]);
        let mut chunk = b"MTrk".to_vec();
        chunk.extend_from_slice(&(body.len() as u32).to_be_bytes());
        chunk.extend_from_slice(&body);
        chunk
    }

    fn header(tracks: u16) -> Vec<u8> {
        let mut h = b"MThd".to_vec();
        h.extend_from_slice(&[0, 0, 0, 6, 0, 1]);
        h.extend_from_slice(&tracks.to_be_bytes());
        h.extend_from_slice(&[0, 96]);
        h
    }

    #[test]
    fn cycles_of_xz_streams_are_counted_per_stream() {
        let dir = std::env::temp_dir().join(format!("fmp_xz_cycle_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (first, a, b) = (track(20, 50), track(30, 300), track(60, 200));
        // first, then (a b) five times: a and b are never next to themselves.
        let mut plain = header(11);
        plain.extend_from_slice(&first);
        let mut streams = xz(&[&header(11)[..], &first].concat());
        for _ in 0..5 {
            plain.extend_from_slice(&[&a[..], &b].concat());
            streams.extend_from_slice(&[xz(&a), xz(&b)].concat());
        }
        std::fs::write(dir.join("plain.mid"), &plain).unwrap();
        std::fs::write(dir.join("cycle.mid.xz"), &streams).unwrap();
        let load = |name: &str| MidiFile::open_and_parse(dir.join(name), 127, &Progress::default()).unwrap();
        let (want, got) = (load("plain.mid"), load("cycle.mid.xz"));
        assert_eq!(got.track_copies, [1, 5, 5]);
        assert_eq!((got.tracks.len(), got.source_tracks), (3, 11));
        assert_eq!((got.note_count, got.file_bytes, got.end_tick), (want.note_count, want.file_bytes, want.end_tick));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_track_of_several_streams_that_repeats_is_counted() {
        // The real file's shape: each track is several xz streams (cut anywhere), and the
        // same streams come back for every copy. Read whole or as the layer is scanned.
        let (first, t) = (track(20, 50), track(30, 300));
        let (x, y) = (t.len() / 3, t.len() * 2 / 3 + 1);
        let mut plain = header(7);
        plain.extend_from_slice(&first);
        let mut streams = xz(&[&header(7)[..], &first].concat());
        for _ in 0..6 {
            plain.extend_from_slice(&t);
            streams.extend_from_slice(&[xz(&t[..x]), xz(&t[x..y]), xz(&t[y..])].concat());
        }
        let want = MidiFile::parse_stream(Box::new(Plain(Cursor::new(plain))), 127, &Progress::default()).unwrap();
        for budget in [XZ_SCAN_MAX, 0] {
            let XzScan::Repeats(r) = scan_xz_within(Cursor::new(streams.clone()), budget, None, None).unwrap() else { panic!() };
            let got = MidiFile::parse_stream(Box::new(r), 127, &Progress::default()).unwrap();
            assert_eq!(got.track_copies, [1, 6], "budget {budget}");
            assert_eq!((got.note_count, got.file_bytes, got.end_tick), (want.note_count, want.file_bytes, want.end_tick));
        }
    }

    #[test]
    fn a_long_layer_is_scanned_as_it_is_read() {
        // Scanned up front for 0 bytes: everything after the first stream is found while
        // the parser reads. Whole tracks that repeat are counted; pairs are read.
        let (first, a, b) = (track(20, 50), track(30, 300), track(60, 200));
        let load = |streams: &[u8]| {
            let XzScan::Repeats(r) = scan_xz_within(Cursor::new(streams.to_vec()), 0, None, None).unwrap() else { panic!() };
            MidiFile::parse_stream(Box::new(r), 127, &Progress::default()).unwrap()
        };
        let mut streams = xz(&[&header(11)[..], &first].concat());
        let mut plain = header(11);
        plain.extend_from_slice(&first);
        for _ in 0..5 {
            plain.extend_from_slice(&[&a[..], &b].concat());
            streams.extend_from_slice(&[xz(&a), xz(&b)].concat());
        }
        let got = load(&streams);
        let want = MidiFile::parse_stream(Box::new(Plain(Cursor::new(plain))), 127, &Progress::default()).unwrap();
        assert_eq!(got.track_copies, [1, 5, 5]);
        assert_eq!((got.tracks.len(), got.source_tracks), (3, 11));
        assert_eq!((got.note_count, got.file_bytes, got.end_tick), (want.note_count, want.file_bytes, want.end_tick));

        let pair = [track(30, 20), track(50, 20)].concat();
        let mut streams = xz(&[&header(7)[..], &first].concat());
        for _ in 0..3 {
            streams.extend_from_slice(&xz(&pair));
        }
        assert_eq!(load(&streams).track_copies, [1; 7]);
    }


    #[test]
    fn the_size_of_xz_layers_comes_from_their_indexes() {
        let (a, b) = (vec![7u8; 1000], vec![9u8; 70_000]);
        let two = [xz(&a), vec![0; 8], xz(&b)].concat();
        assert_eq!(xz_total(&two), Some(71_000));
        assert_eq!(xz_total(&xz(&[])), Some(0));
        assert_eq!(xz_total(b"not xz at all, not xz at all, not xz at all"), None);
    }

    #[test]
    fn the_rest_of_a_layer_is_trusted_only_if_it_is_whole_copies() {
        let (first, t, odd) = (track(20, 50), track(30, 300), track(70, 40));
        let (x, y) = (t.len() / 3, t.len() * 2 / 3 + 1);
        let load = |streams: &[u8], verify: bool| {
            let progress = Progress::default();
            progress.verify_all.store(verify, std::sync::atomic::Ordering::Relaxed);
            let total = Some(streams.len() as u64);
            let XzScan::Repeats(r) = scan_xz_within(Cursor::new(streams.to_vec()), 0, Some(&progress), total).unwrap() else { panic!() };
            let m = MidiFile::parse_stream(Box::new(r), 127, &progress).unwrap();
            (m, progress.trusted_tracks.load(std::sync::atomic::Ordering::Relaxed))
        };
        let (mut plain, mut streams) = (header(41), xz(&[&header(41)[..], &first].concat()));
        plain.extend_from_slice(&first);
        for _ in 0..40 {
            plain.extend_from_slice(&t);
            streams.extend_from_slice(&[xz(&t[..x]), xz(&t[x..y]), xz(&t[y..])].concat());
        }
        let want = MidiFile::parse_stream(Box::new(Plain(Cursor::new(plain.clone()))), 127, &Progress::default()).unwrap();
        // whole copies to the end: the last ones are taken on trust; or all read
        let (got, trusted) = load(&streams, false);
        assert_eq!((got.track_copies.clone(), trusted), (vec![1, 40], 31));
        assert_eq!((got.note_count, got.file_bytes), (want.note_count, want.file_bytes));
        let (got, trusted) = load(&streams, true);
        assert_eq!((got.track_copies, trusted), (vec![1, 40], 0));
        // a different track at the end: not whole copies, so everything is read
        let mut streams = streams;
        streams.extend_from_slice(&xz(&odd));
        let (got, trusted) = load(&streams, false);
        assert_eq!((got.track_copies, trusted), (vec![1, 40, 1], 0));
    }

    #[test]
    fn repeated_xz_streams_are_read_once() {
        let dir = std::env::temp_dir().join(format!("fmp_xz_repeat_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let load = |name: &str| MidiFile::open_and_parse(dir.join(name), 127, &Progress::default()).unwrap();

        // A tempo-like first track, then one track repeated 40 times; and the same as a
        // plain file.
        let (first, repeated) = (track(20, 50), track(30, 300));
        let mut plain = header(41);
        plain.extend_from_slice(&first);
        for _ in 0..40 {
            plain.extend_from_slice(&repeated);
        }
        std::fs::write(dir.join("plain.mid"), &plain).unwrap();
        let mut streams = xz(&[&header(41)[..], &first].concat());
        let one = xz(&repeated);
        for _ in 0..40 {
            streams.extend_from_slice(&one);
        }
        std::fs::write(dir.join("rep.mid.xz"), &streams).unwrap();
        // nested in another layer too (that is how the real file comes)
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&streams).unwrap();
        std::fs::write(dir.join("rep.mid.xz.gz"), gz.finish().unwrap()).unwrap();

        let want = load("plain.mid");
        for name in ["rep.mid.xz", "rep.mid.xz.gz"] {
            let m = load(name);
            assert_eq!(m.track_copies, [1, 40], "{name}");
            assert_eq!((m.tracks.len(), m.source_tracks), (2, 41), "{name}");
            assert_eq!(m.note_count, want.note_count, "{name}");
            assert_eq!((m.file_bytes, m.end_tick, m.note_tracks), (want.file_bytes, want.end_tick, want.note_tracks), "{name}");
            assert_eq!(m.channels_used, want.channels_used);
            // the stored track plays the same as every copy of it in the plain file
            let notes = |m: &MidiFile, i: usize| {
                let (mut r, mut out) = (crate::parser::TrackReader::default(), vec![]);
                while let Some(x) = r.next(m.track(i)) {
                    out.push(x);
                }
                out
            };
            assert_eq!(notes(&m, 1), notes(&want, 1), "{name}");
            assert_eq!(notes(&want, 1), notes(&want, 40));
        }

        // A repeated run of two tracks can't be counted instead of read: decoded in full.
        let pair = [track(30, 20), track(50, 20)].concat();
        let mut plain = header(7);
        plain.extend_from_slice(&first);
        let mut streams = xz(&[&header(7)[..], &first].concat());
        for _ in 0..3 {
            plain.extend_from_slice(&pair);
            streams.extend_from_slice(&xz(&pair));
        }
        std::fs::write(dir.join("plain2.mid"), &plain).unwrap();
        std::fs::write(dir.join("pairs.mid.xz"), &streams).unwrap();
        let (want, got) = (load("plain2.mid"), load("pairs.mid.xz"));
        assert_eq!(got.track_copies, [1; 7]);
        assert_eq!((got.note_count, got.file_bytes), (want.note_count, want.file_bytes));

        // An ordinary xz file, and one with different streams, load as usual.
        std::fs::write(dir.join("one.mid.xz"), xz(&plain)).unwrap();
        let mixed = [xz(&[&header(3)[..], &first].concat()), xz(&track(40, 10)), xz(&track(41, 10))].concat();
        std::fs::write(dir.join("mixed.mid.xz"), mixed).unwrap();
        assert_eq!(load("one.mid.xz").note_count, want.note_count);
        assert_eq!(load("mixed.mid.xz").track_copies, [1, 1, 1]);

        // Repeats of repeats: each stream of that file compressed alone (as the real file
        // does), so the layer above is known from the runs, without producing the copies.
        let (first, repeated) = (track(20, 50), track(30, 300));
        let mut plain = header(41);
        plain.extend_from_slice(&first);
        let (head, one) = (xz(&[&header(41)[..], &first].concat()), xz(&repeated));
        let mut inner = [&head[..]].concat();
        let mut outer = xz(&head);
        for _ in 0..40 {
            plain.extend_from_slice(&repeated);
            inner.extend_from_slice(&one);
            outer.extend_from_slice(&xz(&one));
        }
        std::fs::write(dir.join("plain3.mid"), &plain).unwrap();
        std::fs::write(dir.join("nested.mid.xz.xz"), &outer).unwrap();
        let mut scan = match scan_xz(Cursor::new(&outer), None, None).unwrap() {
            XzScan::Repeats(r) => r,
            XzScan::Plain(_) => panic!("the outer streams repeat"),
        };
        let runs = scan.xz_runs().expect("the runs decode to whole streams");
        assert_eq!(runs, [(head.clone(), 1), (one.clone(), 40)]);
        let (want, got) = (load("plain3.mid"), load("nested.mid.xz.xz"));
        assert_eq!(got.track_copies, [1, 40]);
        assert_eq!((got.note_count, got.file_bytes), (want.note_count, want.file_bytes));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
