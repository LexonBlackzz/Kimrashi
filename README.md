<p align="center"><img src="assets/icon.png" width="128" alt="Kimrashi icon"></p>

# Kimrashi

Kimrashi is a MIDI player for Windows that loads the entire song into memory in a compact packed form (about 0.34 bytes per note), then plays and renders it from there with no streaming from disk. It has a GPU-rendered piano roll and is built for black MIDI.

- Plays through [KDMAPI](https://github.com/KeppySoftware/OmniMIDI) (OmniMIDI / any KDMAPI-compatible synth) or a regular Windows MIDI output
- Loads `.mid` files directly from `.zip`, `.7z`, `.rar`, `.gz`, `.xz`, `.zst` and `.tar` archives, nested
- Parallel MIDI parsing, note skipping when the synth falls behind, tempo control, SVMS telemetry
- Headless playback benchmark (see `src/bench.rs`)

## Performance

Load time and memory use on some large black MIDI files (timings include parsing; the file is then played entirely from the packed copy in RAM):

Tested on an Intel Core i5-13600KF, 32 GB DDR4-3600 CL16 and a WD Blue SN570 1 TB NVMe SSD.

| File | Notes | File size | Load time | Notes/s | Packed in RAM | Bytes/note | Packed : file |
|---|---|---|---|---|---|---|---|
| DYHTM Community Merge | 824.17M | 6.15 GB | 3.56 s | 236.10M | 272.7 MB | 0.33 | 0.04:1 |
| When ooo 000 isn't home Full | 1.18B | 8.80 GB | 5.82 s | 235.64M | 195.6 MB | 0.17 | 0.02:1 |
| Toilet Story 3 | 224.31M | 1.67 GB | 2.51 s | 176.07M | 260.5 MB | 1.16 | 0.14:1 |
| The Nuker 3 F3 | 529.41M | 3.94 GB | 11.37 s | 46.84M | 218.9 MB | 0.41 | 0.05:1 |
| Window | 171.43M | 1.27 GB | 5.09 s | 33.82M | 80.6 MB | 0.47 | 0.06:1 |

Averaged over these files, the packed copy is about 0.05:1 of the file size (0.02:1 to 0.14:1).

Toilet Story 3 has over 30,000 tracks, which is why it packs less well. Files made of a few huge tracks (The Nuker 3 F3) load more slowly, since a track is packed by one thread; big tracks that use many channels are split by channel across threads.

## Build

Requires a recent stable [Rust](https://rustup.rs) toolchain.

```
cargo build --release
```

The binary is `target/release/kimrashi.exe`. To use KDMAPI, install OmniMIDI (or SVMS) so `OmniMIDI.dll` is on the loader path.

To measure rendering without the synth or vsync, use `--benchmark-render` with a
MIDI path, `polyphony`, or `nps`. It reports advance/build/upload CPU times and a
GPU timestamp for the real note pipeline. `KIM_FRAMELOG` also writes the usual CSV.

```powershell
$env:KIM_FRAMELOG = 'frames.csv'
.\target\release\kimrashi.exe --benchmark-render 'song.mid' --from 104 --frames 180
.\target\release\kimrashi.exe --benchmark-render polyphony --verify
```

`--window` changes the visible seconds. `--verify` compares GPU pixels against the
previous unculled relative-time renderer at three checkpoints; `--verify-every 1`
checks every frame. `precision --window 0.01 --verify` checks a long song near the
32-bit timestamp wrap. See [renderer-performance.md](renderer-performance.md)
for the measurements and culling guarantees.

## Updates

Kimrashi asks GitHub for the latest release once a day when it starts; turn that off under Settings > Updates. When a newer release exists, a button in the toolbar shows its notes. "Update now" downloads the new `kimrashi.exe`, checks it against the SHA-256 GitHub publishes for it and against a signature made with the release key built into the program, replaces itself and restarts. `settings.txt` stays as it is.

A release is built and signed with `release.ps1` (private key in `%USERPROFILE%\.kimrashi\signing.key`, made once with `kimrashi-sign keygen`). Upload `kimrashi.exe` and `kimrashi.exe.sig`; a release without the signature only offers the release page.

## License

GPL-3, see [LICENSE](LICENSE). The note shader (`shaders/notes.wgsl`) is based on
[Wasabi](https://github.com/BlackMIDIDevs/wasabi), which is GPLv3.
