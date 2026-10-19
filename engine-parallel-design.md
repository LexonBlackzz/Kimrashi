# Multi-threaded playback engine: design and expected gains

Status: proposal only, nothing built. Measured 19 October 2026 on an i5-13600KF
(20 threads), "The Nuker 3 Ultimate F4" (2.08 B notes) and "Funky Stars" (2.29 B notes).

## 1. What limits NPS today

`Decoder::run` (one thread, `player-decode`) calls `Engine::process_tick` for every tick,
in slices of at most 500 µs of song time, at most 3 ms ahead of the clock. The engine alone
(`kimrashi --engine <file>`, no synth, no clock) does:

| | Nuker 3 | Funky Stars |
|---|---|---|
| no skipping | 61 M notes/s | 68 M notes/s |
| skip interval 2 ms | 74 M notes/s | not measured |

That is about 16 ns per note, and it is the 80-100 M NPS ceiling. The full loop with
nothing skipped is slower still (22 M notes/s) because every message is timestamped and sent.

A line-level sample of the engine (Nuker 3, 125 k samples) put the time here:

| Step | Share |
|---|---|
| Reading the next record of a track (`TrackReader::next_at`, `body`) | ~32% |
| Skip decision (`Skipper::note_on`) | 11% |
| Re-sorting the note-off queue (`RadixHeap::min_key`) | 10% |
| Note-off handling (`Skipper::note_off`) | 6% |
| Pushing each sent note's note-off | 6% |
| The rest (message building, counters, track bookkeeping) | ~35% |

The workload: 5.3 M ticks, 125 M track visits (about 24 tracks per tick), about 16 notes
decoded per track visit. Tried and dropped, both with an identical message hash and no
speed-up: merging the skipper's four per-slot arrays into one struct (+1-2%, noise) and
prefetching the next tracks' data (+1%, noise). Memory misses are not the cost; it is the
per-note work itself, so only more threads help.

## 2. What is independent

Per (channel, key) "slot" the engine keeps: the skipper state (`last_on_us`, `sounding`,
`hold_until`, `demand`), and that slot's pending note-offs. A note-on or note-off decision
reads and writes only its own slot. Global things are only: the output message order, the
polyphony and counters (telemetry), and `regulate` (which reads all `demand` once per slice).

So the serial part is not the skip logic, only putting messages in order.

## 3. Options

### A. Parse in parallel, everything else serial
Workers each own a group of tracks and parse ahead into sorted event lists; one thread merges
them and runs the skipper and note-offs as today.
- Removes only the ~32% parse cost, adds a merge (~4 ns per event).
- Expected: at most **1.2x**. Not worth the complexity.

### B. Parse in parallel, then split by slot across threads (recommended)
Three stages per slice (a slice is 500 µs or more of song time; a dense slice is ~40 k events
at 80 M notes/s, about 1 MB):

1. **Parse** (G threads, each owns a group of tracks and their `TrackReader`s): decode every
   record in the slice's tick range and scatter it into per-partition buckets. A note event is
   (tick, track, order within track, ch, key, vel, duration). Non-note records (controllers,
   program changes, SysEx) go to a pass-through bucket.
2. **Decide** (P threads, each owns a set of slots, chosen by hashing (ch, key)): merge the G
   buckets for its slots in (tick, track, order) order and run `note_on`/`note_off` and the
   note-off queue for those slots only. Output: the messages to send, each tagged with a
   sort key.
3. **Merge** (one thread): k-way merge of the P outputs and the pass-through by sort key into
   the slice's message list. It only touches messages that are actually sent, and skipping
   removes most notes before this point.

The skipper's rules stay exactly as they are; they just run on per-slot state held by the
partition that owns the slot.

### C. Cheaper changes inside the serial engine
Nothing found beyond the two dropped above. Remaining ideas (counter updates once per track
visit, avoiding the double decode in `peek_tick`) are about 1-3% each.

## 4. The hard part: reproducing the exact output order

Today's order within a tick: due note-offs first, in the order they come out of the radix
heap, then every track's records in track order. To keep the output **byte-identical** (so it
can be checked with the existing hash), every message needs a canonical sort key:

