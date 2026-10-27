//! App/tray icon drawn in code (no image files to ship): Prune's scissors on a
//! rounded green square. Handles sit on the left and the blades cross right of
//! centre, so the cutting part reads clearly to the right.

/// Distance from p to the segment a-b.
fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
    let (dx, dy) = (p.0 - (a.0 + abx * t), p.1 - (a.1 + aby * t));
    (dx * dx + dy * dy).sqrt()
}

/// RGBA pixels of a `size`×`size` icon.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    let radius = s * 0.23;

    // Geometry in 0..1 units.
    let ring_r = 0.115; // handle ring radius (to the middle of the stroke)
    let stroke = 0.068; // line thickness
    let rings = [(0.28, 0.33), (0.28, 0.67)];
    let pivot = (0.56, 0.50);
    let tips = [(0.84, 0.67), (0.84, 0.33)]; // upper handle -> lower tip, and vice versa
    let (bg, fg) = ((15.0, 110.0, 86.0), (255.0, 255.0, 255.0)); // #0F6E56, white

    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square coverage (1px anti-aliasing).
            let dx = (fx - s / 2.0).abs() - (s / 2.0 - radius);
            let dy = (fy - s / 2.0).abs() - (s / 2.0 - radius);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - radius;
            let a_bg = (0.5 - outside).clamp(0.0, 1.0);
            if a_bg <= 0.0 {
                continue;
            }

            // Scissors: two rings plus two blades, each blade running from its
            // ring's edge through the pivot to the opposite tip.
            let p = (fx / s, fy / s);
            let mut d = f32::MAX;
            for (i, c) in rings.iter().enumerate() {
                let to_c = ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt();
                d = d.min((to_c - ring_r).abs() - stroke / 2.0);
                let (vx, vy) = (pivot.0 - c.0, pivot.1 - c.1);
                let len = (vx * vx + vy * vy).sqrt();
                let start = (c.0 + vx / len * ring_r, c.1 + vy / len * ring_r);
                let tip = tips[i];
                // Blades taper slightly towards the tip.
                let t = ((p.0 - start.0) / (tip.0 - start.0)).clamp(0.0, 1.0);
                let w = stroke * (1.0 - 0.35 * t);
                d = d.min(seg_dist(p, start, pivot).min(seg_dist(p, pivot, tip)) - w / 2.0);
            }
            let a_fg = (0.5 - d * s).clamp(0.0, 1.0);

            let mix = |b: f32, f: f32| b + (f - b) * a_fg;
            let i = ((y * size + x) * 4) as usize;
            px[i] = mix(bg.0, fg.0) as u8;
            px[i + 1] = mix(bg.1, fg.1) as u8;
            px[i + 2] = mix(bg.2, fg.2) as u8;
            px[i + 3] = (a_bg * 255.0) as u8;
        }
    }
    px
}
