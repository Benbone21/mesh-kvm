#![allow(dead_code)]
// Universal Linux evdev KEY_* constants and bidirectional Windows VK / PS/2 scancode translation.

pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_KPASTERISK: u16 = 55;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_KP7: u16 = 71;
pub const KEY_KP8: u16 = 72;
pub const KEY_KP9: u16 = 73;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KP4: u16 = 75;
pub const KEY_KP5: u16 = 76;
pub const KEY_KP6: u16 = 77;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_KP1: u16 = 79;
pub const KEY_KP2: u16 = 80;
pub const KEY_KP3: u16 = 81;
pub const KEY_KP0: u16 = 82;
pub const KEY_KPDOT: u16 = 83;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;

pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_KPSLASH: u16 = 98;
pub const KEY_SYSRQ: u16 = 99;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_COMPOSE: u16 = 127;

pub const KEY_F13: u16 = 183;
pub const KEY_F14: u16 = 184;
pub const KEY_F15: u16 = 185;
pub const KEY_F16: u16 = 186;
pub const KEY_F17: u16 = 187;
pub const KEY_F18: u16 = 188;
pub const KEY_F19: u16 = 189;
pub const KEY_F20: u16 = 190;
pub const KEY_F21: u16 = 191;
pub const KEY_F22: u16 = 192;
pub const KEY_F23: u16 = 193;
pub const KEY_F24: u16 = 194;

pub const KEY_NEXTSONG: u16 = 163;
pub const KEY_PLAYPAUSE: u16 = 164;
pub const KEY_PREVIOUSSONG: u16 = 165;
pub const KEY_STOPCD: u16 = 166;

