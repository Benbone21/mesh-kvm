//! Pure pieces of the Windows backend. They live outside `windows.rs` because that file is
//! cfg-gated, so its tests cannot run on the Debian machine that builds this project.

use crate::keymap::{
    KEY_F11, KEY_F12, KEY_G, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL,
    KEY_SCROLLLOCK,
};
use crate::protocol::{
    MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use std::sync::{Mutex, MutexGuard};

// Win32 values, restated so this file builds on Linux. `windows.rs` asserts they match.
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_XBUTTONDOWN: u32 = 0x020B;
pub const WM_XBUTTONUP: u32 = 0x020C;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;
pub const XBUTTON1: u32 = 1;
pub const XBUTTON2: u32 = 2;
pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x1;
pub const KEYEVENTF_KEYUP: u32 = 0x2;
pub const KEYEVENTF_SCANCODE: u32 = 0x8;

/// Held-key table size: every evdev key that `evdev_to_win_scan` maps is below this.
pub const KEY_SLOTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HookGate {
    Route,
    /// Hand the event to the OS untouched and drop the pin.
    PassToOs,
}

/// What a hook does first. A poisoned router may be half-updated, so input fails open: it must
/// never be swallowed on the word of state nobody can trust.
pub fn hook_gate(router_poisoned: bool) -> HookGate {
    if router_poisoned {
        HookGate::PassToOs
    } else {
        HookGate::Route
    }
}

/// Locks `mutex`, or `None` once a panic has poisoned it.
pub fn lock_live<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    mutex.lock().ok()
}

/// Movement carried by one mouse-hook event at `pt`, or `None` for an event that is only our
/// own pin coming back.
///
/// While pinned, movement is measured from where the cursor really is. That is the pin unless
/// something else moved it, and measuring from the pin then would add a constant to every event.
/// While local, it is measured from `last`, which must already be clamped to the screen.
pub fn move_offset(
    pt: (i32, i32),
    last: (i32, i32),
    pin: Option<(i32, i32)>,
    cursor: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    let base = match pin {
        Some(pin) if pt == pin => return None,
        Some(pin) => cursor.unwrap_or(pin),
        None => last,
    };
    Some((pt.0.saturating_sub(base.0), pt.1.saturating_sub(base.1)))
}

/// Where the cursor ends up for a hook position, which can lie past the screen edge.
pub fn clamp_to_screen(pt: (i32, i32), w: i32, h: i32) -> (i32, i32) {
    (pt.0.clamp(0, w - 1), pt.1.clamp(0, h - 1))
}

fn high_word(mouse_data: u32) -> u16 {
    (mouse_data >> 16) as u16
}

/// The `MOUSE_BTN_*` button and pressed state for a mouse-hook message.
pub fn mouse_button_event(msg: u32, mouse_data: u32) -> Option<(u8, bool)> {
    let x_button = || match high_word(mouse_data) as u32 {
        XBUTTON1 => Some(MOUSE_BTN_SIDE),
        XBUTTON2 => Some(MOUSE_BTN_EXTRA),
        _ => None,
    };
    match msg {
        WM_LBUTTONDOWN => Some((MOUSE_BTN_LEFT, true)),
        WM_LBUTTONUP => Some((MOUSE_BTN_LEFT, false)),
        WM_RBUTTONDOWN => Some((MOUSE_BTN_RIGHT, true)),
        WM_RBUTTONUP => Some((MOUSE_BTN_RIGHT, false)),
        WM_MBUTTONDOWN => Some((MOUSE_BTN_MIDDLE, true)),
        WM_MBUTTONUP => Some((MOUSE_BTN_MIDDLE, false)),
        WM_XBUTTONDOWN => x_button().map(|b| (b, true)),
        WM_XBUTTONUP => x_button().map(|b| (b, false)),
        _ => None,
    }
}

