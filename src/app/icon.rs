//! The mesh-kvm icon, drawn in code so the tray and the window share one picture: two amber
//! screens on a dark panel.

include!("icon_pixels.rs");

/// The same picture the way the status notifier wants it: alpha, red, green, blue.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn argb() -> Vec<u8> {
    rgba().chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// The sizes the desktop entry's icon is written in, so a panel or menu never has to blur one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const ICON_SIZES: [u32; 5] = [32, 48, 64, 128, 256];

/// The icon at its own 32px size as a PNG file.
#[cfg(test)]
pub fn png() -> Vec<u8> {
    png_scaled(SIZE)
}

/// The icon at `size` by `size` as a PNG file, for the desktop entry. The pixels are stored
/// uncompressed. Each output pixel takes the nearest pixel of the
/// 32px picture, so the pixel art stays sharp.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn png_scaled(size: u32) -> Vec<u8> {
    let pixels = rgba();
    let nearest = |at: u32| ((2 * at + 1) * SIZE / (2 * size)) as usize;
    let mut raw = Vec::with_capacity((size as usize * 4 + 1) * size as usize);
    for y in 0..size {
        raw.push(0); // no filter
        let line = &pixels[nearest(y) * SIZE as usize * 4..][..SIZE as usize * 4];
        for x in 0..size {
            raw.extend_from_slice(&line[nearest(x) * 4..][..4]);
        }
    }
    // zlib: header, stored blocks of at most 64 KiB, checksum.
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(u16::MAX as usize).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits, RGBA, no interlace
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
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
    fn png_has_signature_and_size() {
        let png = png();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[16..24], &[0, 0, 0, 32, 0, 0, 0, 32]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn a_scaled_icon_is_the_same_picture_bigger() {
        let big = png_scaled(64);
        assert_eq!(&big[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&big[12..16], b"IHDR");
        assert_eq!(&big[16..24], &[0, 0, 0, 64, 0, 0, 0, 64]);
        assert_eq!(png_scaled(32), png());
    }

    #[test]
    fn crc_matches_the_png_specification_example() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
    }

    #[test]
    fn ico_holds_one_32_bit_image() {
        let ico = ico();
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!((ico[6], ico[7]), (32, 32));
        let size = u32::from_le_bytes(ico[14..18].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(ico[18..22].try_into().unwrap()) as usize;
        assert_eq!(offset + size, ico.len());
        // Bottom-up BGRA: the first stored row is the picture's last, and its corner is clear.
        assert_eq!(ico[offset + 40 + 3], 0);
    }

    #[test]
    fn argb_puts_alpha_first() {
        assert_eq!(pixel(&argb(), 3, 15), [0xff, 0xff, 0xb2, 0x2e]);
    }
}