/// Translate Windows low-level keyboard hook `(vkCode, scanCode, extended)` into Linux `evdev` scancode (`KEY_*`).
pub fn win_vk_scan_to_evdev(vk: u32, scan: u32, extended: bool) -> Option<u16> {
    // First handle extended scancodes and explicit Virtual-Key mappings
    match vk {
        0x08 => return Some(KEY_BACKSPACE), // VK_BACK
        0x09 => return Some(KEY_TAB),       // VK_TAB
        0x0D => {
            // VK_RETURN
            return Some(if extended { KEY_KPENTER } else { KEY_ENTER });
        }
        0x13 => return Some(KEY_PAUSE),      // VK_PAUSE
        0x14 => return Some(KEY_CAPSLOCK),   // VK_CAPITAL
        0x1B => return Some(KEY_ESC),        // VK_ESCAPE
        0x20 => return Some(KEY_SPACE),      // VK_SPACE
        0x21 => return Some(KEY_PAGEUP),     // VK_PRIOR
        0x22 => return Some(KEY_PAGEDOWN),   // VK_NEXT
        0x23 => return Some(KEY_END),        // VK_END
        0x24 => return Some(KEY_HOME),       // VK_HOME
        0x25 => return Some(KEY_LEFT),       // VK_LEFT
        0x26 => return Some(KEY_UP),         // VK_UP
        0x27 => return Some(KEY_RIGHT),      // VK_RIGHT
        0x28 => return Some(KEY_DOWN),       // VK_DOWN
        0x2C => return Some(KEY_SYSRQ),      // VK_SNAPSHOT (PrintScreen)
        0x2D => return Some(KEY_INSERT),     // VK_INSERT
        0x2E => return Some(KEY_DELETE),     // VK_DELETE
        0x30 => return Some(KEY_0),
        0x31 => return Some(KEY_1),
        0x32 => return Some(KEY_2),
        0x33 => return Some(KEY_3),
        0x34 => return Some(KEY_4),
        0x35 => return Some(KEY_5),
        0x36 => return Some(KEY_6),
        0x37 => return Some(KEY_7),
        0x38 => return Some(KEY_8),
        0x39 => return Some(KEY_9),
        0x41 => return Some(KEY_A),
        0x42 => return Some(KEY_B),
        0x43 => return Some(KEY_C),
        0x44 => return Some(KEY_D),
        0x45 => return Some(KEY_E),
        0x46 => return Some(KEY_F),
        0x47 => return Some(KEY_G),
        0x48 => return Some(KEY_H),
        0x49 => return Some(KEY_I),
        0x4A => return Some(KEY_J),
        0x4B => return Some(KEY_K),
        0x4C => return Some(KEY_L),
        0x4D => return Some(KEY_M),
        0x4E => return Some(KEY_N),
        0x4F => return Some(KEY_O),
        0x50 => return Some(KEY_P),
        0x51 => return Some(KEY_Q),
        0x52 => return Some(KEY_R),
        0x53 => return Some(KEY_S),
        0x54 => return Some(KEY_T),
        0x55 => return Some(KEY_U),
        0x56 => return Some(KEY_V),
        0x57 => return Some(KEY_W),
        0x58 => return Some(KEY_X),
        0x59 => return Some(KEY_Y),
        0x5A => return Some(KEY_Z),
        0x5B => return Some(KEY_LEFTMETA),  // VK_LWIN
        0x5C => return Some(KEY_RIGHTMETA), // VK_RWIN
        0x5D => return Some(KEY_COMPOSE),   // VK_APPS
        0x60 => return Some(KEY_KP0),       // VK_NUMPAD0
        0x61 => return Some(KEY_KP1),
        0x62 => return Some(KEY_KP2),
        0x63 => return Some(KEY_KP3),
        0x64 => return Some(KEY_KP4),
        0x65 => return Some(KEY_KP5),
        0x66 => return Some(KEY_KP6),
        0x67 => return Some(KEY_KP7),
        0x68 => return Some(KEY_KP8),
        0x69 => return Some(KEY_KP9),
        0x6A => return Some(KEY_KPASTERISK), // VK_MULTIPLY
        0x6B => return Some(KEY_KPPLUS),     // VK_ADD
        0x6D => return Some(KEY_KPMINUS),    // VK_SUBTRACT
        0x6E => return Some(KEY_KPDOT),      // VK_DECIMAL
        0x6F => return Some(KEY_KPSLASH),    // VK_DIVIDE
        0x70 => return Some(KEY_F1),
        0x71 => return Some(KEY_F2),
        0x72 => return Some(KEY_F3),
        0x73 => return Some(KEY_F4),
        0x74 => return Some(KEY_F5),
        0x75 => return Some(KEY_F6),
        0x76 => return Some(KEY_F7),
        0x77 => return Some(KEY_F8),
        0x78 => return Some(KEY_F9),
        0x79 => return Some(KEY_F10),
        0x7A => return Some(KEY_F11),
        0x7B => return Some(KEY_F12),
        0x7C => return Some(KEY_F13),
        0x7D => return Some(KEY_F14),
        0x7E => return Some(KEY_F15),
        0x7F => return Some(KEY_F16),
        0x80 => return Some(KEY_F17),
        0x81 => return Some(KEY_F18),
        0x82 => return Some(KEY_F19),
        0x83 => return Some(KEY_F20),
        0x84 => return Some(KEY_F21),
        0x85 => return Some(KEY_F22),
        0x86 => return Some(KEY_F23),
        0x87 => return Some(KEY_F24),
        0x90 => return Some(KEY_NUMLOCK),    // VK_NUMLOCK
        0x91 => return Some(KEY_SCROLLLOCK), // VK_SCROLL
        0xA0 => return Some(KEY_LEFTSHIFT),  // VK_LSHIFT
        0xA1 => return Some(KEY_RIGHTSHIFT), // VK_RSHIFT
        0xA2 => return Some(KEY_LEFTCTRL),   // VK_LCONTROL
        0xA3 => return Some(KEY_RIGHTCTRL),  // VK_RCONTROL
        0xA4 => return Some(KEY_LEFTALT),    // VK_LMENU
        0xA5 => return Some(KEY_RIGHTALT),   // VK_RMENU
        0x10 => return Some(if scan == 0x36 { KEY_RIGHTSHIFT } else { KEY_LEFTSHIFT }),
        0x11 => return Some(if extended { KEY_RIGHTCTRL } else { KEY_LEFTCTRL }),
        0x12 => return Some(if extended { KEY_RIGHTALT } else { KEY_LEFTALT }),
        0xAD => return Some(KEY_MUTE),         // VK_VOLUME_MUTE
        0xAE => return Some(KEY_VOLUMEDOWN),   // VK_VOLUME_DOWN
        0xAF => return Some(KEY_VOLUMEUP),     // VK_VOLUME_UP
        0xB0 => return Some(KEY_NEXTSONG),     // VK_MEDIA_NEXT_TRACK
        0xB1 => return Some(KEY_PREVIOUSSONG), // VK_MEDIA_PREV_TRACK
        0xB2 => return Some(KEY_STOPCD),       // VK_MEDIA_STOP
        0xB3 => return Some(KEY_PLAYPAUSE),    // VK_MEDIA_PLAY_PAUSE
        0xBA => return Some(KEY_SEMICOLON),    // VK_OEM_1 (;:)
        0xBB => return Some(KEY_EQUAL),        // VK_OEM_PLUS (=+)
        0xBC => return Some(KEY_COMMA),        // VK_OEM_COMMA (,<)
        0xBD => return Some(KEY_MINUS),        // VK_OEM_MINUS (-_)
        0xBE => return Some(KEY_DOT),          // VK_OEM_PERIOD (.>)
        0xBF => return Some(KEY_SLASH),        // VK_OEM_2 (/?)
        0xC0 => return Some(KEY_GRAVE),        // VK_OEM_3 (`~)
        0xDB => return Some(KEY_LEFTBRACE),    // VK_OEM_4 ([{)
        0xDC => return Some(KEY_BACKSLASH),    // VK_OEM_5 (\|)
        0xDD => return Some(KEY_RIGHTBRACE),   // VK_OEM_6 (]})
        0xDE => return Some(KEY_APOSTROPHE),   // VK_OEM_7 ('")
        0xE2 => return Some(86),               // VK_OEM_102 (KEY_102ND)
        _ => {}
    }

    // Fallback to hardware PS/2 Set 1 make scancode (identical to Linux evdev 1..=88 when not extended)
    if !extended && (1..=88).contains(&scan) {
        return Some(scan as u16);
    }

    None
}

