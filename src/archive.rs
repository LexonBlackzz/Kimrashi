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
//! An xz layer that is one stream repeated back to back (see `scan_xz`) is not decoded
//! repeat by repeat: if a repeat is a whole MIDI track, the track is parsed once and
//! counted for every copy.

use std::fs::File;
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
// xz files made of one stream repeated
// ------------------------------------------------------------------
//
// An .xz file can be several streams back to back. If the same stream is repeated many
// times (a MIDI whose tracks are one track copied thousands of times, compressed one copy
// at a time), the bytes of the repeats are the same, and so is what they decode to. So the
// layer is read as runs of identical streams; each run is decoded once, and if the MIDI
// parser finds that a run is exactly one track, the copies are never decoded or parsed:
// the track is stored once and counted `n` times (see `Source`).

const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0];
/// A stream longer than this is not compared with others (and a layer whose first stream
/// is longer is read as usual).
const XZ_STREAM_MAX: usize = 64 << 20;
/// Most bytes of distinct streams held while looking for repeats.
const XZ_STORE_MAX: usize = 256 << 20;

/// One stream's bytes, and how many times in a row it occurs.
struct XzRun {
    bytes: Vec<u8>,
    count: u64,
}

/// What `scan_xz` found.
enum XzScan<'a> {
    /// Some stream repeats: the layer, decoded run by run.
    Repeats(RepeatedXz),
    /// Nothing to gain: the bytes as they were (to decode as usual).
    Plain(Box<dyn Read + 'a>),
}

/// The bytes of `runs`, each repeated, then `then`.
struct Expanded<R> {
    runs: Vec<XzRun>,
    run: usize,
    copy: u64,
    pos: usize,
    then: R,
}

impl<R: Read> Read for Expanded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while let Some(run) = self.runs.get(self.run) {
            if self.pos < run.bytes.len() {
                let n = buf.len().min(run.bytes.len() - self.pos);
                buf[..n].copy_from_slice(&run.bytes[self.pos..][..n]);
                self.pos += n;
                return Ok(n);
            }
            self.pos = 0;
            self.copy += 1;
            if self.copy >= run.count {
                self.copy = 0;
                self.run += 1;
            }
        }
        self.then.read(buf)
    }
}

/// Positions of `XZ_MAGIC` in `bytes[from..]`.
fn magics(bytes: &[u8], from: usize) -> Vec<usize> {
    let mut found = vec![];
    let mut at = from;
    while let Some(i) = bytes.get(at..).and_then(|rest| rest.iter().position(|&b| b == XZ_MAGIC[0])) {
        let i = at + i;
        if bytes[i..].starts_with(&XZ_MAGIC) {
            found.push(i);
        }
        at = i + 1;
    }
    found
}

/// A finished stream's bytes, up to its footer (a stream may be followed by padding).
fn stream_end(piece: &[u8]) -> Option<usize> {
    let end = piece.iter().rposition(|&b| b != 0)? + 1;
    (end >= 32 && piece[end - 2..end] == *b"YZ").then_some(end)
}

