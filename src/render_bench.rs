//! `--benchmark-render <file|polyphony|nps|held|precision> [--from seconds] [--frames n]`
//! ` [--window seconds] [--verify]`
//! Runs the real decode/build/upload/draw path at 1920x900, with a five-second
//! window and deterministic 60 Hz song time. GPU times exclude fences/readback;
//! frame budget is CPU advance + build + upload + GPU draw. No synth or vsync.

use std::{future::Future, io::Cursor, sync::Arc, task::{Context, Poll, Wake, Waker}, time::{Duration, Instant}};
use eframe::wgpu;
use crate::{gpu, parser::{MidiFile, Plain, Progress}, song::{SongData, TempoMap}, FrameLog, FrameTimes, VisibleNotes, VisualSong};

fn block_on<T>(future: impl Future<Output = T>) -> T {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark { fn wake(self: Arc<Self>) { self.0.unpark(); } }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn vlv(out: &mut Vec<u8>, mut value: u32) {
    let mut bytes = [0u8; 5];
    bytes[4] = (value & 127) as u8;
    let mut i = 4;
    value >>= 7;
    while value != 0 { i -= 1; bytes[i] = (value & 127) as u8 | 128; value >>= 7; }
    out.extend_from_slice(&bytes[i..]);
}

/// Long overlapping notes versus short, rapidly entering notes, on 32 tracks.
fn generated(polyphony: bool) -> Arc<SongData> {
    let mut file = b"MThd\0\0\0\x06\0\x01\0\x20".to_vec();
    file.extend_from_slice(&5000u16.to_be_bytes()); // 100 us per tick
    for track in 0..32u8 {
        let count = if polyphony { 50_000 } else { 100_000 };
        let mut body = Vec::new();
        for i in 0..count {
            vlv(&mut body, 1);
            body.extend_from_slice(&[0x90 | (track & 15), 21 + (i % 88) as u8, 100]);
            if !polyphony && i >= 48 {
                body.extend_from_slice(&[0, 0x80 | (track & 15), 21 + ((i - 48) % 88) as u8, 0]);
            }
        }
        if polyphony { vlv(&mut body, 100_000); } else { vlv(&mut body, 1); }
        // End every outstanding note; the first delta was written above.
        let first = if polyphony { 0 } else { count - 48 };
        for i in first..count {
            if i != first { vlv(&mut body, 0); }
            body.extend_from_slice(&[0x80 | (track & 15), 21 + (i % 88) as u8, 0]);
        }
        body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend(body);
    }
    let midi = Arc::new(MidiFile::parse_stream(Box::new(Plain(Cursor::new(file))), 127, &Progress::default()).unwrap());
    let tempo = Arc::new(TempoMap::from_midi(&midi));
    Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO })
}

/// ~100k notes held at once at only 1000 notes/s: random pitch, lengths of 50-150 s, 32 tracks.
fn held() -> Arc<SongData> {
    let mut file = b"MThd\x00\x00\x00\x06\x00\x01\x00 ".to_vec();
    file.extend_from_slice(&5000u16.to_be_bytes()); // 100 us per tick
    let mut seed = 12345u64;
    let mut events: Vec<Vec<(u64, [u8; 3])>> = vec![Vec::new(); 32];
    for i in 0..200_000u64 {
        seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
        let (track, key) = ((i % 32) as usize, 21 + (seed % 88) as u8);
        let start = i * 10; // 1 ms
        let end = start + 500_000 + (seed >> 8) % 1_000_000;
        events[track].push((start, [0x90 | track as u8 & 15, key, 100]));
        events[track].push((end, [0x80 | track as u8 & 15, key, 0]));
    }
    for ev in &mut events {
        ev.sort_by_key(|e| (e.0, e.1[0] >> 4)); // offs (8) before ons (9) at a tick
        let mut body = Vec::new();
        let mut last = 0;
        for (tick, msg) in ev.iter() {
            vlv(&mut body, (tick - last) as u32);
            last = *tick;
            body.extend_from_slice(msg);
        }
        body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend(body);
    }
    let midi = Arc::new(MidiFile::parse_stream(Box::new(Plain(Cursor::new(file))), 127, &Progress::default()).unwrap());
    let tempo = Arc::new(TempoMap::from_midi(&midi));
    Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO })
}

