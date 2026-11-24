//! The engine's track reading on a thread of its own.
//!
//! Reading the tracks never depends on what the skipper decides, so a reader thread can run
//! ahead of the engine: it walks the tracks tick by tick as the serial engine does and hands
//! over each tick's records, track by track, in blocks. The engine plays them through the
//! same note-off queue and skipper as before, so its messages are exactly the serial ones.

use super::*;

/// A tick's records as the reader saw them.
#[derive(Clone, Copy)]
pub(super) enum Rec {
    /// The records after this, up to the next `Tick`, are at this tick.
    Tick(u32),
    /// The records after this are this track's.
    Track(u32),
    Note { ch: u8, key: u8, vel: u8, dur: Option<u32> },
    /// A bulk run (`TrackReader::same_tick_run`), right after a note.
    Run { ch: u8, vel: u8, n: u8, dur: Option<u32>, keys: [u8; 16] },
    Other { status: u8, d1: u8, d2: u8 },
    Sysex { status: u8, at: usize, len: u32 },
}

/// `Due::Track` of the reader's next tick in the engine's queue, standing for the tracks'.
pub(super) const READER: u32 = u32::MAX;

/// Records per block, about (a block ends with a tick).
const BLOCK: usize = 4096;
/// Blocks the reader may be ahead.
const BLOCKS_AHEAD: usize = 8;

/// The engine's end of the reader thread.
pub(super) struct Pipe {
    rx: Receiver<Vec<Rec>>,
    spare: Sender<Vec<Rec>>,
    pub(super) block: Vec<Rec>,
    pub(super) at: usize,
}

impl Pipe {
    /// Start reading `readers` from their positions; `due` holds each track's next tick.
    pub(super) fn new(song: Arc<SongData>, readers: Vec<TrackReader>, due: RadixHeap<u32>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(BLOCKS_AHEAD);
        let (spare, spares) = crossbeam_channel::bounded(BLOCKS_AHEAD + 2);
        thread::Builder::new()
            .name("engine-reader".into())
            .spawn(move || read(&song, readers, due, tx, spares))
            .expect("spawn reader thread");
        Self { rx, spare, block: Vec::new(), at: 0 }
    }

    /// The next tick with records (waiting for the reader if it has to).
    pub(super) fn peek_tick(&mut self) -> Option<u32> {
        if self.at == self.block.len() {
            let next = self.rx.recv().ok()?;
            let _ = self.spare.try_send(std::mem::replace(&mut self.block, next));
            self.at = 0;
        }
        match self.block.get(self.at) {
            Some(&Rec::Tick(t)) => Some(t),
            _ => None,
        }
    }
}

fn read(song: &SongData, mut readers: Vec<TrackReader>, mut due: RadixHeap<u32>, tx: Sender<Vec<Rec>>, spares: Receiver<Vec<Rec>>) {
    let (mut tracks, mut keys) = (Vec::new(), [0u8; 16]);
    let fresh = |spares: &Receiver<Vec<Rec>>| {
        let mut b = spares.try_recv().unwrap_or_else(|_| Vec::with_capacity(BLOCK * 2));
        b.clear();
        b
    };
    let mut block = fresh(&spares);
    while let Some(tick) = due.min_key() {
        due.take_min(&mut tracks);
        tracks.sort_unstable();
        block.push(Rec::Tick(tick));
        for &i in &tracks {
            let data = song.midi.track(i as usize);
            let r = &mut readers[i as usize];
            block.push(Rec::Track(i));
            while let Some(rec) = r.next_at(data, tick) {
                match rec {
                    Record::Note { ch, key, vel, dur } => {
                        block.push(Rec::Note { ch, key, vel, dur });
                        while let Some((ch, vel, dur, n)) = r.same_tick_run(data, &mut keys) {
                            block.push(Rec::Run { ch, vel, n: n as u8, dur, keys });
                        }
                    }
                    Record::Other { status, d1, d2 } => block.push(Rec::Other { status, d1, d2 }),
                    Record::Sysex { status, at, len } => block.push(Rec::Sysex { status, at, len }),
                }
            }
            if let Some(t) = r.peek_tick(data) {
                due.push(t, i);
            }
        }
        tracks.clear();
        if block.len() >= BLOCK {
            let next = fresh(&spares);
            if tx.send(std::mem::replace(&mut block, next)).is_err() {
                return; // the engine moved on (a seek)
            }
        }
    }
    if !block.is_empty() {
        let _ = tx.send(block);
    }
}
