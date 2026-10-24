//! Piano From Above's keyboard as gradient quads, shared by the window (egui mesh) and video export (software fill).
//! Geometry and shades follow PFA's `MainScreen::RenderKeys`.

pub type Rgb = [u8; 3];
/// Corners top-left, top-right, bottom-right, bottom-left.
pub struct Quad { pub p: [[f32; 2]; 4], pub c: [Rgb; 4] }

/// PFA shades by scaling HSV value; that is a scale of RGB, clipped at full value.
fn shade(c: Rgb, f: f32) -> Rgb {
    let k = (f).min(255.0 / c.iter().copied().max().unwrap_or(1).max(1) as f32);
    c.map(|v| (v as f32 * k).round() as u8)
}

/// (primary, dark, very dark)
fn set(c: Rgb, dark: f32, very_dark: f32) -> [Rgb; 3] { [c, shade(c, dark), shade(c, very_dark)] }

fn rect(q: &mut Vec<Quad>, x: f32, y: f32, w: f32, h: f32, c: [Rgb; 4]) {
    q.push(Quad { p: [[x, y], [x + w, y], [x + w, y + h], [x, y + h]], c });
}

fn skew(q: &mut Vec<Quad>, p: [[f32; 2]; 4], c: [Rgb; 4]) { q.push(Quad { p, c }); }

/// PFA's keyboard height in a `total_h` x `w` view: as tall as its keys' proportions ask, at most a quarter of the view.
pub fn key_height(total_h: f32, w: f32, lo: u8, hi: u8) -> f32 {
    let black = crate::is_black_key;
    let whites = (lo..=hi).filter(|&p| !black(p)).count().max(1) as f32;
    let buffer = (black(lo) as u8 + black(hi) as u8) as f32 * 0.65 / 2.0;
    let ideal = ((w / (whites + buffer) / 0.1775) / 0.95 + 2.0) / 0.93;
    ideal.min(total_h * 0.25).round()
}

/// Pixel columns `(pitch, left, right)` of the white and the sharp keys across `(x0, w)`: from the notes'
/// `columns` when they follow the keys, else whites evenly spread with sharps on the lines between.
pub fn layout(columns: Option<&[(f32, f32); 128]>, lo: u8, hi: u8, x0: f32, w: f32) -> (Vec<(u8, f32, f32)>, Vec<(u8, f32, f32)>) {
    let black = crate::is_black_key;
    let count = (lo..=hi).filter(|&p| !black(p)).count().max(1) as f32;
    let white_w = w / count;
    let (mut whites, mut sharps) = (Vec::new(), Vec::new());
    let mut index = 0.0;
    for p in lo..=hi {
        let (l, r) = match (columns, black(p)) {
            (Some(c), _) => (x0 + c[p as usize].0 * w, x0 + c[p as usize].1 * w),
            (None, true) => (x0 + index * white_w - white_w * 0.325, x0 + index * white_w + white_w * 0.325),
            (None, false) => (x0 + index * white_w, x0 + (index + 1.0) * white_w),
        };
        if black(p) { sharps.push((p, l, r)) } else { whites.push((p, l, r)); index += 1.0 }
    }
    (whites, sharps)
}