- tick;
- phase (note-offs before records);
- for a note-off: the sequence number of the push that created it (its note-on's position in
  the global event order, or the postponed note-off's position when it was re-pushed);
- for a record: (track, order within track).

The radix heap keeps pushes of equal keys in push order, so "order of push" is well defined.
Postponed note-offs (re-pushed during processing of a later tick) need the sequence number of
the moment they were re-pushed. This is the part that needs care and a property test, not a
design problem.

## 5. Expected gains

Work per event rises from ~16 ns to ~22-25 ns (scatter write and read, bucket merge), but is
spread over threads. With the skipper active (the case that matters, because that is when NPS
is high) the serial merge touches few messages.

| Worker threads (parse + decide) | Expected engine rate vs today |
|---|---|
| 1-2 (no benefit, stay single-threaded) | 1x |
| 4 | ~2x |
| 8 | ~3-4x |
| 12-16 | ~4-6x |

On the 20-thread test machine that would take the 74 M notes/s ceiling to roughly
**250-350 M notes/s**. These are estimates, not measurements; the real figure depends on how
evenly the notes spread over slots. A black MIDI that hammers a few keys gives a few busy
partitions and many idle ones; a single hot slot is still fine (about 3 ns per note-on on one
thread, so over 300 M/s on that slot alone).

What this does not change: how many notes the synth can take, what is audible, or the
renderer. It only raises how many notes per second the skipper can look at.

## 6. Risks

1. **Output order / skipper semantics.** Mitigated by keeping the hash identical on both
   files, with and without a skip interval, at every phase (the `--engine` bench already does
   this). Cannot be judged by ear from here.
2. **Competing for cores.** The synth, the renderer's decode pool and the UI run on the same
   CPU. Engine workers must only start when the song needs them (engine behind its slice time,
   or a sustained rate above ~40 M notes/s) and stop when it does not, so weak machines and
   sparse songs are unaffected. This is the same constraint as the memory one: better on
   strong machines, never worse on weak ones.
3. **Skipper feedback.** `Costs::engine_ns` is measured as wall time per decoded note; with
   workers it falls, which changes the budget `regulate` computes. It needs recalibrating
   against thread time, and the real-time loop (`lag_us` feedback) needs testing under load.
4. **Seek.** `Cursor::seek` builds readers and the note-off chase for one thread; it must
   split them by track group and by slot owner.
5. **Telemetry.** Polyphony and peak polyphony become per-slice sums, so the peak can differ
   slightly from today.

## 7. Plan and checkpoints

Each phase ends with the hash unchanged on Nuker 3 and Funky Stars, with and without
`ENGINE_INTERVAL_US=2000`, and the unit tests passing.

| Phase | What | Expected speed | Effort |
|---|---|---|---|
| 0 | `--engine` bench with hash (done, `4147c42`) | 1x | done |
| 1 | Move slot state and note-off queue into a `Partition`; one partition = today's behaviour | 1x | 1-2 days |
| 2 | Stage 1 as a separate pass producing events; single partition consumes them | 0.8-0.9x | 2-3 days |
| 3 | P partitions on threads, sequence-number ordering, merge; offline in `--engine` first | 3-5x (8 threads) | 3-5 days |
| 4 | Plug into `Decoder` (slice fork-join), engage only when needed, feedback and seek | as phase 3 | 2-4 days |

Stop points: after phase 2 if it is slower than 0.8x; after phase 3 if the offline rate is
under 2x on 8 threads. Total: roughly one to two weeks, and the last phase needs listening
tests on a real synth, which only you can do.

## 8. Recommendation

Worth doing only if hitting the 80-100 M NPS ceiling on real songs is a real problem for you,
since it does nothing for visual lag or for what the synth can play. If it is, start with
phases 1 and 2: they are safe, hash-checkable, and tell us the true overhead before any
threading goes in.

## 9. What was built and measured

Built as `src/player/par.rs` (phases 1-4 collapsed: a serial phase 2 would have been throwaway).
Chunks of tracks parse in parallel, partitions (a range of slots each) run the skipper, one thread
emits. Spinning worker threads (no thread-pool wake-up latency), chunks and slot ranges rebalanced
by measured load. The decoder switches to it after 16 slices in a row behind the clock and back
after about 400 loops of waiting; `KIM_ENGINE_THREADS=0` turns it off.

Engine alone, Nuker 3 dense stretch (430-440 s, 237M notes, skip interval 2 ms):

| | notes/s |
|---|---|
| serial | 73M |
| 1 thread | 80M |
| 4 threads | 138M |
| 8 threads | 131M |

First attempt (thread pool, sorting events and messages) was 2.7x slower than serial. Message
streams match the serial engine (note-offs of a tick by slot) on both files with and without
skipping (`ENGINE_HASH`, `ENGINE_THREADS`), and in a unit test with rebalancing and mode switches.
Whole files are not faster: sparse parts pay for the synchronisation, which is why the decoder only
uses it when behind. Not tested with a real synth.

Ideas checked and dropped: levels of pre-thinned notes (Nuker 3 keeps ~35% of notes at any
interval up to 8 ms, so a level is as big as the song) and caching decoded chunks (decoding is
under half the per-note cost; chunks are already stored once, 7-24 uses each).
