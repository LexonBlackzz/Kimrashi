# Velocity matrix view

## Intent

Some MIDIs draw pictures in velocity space: every sounding note lights the cell at (key, velocity),
and the song animates text and shapes that way (e.g. "LMAO HI", 12.8 M notes on 128 tracks, keys
2–126, velocities 21–105, 6,214 distinct cells). The user wants a view that shows this, live and in
export, black-MIDI capable.

## Goals / non-goals / anti-goals

- Goals: a `Matrix` view mode; a 128 × 128 grid, keys across, velocity up; a cell is lit while at
  least one note on it sounds (the user's call); live and export (`--view matrix`).
- Non-goals: fading trails (a possible slider later), a keyboard, note speed (no time axis).
- Anti-goals: no extra memory per note; the other views don't change (`--verify` passes).

## Approaches

1. **Matrix walker (chosen by the user).** Its own reader per track over the packed song
   (`MidiFile` tracks, `TrackReader`), kept in step with the playback time; the same code live and
   in export.
2. Tap the player thread as it sends notes: exact to what you hear, but export has no player.
3. Velocity on the on-screen notes: +1 byte per note, and Hide covered notes merges notes that
   sound together on one key. Rejected.

## How it works

- Work in ticks: each frame, `now_us → tick` (`TempoMap::time_to_tick`); a note sounds over
  `[tick, tick + dur)` (`dur` None = never released).
- State: per track a `TrackReader`; a min-heap of sounding notes by end tick; per cell a count and
  a colour. Advancing reads every note starting up to `tick` (count up, push), then pops notes
  ending by `tick` (count down).
- Jumps: going backwards, or forward more than the seek index's step, restarts from
  `SeekIndex::resume` (its readers, plus its long notes that began before it and still sound);
  without an index (small files), from the start.
- Colour (my call): a cell takes the colour of the most recently started note on it, the same
  track/channel colour the roll would give that note; it keeps it while older notes still sound.
- Drawing (my call): the full grid stretched to fill the view (cells need not be square),
  velocity 127 at the top, background as the roll. Live: one egui mesh of lit cells. Export: CPU
  rectangle fills, no GPU.
- Velocities are as parsed: with "Velocity levels" below Exact they are rounded, and so is the picture.

## Risks

- Black MIDIs at tens of millions of notes per second: the walker reads as many records per frame
  as the player does. Measure on the Septette and the Nuker at 1×; if too slow, move it to a thread.

## Plan (native, TDD)

1. `matrix.rs`: the walker. Tests: two overlapping notes on one key at different velocities both
   light their cells; a note ends on time; a seek backwards gives the same grid as walking from 0.
2. `ViewMode::Matrix`, UI and export flag; draw live and in export. Test: an exported frame has
   the cell of a sounding (key, velocity) lit and an unused cell background.
3. Measure on black MIDIs; check LMAO HI by eye; self-review; commit.