/// Translate a Linux `evdev` key into the PS/2 set-1 make scancode and whether Windows needs
/// `KEYEVENTF_EXTENDEDKEY` for it. Returns `None` for keys with no stable set-1 code
/// (F13 to F24, Pause) or unknown keys.
pub fn evdev_to_win_scan(evdev: u16) -> Option<(u16, bool)> {
    let extended = |scan: u16| Some((scan, true));
    match evdev {
        // Evdev 1..=88 equals the set-1 make code for the non-extended block.
        1..=83 | 86..=88 => Some((evdev, false)),
        KEY_KPENTER => extended(0x1C),
        KEY_RIGHTCTRL => extended(0x1D),
        KEY_KPSLASH => extended(0x35),
        KEY_SYSRQ => extended(0x37),
        KEY_RIGHTALT => extended(0x38),
        KEY_HOME => extended(0x47),
        KEY_UP => extended(0x48),
        KEY_PAGEUP => extended(0x49),
        KEY_LEFT => extended(0x4B),
        KEY_RIGHT => extended(0x4D),
        KEY_END => extended(0x4F),
        KEY_DOWN => extended(0x50),
        KEY_PAGEDOWN => extended(0x51),
        KEY_INSERT => extended(0x52),
        KEY_DELETE => extended(0x53),
        KEY_MUTE => extended(0x20),
        KEY_VOLUMEDOWN => extended(0x2E),
        KEY_VOLUMEUP => extended(0x30),
        KEY_LEFTMETA => extended(0x5B),
        KEY_RIGHTMETA => extended(0x5C),
        KEY_COMPOSE => extended(0x5D),
        KEY_NEXTSONG => extended(0x19),
        KEY_PLAYPAUSE => extended(0x22),
        KEY_PREVIOUSSONG => extended(0x10),
        KEY_STOPCD => extended(0x24),
        _ => None,
    }
}

