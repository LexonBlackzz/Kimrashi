# Kimrashi v0.2

Loads much bigger files with much less memory, draws dense songs without falling over,
and keeps up better with synths that can't take every note.

## Loading

- **Far less RAM while loading.** Tracks are packed in a single pass through a 1 MB window
  instead of being read whole. A 60M-note single-track file peaked at 1.4 GB before and
  276 MB now. Tracks over 64 MB in an archive are packed straight off the stream.
- **Faster parsing**: 20-30% on real black MIDIs (a 3 GB file: 1.56 s -> 1.15 s). A big
  single track whose channels are all busy is now split by channel across threads
  (60M notes, 16 channels: 1.8 s -> 1.1 s).
- **7z fixes.** The 7z library's multi-threaded decoder kept whole decoded units in memory
  and ran out of RAM on very compressible files (the 10-billion-note 7z hit 27 GB in
  17 s). It now decodes on one thread and loads in about a minute at ~400 MB. Solid
  archives also read the wrong data when the wanted file wasn't first (The Nuker 3.7z
  failed with "Not a MIDI file"); fixed.
- **Repeated tracks in xz.** An xz layer that is one stream repeated back to back is read
  once: if the repeat is a whole MIDI track, it is parsed and stored once and counted
  for every copy. A 21.3 TB, 2.66-trillion-note file made of one track copied 65,534
  times loads in 4-6 s (2.2 MB packed). Anything that isn't a repeat loads as before.
- **Cancel** a load from the progress bar (opening another file cancels it too).
- `kimrashi file.mid --from m:ss` opens a file at startup and seeks into it.
- The note counter was a 32-bit number that stopped at 4.29 billion; it is 64-bit now, and
  sizes and counts go up to trillions.

## Playback

- **Smarter note skipping.** The skipper works out, every 4 ms, how many notes per second the
  thread can send (from the measured cost per synth message) and gives every key the same
  minimum re-strike interval at which they fit, instead of reacting to lag after it has
  built up. Keys played less often than that are never touched. With a stand-in synth
  charging ~100 ns per message, lag on a 20M notes/s song went from 12-22 ms to 0 while
  sending 25% more notes. Not yet tried against a real OmniMIDI.
- The playback thread does less: everything due is processed and sent in one batch, and
  skipped notes leave nothing behind. Polyphony now counts notes actually sent.
- **Space** plays/pauses. **Stop** is now **Unload** (it closes the song).

## Graphics

- **No more frame-rate collapse on very dense songs.** SFTDP LAST V at a 0.06 s note speed
  fell from ~300 fps to ~14 fps for 10+ seconds; decoding was giving each of its 60,000
  tracks its own allocations and the survivors were sorted on one thread. Now 120-240 fps in
  the worst stretch.
- **Exact culling.** Only notes that are fully hidden are dropped, as long as there are up to 3M
  notes on screen; the lossy merging of nearly-hidden notes (which turned 1-2 pixel notes
  into mosaic) only happens beyond that. Layering is Piano From Above's: later start on
  top, ties to the higher track.
- **Layout toggle** (Settings -> Display): *Even* (a column per semitone, as before) or
  *Piano* (white keys equal, sharps narrower and in front, like the keys and like PFA).
- **Auto colouring** (new default): by channel for a file with up to 16 note tracks that uses
  several channels, by track for files with many tracks, by channel for a single track.
  *By channel* and *By track* are still there.
- Finished notes are swept out of the window now and then instead of every frame, and
  decoding copies less: frame CPU time on a 900K-note screen fell by ~30%.
- Set `KIM_FRAMELOG=path` to write one line per second of frame timings.

## Build

- `cargo build --release` takes seconds after a change (was ~100 s): fat LTO and a single
  codegen unit made no measurable difference to speed here.

## Known limitations

- Seeking far into a dense part of a huge file is slow (the player and the display step
  through every note to get there). A skip index is planned.
- Repeat detection covers repeated xz streams only (not zstd/gzip frames or repeats inside
  a track).
- The skipping changes were measured against a stand-in synth, not the real OmniMIDI.