/// The keyboard filling `(x0, y0, w, h)`. `whites` / `sharps` are `(pitch, left, right)` in pixels,
/// `active[p]` is `Some(color)` for a pressed key (`None` color = default lit).
pub fn keyboard(x0: f32, y0: f32, w: f32, h: f32, background: Rgb, whites: &[(u8, f32, f32)], sharps: &[(u8, f32, f32)],
    active: &dyn Fn(u8) -> Option<Rgb>) -> Vec<Quad>
{
    let (kb, red, white, sharp) = (set([153; 3], 0.4, 0.0), set([152, 10, 13], 0.5, 0.2), set([255; 3], 0.8, 0.6), set([64; 3], 0.5, 0.0));
    let trans = (h * 0.02).round().max(3.0);
    let red_h = (h * 0.05).round();
    let spacer = 2.0;
    let top = ((h - spacer - red_h - trans) * 0.95).round();
    let near = h - spacer - red_h - trans - top;
    let keys_y = y0 + trans + red_h + spacer;
    let sharp_h = top * 0.67;
    let mut q = Vec::new();

    rect(&mut q, x0, y0, w, h, [kb[2]; 4]);
    rect(&mut q, x0, y0, w, trans, [background, background, kb[2], kb[2]]);
    rect(&mut q, x0, y0 + trans, w, red_h, [red[1], red[1], red[0], red[0]]);
    rect(&mut q, x0, y0 + trans + red_h, w, spacer, [kb[1]; 4]);

    for &(p, l, r) in whites {
        let cx = r - l;
        let gap = (cx * 0.05).round().max(1.0);
        let x = l + gap - (gap / 2.0).round();
        let kw = cx - gap;
        let lit = active(p);
        let marker = |q: &mut Vec<Quad>, bottom: f32, c: Rgb| if p == 60 {
            let mx = (cx * 0.25).round();
            let mw = cx - mx * 2.0 - gap;
            let my = (bottom - mw - 5.0).max(keys_y + sharp_h + 5.0);
            rect(q, x + mx, my, mw, bottom - 5.0 - my, [c; 4]);
        };
        match lit {
            None => {
                rect(&mut q, x, keys_y, kw, top + near, [white[1], white[1], white[0], white[0]]);
                rect(&mut q, x, keys_y + top, kw, near, [white[1], white[1], white[2], white[2]]);
                rect(&mut q, x, keys_y + top, kw, 2.0, [kb[1], kb[1], white[2], white[2]]);
                marker(&mut q, keys_y + top, white[1]);
            }
            Some(c) => {
                let c = set(c, 0.5, 0.2);
                rect(&mut q, x, keys_y, kw, top + near - 2.0, [c[1], c[1], c[0], c[0]]);
                rect(&mut q, x, keys_y + top + near - 2.0, kw, 2.0, [c[1]; 4]);
                marker(&mut q, keys_y + top + near - 2.0, c[1]);
            }
        }
        let gx = (x + kw).round();
        rect(&mut q, gx, keys_y, gap, top + near, [kb[2], kb[0], kb[0], kb[2]]);
    }

    for &(p, l, r) in sharps {
        let cx = r - l;
        let top_w = cx * 0.7;
        let (t1, t2) = (l + (cx - top_w) / 2.0, l + (cx + top_w) / 2.0);
        let lit = active(p).map(|c| set(c, 0.5, 0.2));
        let n = if lit.is_some() { near * 0.25 } else { near };
        let (a, b, base) = (keys_y - n, keys_y + sharp_h - n, keys_y + sharp_h);
        // Idle sharps shade grey to black; pressed ones shade the key's color to its dark.
        let (side, face, g1, g2, f) = match lit {
            None => ([sharp[0], sharp[0], sharp[2], sharp[2]], sharp[2], [sharp[1], sharp[1], sharp[0], sharp[0]],
                [sharp[0], sharp[0], sharp[2], sharp[2]], [0.45, 0.35, 0.35, 0.45, 0.65, 0.55]),
            Some(c) => ([c[0], c[0], c[1], c[1]], c[1], [c[0]; 4], [c[0], c[0], c[1], c[1]], [0.35, 0.25, 0.25, 0.35, 0.75, 0.65]),
        };
        skew(&mut q, [[t1, b], [t2, b], [r, base], [l, base]], side);
        skew(&mut q, [[t1, a], [t1, b], [l, base], [l, keys_y]], side);
        skew(&mut q, [[t2, b], [t2, a], [r, keys_y], [r, base]], side);
        rect(&mut q, t1, a, t2 - t1, sharp_h, [face; 4]);
        // f: gloss 1 right/left bottom, gloss 2 left-top, right-top, right-bottom, left-bottom.
        let (g1r, g1l) = (f[0], f[1]);
        let (g2l, g2r, g2br, g2bl) = (f[2], f[3], f[4], f[5]);
        skew(&mut q, [[t1, a], [t2, a], [t2, a + sharp_h * g1r], [t1, a + sharp_h * g1l]], g1);
        skew(&mut q, [[t1, a + sharp_h * g2l], [t2, a + sharp_h * g2r], [t2, a + sharp_h * g2br], [t1, a + sharp_h * g2bl]], g2);
    }
    q
}

/// Software fill of `quads` into an RGBA buffer `width` wide (pixel centres, bilinear-free: two triangles, barycentric colour).
pub fn fill(buf: &mut [u8], width: u32, quads: &[Quad]) {
    let height = (buf.len() / (width as usize * 4)) as i32;
    for q in quads {
        for tri in [[0, 1, 2], [0, 2, 3]] {
            let (a, b, c) = (q.p[tri[0]], q.p[tri[1]], q.p[tri[2]]);
            let area = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
            if area.abs() < 1e-6 { continue; }
            let min_x = a[0].min(b[0]).min(c[0]).floor().max(0.0) as i32;
            let max_x = (a[0].max(b[0]).max(c[0]).ceil() as i32).min(width as i32);
            let min_y = a[1].min(b[1]).min(c[1]).floor().max(0.0) as i32;
            let max_y = (a[1].max(b[1]).max(c[1]).ceil() as i32).min(height);
            for y in min_y..max_y {
                for x in min_x..max_x {
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w1 = ((px - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (py - a[1])) / area;
                    let w2 = ((b[0] - a[0]) * (py - a[1]) - (px - a[0]) * (b[1] - a[1])) / area;
                    let w0 = 1.0 - w1 - w2;
                    if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 { continue; }
                    let at = |k: usize| q.c[tri[0]][k] as f32 * w0 + q.c[tri[1]][k] as f32 * w1 + q.c[tri[2]][k] as f32 * w2;
                    let o = (y as usize * width as usize + x as usize) * 4;
                    for k in 0..3 { buf[o + k] = at(k).round().clamp(0.0, 255.0) as u8; }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_white_key_is_lighter_at_the_bottom_of_its_face_and_gaps_are_dark() {
        let q = keyboard(0.0, 0.0, 40.0, 100.0, [20, 20, 24], &[(60, 0.0, 20.0), (62, 20.0, 40.0)], &[], &|_| None);
        let mut buf = vec![0u8; 40 * 100 * 4];
        fill(&mut buf, 40, &q);
        let px = |x: usize, y: usize| buf[(y * 40 + x) * 4];
        assert!(px(8, 60) > px(8, 12), "gradient runs dark to light down the face");
        assert!(px(19, 40) < 100, "the gap between keys is dark");
    }
}
