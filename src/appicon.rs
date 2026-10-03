//! Shared M-glyph icon rasterizer: zero assets, zero dependencies.
//! Both binaries draw the same mark; only the palette differs
//! (tray = accent blue, main window = warm ink on paper theme).

/// Render a rounded-square M glyph as RGBA bytes (row-major).
pub fn m_rgba(bg: (u8, u8, u8), fg: (u8, u8, u8), size: u32) -> Vec<u8> {
    let (w, h) = (size as i32, size as i32);
    let r = (7 * size / 32) as i32;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let mut set = |x: i32, y: i32, c: (u8, u8, u8)| {
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        px[i] = c.0;
        px[i + 1] = c.1;
        px[i + 2] = c.2;
        px[i + 3] = 255;
    };
    let inside = |x: i32, y: i32| {
        let cx = x.clamp(r, w - 1 - r);
        let cy = y.clamp(r, h - 1 - r);
        (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
    };
    for y in 0..h {
        for x in 0..w {
            if inside(x, y) {
                set(x, y, bg);
            }
        }
    };
    // "M": two side bars + diagonals from top edge to center.
    let s = size as f32 / 32.0;
    let bar = |x0: i32| ((x0 as f32 * s) as i32)..(((x0 + 3) as f32 * s).ceil() as i32);
    let y0 = (8.0 * s) as i32;
    let y1 = (24.0 * s) as i32;
    for y in y0..y1 {
        for x in bar(7) {
            set(x, y, fg);
        }
        for x in bar(22) {
            set(x, y, fg);
        }
    }
    for t in 0..(8.0 * s) as i32 {
        for dx in 0..(2.0 * s).ceil() as i32 {
            for dy in 0..(2.0 * s).ceil() as i32 {
                set((10.0 * s) as i32 + t + dx, (8.0 * s) as i32 + t + dy, fg);
                set((20.0 * s) as i32 - t + dx, (8.0 * s) as i32 + t + dy, fg);
            }
        }
    }
    px
}