/// Read `r` as xz streams back to back, grouping identical ones.
fn scan_xz<'a, R: Read + 'a>(mut r: R) -> std::io::Result<XzScan<'a>> {
    let mut runs: Vec<XzRun> = Vec::new();
    let mut stored = 0usize;
    // The stream being read: starts with its magic.
    let mut cur: Vec<u8> = Vec::new();
    let mut block = vec![0u8; 1 << 20];
    let mut eof = false;
    let give_up = |runs, cur, r| -> XzScan<'a> {
        XzScan::Plain(Box::new(Expanded { runs, run: 0, copy: 0, pos: 0, then: Cursor::new(cur).chain(r) }))
    };
    loop {
        let n = r.read(&mut block)?;
        let from = cur.len().saturating_sub(XZ_MAGIC.len() - 1).max(1);
        cur.extend_from_slice(&block[..n]);
        eof = n == 0;
        if cur.len() < XZ_MAGIC.len() && !eof {
            continue; // a short read
        }
        if cur.len() < XZ_MAGIC.len() || cur[..XZ_MAGIC.len()] != XZ_MAGIC {
            return Ok(give_up(runs, cur, r)); // not an xz stream (or no more of them)
        }
        // Everything before the last magic found is whole streams.
        let starts = magics(&cur, from);
        let mut done = 0;
        for w in 0..starts.len() + usize::from(eof) {
            let end = starts.get(w).copied().unwrap_or(cur.len());
            let piece = &cur[done..end];
            if stream_end(piece).is_none() {
                if end < cur.len() {
                    continue; // a magic inside the stream's data, not the next stream
                }
                return Ok(give_up(runs, cur, r)); // not a stream, or cut off
            }
            match runs.last_mut() {
                Some(run) if run.bytes == piece => run.count += 1,
                _ => {
                    stored += piece.len();
                    runs.push(XzRun { bytes: piece.to_vec(), count: 1 });
                }
            }
            done = end;
        }
        cur.drain(..done);
        if cur.len() > XZ_STREAM_MAX || stored > XZ_STORE_MAX {
            return Ok(give_up(runs, cur, r));
        }
        if eof {
            break;
        }
    }
    if runs.iter().all(|run| run.count == 1) {
        return Ok(give_up(runs, cur, r));
    }
    Ok(XzScan::Repeats(RepeatedXz::new(runs)))
}

/// The decoded bytes of runs of identical xz streams, each run's first copy only if the
/// parser says the rest are tracks it can count instead of read.
struct RepeatedXz {
    runs: Vec<XzRun>,
    /// The run being read, and which copy of it (0 = not started).
    run: usize,
    copy: u64,
    decoder: Option<Box<dyn Read>>,
    /// Decoded bytes handed out so far, and where the current run began.
    pos: u64,
    run_start: u64,
    /// A byte read ahead to see whether the decoder was at its end.
    peeked: Option<u8>,
    /// The parser took the rest of this run as repeats.
    skipped: bool,
}

impl RepeatedXz {
    fn new(runs: Vec<XzRun>) -> Self {
        Self { runs, run: 0, copy: 0, decoder: None, pos: 0, run_start: 0, peeked: None, skipped: false }
    }

    fn decode(&self) -> Box<dyn Read> {
        Self::decode_run(&self.runs[self.run])
    }

    fn decode_run(run: &XzRun) -> Box<dyn Read> {
        let end = stream_end(&run.bytes).unwrap_or(run.bytes.len());
        Box::new(lzma_rust2::XzReader::new(Cursor::new(run.bytes[..end].to_vec()), true))
    }
}

impl Read for RepeatedXz {
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
            if self.run >= self.runs.len() {
                return Ok(0);
            }
            if self.decoder.is_none() {
                if self.copy == 0 {
                    self.run_start = self.pos;
                }
                self.copy += 1;
                self.decoder = Some(self.decode());
            }
            let n = self.decoder.as_mut().unwrap().read(buf)?;
            if n > 0 {
                self.pos += n as u64;
                return Ok(n);
            }
            // This copy is done: the next copy of the run (unless the parser counted
            // them), or the next run.
            self.decoder = None;
            if self.copy >= self.runs[self.run].count || self.skipped {
                self.run += 1;
                self.copy = 0;
                self.skipped = false;
            }
        }
    }
}

