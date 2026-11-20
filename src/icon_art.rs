//! The Clipikon icon ("brackets": four rounded crop corners around a record dot), rasterized analytically so
//! every size is drawn for itself rather than scaled. Shared by build.rs (the exe's .ico) and the app (tray
//! and window icons), so it depends on nothing but std.

/// Which version to draw.
#[derive(Clone, Copy)]
pub enum Style {
    /// Full color: blue gradient corners, red dot. For the exe and windows.
    Color,
    /// One color for the tray; `dot` differs from `fg` while recording.
    Mono { fg: [u8; 3], dot: [u8; 3] },
}

/// Geometry of the top-left corner in a `grid`-unit square (the other three are mirror images).
struct Shape {
    grid: f32,
    /// The vertical stroke runs from y = `leg` up to the arc, the horizontal one from the arc to x = `leg`.
    leg: f32,
    /// Distance of the strokes' centerline from the edge.
    inset: f32,
    radius: f32,
    stroke: f32,
    dot: f32,
}

// The color icon and the mono one are drawn on different grids, as in the design: the mono one has
// heavier strokes relative to its size, to stay legible at 16 px.
const COLOR: Shape = Shape { grid: 64.0, leg: 24.0, inset: 8.0, radius: 6.0, stroke: 7.0, dot: 11.0 };
const MONO: Shape = Shape { grid: 32.0, leg: 12.0, inset: 4.0, radius: 4.0, stroke: 3.0, dot: 5.5 };

/// `size`×`size` RGBA pixels, straight (not premultiplied) alpha.
pub fn render(size: u32, style: Style) -> Vec<u8> {
    let shape = match style {
        Style::Color => &COLOR,
        Style::Mono { .. } => &MONO,
    };
    let scale = size as f32 / shape.grid;
    // Below ~2 px a stroke turns to mush; small tray sizes get a minimum.
    let half_stroke = (shape.stroke * scale).max(1.75) / 2.0;
    let dot_r = shape.dot * scale;
    let c = shape.grid / 2.0;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // Pixel center in grid units, folded into the top-left quadrant.
            let (u, v) = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
            let (fu, fv) = (u.min(shape.grid - u), v.min(shape.grid - v));
            let d = corner_distance(shape, fu, fv) * scale;
            let bracket = (half_stroke + 0.5 - d).clamp(0.0, 1.0);
            let dd = ((u - c).powi(2) + (v - c).powi(2)).sqrt() * scale;
            let dot = (dot_r + 0.5 - dd).clamp(0.0, 1.0);

            let (rgb, a) = match style {
                Style::Color => {
                    // Diagonal gradient #60CDFF → #3B6FE0 on the corners; the dot is solid red.
                    let t = ((u + v) / (2.0 * shape.grid)).clamp(0.0, 1.0);
                    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
                    let corner = [lerp(0x60, 0x3B), lerp(0xCD, 0x6F), lerp(0xFF, 0xE0)];
                    if dot > 0.0 { ([0xE5, 0x48, 0x4D], dot) } else { (corner, bracket) }
                }
                Style::Mono { fg, dot: dot_rgb } => {
                    if dot > 0.0 { (dot_rgb, dot) } else { (fg, bracket) }
                }
            };
            let i = ((y * size + x) * 4) as usize;
            out[i..i + 3].copy_from_slice(&rgb);
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

/// Distance (grid units) from a point in the top-left quadrant to the corner's centerline: a vertical leg,
/// a quarter arc, a horizontal leg.
fn corner_distance(s: &Shape, x: f32, y: f32) -> f32 {
    let arc_c = s.inset + s.radius;
    let seg = |ax: f32, ay: f32, bx: f32, by: f32| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
    };
    let vertical = seg(s.inset, s.leg, s.inset, arc_c);
    let horizontal = seg(arc_c, s.inset, s.leg, s.inset);
    let (dx, dy) = (x - arc_c, y - arc_c);
    let arc = if dx <= 0.0 && dy <= 0.0 { ((dx * dx + dy * dy).sqrt() - s.radius).abs() } else { f32::INFINITY };
    vertical.min(horizontal).min(arc)
}

/// An .ico file holding PNG-compressed images of the given sizes, each from `encode_png(size, rgba)`.
#[allow(dead_code)] // used by build.rs
pub fn ico(sizes: &[u32], style: Style, encode_png: impl Fn(u32, &[u8]) -> Vec<u8>) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes.iter().map(|&s| (s, encode_png(s, &render(s, style)))).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, png) in &images {
        let dim = if *s >= 256 { 0 } else { *s as u8 }; // 0 means 256
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        out.extend_from_slice(png);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn dot_corners_and_empty_middle_of_edges() {
        for size in [16, 20, 24, 32, 64, 256] {
            for style in [Style::Color, Style::Mono { fg: [255, 255, 255], dot: [0xE5, 0x48, 0x4D] }] {
                let img = render(size, style);
                let c = size / 2;
                assert_eq!(px(&img, size, c, c), [0xE5, 0x48, 0x4D, 255], "dot at {size}");
                // Mid-edge (between two corners) is empty; the corner's leg is drawn.
                assert_eq!(px(&img, size, c, 0)[3], 0, "edge middle at {size}");
                let leg = (size as f32 * 0.14) as u32; // on the corner's vertical leg
                assert!(px(&img, size, leg, size / 3)[3] > 0 || px(&img, size, leg + 1, size / 3)[3] > 0, "leg at {size}");
            }
        }
    }

    /// `CLIPIKON_ICON_DUMP=<dir> cargo test`: writes the tray (mono) renders as PNGs to look at.
    #[test]
    fn dump_for_review() {
        let Some(dir) = std::env::var_os("CLIPIKON_ICON_DUMP") else { return };
        for (name, style) in [
            ("mono-white", Style::Mono { fg: [255, 255, 255], dot: [255, 255, 255] }),
            ("mono-white-rec", Style::Mono { fg: [255, 255, 255], dot: [0xE5, 0x48, 0x4D] }),
            ("mono-black", Style::Mono { fg: [0x1A, 0x1A, 0x1A], dot: [0x1A, 0x1A, 0x1A] }),
        ] {
            for size in [16, 20, 24, 32] {
                let file = std::fs::File::create(std::path::Path::new(&dir).join(format!("{name}-{size}.png"))).unwrap();
                let mut enc = png::Encoder::new(file, size, size);
                enc.set_color(png::ColorType::Rgba);
                enc.write_header().unwrap().write_image_data(&render(size, style)).unwrap();
            }
        }
    }
}
