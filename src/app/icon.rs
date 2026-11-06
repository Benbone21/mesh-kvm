//! The mesh-kvm icon, drawn in code so the tray and the window share one picture: two amber
//! screens on a dark panel.

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

/// The same picture the way the status notifier wants it: alpha, red, green, blue.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn argb() -> Vec<u8> {
    rgba().chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
}

pub fn egui_icon() -> eframe::egui::IconData {
    eframe::egui::IconData { rgba: rgba(), width: SIZE, height: SIZE }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn icon_is_32_by_32_rgba() {
        assert_eq!(rgba().len(), 32 * 32 * 4);
    }

    #[test]
    fn icon_uses_the_amber_and_panel_colours() {
        let p = rgba();
        assert_eq!(pixel(&p, 3, 15), [0xff, 0xb2, 0x2e, 0xff], "screen border");
        assert_eq!(pixel(&p, 9, 15), [0x20, 0x35, 0x3c, 0xff], "screen glass");
        assert_eq!(pixel(&p, 0, 0)[3], 0, "rounded corner is clear");
    }

    #[test]
    fn argb_puts_alpha_first() {
        assert_eq!(pixel(&argb(), 3, 15), [0xff, 0xff, 0xb2, 0x2e]);
    }
}