/// Returns true if an evdev scancode is a modifier key (`Ctrl`, `Shift`, `Alt`, `Meta`).
pub fn is_modifier_evdev(scancode: u16) -> bool {
    matches!(
        scancode,
        KEY_LEFTCTRL
            | KEY_RIGHTCTRL
            | KEY_LEFTSHIFT
            | KEY_RIGHTSHIFT
            | KEY_LEFTALT
            | KEY_RIGHTALT
            | KEY_LEFTMETA
            | KEY_RIGHTMETA
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only evdev -> Windows VK table, used to drive the round trip through
    /// `win_vk_scan_to_evdev`.
    const VK_TABLE: &[(u16, u32)] = &[
        (KEY_ESC, 0x1B), (KEY_1, 0x31), (KEY_2, 0x32), (KEY_3, 0x33), (KEY_4, 0x34),
        (KEY_5, 0x35), (KEY_6, 0x36), (KEY_7, 0x37), (KEY_8, 0x38), (KEY_9, 0x39),
        (KEY_0, 0x30), (KEY_MINUS, 0xBD), (KEY_EQUAL, 0xBB), (KEY_BACKSPACE, 0x08),
        (KEY_TAB, 0x09), (KEY_Q, 0x51), (KEY_W, 0x57), (KEY_E, 0x45), (KEY_R, 0x52),
        (KEY_T, 0x54), (KEY_Y, 0x59), (KEY_U, 0x55), (KEY_I, 0x49), (KEY_O, 0x4F),
        (KEY_P, 0x50), (KEY_LEFTBRACE, 0xDB), (KEY_RIGHTBRACE, 0xDD), (KEY_ENTER, 0x0D),
        (KEY_LEFTCTRL, 0xA2), (KEY_A, 0x41), (KEY_S, 0x53), (KEY_D, 0x44), (KEY_F, 0x46),
        (KEY_G, 0x47), (KEY_H, 0x48), (KEY_J, 0x4A), (KEY_K, 0x4B), (KEY_L, 0x4C),
        (KEY_SEMICOLON, 0xBA), (KEY_APOSTROPHE, 0xDE), (KEY_GRAVE, 0xC0),
        (KEY_LEFTSHIFT, 0xA0), (KEY_BACKSLASH, 0xDC), (KEY_Z, 0x5A), (KEY_X, 0x58),
        (KEY_C, 0x43), (KEY_V, 0x56), (KEY_B, 0x42), (KEY_N, 0x4E), (KEY_M, 0x4D),
        (KEY_COMMA, 0xBC), (KEY_DOT, 0xBE), (KEY_SLASH, 0xBF), (KEY_RIGHTSHIFT, 0xA1),
        (KEY_KPASTERISK, 0x6A), (KEY_LEFTALT, 0xA4), (KEY_SPACE, 0x20), (KEY_CAPSLOCK, 0x14),
        (KEY_F1, 0x70), (KEY_F2, 0x71), (KEY_F3, 0x72), (KEY_F4, 0x73), (KEY_F5, 0x74),
        (KEY_F6, 0x75), (KEY_F7, 0x76), (KEY_F8, 0x77), (KEY_F9, 0x78), (KEY_F10, 0x79),
        (KEY_NUMLOCK, 0x90), (KEY_SCROLLLOCK, 0x91), (KEY_KP7, 0x67), (KEY_KP8, 0x68),
        (KEY_KP9, 0x69), (KEY_KPMINUS, 0x6D), (KEY_KP4, 0x64), (KEY_KP5, 0x65),
        (KEY_KP6, 0x66), (KEY_KPPLUS, 0x6B), (KEY_KP1, 0x61), (KEY_KP2, 0x62),
        (KEY_KP3, 0x63), (KEY_KP0, 0x60), (KEY_KPDOT, 0x6E), (86, 0xE2),
        (KEY_F11, 0x7A), (KEY_F12, 0x7B), (KEY_KPENTER, 0x0D), (KEY_RIGHTCTRL, 0xA3),
        (KEY_KPSLASH, 0x6F), (KEY_SYSRQ, 0x2C), (KEY_RIGHTALT, 0xA5), (KEY_HOME, 0x24),
        (KEY_UP, 0x26), (KEY_PAGEUP, 0x21), (KEY_LEFT, 0x25), (KEY_RIGHT, 0x27),
        (KEY_END, 0x23), (KEY_DOWN, 0x28), (KEY_PAGEDOWN, 0x22), (KEY_INSERT, 0x2D),
        (KEY_DELETE, 0x2E), (KEY_MUTE, 0xAD), (KEY_VOLUMEDOWN, 0xAE), (KEY_VOLUMEUP, 0xAF),
        (KEY_LEFTMETA, 0x5B), (KEY_RIGHTMETA, 0x5C), (KEY_COMPOSE, 0x5D),
        (KEY_NEXTSONG, 0xB0), (KEY_PLAYPAUSE, 0xB3), (KEY_PREVIOUSSONG, 0xB1),
        (KEY_STOPCD, 0xB2),
    ];

    fn vk_for(key: u16) -> Option<u32> {
        VK_TABLE.iter().find(|(k, _)| *k == key).map(|(_, vk)| *vk)
    }

    #[test]
    fn letters_map_to_scancodes() {
        assert_eq!(evdev_to_win_scan(KEY_A), Some((0x1E, false)));
        assert_eq!(evdev_to_win_scan(KEY_ENTER), Some((0x1C, false)));
    }

    #[test]
    fn extended_keys_are_flagged() {
        assert_eq!(evdev_to_win_scan(KEY_RIGHTCTRL), Some((0x1D, true)));
        assert_eq!(evdev_to_win_scan(KEY_LEFT), Some((0x4B, true)));
        assert_eq!(evdev_to_win_scan(KEY_LEFTMETA), Some((0x5B, true)));
        assert_eq!(evdev_to_win_scan(KEY_KPENTER), Some((0x1C, true)));
    }

    #[test]
    fn every_key_round_trips() {
        let mut mapped = 0;
        for key in 0..=u16::MAX {
            let Some((scan, ext)) = evdev_to_win_scan(key) else { continue };
            let vk = vk_for(key).unwrap_or_else(|| panic!("no test VK for evdev {key}"));
            assert_eq!(
                win_vk_scan_to_evdev(vk, scan as u32, ext),
                Some(key),
                "evdev {key} vk {vk:#x} scan {scan:#x} ext {ext}"
            );
            mapped += 1;
        }
        // Every entry in the test table is actually mapped (no stale rows).
        assert_eq!(mapped, VK_TABLE.len());
    }

    #[test]
    fn unknown_key_is_none() {
        assert_eq!(evdev_to_win_scan(0xFFFF), None);
        assert_eq!(evdev_to_win_scan(KEY_F13), None);
    }
}
