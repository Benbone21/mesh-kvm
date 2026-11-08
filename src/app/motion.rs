//! What moves, and how long it takes: the dialog fade, the status square's cross-fade and the
//! pairing countdown. Each is a plain function of elapsed time, and all of them stand still when
//! the system asks for reduced motion. Announcements for a screen reader live here too.

use crate::secure::pairing::CODE_SECONDS;
use eframe::egui::{Color32, Id, Ui};
use std::time::Duration;

/// How long a dialog takes to fade in.
pub const DIALOG_FADE: f32 = 0.12;
/// How long the status square takes to change colour.
pub const SQUARE_FADE: f32 = 0.2;

/// Whether the system asks for no animation: Windows' client area animation setting, or KDE's
/// animation speed set to zero. If neither says so, motion stays on.
pub fn reduced_motion() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
        let mut on: i32 = 1;
        // SAFETY: the call writes one BOOL to `on`, which is one.
        let read = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
        read != 0 && on == 0
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .map(|home| std::path::Path::new(&home).join(".config/kdeglobals"))
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|text| kde_reduced(&text))
    }
}

/// Whether the `[KDE]` group of a `kdeglobals` file sets `AnimationDurationFactor` to zero.
#[cfg(any(not(target_os = "windows"), test))]
pub fn kde_reduced(kdeglobals: &str) -> bool {
    let mut in_kde = false;
    for line in kdeglobals.lines().map(str::trim) {
        if line.starts_with('[') {
            in_kde = line == "[KDE]";
        } else if in_kde {
            if let Some(("AnimationDurationFactor", value)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                return value.parse::<f32>().is_ok_and(|factor| factor == 0.0);
            }
        }
    }
    false
}

/// How far a dialog has faded in, from 0 to 1, easing out. Whole at once when `reduced`.
pub fn fade_in(elapsed: f32, reduced: bool) -> f32 {
    if reduced {
        return 1.0;
    }
    let left = 1.0 - (elapsed / DIALOG_FADE).clamp(0.0, 1.0);
    1.0 - left * left
}

/// The colour `elapsed` seconds into a change from `from` to `to`, easing in and out. The new
/// colour at once when `reduced`.
pub fn cross_fade(from: Color32, to: Color32, elapsed: f32, reduced: bool) -> Color32 {
    let t = (elapsed / SQUARE_FADE).clamp(0.0, 1.0);
    if reduced || t >= 1.0 {
        return to;
    }
    if t <= 0.0 {
        return from;
    }
    from.lerp_to_gamma(to, t * t * (3.0 - 2.0 * t))
}

/// The share of a code's time still to go. It runs down smoothly between polls, which say the
/// whole seconds left, or a second at a time when `reduced`.
pub fn countdown_share(seconds_left: u64, since_poll: Duration, reduced: bool) -> f32 {
    let left = if reduced { seconds_left as f32 } else { (seconds_left as f32 - since_poll.as_secs_f32()).max(0.0) };
    (left / CODE_SECONDS as f32).clamp(0.0, 1.0)
}

/// What a screen reader is told when `seconds_left` is reached.
pub fn countdown_announcement(seconds_left: u64) -> Option<&'static str> {
    match seconds_left {
        60 => Some("1 minute left"),
        30 => Some("30 seconds left"),
        10 => Some("10 seconds left"),
        _ => None,
    }
}

/// Has a screen reader say `text` politely, once, when it changes. Nothing is read for an empty
/// `text`. Does nothing when no screen reader is listening.
pub fn announce(ui: &Ui, salt: impl std::hash::Hash, text: &str) {
    if text.is_empty() {
        return;
    }
    let id = ui.id().with(("announcement", salt));
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_role(eframe::egui::accesskit::Role::Label);
        node.set_live(eframe::egui::accesskit::Live::Polite);
        node.set_label(text);
    });
}

/// Makes `id`'s node a polite live region, so what it says is read out when it changes.
pub fn live(ctx: &eframe::egui::Context, id: Id) {
    ctx.accesskit_node_builder(id, |node| node.set_live(eframe::egui::accesskit::Live::Polite));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::theme::{ACCENT, WARN};

    #[test]
    fn reduced_motion_drops_the_fades_and_steps_the_countdown() {
        assert_eq!(fade_in(0.0, true), 1.0);
        assert_eq!(fade_in(0.0, false), 0.0);
        assert_eq!(fade_in(DIALOG_FADE, false), 1.0);
        assert_eq!(cross_fade(WARN, ACCENT, 0.0, true), ACCENT);
        assert_eq!(cross_fade(WARN, ACCENT, SQUARE_FADE, false), ACCENT);
        assert_eq!(cross_fade(WARN, ACCENT, 0.0, false), WARN);
        assert_eq!(countdown_share(94, Duration::from_millis(500), true), 94.0 / 120.0);
        assert!((countdown_share(94, Duration::from_millis(500), false) - 93.5 / 120.0).abs() < 1e-4);
        assert_eq!(countdown_share(0, Duration::from_secs(3), false), 0.0);
    }

    #[test]
    fn the_countdown_is_announced_three_times() {
        let said: Vec<_> = (0..=120).rev().filter_map(countdown_announcement).collect();
        assert_eq!(said, ["1 minute left", "30 seconds left", "10 seconds left"]);
    }

    #[test]
    fn kde_turns_animations_off_with_a_zero_factor() {
        assert!(kde_reduced("[KDE]\nAnimationDurationFactor=0\n"));
        assert!(!kde_reduced("[KDE]\nAnimationDurationFactor=0.5\n"));
        assert!(!kde_reduced("[General]\nAnimationDurationFactor=0\n"));
        assert!(!kde_reduced(""));
    }

    #[test]
    fn a_fade_runs_between_its_ends() {
        let mid = fade_in(DIALOG_FADE / 2.0, false);
        assert!(mid > 0.5 && mid < 1.0, "{mid}");
        let colour = cross_fade(WARN, ACCENT, SQUARE_FADE / 2.0, false);
        assert!(colour != WARN && colour != ACCENT);
    }
}