/// Twelve million notes in five milliseconds, following ten seconds of music.
fn wall() -> Arc<SongData> {
    let mut file = b"MThd\0\0\0\x06\0\x01\0\x40".to_vec();
    file.extend_from_slice(&5000u16.to_be_bytes());
    let mut seed = 12345u64;
    for track in 0..64u8 {
        let mut events = Vec::with_capacity(376_000);
        for i in 0..1000u32 {
            let tick = i * 100;
            let key = 21 + (i % 88) as u8;
            events.push((tick, [0x90 | (track & 15), key, 100]));
            events.push((tick + 40, [0x80 | (track & 15), key, 0]));
        }
        for i in 0..187_500u32 {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            let tick = 100_000 + i / 3750;
            let key = 21 + (seed % 88) as u8;
            let end = tick + 100 + ((seed >> 8) % 50_000) as u32;
            events.push((tick, [0x90 | (track & 15), key, 100]));
            events.push((end, [0x80 | (track & 15), key, 0]));
        }
        events.sort_by_key(|e| (e.0, e.1[0] >> 4));
        let mut body = Vec::new();
        let mut last = 0;
        for (tick, msg) in events {
            vlv(&mut body, tick - last);
            last = tick;
            body.extend_from_slice(&msg);
        }
        body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend(body);
    }
    let midi = Arc::new(MidiFile::parse_stream(Box::new(Plain(Cursor::new(file))), 127, &Progress::default()).unwrap());
    let tempo = Arc::new(TempoMap::from_midi(&midi));
    Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO })
}

/// Very old notes ending in a close zoom, across the low timestamp word's wrap.
fn precision_song(from_us: u64) -> Arc<SongData> {
    let mut file = b"MThd\0\0\0\x06\0\x01\0\x03".to_vec();
    file.extend_from_slice(&5000u16.to_be_bytes());
    for (track, end) in [from_us + 10_000, from_us + 343_333, from_us + 3_326_667].into_iter().enumerate() {
        let mut body = vec![0, 0x90 | track as u8, 60, 100];
        vlv(&mut body, (end / 100) as u32);
        body.extend_from_slice(&[0x80 | track as u8, 60, 0, 0, 0xff, 0x2f, 0]);
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend(body);
    }
    let midi = Arc::new(MidiFile::parse_stream(Box::new(Plain(Cursor::new(file))), 127, &Progress::default()).unwrap());
    let tempo = Arc::new(TempoMap::from_midi(&midi));
    Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO })
}

