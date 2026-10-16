Renderer measurements, 12 October 2026

Intel Core i5-13600KF, 32 GB RAM, NVIDIA RTX 3060. The existing `--benchmark`
tests loading and playback, not drawing. Its initial baseline was 651 points
(23.36 Mnotes/s single-thread parse, 124.18 Mnotes/s parallel parse, 17.44 Mnotes/s
playback). A new built-in `--benchmark-render` mode exercises the real
VisibleNotes, instance builder, upload and wgpu pipeline on an offscreen target.

The final full `--benchmark` run completed with 1010 points: 37.60 Mnotes/s
single-thread parse, 227.67 Mnotes/s parallel parse, 27.09 Mnotes/s playback;
xz MIDI decode 77.84 MB/s, xz long matches 1657.83 MB/s, layer scan 7020.09 MB/s.
Those loading/playback implementations are unchanged by this renderer work;
the broader score's increase is not attributed to note culling. Its log is
`target/benchmark-after.txt`.

Initial measurement before renderer changes, normal process priority:

| Workload | Advance | Build | Upload | GPU draw | Note frame budget |
|---|---:|---:|---:|---:|---:|
| Long overlapping notes | 0.001 ms | 0.190 ms | 0.044 ms | 4.695 ms | 4.930 ms |
| Black Rose Apostle, from 104 s | 0.669 ms | 8.386 ms | 1.650 ms | 1.174 ms | 11.880 ms |

FrameLog confirmed that the dense MIDI spent most CPU time in building instances;
the long-note workload spent most of its time on the GPU. No compute pass was
needed. The synthetic short-note workload measured 0.611 ms initially.

Final comparison: five alternating before/after pairs at high process priority,
1920x900 physical pixels, five visible seconds, piano layout, bordered notes,
deterministic 60 Hz song time. Each run excludes 20 warm-up frames and measures
180 frames. Entries are medians of the five per-run averages; component medians
need not sum exactly to the median frame budget. Verification was disabled for
these timings. The before executable includes measurement instrumentation but
uses the original renderer and the user's existing uncommitted changes.

| Workload | Before | After | Speedup | Before p95 | After p95 | Steady peak instances, before / after |
|---|---:|---:|---:|---:|---:|---:|
| High polyphony, generated 1.6M notes on 32 tracks | 4.647 ms | 0.590 ms | 7.87x | 5.104 ms | 1.641 ms | 50,000 / 16,728 |
| High NPS, Black Rose Apostle from 104 s, about 1.29M notes/s entering the window | 6.605 ms | 3.496 ms | 1.89x | 7.342 ms | 3.879 ms | 1,029,391 / 126,289 |
| Moderate density, generated 320K notes/s | 0.386 ms | 0.382 ms | 1.01x | 0.463 ms | 0.471 ms | 50,057 / 50,057 |

| Workload / version | Retain | Decode | Sort | Cull / index | Build | Upload | GPU draw |
|---|---:|---:|---:|---:|---:|---:|---:|
| High polyphony, before | 0.0000 | 0.0004 | 0.0000 | 0.0001 | 0.1790 | 0.0341 | 4.4365 |
| High polyphony, after | 0.0000 | 0.0002 | 0.0000 | 0.0003 | 0.1716 | 0.0077 | 0.4108 |
| High NPS, before | 0.0000 | 0.1370 | 0.2325 | 0.0415 | 4.4094 | 0.8262 | 0.9577 |
| High NPS, after | 0.0001 | 0.1326 | 0.2035 | 0.0736 | 2.6656 | 0.1265 | 0.2959 |
| Moderate density, before | 0.0000 | 0.0177 | 0.0395 | 0.0067 | 0.1711 | 0.0245 | 0.1268 |
| Moderate density, after | 0.0000 | 0.0177 | 0.0397 | 0.0063 | 0.1657 | 0.0252 | 0.1270 |

