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

Toilet Story 3 has over 30,000 tracks, which is why it packs less well. Files that rely heavily on channels within tracks (The Nuker 3 F3) load more slowly for now.

## Build

Requires a recent stable [Rust](https://rustup.rs) toolchain.

```
cargo build --release
```

The binary is `target/release/kimrashi.exe`. To use KDMAPI, install OmniMIDI (or SVMS) so `OmniMIDI.dll` is on the loader path.

## License

GPL-3, see [LICENSE](LICENSE). The note shader (`shaders/notes.wgsl`) is based on
[Wasabi](https://github.com/BlackMIDIDevs/wasabi), which is GPLv3.