pub fn run_from_args() -> bool {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(i) = args.iter().position(|a| a == "--benchmark-render") else { return false };
    let Some(input) = args.get(i + 1) else { panic!("--benchmark-render needs a file, polyphony, or nps") };
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let from_us = value("--from").map_or(if input == "precision" { (1u64 << 32) - 5000 } else { 3_000_000 }, |s| crate::bench::parse_time(s).unwrap());
    let speed = value("--window").map_or(5.0, |s| s.parse::<f32>().unwrap()).clamp(0.01, 15.0);
    let future_us = (speed * 1e6) as u64;
    let frames = value("--frames").map_or(180, |s| s.parse::<usize>().unwrap());
    assert!(frames > 0, "--frames must be positive");
    let verify = args.iter().any(|a| a == "--verify");
    let verify_every = value("--verify-every").map(|s| s.parse::<usize>().unwrap().max(1));
    let song = match input.as_str() {
        "wall" => wall(), "polyphony" => generated(true), "nps" => generated(false), "held" => held(), "precision" => precision_song(from_us),
        path => SongData::load(path, 127, &Progress::default()).unwrap(),
    };
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
    println!("Renderer benchmark: {input}, from {:.3}s, {frames} frames, 1920x900, piano, bordered, {speed}s window\nGPU: {}", from_us as f64 / 1e6, adapter.get_info().name);
    assert!(adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY), "GPU timestamp queries unavailable");
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: wgpu::Features::TIMESTAMP_QUERY,
        required_limits: adapter.limits(), ..Default::default()
    })).unwrap();
    let mut renderer = gpu::RenderBenchmark::new(device, queue, 1920, 900);
    let visual = VisualSong::new(song);
    let mut visible = VisibleNotes::default();
    visible.set_collapse(!args.iter().any(|a| a == "--no-collapse-overlaps"));
    let mut reference_readers = vec![crate::TrackReader::default(); visual.data.midi.tracks.len()];
    let mut reference_window = Vec::new();
    let mut instances = Arc::new(gpu::InstanceRing::default());
    let mut keys = [gpu::KeyPosition::zeroed(); 256];
    use bytemuck::Zeroable;
    let columns = crate::key_columns(true, visual.pitch_min, visual.pitch_max);
    for p in visual.pitch_min..=visual.pitch_max {
        keys[p as usize].left = columns[p as usize].0;
        keys[p as usize].right = columns[p as usize].1;
    }
    let mut log = FrameLog::from_env();
    let mut sums = [Duration::ZERO; 8];
    let mut build_parts = [Duration::ZERO; 4];
    let mut row_counts = [0usize; 2];
    let mut budgets = Vec::new();
    let mut counts = (0usize, 0usize);
    let mut initial = Duration::ZERO;
    for f in 0..frames + 20 {
        let now = from_us + f as u64 * 1_000_000 / 60;
        let end = now + future_us;
        let t0 = Instant::now();
        visible.advance(&visual.data, now, end, future_us / 900);
        let t1 = Instant::now();
        crate::build_note_instances(&visual, &mut visible, now, end, speed, 900, false, true, &mut instances);
        let build = t1.elapsed();
        let cpu = t0.elapsed();
        let (upload, gpu) = renderer.frame(&instances, &keys, speed, now);
        let times = FrameTimes { advance: visible.timing, build, instances: instances.len(), decoded: visible.last_batch };
        if let Some(log) = &mut log { log.record(cpu, &times); }
        if verify_every.is_some_and(|every| f % every == 0) || (verify && (f == 0 || f == 20 || f == frames + 19)) {
            let pixels = renderer.pixels();
            reference_window.retain(|n: &crate::NoteSegment| n.end_us + crate::NOTE_HOLD_MICROS > now);
            reference_window.extend(crate::decode_notes(&visual.data, &mut reference_readers, now, end).notes);
            let reference = reference_instances(&visual, &reference_window, now, end);
            renderer.reference_frame(&reference, &keys, speed, now);
            let expected = renderer.pixels();
            let different = pixels.chunks_exact(4).zip(expected.chunks_exact(4)).filter(|(a, b)| a != b).count();
            assert_eq!(different, 0, "GPU pixels differ from the original unculled relative-time renderer at frame {f}");
            println!("frame {f}: GPU pixels equal ({}, reference {} instances)", instances.len(), reference.len());
        }
        if f == 0 {
            initial = cpu;
            println!("initial_parts_ms: retain={:.3}, decode={:.3}, sort={:.3}, cull={:.3}, build={:.3}",
                times.advance.retain.as_secs_f64() * 1e3, times.advance.decode.as_secs_f64() * 1e3,
                times.advance.sort.as_secs_f64() * 1e3, times.advance.cull.as_secs_f64() * 1e3, build.as_secs_f64() * 1e3);
        }
        if f < 20 { continue; }
        let a = visible.timing;
        let values = [a.retain, a.decode, a.sort, a.cull, build, upload, gpu, cpu + upload + gpu];
        for (sum, v) in sums.iter_mut().zip(values) { *sum += v; }
        for (sum, v) in build_parts.iter_mut().zip(visible.build_profile) { *sum += v; }
        for (sum, v) in row_counts.iter_mut().zip(visible.row_counts) { *sum += v; }
        budgets.push(values[7].as_secs_f64() * 1e3);
        counts.0 = counts.0.max(instances.len()); counts.1 += visible.last_batch;
    }
    if let Some(log) = &mut log { log.flush(); }
    let ms = |d: Duration| d.as_secs_f64() * 1e3 / frames as f64;
    budgets.sort_by(f64::total_cmp);
    println!("initial_decode_build_ms={:.3}, max_instances={}, new_per_frame={}", initial.as_secs_f64() * 1e3, counts.0, counts.1 / frames);
    println!("build_parts_ms: keyboard={:.4}, rows={:.4}, bitset={:.4}, packing={:.4}",
        ms(build_parts[0]), ms(build_parts[1]), ms(build_parts[2]), ms(build_parts[3]));
    println!("rows_per_frame: reused={} queried={}", row_counts[0] / frames, row_counts[1] / frames);
    println!("retain_ms,decode_ms,sort_ms,cull_ms,build_ms,upload_ms,gpu_ms,frame_budget_ms,p95_ms,p99_ms,worst_ms");
    println!("{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}", ms(sums[0]), ms(sums[1]), ms(sums[2]), ms(sums[3]), ms(sums[4]), ms(sums[5]), ms(sums[6]), ms(sums[7]), budgets[(frames * 95 / 100).min(frames - 1)], budgets[(frames * 99 / 100).min(frames - 1)], budgets[frames - 1]);
    true
}

fn reference_instances(song: &VisualSong, window: &[crate::NoteSegment], now: u64, end: u64) -> Arc<gpu::InstanceRing> {
    // Keep the original loop and byte layout independent of pixel selection.
    Arc::new(gpu::InstanceRing::from_ordered((0..2).flat_map(|pass| window.iter().rev()
        .filter(move |n| crate::is_black_key(n.pitch) == (pass == 0)))
        .filter(|n| n.start_us < end && n.end_us + crate::NOTE_HOLD_MICROS > now)
        .map(|note| {
            let color = song.color_for(note, false);
            let packed = note.pitch as u32 | ((color.r() as u32) << 24) | ((color.g() as u32) << 16) | ((color.b() as u32) << 8);
            gpu::NoteInstance {
                start_us: [((note.start_us as i64 - now as i64) as f32 / 1e6).to_bits(),
                    ((note.end_us - note.start_us) as f32 / 1e6).max(0.000_1).to_bits()],
                len: f32::from_bits(packed), key_color: 1,
            }
        })))
}