All component numbers above are milliseconds. The note frame budget is CPU
advance + build + upload + GPU render-pass time. GPU timestamps include the
offscreen color/depth clears and vertex/fragment work, and exclude readback and
fence waits. This is not whole-app frame time or presented FPS: UI, synth, vsync,
command submission and GPU transfer execution outside the pass are excluded.
FrameLog's FPS includes the benchmark's readback waits, so it is also not display
FPS. Raw logs and the summary are under `target/measured-*.{txt,csv}` and
`target/render-bench-summary.json` in this workspace.

Large seeks initially use the original exact instance walk, then build the row
index in chunks over subsequent frames. This avoids adding index construction to
the first seek frame. Entering batches update the completed index incrementally;
sweeps reset it safely. The jumped parallel decode path is preserved. A temporary
same-process harness compared the original decoder/build loop with the new first
seek frame after warm-up, using the same loaded dense song and ten alternating
pairs: mean 137.328 -> 136.918 ms; median 137.448 -> 138.553 ms. Individual runs
spanned 130.981–143.425 ms. Independent process startup was much noisier (the
final five-run cold medians were 95.755 / 131.529 ms, mostly variable decode time),
so it is not used as a warm seek comparison. The temporary harness was removed;
its output remains in `target/warm-seek-comparison.txt`.

Culling is exact at pixel resolution. Decode only removes notes completely
covered by a later note. A per-key start/end index finds the last drawn notes
covering each physical pixel row; conservative edge guards retain uncertain
samples. A bitset deduplicates winners while preserving the original start-time
and equal-start track order. Spans and colors are never merged or extended.
Sharps still precede naturals in the front-to-back GPU order. Keyboard colors are
computed independently, including sounding notes that cover no raster sample.

There is no lossy color/slack merge anymore. Only detail with no visible sample
is omitted. The exact decoded window is retained, so every zoom and scrolling
phase can select detail again without rewinding readers or decoding the song
again. Sparse windows use the cheaper direct exact path. The auxiliary row index
uses about 24 bytes per retained note plus small block trees and bitsets; this is
the memory tradeoff for retaining detail and avoiding a full color/build walk.

Instances keep integer absolute microseconds in the existing 16-byte layout.
The shader subtracts a split `now` uniform before conversion. Integer rounding
and compensated division preserve the former CPU conversion at pixel boundaries,
including hour-long notes. Unchanged instance lists reuse their Arc and skip GPU
uploads. Large changed lists upload only their differing range; small changed
lists use the cheaper direct copy. A weak upload identity avoids cloning the
entire CPU instance vector when the builder reuses its allocation.

Validation: `cargo test` passes (37 passed, one pre-existing ignored test), and
`cargo build --release` is warning-free. New tests compare three-track survivors
and colors against an independent CPU painter, including border shading, channel
colors, piano columns, both scroll directions, close zoom, cache expiry, partial
index construction, seek/rewind and sweeps. GPU readback matches the previous
unculled relative-time shader for all 200 frames each of the dense MIDI, regular
short-note grid, and three-hour close-zoom precision case; the polyphony case and
a close-zoom dense-file run also match at three checkpoints.

Example reproduction:

```powershell
$env:KIM_FRAMELOG = 'frames.csv'
.\target\release\kimrashi.exe --benchmark-render polyphony --frames 180
.\target\release\kimrashi.exe --benchmark-render 'D:\Black MIDI\Not my MIDIs\Black Rose Apostle.mid' --from 104 --frames 180
.\target\release\kimrashi.exe --benchmark-render nps --frames 180
.\target\release\kimrashi.exe --benchmark-render 'D:\Black MIDI\Not my MIDIs\Black Rose Apostle.mid' --from 104 --frames 180 --verify-every 1
.\target\release\kimrashi.exe --benchmark-render precision --from 10800 --window 0.01 --frames 180 --verify-every 1
```

Renderer burst work, 12 October 2026 (background decode)

