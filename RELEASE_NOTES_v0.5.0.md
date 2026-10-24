# Kimrashi v0.5.0

## Video export

You can now render a song straight to an MP4. Load a MIDI, click **Export video…** in the toolbar, pick your settings and hit **Start export**.

- **Resolution and FPS:** 720p to 4K, 24 to 144 fps, with your own note speed.
- **Encoders:** x264 (CPU), NVENC (NVIDIA), QSV (Intel) and AMF (AMD), each with a speed preset from ultrafast to veryslow and a quality slider.
- **Audio (optional):** attach an audio file and it is synced so it starts exactly on the first note.
- **Intro:** a configurable delay before the first note (3 s by default) with the song title fading in and out on a darkened band. A small "Rendered with Kimrashi" watermark shows during the intro. Both can be switched off.
- **Live preview and speed readout:** watch what is being rendered, with progress, ETA and render speed (e.g. 4.3x realtime).
- **Your look is respected:** the export uses your note palette, piano or even key layout, note direction, flat notes and overlap settings.
- **Command line:** `kimrashi --export-video song.mid out.mp4 [--width --height --fps --speed --lead --tail --encoder --preset --crf --audio --no-title --no-watermark]`

Export needs [ffmpeg](https://ffmpeg.org/download.html) on your PATH, or set its location in the export window.

If you share a render, a link to this page in the description would be really appreciated. Entirely optional.

## Also in this release

- Settings, themes and note palettes are now saved in `settings.json` next to the player.
- Faster note rendering on dense MIDIs (see `renderer-performance.md`), plus a `--benchmark-render` mode for measuring it.

## Notes and known limits

- Song titles with Chinese, Japanese or Korean characters won't render in the intro yet.
- The in-app updater will offer this version once this release is published.
