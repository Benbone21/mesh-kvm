//! Platform input backend layer: traits the OS code implements, and `apply`, which turns
//! router `Action`s into backend calls and peer packets.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

use crate::protocol::{NodeId, Payload};
use crate::router::Action;

/// Times focus and panic packets are sent, because the UDP link is lossy.
const REPEAT_CRITICAL: usize = 3;

#[allow(dead_code)]
pub trait InputBackend {
    fn set_blocking(&mut self, mouse: bool, keyboard: bool);
    fn inject_move(&mut self, dx: i32, dy: i32);
    fn inject_button(&mut self, button: u8, pressed: bool);
    fn inject_wheel(&mut self, dx: i32, dy: i32);
    fn inject_key(&mut self, scancode: u16, pressed: bool);
    fn warp_cursor(&mut self, x: i32, y: i32);
    fn release_held(&mut self);
}

#[allow(dead_code)]
pub trait PeerSender {
    fn send(&mut self, payload: Payload);
}

/// Runs router actions against the local backend and the peer link. `x`/`y` on a focus
/// switch pass through unchanged, including `router::KEEP_CURSOR`.
#[allow(dead_code)]
pub fn apply(actions: &[Action], backend: &mut dyn InputBackend, peer: &mut dyn PeerSender) {
    for action in actions {
        match *action {
            Action::ForwardMove { dx, dy } => peer.send(Payload::MouseMoveDelta { dx, dy }),
            Action::ForwardButton { button, pressed } => {
                peer.send(Payload::MouseButton { button, pressed })
            }
            Action::ForwardWheel { dx, dy } => peer.send(Payload::MouseWheel {
                delta_x: dx,
                delta_y: dy,
            }),
            Action::ForwardKey { scancode, pressed } => {
                peer.send(Payload::KeyEvent { scancode, pressed })
            }
            Action::InjectMove { dx, dy } => backend.inject_move(dx, dy),
            Action::InjectButton { button, pressed } => backend.inject_button(button, pressed),
            Action::InjectWheel { dx, dy } => backend.inject_wheel(dx, dy),
            Action::InjectKey { scancode, pressed } => backend.inject_key(scancode, pressed),
            Action::SendFocus { target, x, y } => {
                // An unknown device id has no wire representation, so nothing is sent.
                if let Ok(target_node) = NodeId::try_from(target) {
                    for _ in 0..REPEAT_CRITICAL {
                        peer.send(Payload::FocusSwitchAt { target_node, x, y });
                    }
                }
            }
            Action::SendPanic => {
                for _ in 0..REPEAT_CRITICAL {
                    peer.send(Payload::PanicRelease);
                }
            }
            Action::WarpCursor { x, y } => backend.warp_cursor(x, y),
            Action::ReleaseHeld => backend.release_held(),
            Action::SetBlocking { mouse, keyboard } => backend.set_blocking(mouse, keyboard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::KEEP_CURSOR;

    #[derive(Default)]
    struct FakeBackend(Vec<String>);
    impl InputBackend for FakeBackend {
        fn set_blocking(&mut self, mouse: bool, keyboard: bool) {
            self.0.push(format!("block {mouse} {keyboard}"));
        }
        fn inject_move(&mut self, dx: i32, dy: i32) {
            self.0.push(format!("move {dx} {dy}"));
        }
        fn inject_button(&mut self, button: u8, pressed: bool) {
            self.0.push(format!("button {button} {pressed}"));
        }
        fn inject_wheel(&mut self, dx: i32, dy: i32) {
            self.0.push(format!("wheel {dx} {dy}"));
        }
        fn inject_key(&mut self, scancode: u16, pressed: bool) {
            self.0.push(format!("key {scancode} {pressed}"));
        }
        fn warp_cursor(&mut self, x: i32, y: i32) {
            self.0.push(format!("warp {x} {y}"));
        }
        fn release_held(&mut self) {
            self.0.push("release".into());
        }
    }

    #[derive(Default)]
    struct FakePeer(Vec<Payload>);
    impl PeerSender for FakePeer {
        fn send(&mut self, payload: Payload) {
            self.0.push(payload);
        }
    }

    #[test]
    fn apply_dispatches_every_action() {
        let actions = [
            Action::ForwardMove { dx: 1, dy: -2 },
            Action::ForwardButton { button: 3, pressed: true },
            Action::ForwardWheel { dx: 4, dy: 5 },
            Action::ForwardKey { scancode: 30, pressed: false },
            Action::InjectMove { dx: 6, dy: 7 },
            Action::InjectButton { button: 1, pressed: false },
            Action::InjectWheel { dx: 8, dy: 9 },
            Action::InjectKey { scancode: 31, pressed: true },
            Action::SendFocus { target: 1, x: 10, y: KEEP_CURSOR },
            Action::SendPanic,
            Action::WarpCursor { x: 11, y: 12 },
            Action::ReleaseHeld,
            Action::SetBlocking { mouse: true, keyboard: false },
        ];
        let mut b = FakeBackend::default();
        let mut p = FakePeer::default();
        apply(&actions, &mut b, &mut p);

        assert_eq!(
            b.0,
            vec![
                "move 6 7",
                "button 1 false",
                "wheel 8 9",
                "key 31 true",
                "warp 11 12",
                "release",
                "block true false",
            ]
        );

        let focus = Payload::FocusSwitchAt { target_node: NodeId::Windows, x: 10, y: KEEP_CURSOR };
        // Payload has no PartialEq, so compare Debug output.
        let mut want = vec![
            Payload::MouseMoveDelta { dx: 1, dy: -2 },
            Payload::MouseButton { button: 3, pressed: true },
            Payload::MouseWheel { delta_x: 4, delta_y: 5 },
            Payload::KeyEvent { scancode: 30, pressed: false },
        ];
        want.extend(std::iter::repeat_with(|| focus.clone()).take(3));
        want.extend(std::iter::repeat_with(|| Payload::PanicRelease).take(3));
        assert_eq!(format!("{:?}", p.0), format!("{:?}", want));
    }
}