Decode, stable start sorting (indexed track-order collection), and both coverage
culls now run on a named standard-library worker. The first frame and large
seeks remain synchronous. Rewind, seek, and song identity changes cancel the old
generation without joining it in a frame; a bounded channel limits queued
finished batches. Atomic playhead/window targets request two seconds of margin.
The ordinary append/index budget is 300,000 notes; the 100 ms sound-safety wait
takes precedence if that frontier has not arrived. This exceptional wait is
not a hard frame-time guarantee.

The new wall generator places 12M notes over 5 ms after ten seconds of ordinary
music. Benchmark output includes p99 and worst in addition to average/p95.
Verification uses independent synchronous readers instead of comparing against
the same potentially incomplete window. Baseline wall: average 0.3850 ms, p95
0.4766, p99 1.1734, worst 105.1691. Worker wall: average 0.2995 ms, p95 0.5210,
p99 0.7265, worst 5.3653 (900 measured frames after 20 warm-up frames).
These are unpaced offscreen note budgets, not whole-application frame times.
DYHTM still needs the following overlap/instance changes: packing, sweeping,
and indexing can dominate even after decode moves off the UI thread.

Overlap collapse (step 2)

collapse_overlaps defaults to true; the Display settings checkbox disables it.
Benchmark exact comparisons use --no-collapse-overlaps. Overlapping spans on
the same original track, channel, and key become their connected interval union;
touching spans and different channels stay separate. Internal restrike borders
disappear, and a union can cover colours from other tracks for longer. Playback,
packed parsing, archives, and the synth are unchanged.

Packed renderer records have already paired note-offs with durations. They do
not expose raw note-on/off events. An O(1) interval union per packed note computes
the same zero/nonzero active-count boundaries without storing an off-event heap.
Final lengths are cached before publication, so later GPU instances never need
their length patched. This requires a full packed renderer scan on the first
frame when collapse is enabled, rather than just scanning the visible window.
That initialization is excluded from steady-state timings and may be substantial.
Rewinds of the same song reuse the cache. The exact path retains windowed decode.
Tests cover interval union, separate channels, touching spans, and no-overlap
note/pixel equivalence; release tests: 41 passed, one pre-existing ignored.

GPU instance slots (step 3)

Note data now lives in reusable storage-buffer slots. The row selector and sparse
path cache each note's slot; changed frames rebuild only a u32 draw-ID list.
Scrolling writes the absolute-time uniform, not the note data. Slots survive
temporary pixel occlusion and are recycled after end + the existing 1 ms hold.
Slot versions prevent a song/reset from reusing stale GPU contents. Buffer growth
requires republishing resident slots. The draw-ID list remains newest-first with
sharps first in piano layout, so the existing per-draw depth strategy is unchanged.
An independent frozen shader preserves the original relative-time reference.

This is a slot pool with expiration and reuse, rather than a FIFO that assumes
start order is also end order: MIDI long notes can outlive many later notes.
Instance packing/upload now follows newly selected notes; row queries, draw-ID
collection, and periodic window compaction still have view/window-dependent cost.
Release tests include stable slot reuse and unchanged painter comparisons.


Final burst integration, 13 October 2026

The final implementation supersedes the temporary full-song overlap cache described
above. It uses streaming per-track/channel/key interval unions over the existing
paired packed notes. Compact tick state skips expired groups before allocating
or converting times. The initial window and ten seconds of margin are decoded on
the permitted synchronous seek/start frame; only the current viewport is published.
The worker owns the readers and prepared future notes, has a separate four-thread
Rayon pool, and publishes completed sorted batches through a bounded std channel.
Atomic playhead/window targets and generations handle continuing playback and
cancellation. Ordinary frames consume one batch of at most 300,000 entries;
delivery through now + 100 ms takes precedence if the sound frontier falls behind.