impl Source for RepeatedXz {
    // When every run decodes to exactly one xz stream (a layer of xz made of repeats of
    // xz streams), the layer above is those streams, each repeated as often as the run
    // is: known without producing a byte of the repeats.
    fn xz_runs(&mut self) -> Option<Vec<(Vec<u8>, u64)>> {
        if self.run != 0 || self.copy != 0 || self.decoder.is_some() || self.peeked.is_some() || self.pos != 0 {
            return None;
        }
        let mut out: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut stored = 0;
        for run in &self.runs {
            let mut decoder = Self::decode_run(run);
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
                Some((last, count)) if *last == bytes => *count += run.count,
                _ => out.push((bytes, run.count)),
            }
        }
        Some(out)
    }

    fn extra_copies(&mut self, start: u64, end: u64) -> u64 {
        let Some(run) = self.runs.get(self.run) else { return 0 };
        // A whole run, one track: its first copy began where the track did and ends here.
        if run.count < 2 || self.copy != 1 || self.skipped || self.run_start != start || self.pos != end {
            return 0;
        }
        if let Some(decoder) = &mut self.decoder {
            let mut next = [0u8; 1];
            match decoder.read(&mut next) {
                Ok(0) => {}
                Ok(_) => {
                    self.peeked = Some(next[0]); // more follows: not one track
                    return 0;
                }
                Err(_) => return 0,
            }
        }
        self.skipped = true;
        run.count - 1
    }
}


/// Load the MIDI inside the archive or compressed file at `path`.
pub fn open(path: &Path, vel_levels: u8, progress: &Progress) -> Result<MidiFile, ParseError> {
    let size = std::fs::metadata(path)?.len();
    let mut ctx = Ctx { vel_levels, progress, layers: Vec::new() };
    let mut midi = ctx.file(path, 0)?;
    midi.source = Some((ctx.layers.join(" › "), size));
    Ok(midi)
}

struct Ctx<'a> {
    vel_levels: u8,
    progress: &'a Progress,
    /// The formats unwrapped so far, outermost first.
    layers: Vec<&'static str>,
}

impl<'a> Ctx<'a> {
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
                self.layers.push("xz");
                // A file that is one stream repeated many times is read specially.
                if let XzScan::Repeats(repeated) = scan_xz(self.reader(path)?)? {
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
                        let xz = lzma_rust2::XzReader::new(self.reader(path)?, true);
                        self.stream(plain(xz), depth + 1)
                    }
                }
            }
            Kind::Gz | Kind::Zstd | Kind::Tar => {
                let r = self.reader(path)?;
                self.stream(plain(r), depth)
            }
            Kind::Zip => {
                self.layers.push("zip");
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
                self.layers.push("7z");
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
                self.layers.push("rar");
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
        if let Some(runs) = r.xz_runs() {
            self.layers.push("xz");
            let runs = runs.into_iter().map(|(bytes, count)| XzRun { bytes, count }).collect();
            return self.stream(Box::new(RepeatedXz::new(runs)), depth + 1);
        }
        let mut head = Vec::with_capacity(512);
        (&mut r).take(512).read_to_end(&mut head)?;
        let kind = Kind::sniff(&head).ok_or("Not a MIDI file or a supported archive")?;
        let mut r: Box<dyn Source + '_> = Box::new(Prefixed { head: Cursor::new(head), rest: r });
        match kind {
            Kind::Midi => MidiFile::parse_stream(r, self.vel_levels),
            Kind::Gz => {
                self.layers.push("gz");
                self.stream(plain(flate2::read::MultiGzDecoder::new(r)), depth + 1)
            }
            Kind::Xz => {
                self.layers.push("xz");
                match scan_xz(r)? {
                    XzScan::Repeats(repeated) => self.stream(Box::new(repeated), depth + 1),
                    XzScan::Plain(bytes) => self.stream(plain(lzma_rust2::XzReader::new(bytes, true)), depth + 1),
                }
            }
            Kind::Zstd => {
                self.layers.push("zst");
                let mut zst = zstd::stream::read::Decoder::new(r)?;
                // Allow the large windows `zstd --long` / `--ultra` produce (up to 2 GB).
                zst.window_log_max(31)?;
                self.stream(plain(zst), depth + 1)
            }
            Kind::Tar => {
                self.layers.push("tar");
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
        let mut scan = match scan_xz(Cursor::new(&outer)).unwrap() {
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
