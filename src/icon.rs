//! The window icon, drawn at startup so no image file has to travel with the
//! exe: a walnut cat head with gilt eyes, matching the mark in the masthead.

const WALNUT: [u8; 3] = [0x6B, 0x4F, 0x35];
const DEEP: [u8; 3] = [0x4A, 0x35, 0x24];
const GILT: [u8; 3] = [0xD9, 0xAE, 0x58];

/// What colour, if any, covers point (x, y) in a 0..1 square.
fn sample(x: f32, y: f32) -> Option<[u8; 3]> {
    // Eyes, then pupils, sit on top of the head.
    for cx in [0.36f32, 0.64] {
        let (dx, dy) = ((x - cx) / 0.075, (y - 0.56) / 0.095);
        if dx * dx + dy * dy <= 1.0 {
            let (px, py) = ((x - cx) / 0.025, (y - 0.56) / 0.07);
            return Some(if px * px + py * py <= 1.0 { DEEP } else { GILT });
        }
    }
    // Head.
    let (dx, dy) = ((x - 0.5) / 0.42, (y - 0.6) / 0.34);
    if dx * dx + dy * dy <= 1.0 {
        return Some(WALNUT);
    }
    // Ears: triangles rising from the top of the head.
    let ear = |ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32| {
        let s = |px: f32, py: f32, qx: f32, qy: f32| (x - qx) * (py - qy) - (px - qx) * (y - qy);
        let (d1, d2, d3) = (s(ax, ay, bx, by), s(bx, by, cx, cy), s(cx, cy, ax, ay));
        let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(neg && pos)
    };
    if ear(0.12, 0.48, 0.16, 0.06, 0.46, 0.32) || ear(0.88, 0.48, 0.84, 0.06, 0.54, 0.32) {
        return Some(DEEP);
    }
    None
}

/// `size`×`size` RGBA, 4×4 supersampled for smooth edges.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut hits) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = sample(x, y) {
                        r += c[0] as u32;
                        g += c[1] as u32;
                        b += c[2] as u32;
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.extend_from_slice(&[(r / hits) as u8, (g / hits) as u8, (b / hits) as u8, (hits * 255 / (SS * SS)) as u8]);
            }
        }
    }
    out
}
