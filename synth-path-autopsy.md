# SVMS path autopsy, and the direct page feed

## Where the time went (SVMS's own figures + a code read)
kimrashi already calls SVMS's native batch API (`send_short_batch`, 16 bytes/event). Past ~60M events/s the
ceiling was not our decoder (100M/s) but SVMS's two ingest threads:

| stage | cost per event |
|---|---|
| kimrashi engine (decode + skip) | ~10 ns |
| ring submit on the sender thread (`PrepareShortEvent`, sequence, ring push) | 11 ns live (SVMS changelog 2026-10-16), 15 ns measured here |
| compiler thread (drain + compile + sort) | ~10.7 ns |
| audio-callback extraction | ~1.7 ns |
| render | dominates at high voice counts |

## What was built (SVMS repo, uncommitted; kimrashi side committed)
`send_short_batch_direct` (cap `SVMS_CAP_DIRECT_PAGES`, appended to `SVMS_Interface`). The caller thread compiles the
batch (`CompileDirectRun`, `SVMSDirectIngest.h`) and appends it to one open page; the page is published when full or
taken by the audio callback right before it imports pages. No ring, no per-event sequencing/gating, no compiler
thread in the path. The open-page handoff is a CAS on (page index, count): the audio thread never waits for the
producer (a first version used a spin lock and hung under load: priority inversion with a lower-priority sender).

Kept from the ring path: sequences and termination fences (reset, CC120, CC123), exact target frames, CC/bend collapse
(within a batch), priority-velocity shedding, exact note merge. Declined, nothing consumed, fall back to
`send_short_batch`: min note length, note-on coalescing, sharded master, audio not started.

kimrashi (`src/svms.rs`) tries the direct entry first and goes back to `send_short_batch` for 256 batches after a
decline. `KIM_SVMS_DIRECT=0` turns it off.

## Measured (silent: user's event settings, master volume 0)
- Compile step alone: 4.1 -> 2.0 ns/event; equivalence with the ring compiler checked on random messages
  (`svms_v3_direct_ingest_test`).
- Sender-side submit: 11-15 ns -> 3-5.6 ns per event.
- Live driver, 608k to 4.2M events per run, both paths: dispatched + collapsed + merged = sent, exactly, every run.
- Real kimrashi on Nuker 6:58-7:12 with a 16k-voice sine font: no hang, no stall; but that passage is render-bound
  (load in the thousands of %), so the two modes look alike there. The gain is in the ingest threads, not in render.

## Not verified
- Anything by ear. Nothing was changed in event timing (same frames, same order), but it has not been listened to.
- The effect on a machine where the sender/compiler thread is the actual limit (needs a lighter-render passage).
- Cross-batch CC collapse (the compiler thread collapses across a whole drained page; direct collapses per batch).