Coverage culling remains exact. In collapse mode, permanently discarding a covered
open union would be unsafe because it could extend later; only completed unions
are coverage-culled, while pixel culling still removes hidden samples. Only unions
which can extend beyond their decoded frontier need an identity lookup. Extensions
retain their original start and update the affected key tree and cached GPU span.
Packed-track identities distinguish tracks independently of the existing u16 colour
index. Byte-identical packed tracks are rendered only from their last copy: that
copy covers every earlier rectangle, with the same start-order tie semantics.

Expired note pages keep stable IDs. Pages are retired after all their notes finish,
with 4096 metadata retirements and 50,000 key-index entries compacted per frame.
Key positions carry an index epoch so extending an open span does not scan equal
starts. Seek initialization reserves the known lookahead's index/cache capacity.
GPU slots keep versioned CPU references, retire unused replacements, and share
identical packed rectangles without drawing duplicate opaque instances. Integer
expiry takes the latest expiry among shared rectangles. Expiry buckets retire
4096 entries per changed draw list, avoiding a mass heap-pop frame.

max_instances defaults to 1048576 (was 262144; the cap was lossy and cost more than it saved); 0 disables it. The cap applies to the rendered
instance list, not the retained note records. It first removes the oldest-started
sounding instances, protecting sharps within that group in piano layout. If that
does not reach the limit, it removes the oldest remaining instances, again
protecting sharps in piano layout. Equal starts drop the lower-priority draw first.
A dropped cached slot stays suppressed until expiry or retirement; replacing an
extended collapsed span creates a new slot and reconsiders it under the limit.
Keyboard colour calculation remains independent of the instance cap.

collapse_overlaps defaults to true. It joins overlapping intervals on the same
track/channel/key and removes their internal restrike borders; touching intervals
stay separate because packed records do not retain same-tick note-off ordering.
Different tracks/channels remain distinct. The no-overlap path is unchanged.
In the app, uncheck Display > Overlaps and set Instance limit to 0 to disable both
approximations. CLI comparisons use --no-collapse-overlaps --max-instances 0.

Final single-run comparison, normal process priority, sequential runs on the same
RTX 3060/i5-13600KF machine, 1920x900 piano layout, bordered notes, five-second
window and deterministic unpaced 60 Hz song time. Wall and DYHTM use 900 measured
frames, excluding 20 warm-up frames; the others use 180. Wall starts at 3 s,
DYHTM at 4:35, held at 100 s, polyphony and NPS at 3 s. The baseline is the initial
HEAD renderer with only wall generation and percentile reporting added. The final
runs use the two default approximations. CPU peak working set includes loading
and synchronous initialization; it excludes GPU VRAM. These are note frame
budgets, not complete application frame times.

| Workload | Average ms before / after | p95 ms | p99 ms | Worst ms | Peak working set GB |
|---|---:|---:|---:|---:|---:|
| wall | 0.385 / 0.125 | 0.477 / 0.286 | 1.173 / 0.327 | 105.169 / 0.479 | 0.36 / 0.30 |
| dyhtm | 11.963 / 11.011 | 21.002 / 17.042 | 24.460 / 20.333 | 348.182 / 24.157 | 2.66 / 7.04 |
| held | 0.619 / 0.346 | 0.681 / 0.369 | 1.393 / 0.391 | 2.004 / 0.400 | 0.26 / 0.24 |
| polyphony | 0.728 / 0.047 | 2.503 / 0.051 | 3.315 / 0.054 | 4.140 / 0.063 | 0.29 / 0.11 |
| nps | 0.404 / 0.682 | 0.483 / 0.820 | 0.568 / 1.069 | 1.267 / 1.333 | 0.28 / 0.26 |

Acceptance is NOT fully met. Wall meets p99 < 8 ms and worst < 16 ms. DYHTM's worst
fell from 348.182 to 24.157 ms, but p99 remains 20.333 ms and fails both requested
limits. Its memory use also increased, primarily for lookahead and reserved index
capacity. CPU draw-ID collection/slot access and GPU drawing remain significant;
this implementation still visits the selected ID list when selection changes.
The moderate NPS case regressed slightly with storage indirection and accounting.
No claim of hitch-free 10M rendering or universal 150M notes/s throughput is made.