/// Wheel `(dx, dy)` for a mouse-hook message, in wire units (120 per notch).
pub fn wheel_delta(msg: u32, mouse_data: u32) -> Option<(i32, i32)> {
    let delta = high_word(mouse_data) as i16 as i32;
    match msg {
        WM_MOUSEWHEEL => Some((0, delta)),
        WM_MOUSEHWHEEL => Some((delta, 0)),
        _ => None,
    }
}

/// `mask` after `button` changes state. One bit per `MOUSE_BTN_*` value, from bit 0.
pub fn button_mask(mask: u8, button: u8, pressed: bool) -> u8 {
    if !(1..=8).contains(&button) {
        return mask;
    }
    let bit = 1 << (button - 1);
    if pressed {
        mask | bit
    } else {
        mask & !bit
    }
}

/// `SendInput` flags for one scancode key event.
pub fn key_flags(extended: bool, pressed: bool) -> u32 {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    flags
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hotkey {
    ManualSwitch,
    Panic,
    GameLock,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyRoute {
    Hotkey(Hotkey),
    /// Repeat or release of a hotkey whose press was kept from the OS.
    ConsumedSwallowed,
    /// Repeat or release of a hotkey whose press went on to the OS.
    ConsumedToOs,
    /// An ordinary key: the router decides.
    Route,
}

#[derive(Clone, Copy, PartialEq)]
enum HotkeyKeyState {
    Up,
    /// Down as an ordinary key, so its repeats must not fire a hotkey.
    Ordinary,
    Swallowed,
    ToOs,
}

/// What the OS believes about Ctrl and Alt, from `GetAsyncKeyState`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OsModifiers {
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ModifierState {
    Up,
    /// Down, and the OS saw the press, so it can confirm this.
    SeenByOs,
    /// Down, but the hook swallowed the press: the OS will report it as up.
    Swallowed,
}

/// Whether a side of this modifier is really down. A press the OS saw but no longer reports
/// lost its key-up on the secure desktop, where hooks see nothing, so it is cleared.
fn confirm_modifier(sides: &mut [ModifierState; 2], os_down: bool) -> bool {
    let down = sides.iter().any(|&side| side != ModifierState::Up);
    if down && !os_down && !sides.contains(&ModifierState::Swallowed) {
        *sides = [ModifierState::Up; 2];
        return false;
    }
    down
}

const HOTKEY_KEYS: [u16; 4] = [KEY_F11, KEY_F12, KEY_SCROLLLOCK, KEY_G];

/// Finds hotkeys in the physical key stream, so a hotkey's own key never reaches the router.
/// Ctrl and Alt are ordinary keys here: they are tracked and passed on.
pub struct HotkeyFilter {
    /// Left, then right.
    ctrl: [ModifierState; 2],
    alt: [ModifierState; 2],
    keys: [HotkeyKeyState; HOTKEY_KEYS.len()],
}

impl HotkeyFilter {
    pub const fn new() -> Self {
        Self {
            ctrl: [ModifierState::Up; 2],
            alt: [ModifierState::Up; 2],
            keys: [HotkeyKeyState::Up; HOTKEY_KEYS.len()],
        }
    }

    fn modifier(&mut self, code: u16) -> Option<&mut ModifierState> {
        match code {
            KEY_LEFTCTRL => Some(&mut self.ctrl[0]),
            KEY_RIGHTCTRL => Some(&mut self.ctrl[1]),
            KEY_LEFTALT => Some(&mut self.alt[0]),
            KEY_RIGHTALT => Some(&mut self.alt[1]),
            _ => None,
        }
    }

    /// Records that the hook kept this key's press from the OS. Without this, the panic
    /// hotkey would fail while forwarding, which is when it matters.
    pub fn note_swallowed(&mut self, code: u16) {
        if let Some(state) = self.modifier(code) {
            if *state != ModifierState::Up {
                *state = ModifierState::Swallowed;
            }
        }
    }

    /// Classifies one physical key event. Windows reports an auto-repeat as another press.
    /// Panic needs no link and fires whatever the key did before, as it is the way out. The
    /// other hotkeys are ordinary keys while `link_up` is false.
    pub fn feed(&mut self, code: u16, pressed: bool, link_up: bool, os: OsModifiers) -> KeyRoute {
        if let Some(state) = self.modifier(code) {
            *state = match (pressed, *state) {
                (false, _) => ModifierState::Up,
                (true, ModifierState::Up) => ModifierState::SeenByOs,
                (true, held) => held,
            };
        }
        let Some(slot) = HOTKEY_KEYS.iter().position(|&key| key == code) else {
            return KeyRoute::Route;
        };
        let state = self.keys[slot];
        if !pressed {
            self.keys[slot] = HotkeyKeyState::Up;
            return match state {
                HotkeyKeyState::Swallowed => KeyRoute::ConsumedSwallowed,
                HotkeyKeyState::ToOs => KeyRoute::ConsumedToOs,
                HotkeyKeyState::Up | HotkeyKeyState::Ordinary => KeyRoute::Route,
            };
        }

        let ctrl = confirm_modifier(&mut self.ctrl, os.ctrl);
        let alt = confirm_modifier(&mut self.alt, os.alt);
        let chord = ctrl && alt;
        if chord && matches!(code, KEY_F12 | KEY_SCROLLLOCK) {
            self.keys[slot] = HotkeyKeyState::ToOs;
            return KeyRoute::Hotkey(Hotkey::Panic);
        }
        match state {
            HotkeyKeyState::Swallowed => KeyRoute::ConsumedSwallowed,
            HotkeyKeyState::ToOs => KeyRoute::ConsumedToOs,
            HotkeyKeyState::Ordinary => KeyRoute::Route,
            HotkeyKeyState::Up => {
                let hotkey = match code {
                    KEY_F11 if chord && link_up => Some(Hotkey::ManualSwitch),
                    KEY_G if chord && link_up => Some(Hotkey::GameLock),
                    _ => None,
                };
                self.keys[slot] = match hotkey {
                    Some(_) => HotkeyKeyState::Swallowed,
                    None => HotkeyKeyState::Ordinary,
                };
                hotkey.map_or(KeyRoute::Route, KeyRoute::Hotkey)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{evdev_to_win_scan, KEY_A, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL};
    use crate::keymap::{KEY_F11, KEY_F12, KEY_G, KEY_SCROLLLOCK};
    use crate::protocol::{
        MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
    };

    const PIN: (i32, i32) = (960, 540);
    const OS_DOWN: OsModifiers = OsModifiers { ctrl: true, alt: true };
    const OS_UP: OsModifiers = OsModifiers { ctrl: false, alt: false };

    #[test]
    fn offset_while_local_is_from_the_last_position() {
        assert_eq!(move_offset((105, 197), (100, 200), None, None), Some((5, -3)));
        assert_eq!(move_offset((100, 200), (100, 200), None, None), Some((0, 0)));
    }

    #[test]
    fn offset_while_pinned_is_from_the_pin() {
        assert_eq!(move_offset((970, 530), (0, 0), Some(PIN), Some(PIN)), Some((10, -10)));
        assert_eq!(move_offset((970, 530), (0, 0), Some(PIN), None), Some((10, -10)));
    }

    #[test]
    fn event_at_the_pin_is_no_movement() {
        assert_eq!(move_offset(PIN, (0, 0), Some(PIN), Some(PIN)), None);
        assert_eq!(move_offset(PIN, (0, 0), Some(PIN), Some((5, 5))), None);
    }

    #[test]
    fn cursor_off_the_pin_gives_true_movement_not_a_constant_bias() {
        assert_eq!(move_offset((972, 541), (0, 0), Some(PIN), Some((970, 540))), Some((2, 1)));
    }

    #[test]
    fn offset_does_not_overflow() {
        assert_eq!(
            move_offset((i32::MAX, i32::MIN), (-1, 1), None, None),
            Some((i32::MAX, i32::MIN))
        );
    }

    #[test]
    fn hook_position_past_the_edge_clamps_to_the_screen() {
        assert_eq!(clamp_to_screen((-3, 2000), 1920, 1080), (0, 1079));
        assert_eq!(clamp_to_screen((1925, -1), 1920, 1080), (1919, 0));
        assert_eq!(clamp_to_screen((50, 60), 1920, 1080), (50, 60));
    }

    #[test]
    fn repeated_outward_push_keeps_reporting_outward_movement() {
        // The cursor stays on the edge, so each event must be measured from the clamped pixel.
        let mut last = (0, 500);
        for pt in [(-3, 500), (-2, 500), (-4, 500)] {
            let (dx, _) = move_offset(pt, last, None, None).unwrap();
            assert!(dx < 0);
            last = clamp_to_screen(pt, 1920, 1080);
        }
    }

    #[test]
    fn window_messages_map_to_buttons() {
        let cases = [
            (WM_LBUTTONDOWN, MOUSE_BTN_LEFT, true),
            (WM_LBUTTONUP, MOUSE_BTN_LEFT, false),
            (WM_RBUTTONDOWN, MOUSE_BTN_RIGHT, true),
            (WM_RBUTTONUP, MOUSE_BTN_RIGHT, false),
            (WM_MBUTTONDOWN, MOUSE_BTN_MIDDLE, true),
            (WM_MBUTTONUP, MOUSE_BTN_MIDDLE, false),
        ];
        for (msg, button, pressed) in cases {
            assert_eq!(mouse_button_event(msg, 0), Some((button, pressed)));
        }
    }

    #[test]
    fn x_buttons_use_the_high_word() {
        assert_eq!(mouse_button_event(WM_XBUTTONDOWN, 1 << 16), Some((MOUSE_BTN_SIDE, true)));
        assert_eq!(mouse_button_event(WM_XBUTTONUP, 1 << 16), Some((MOUSE_BTN_SIDE, false)));
        assert_eq!(mouse_button_event(WM_XBUTTONDOWN, 2 << 16), Some((MOUSE_BTN_EXTRA, true)));
        assert_eq!(mouse_button_event(WM_XBUTTONUP, 2 << 16), Some((MOUSE_BTN_EXTRA, false)));
        assert_eq!(mouse_button_event(WM_XBUTTONDOWN, 3 << 16), None);
    }

    #[test]
    fn other_messages_are_not_buttons() {
        assert_eq!(mouse_button_event(WM_MOUSEMOVE, 0), None);
        assert_eq!(mouse_button_event(WM_MOUSEWHEEL, 120 << 16), None);
    }

    #[test]
    fn wheel_delta_is_the_signed_high_word() {
        assert_eq!(wheel_delta(WM_MOUSEWHEEL, 120 << 16), Some((0, 120)));
        assert_eq!(wheel_delta(WM_MOUSEWHEEL, (-120i16 as u16 as u32) << 16), Some((0, -120)));
        assert_eq!(wheel_delta(WM_MOUSEHWHEEL, 60 << 16), Some((60, 0)));
        assert_eq!(wheel_delta(WM_MOUSEHWHEEL, (-30i16 as u16 as u32) << 16), Some((-30, 0)));
        assert_eq!(wheel_delta(WM_MOUSEMOVE, 120 << 16), None);
    }

    #[test]
    fn button_mask_tracks_each_button() {
        let m = button_mask(0, MOUSE_BTN_LEFT, true);
        let m = button_mask(m, MOUSE_BTN_EXTRA, true);
        assert_eq!(m, 0b1_0001);
        assert_eq!(button_mask(m, MOUSE_BTN_LEFT, false), 0b1_0000);
        assert_eq!(button_mask(0b1_0000, MOUSE_BTN_EXTRA, false), 0);
    }

    #[test]
    fn unknown_buttons_leave_the_mask_alone() {
        assert_eq!(button_mask(0b1, 0, true), 0b1);
        assert_eq!(button_mask(0b1, 9, true), 0b1);
    }

    #[test]
    fn key_flags_are_always_scancode() {
        assert_eq!(key_flags(false, true), KEYEVENTF_SCANCODE);
        assert_eq!(key_flags(false, false), KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
        assert_eq!(key_flags(true, true), KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY);
        assert_eq!(
            key_flags(true, false),
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
    }

    #[test]
    fn every_injectable_key_has_a_held_slot() {
        for key in 0..=u16::MAX {
            if evdev_to_win_scan(key).is_some() {
                assert!((key as usize) < KEY_SLOTS, "key {key}");
            }
        }
    }

    fn ctrl_alt(f: &mut HotkeyFilter) {
        assert_eq!(f.feed(KEY_LEFTCTRL, true, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_LEFTALT, true, true, OS_DOWN), KeyRoute::Route);
    }

    #[test]
    fn hotkeys_fire_with_ctrl_and_alt() {
        for (key, hotkey) in [
            (KEY_F11, Hotkey::ManualSwitch),
            (KEY_F12, Hotkey::Panic),
            (KEY_SCROLLLOCK, Hotkey::Panic),
            (KEY_G, Hotkey::GameLock),
        ] {
            let mut f = HotkeyFilter::new();
            ctrl_alt(&mut f);
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::Hotkey(hotkey));
        }
    }

    #[test]
    fn either_side_modifier_counts() {
        let mut f = HotkeyFilter::new();
        f.feed(KEY_RIGHTCTRL, true, true, OS_DOWN);
        f.feed(KEY_RIGHTALT, true, true, OS_DOWN);
        assert_eq!(f.feed(KEY_F12, true, true, OS_DOWN), KeyRoute::Hotkey(Hotkey::Panic));
    }

    #[test]
    fn releasing_one_side_keeps_the_other() {
        let mut f = HotkeyFilter::new();
        f.feed(KEY_LEFTCTRL, true, true, OS_DOWN);
        f.feed(KEY_RIGHTCTRL, true, true, OS_DOWN);
        f.feed(KEY_LEFTCTRL, false, true, OS_DOWN);
        f.feed(KEY_LEFTALT, true, true, OS_DOWN);
        assert_eq!(f.feed(KEY_G, true, true, OS_DOWN), KeyRoute::Hotkey(Hotkey::GameLock));
    }

    #[test]
    fn hotkey_keys_without_both_modifiers_route_normally() {
        let mut f = HotkeyFilter::new();
        assert_eq!(f.feed(KEY_G, true, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_G, false, true, OS_DOWN), KeyRoute::Route);
        f.feed(KEY_LEFTCTRL, true, true, OS_DOWN);
        assert_eq!(f.feed(KEY_F12, true, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_F12, false, true, OS_DOWN), KeyRoute::Route);
    }

    #[test]
    fn other_keys_route_with_modifiers_held() {
        let mut f = HotkeyFilter::new();
        ctrl_alt(&mut f);
        assert_eq!(f.feed(KEY_A, true, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_A, false, true, OS_DOWN), KeyRoute::Route);
    }

    #[test]
    fn panic_works_with_no_link_and_stays_with_the_os() {
        for key in [KEY_F12, KEY_SCROLLLOCK] {
            let mut f = HotkeyFilter::new();
            f.feed(KEY_LEFTCTRL, true, false, OS_DOWN);
            f.feed(KEY_LEFTALT, true, false, OS_DOWN);
            assert_eq!(f.feed(key, true, false, OS_DOWN), KeyRoute::Hotkey(Hotkey::Panic));
            assert_eq!(f.feed(key, false, false, OS_DOWN), KeyRoute::ConsumedToOs);
            assert_eq!(f.feed(key, true, false, OS_DOWN), KeyRoute::Hotkey(Hotkey::Panic));
        }
    }

    #[test]
    fn switch_and_game_lock_need_the_link() {
        for key in [KEY_F11, KEY_G] {
            let mut f = HotkeyFilter::new();
            f.feed(KEY_LEFTCTRL, true, false, OS_DOWN);
            f.feed(KEY_LEFTALT, true, false, OS_DOWN);
            assert_eq!(f.feed(key, true, false, OS_DOWN), KeyRoute::Route);
            assert_eq!(f.feed(key, false, false, OS_DOWN), KeyRoute::Route);
        }
    }

    #[test]
    fn repeat_and_release_of_a_swallowed_hotkey_are_swallowed() {
        for key in [KEY_F11, KEY_G] {
            let mut f = HotkeyFilter::new();
            ctrl_alt(&mut f);
            assert!(matches!(f.feed(key, true, true, OS_DOWN), KeyRoute::Hotkey(_)));
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::ConsumedSwallowed);
            assert_eq!(f.feed(key, false, true, OS_DOWN), KeyRoute::ConsumedSwallowed);
        }
    }

    #[test]
    fn hotkey_release_stays_consumed_after_ctrl_and_alt_go_up() {
        let mut f = HotkeyFilter::new();
        ctrl_alt(&mut f);
        f.feed(KEY_F11, true, true, OS_DOWN);
        assert_eq!(f.feed(KEY_LEFTCTRL, false, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_LEFTALT, false, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_F11, false, true, OS_DOWN), KeyRoute::ConsumedSwallowed);
        assert_eq!(f.feed(KEY_F11, true, true, OS_DOWN), KeyRoute::Route);
    }

    #[test]
    fn panic_fires_from_any_key_history() {
        for key in [KEY_F12, KEY_SCROLLLOCK] {
            // Held as an ordinary key before Ctrl+Alt went down.
            let mut f = HotkeyFilter::new();
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::Route);
            ctrl_alt(&mut f);
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::Hotkey(Hotkey::Panic));
            // Auto-repeat of a panic that already fired.
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::Hotkey(Hotkey::Panic));
            assert_eq!(f.feed(key, false, true, OS_DOWN), KeyRoute::ConsumedToOs);
        }
    }

    #[test]
    fn panic_repeat_without_the_chord_stays_with_the_os() {
        let mut f = HotkeyFilter::new();
        ctrl_alt(&mut f);
        f.feed(KEY_F12, true, true, OS_DOWN);
        f.feed(KEY_LEFTCTRL, false, true, OS_DOWN);
        assert_eq!(f.feed(KEY_F12, true, true, OS_DOWN), KeyRoute::ConsumedToOs);
        assert_eq!(f.feed(KEY_F12, false, true, OS_DOWN), KeyRoute::ConsumedToOs);
    }

    #[test]
    fn stale_modifiers_are_cleared_when_the_os_disagrees() {
        // Ctrl+Alt+Del: the key-ups happened on the secure desktop, where hooks see nothing.
        for key in [KEY_F11, KEY_F12, KEY_SCROLLLOCK, KEY_G] {
            let mut f = HotkeyFilter::new();
            ctrl_alt(&mut f);
            assert_eq!(f.feed(key, true, true, OS_UP), KeyRoute::Route);
            assert_eq!(f.feed(key, false, true, OS_UP), KeyRoute::Route);
            // Cleared for good: a later agreeing OS state does not revive them.
            assert_eq!(f.feed(key, true, true, OS_DOWN), KeyRoute::Route);
        }
    }

    #[test]
    fn only_the_disagreeing_modifier_is_cleared() {
        let mut f = HotkeyFilter::new();
        ctrl_alt(&mut f);
        let ctrl_only = OsModifiers { ctrl: true, alt: false };
        assert_eq!(f.feed(KEY_G, true, true, ctrl_only), KeyRoute::Route);
        f.feed(KEY_G, false, true, ctrl_only);
        // Ctrl survived, so a fresh Alt completes the chord.
        f.feed(KEY_LEFTALT, true, true, OS_DOWN);
        assert_eq!(f.feed(KEY_G, true, true, OS_DOWN), KeyRoute::Hotkey(Hotkey::GameLock));
    }

    #[test]
    fn swallowed_modifiers_need_no_os_confirmation() {
        // While forwarding the hook swallows Ctrl and Alt, so the OS never records them as
        // down. This is the panic that gets the user out, so it must still fire.
        for (key, hotkey) in [
            (KEY_F12, Hotkey::Panic),
            (KEY_SCROLLLOCK, Hotkey::Panic),
            (KEY_F11, Hotkey::ManualSwitch),
            (KEY_G, Hotkey::GameLock),
        ] {
            let mut f = HotkeyFilter::new();
            ctrl_alt(&mut f);
            f.note_swallowed(KEY_LEFTCTRL);
            f.note_swallowed(KEY_LEFTALT);
            assert_eq!(f.feed(key, true, true, OS_UP), KeyRoute::Hotkey(hotkey));
        }
    }

    #[test]
    fn swallowed_state_survives_auto_repeat_and_ends_on_release() {
        let mut f = HotkeyFilter::new();
        ctrl_alt(&mut f);
        f.note_swallowed(KEY_LEFTCTRL);
        f.note_swallowed(KEY_LEFTALT);
        f.feed(KEY_LEFTCTRL, true, true, OS_UP);
        assert_eq!(f.feed(KEY_F12, true, true, OS_UP), KeyRoute::Hotkey(Hotkey::Panic));
        f.feed(KEY_F12, false, true, OS_UP);
        f.feed(KEY_LEFTCTRL, false, true, OS_UP);
        f.feed(KEY_LEFTCTRL, true, true, OS_UP);
        assert_eq!(f.feed(KEY_F12, true, true, OS_UP), KeyRoute::Route);
    }

    #[test]
    fn noting_a_key_that_is_up_changes_nothing() {
        let mut f = HotkeyFilter::new();
        f.note_swallowed(KEY_LEFTCTRL);
        f.note_swallowed(KEY_A);
        f.feed(KEY_LEFTALT, true, true, OS_UP);
        assert_eq!(f.feed(KEY_F12, true, true, OS_UP), KeyRoute::Route);
    }

    #[test]
    fn a_poisoned_router_sends_hooks_straight_to_the_os() {
        assert_eq!(hook_gate(false), HookGate::Route);
        assert_eq!(hook_gate(true), HookGate::PassToOs);
    }

    #[test]
    fn a_poisoned_lock_gives_no_guard() {
        let lock = std::sync::Arc::new(std::sync::Mutex::new(1));
        assert_eq!(lock_live(&lock).as_deref(), Some(&1));
        let held = lock.clone();
        let _ = std::thread::Builder::new()
            .spawn(move || {
                let _guard = held.lock().unwrap();
                std::panic::resume_unwind(Box::new("poison"));
            })
            .unwrap()
            .join();
        assert!(lock.is_poisoned());
        assert!(lock_live(&lock).is_none());
    }

    #[test]
    fn auto_repeat_of_a_key_held_first_never_fires_a_hotkey() {
        // G was forwarded before Ctrl+Alt went down. Firing here would lose G's release
        // and leave it stuck on the peer.
        let mut f = HotkeyFilter::new();
        assert_eq!(f.feed(KEY_G, true, true, OS_DOWN), KeyRoute::Route);
        ctrl_alt(&mut f);
        assert_eq!(f.feed(KEY_G, true, true, OS_DOWN), KeyRoute::Route);
        assert_eq!(f.feed(KEY_G, false, true, OS_DOWN), KeyRoute::Route);
    }
}
