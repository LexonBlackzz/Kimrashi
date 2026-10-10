//! Loading MIDI files from inside archives and compressed files, nested up to
//! `MAX_DEPTH` layers deep (e.g. a .mid.xz inside a .7z, or a .tar.zst).
//!
//! After each layer the output's first bytes say what it is: a MIDI is parsed, another
//! layer is unwrapped. Stream formats (gz, xz, zst, tar) and entries of zip/7z files
//! are decompressed straight into the parser, so the uncompressed MIDI never exists in
//! full. Two cases go through a temporary file instead: rar (the unrar library can only
//! extract to disk), and a zip/7z/rar found *inside* another layer (those need random
//! access). If a layer holds several candidates, the largest is loaded.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::parser::{MidiFile, ParseError, Progress};

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
                        self.stream(Box::new(xz), depth + 1)
                    }
                    _ => {
                        let xz = lzma_rust2::XzReader::new(self.reader(path)?, true);
                        self.stream(Box::new(xz), depth + 1)
                    }
                }
            }
            Kind::Gz | Kind::Zstd | Kind::Tar => {
                let r = self.reader(path)?;
                self.stream(Box::new(r), depth)
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
                self.stream(Box::new(entry), depth + 1)
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
                            result = Some(self.stream(Box::new(reader), depth + 1));
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
    fn stream(&mut self, mut r: Box<dyn Read + '_>, depth: usize) -> Result<MidiFile, ParseError> {
        if depth > MAX_DEPTH {
            return Err("Archive nested too deeply".into());
        }
        let mut head = Vec::with_capacity(512);
        (&mut r).take(512).read_to_end(&mut head)?;
        let kind = Kind::sniff(&head).ok_or("Not a MIDI file or a supported archive")?;
        let mut r: Box<dyn Read + '_> = Box::new(Cursor::new(head).chain(r));
        match kind {
            Kind::Midi => MidiFile::parse_stream(r, self.vel_levels),
            Kind::Gz => {
                self.layers.push("gz");
                self.stream(Box::new(flate2::read::MultiGzDecoder::new(r)), depth + 1)
            }
            Kind::Xz => {
                self.layers.push("xz");
                self.stream(Box::new(lzma_rust2::XzReader::new(r, true)), depth + 1)
            }
            Kind::Zstd => {
                self.layers.push("zst");
                let mut zst = zstd::stream::read::Decoder::new(r)?;
                // Allow the large windows `zstd --long` / `--ultra` produce (up to 2 GB).
                zst.window_log_max(31)?;
                self.stream(Box::new(zst), depth + 1)
            }
            Kind::Tar => {
                self.layers.push("tar");
                // Sequential, so the first candidate is used (sizes aren't known ahead).
                let mut tar = tar::Archive::new(r);
                for entry in tar.entries()? {
                    let entry = entry?;
                    let name = entry.path()?.to_string_lossy().into_owned();
                    if entry.header().entry_type().is_file() && is_candidate(&name) {
                        return self.stream(Box::new(entry), depth + 1);
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
}