Validation: cargo test --release passes (49 passed, one pre-existing ignored test),
and cargo build --release is warning-free. All 200 frames (including warm-up)
pass --verify --verify-every 1 with both approximations disabled on held,
polyphony, the regular NPS grid, precision at 10800 s with a 0.01 s window,
and DYHTM from 4:43. The verifier has independent synchronous readers and uses
the frozen previous relative-time shader. Tests cover worker seek/rewind/song
reset, equal-start track priority, overlap extension across batches, no-overlap
pixel equivalence, stable page IDs and partial-tail reuse, bounded index cleanup,
cap priority/persistence/reuse, identical-track coverage, and shared-span expiry.

Raw results are target/task-{before,final}-*.txt, target/task-final-verify-*.txt,
and target/task-render-summary.json. Reproduction:

```powershell
.\target\release\kimrashi.exe --benchmark-render wall --from 3 --frames 900
.\target\release\kimrashi.exe --benchmark-render 'D:\Black MIDI\Not my MIDIs\DYHTM Community Merge.mid' --from 4:35 --frames 900
.\target\release\kimrashi.exe --benchmark-render 'D:\Black MIDI\Not my MIDIs\DYHTM Community Merge.mid' --from 4:43 --frames 180 --no-collapse-overlaps --max-instances 0 --verify --verify-every 1
```

Hitch and memory fixes, 13 October 2026 (`--paced`, DYHTM from 4:25, 900 frames)

Row cache: the guard is rounded up to a multiple of 16 us (it used to tick up by one as
max_len_us grew, wiping every cached row, ~40 ms), and a collapse-extension only
invalidates rows from the note's old end on, not from its start (~40k rows each second).
Packing: slots are page-chunked (SlotStore, allocated on write); the flat Vec doubled
at 10M/20M/41M note IDs for 50-100 ms stalls. Cap: one reverse pass instead of four.
Memory: the retired-note queue now drains in proportion to its backlog, and key-index
compaction resumes at a rotating key (restarting at key 0 starved the rest, leaving
about 3x the live entries) and runs only while stale entries exceed half the live ones.

| | avg ms | p95 | p99 | worst | peak working set |
|---|---:|---:|---:|---:|---:|
| before | 16.8 | 35.3 | 51.4 | 125 | 5.0 GB |
| after | 17.1 | 30.6 | 51.7 | 60 | 3.2 GB |

Still open: ~85 B of resident state per live note (32 B NoteSegment, 8 B position, 24 B
per key entry plus tree, 16 B slot chunk) and a lookahead of ~19M live notes here.

Instance cap order, revised: it sheds sharps from the top of the screen (newest start)
first, then naturals the same way, and remakes the choice every frame, so a note shed at
the top returns as it scrolls down. The earlier version kept dropped slots hidden until
expiry, which left holes drifting down the screen.

Per-key visible timeline (flatten), 13 October 2026, quiet machine (OBS running), `--paced`, DYHTM

| Scene | Frame avg / p99 / worst before | after | Peak instances before / after |
|---|---:|---:|---:|
| 4:33, 0.2 s window | 11.9 / 27.2 / 36.3 ms | 2.4 / 4.7 / 7.6 ms | 323,309 / 78,886 |
| 4:25, 5 s window | 13.3 / 25.1 / 56.0 ms | 5.7 / 9.3 / 13.1 ms | 376,485 / 144,107 |

Peak working set on the 5 s scene is about the same (2,016 MB before, 1,941 MB after):
the decode still holds the chunk's notes before they are flattened. Exact mode (flatten off)
remains pixel-identical to the reference; flatten mode differs on about 1% of pixels away
from the window edges (a note shorter than a pixel covering another note's border), and in
the 8 rows at either edge, where the old shader outlined and darkened clamped notes.
