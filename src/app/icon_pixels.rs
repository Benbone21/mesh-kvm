// The picture itself, shared by the program (src/app/icon.rs) and the build script (build.rs),
// which both `include!` this file so the exe's icon resource can't drift from the tray icon.
// No inner attributes or `//!` docs here: it is textually included.

pub const SIZE: u32 = 32;

pub const AMBER: [u8; 3] = [0xff, 0xb2, 0x2e];
pub const PANEL: [u8; 3] = [0x20, 0x35, 0x3c];

const CORNER: i32 = 6;
const BORDER: i32 = 2;
/// The two screens as (left, top, right, bottom), right and bottom exclusive.
const SCREENS: [(i32, i32, i32, i32); 2] = [(3, 9, 15, 22), (17, 9, 29, 22)];
/// The foot under each screen.
const FEET: [(i32, i32, i32, i32); 2] = [(6, 24, 12, 26), (20, 24, 26, 26)];

fn inside(rect: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    x >= rect.0 && x < rect.2 && y >= rect.1 && y < rect.3
}

/// True where the rounded panel covers pixel (x, y).
fn on_panel(x: i32, y: i32) -> bool {
    let last = SIZE as i32 - 1;
    let dx = (CORNER - x).max(x - (last - CORNER)).max(0);
    let dy = (CORNER - y).max(y - (last - CORNER)).max(0);
    dx * dx + dy * dy <= CORNER * CORNER
}

fn on_amber(x: i32, y: i32) -> bool {
    let screen = SCREENS.iter().any(|&(l, t, r, b)| {
        inside((l, t, r, b), x, y) && !inside((l + BORDER, t + BORDER, r - BORDER, b - BORDER), x, y)
    });
    screen || FEET.iter().any(|&f| inside(f, x, y))
}

/// SIZE x SIZE pixels, row by row, four bytes each: red, green, blue, alpha.
pub fn rgba() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let [r, g, b] = if on_amber(x, y) { AMBER } else { PANEL };
            let alpha = if on_panel(x, y) { 0xff } else { 0 };
            pixels.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    pixels
}

/// A Windows .ico holding the picture as one 32-bit image.
#[allow(dead_code)] // Only the build script and tests use it.
pub fn ico() -> Vec<u8> {
    let pixels = rgba();
    let row = (SIZE * 4) as usize;
    let mask_row = (SIZE as usize).div_ceil(32) * 4;
    let image_size = 40 + pixels.len() + mask_row * SIZE as usize;
    let mut ico = vec![0, 0, 1, 0, 1, 0];
    ico.extend_from_slice(&[SIZE as u8, SIZE as u8, 0, 0, 1, 0, 32, 0]);
    ico.extend_from_slice(&(image_size as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    // BITMAPINFOHEADER; the height counts the AND mask too.
    ico.extend_from_slice(&40u32.to_le_bytes());
    ico.extend_from_slice(&SIZE.to_le_bytes());
    ico.extend_from_slice(&(SIZE * 2).to_le_bytes());
    ico.extend_from_slice(&[1, 0, 32, 0]);
    ico.extend_from_slice(&[0; 24]);
    for line in pixels.chunks_exact(row).rev() {
        for p in line.chunks_exact(4) {
            ico.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    ico.extend_from_slice(&vec![0; mask_row * SIZE as usize]);
    ico
}
