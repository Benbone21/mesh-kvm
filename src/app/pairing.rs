//! The pairing window: choose, show a code, or type one. It draws what the service last said and
//! says what the person asked for. Talking to the service is the caller's job.

use super::theme::legacy::{DEEP, EDGE, AMBER, RED};
use super::widgets::small;
use crate::secure::pairing::CODE_SECONDS;
use eframe::egui::{self, vec2, Align2, Frame, Margin, RichText, Sense, Stroke, Ui};
use serde::Deserialize;

const WIDTH: f32 = 380.0;
const CODE_DIGITS: usize = 6;
const CODE_SIZE: f32 = 40.0;
/// How the service words a code that didn't match. Other failures are shown as it words them.
const WRONG_CODE: &str = "The code was wrong.";

/// `GET /api/pairing`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PairingStatus {
    /// "idle", "showing", "entering", "paired" or "failed".
    pub state: String,
    pub code: Option<String>,
    pub seconds_left: Option<u64>,
    pub error: Option<String>,
    pub peer_name: Option<String>,
    pub fingerprint: Option<String>,
}

pub struct PairingView<'a> {
    /// None until the service has answered once.
    pub status: Option<&'a PairingStatus>,
    /// The other device's name, for the middle of a sentence.
    pub other: &'a str,
    pub this_device: &'a str,
    pub other_address: Option<&'a str>,
    /// An action is with the service and hasn't been answered yet.
    pub working: bool,
}

/// What the person has typed or chosen in the window.
#[derive(Default)]
pub struct PairingInput {
    /// They chose to type a code. The service has nothing to say about that until they send it.
    pub entering: bool,
    pub code: String,
    /// Why the service wouldn't start what was asked, worded for the person.
    pub refusal: Option<String>,
    focused: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairingRequest {
    Show,
    Enter(String),
    /// Stop whatever is going on, and close the window.
    Cancel,
}

impl PairingInput {
    /// Keeps the digits, to six, so a code can be typed or pasted with or without its space.
    pub fn clean(&mut self) {
        self.code = self.code.chars().filter(char::is_ascii_digit).take(CODE_DIGITS).collect();
    }

    pub fn ready(&self) -> bool {
        self.code.len() == CODE_DIGITS
    }
}

enum Screen<'a> {
    Choice,
    Showing { code: &'a str, left: u64 },
    Entering,
    /// The other device didn't take the code.
    WrongCode,
    Failed(&'a str),
    Paired,
}

fn screen<'a>(view: &PairingView<'a>, input: &PairingInput) -> Screen<'a> {
    let state = view.status.map_or("", |s| s.state.as_str());
    let error = view.status.and_then(|s| s.error.as_deref()).unwrap_or("This didn't work.");
    match (state, view.status.and_then(|s| s.code.as_deref())) {
        ("paired", _) => Screen::Paired,
        _ if view.status.is_some_and(|s| s.peer_name.is_some()) => Screen::Paired,
        ("failed", _) if error.starts_with(WRONG_CODE) => Screen::WrongCode,
        ("failed", _) => Screen::Failed(error),
        ("showing", Some(code)) => Screen::Showing { code, left: view.status.and_then(|s| s.seconds_left).unwrap_or(0) },
        ("entering", _) => Screen::Entering,
        _ if input.entering => Screen::Entering,
        _ => Screen::Choice,
    }
}

/// 482917 as 482 917.
fn grouped(code: &str) -> String {
    let mid = code.char_indices().nth(3).map_or(code.len(), |(i, _)| i);
    let (head, tail) = code.split_at(mid);
    if tail.is_empty() { head.to_string() } else { format!("{head} {tail}") }
}

/// 94 seconds as 1:34.
fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Draws the window. `None` when the person asked for nothing this frame.
pub fn window(ctx: &egui::Context, view: &PairingView, input: &mut PairingInput) -> Option<PairingRequest> {
    let mut request = None;
    let mut open = true;
    let frame = Frame::popup(&ctx.style()).inner_margin(Margin::same(18));
    egui::Window::new("Pair with your other device")
        .id(egui::Id::new("pairing window"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .frame(frame)
        .min_width(WIDTH)
        .max_width(WIDTH)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            request = match screen(view, input) {
                Screen::Choice => choice(ui, view, input),
                Screen::Showing { code, left } => showing(ui, view, code, left),
                Screen::Entering => entering(ui, view, input),
                Screen::WrongCode => {
                    let text = format!("That code didn't match, so it can't be used again. Show a new code on {} and try once more.", view.other);
                    ui.label(RichText::new(text).color(RED));
                    close_button(ui, "Close")
                }
                Screen::Failed(error) => {
                    ui.label(RichText::new(error).color(RED));
                    close_button(ui, "Close")
                }
                Screen::Paired => {
                    ui.label(format!("Paired with {}", view.status.and_then(|s| s.peer_name.as_deref()).unwrap_or(view.other)));
                    ui.label(small("Everything between these devices is encrypted."));
                    close_button(ui, "Done")
                }
            };
        });
    if !open {
        request = Some(PairingRequest::Cancel);
    }
    request
}

fn close_button(ui: &mut Ui, text: &str) -> Option<PairingRequest> {
    ui.add_space(6.0);
    ui.button(text).clicked().then_some(PairingRequest::Cancel)
}

fn working_line(ui: &mut Ui, view: &PairingView) {
    if view.working {
        ui.label(small("Still working on it."));
    }
}

fn refusal_line(ui: &mut Ui, input: &PairingInput) {
    if let Some(why) = &input.refusal {
        ui.label(RichText::new(why).color(RED));
    }
}

fn choice(ui: &mut Ui, view: &PairingView, input: &mut PairingInput) -> Option<PairingRequest> {
    let mut request = None;
    ui.label("Pairing lets these two devices trust each other. You only do this once, and it needs mesh-kvm open on both.");
    if let Some(address) = view.other_address {
        ui.label(small(format!("Other device's address: {address}")));
    }
    working_line(ui, view);
    refusal_line(ui, input);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!view.working, |ui| {
            let text = RichText::new("Show a code here").color(DEEP);
            if ui.add(egui::Button::new(text).fill(AMBER).stroke(Stroke::NONE)).clicked() {
                request = Some(PairingRequest::Show);
            }
            if ui.button("Enter a code here").clicked() {
                input.entering = true;
            }
        });
    });
    request
}

fn showing(ui: &mut Ui, view: &PairingView, code: &str, left: u64) -> Option<PairingRequest> {
    ui.label(format!("On {}, open mesh-kvm, choose Enter a code here, and type:", view.other));
    ui.add_space(4.0);
    ui.label(RichText::new(grouped(code)).size(CODE_SIZE).monospace().strong());
    let share = left as f32 / CODE_SECONDS as f32;
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter().rect_filled(bar, 2.0, EDGE);
    let rest = egui::Rect::from_min_size(bar.min, vec2(bar.width() * share.clamp(0.0, 1.0), bar.height()));
    ui.painter().rect_filled(rest, 2.0, AMBER);
    ui.add_space(4.0);
    ui.label(small(format!("Good for {} more, and for one try. Waiting for {}.", clock(left), view.other)));
    close_button(ui, "Cancel")
}

fn entering(ui: &mut Ui, view: &PairingView, input: &mut PairingInput) -> Option<PairingRequest> {
    let mut request = None;
    ui.label(format!("Type the code {} is showing.", view.other));
    let edit = egui::TextEdit::singleline(&mut input.code)
        .hint_text("000000")
        .font(egui::FontId::monospace(24.0))
        .desired_width(180.0);
    let field = ui.add_enabled(!view.working, edit);
    if !input.focused {
        input.focused = true;
        field.request_focus();
    }
    input.clean();
    let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.label(small(format!(
        "You're typing this on {}'s own keyboard, as the devices can't share one until they're paired.",
        view.this_device
    )));
    working_line(ui, view);
    refusal_line(ui, input);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            request = Some(PairingRequest::Cancel);
        }
        let ready = input.ready() && !view.working;
        let text = RichText::new("Pair").color(DEEP);
        let pair = ui.add_enabled(ready, egui::Button::new(text).fill(AMBER).stroke(Stroke::NONE));
        if ready && (pair.clicked() || submitted) {
            request = Some(PairingRequest::Enter(input.code.clone()));
        }
    });
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::theme;
    use eframe::egui::{self, Pos2, RawInput, Rect};

    fn status(json: &str) -> PairingStatus {
        serde_json::from_str(json).unwrap()
    }

    fn draw(view: &PairingView, input: &mut PairingInput, size: [f32; 2]) -> (String, Option<PairingRequest>, Option<Rect>) {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut last = (String::new(), None, None);
        for _ in 0..3 {
            let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size.into())), ..Default::default() };
            let mut request = None;
            let output = ctx.run(raw, |ctx| request = window(ctx, view, input));
            let mut text = String::new();
            for clipped in &output.shapes {
                if let egui::Shape::Text(shape) = &clipped.shape {
                    text.push_str(shape.galley.text());
                    text.push('\n');
                }
            }
            last = (text, request, ctx.memory(|m| m.area_rect(egui::Id::new("pairing window"))));
        }
        last
    }

    fn view(status: Option<&PairingStatus>) -> PairingView<'_> {
        PairingView { status, other: "Laptop", this_device: "Desk", other_address: Some("192.168.8.210"), working: false }
    }

    #[test]
    fn an_unpaired_device_offers_both_choices() {
        let (text, request, _) = draw(&view(None), &mut PairingInput::default(), [800.0, 600.0]);
        for line in [
            "Pair with your other device",
            "Pairing lets these two devices trust each other. You only do this once, and it needs mesh-kvm open on both.",
            "Other device's address: 192.168.8.210",
            "Show a code here",
            "Enter a code here",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert_eq!(request, None);
        let mut unknown = view(None);
        unknown.other_address = None;
        let (text, ..) = draw(&unknown, &mut PairingInput::default(), [800.0, 600.0]);
        assert!(!text.contains("Other device's address"), "{text}");
    }

    #[test]
    fn showing_draws_the_code_and_time_left() {
        let showing = status(r#"{"state":"showing","code":"482917","seconds_left":94}"#);
        let (text, ..) = draw(&view(Some(&showing)), &mut PairingInput::default(), [800.0, 600.0]);
        for line in [
            "On Laptop, open mesh-kvm, choose Enter a code here, and type:",
            "482 917",
            "Good for 1:34 more, and for one try. Waiting for Laptop.",
            "Cancel",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn entering_explains_the_keyboard_and_waits_for_six_digits() {
        let mut input = PairingInput { entering: true, ..Default::default() };
        let (text, ..) = draw(&view(None), &mut input, [800.0, 600.0]);
        for line in [
            "Type the code Laptop is showing.",
            "You're typing this on Desk's own keyboard, as the devices can't share one until they're paired.",
            "Cancel",
            "Pair",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(!input.ready());
        input.code = "482 917".into();
        input.clean();
        assert!(input.ready());
    }

    #[test]
    fn a_wrong_code_says_it_cannot_be_used_again() {
        let failed = status(r#"{"state":"failed","error":"The code was wrong. Show a new code and try again."}"#);
        let (text, ..) = draw(&view(Some(&failed)), &mut PairingInput { entering: true, ..Default::default() }, [800.0, 600.0]);
        assert!(
            text.contains("That code didn't match, so it can't be used again. Show a new code on Laptop and try once more."),
            "{text}"
        );
        assert!(text.contains("Close"), "{text}");
        let other = status(r#"{"state":"failed","error":"The code ran out. Show a new one."}"#);
        let (text, ..) = draw(&view(Some(&other)), &mut PairingInput::default(), [800.0, 600.0]);
        assert!(text.contains("The code ran out. Show a new one."), "{text}");
        assert!(!text.contains("didn't match"), "{text}");
    }

    #[test]
    fn a_pairing_that_is_working_says_so() {
        let mut busy = view(None);
        busy.working = true;
        let (text, ..) = draw(&busy, &mut PairingInput::default(), [800.0, 600.0]);
        assert!(text.contains("Still working on it."), "{text}");
    }

    #[test]
    fn the_window_draws_at_the_smallest_size() {
        let showing = status(r#"{"state":"showing","code":"482917","seconds_left":94}"#);
        let failed = status(r#"{"state":"failed","error":"The code was wrong. Show a new code and try again."}"#);
        let paired = status(r#"{"state":"paired","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#);
        let size = [760.0, 560.0];
        for status in [None, Some(&showing), Some(&failed), Some(&paired)] {
            for entering in [false, true] {
                let mut input = PairingInput { entering, ..Default::default() };
                let (_, _, area) = draw(&view(status), &mut input, size);
                let area = area.expect("the window is up");
                assert!(Rect::from_min_size(Pos2::ZERO, size.into()).contains_rect(area), "{area:?}");
            }
        }
    }

    #[test]
    fn the_code_box_takes_digits_only_with_or_without_a_space() {
        let cleaned = |typed: &str| {
            let mut input = PairingInput { code: typed.into(), ..Default::default() };
            input.clean();
            input.code
        };
        assert_eq!(cleaned("482917"), "482917");
        assert_eq!(cleaned("482 917"), "482917");
        assert_eq!(cleaned("48a2-9 17x"), "482917");
        assert_eq!(cleaned("4829179999"), "482917");
        assert_eq!(cleaned("\u{664}\u{665}12"), "12", "only ASCII digits");
        assert_eq!(cleaned(""), "");
        let mut input = PairingInput { code: "48291".into(), ..Default::default() };
        assert!(!input.ready());
        input.code.push('7');
        assert!(input.ready());
    }

    #[test]
    fn the_time_is_shown_as_minutes_and_seconds() {
        assert_eq!(clock(94), "1:34");
        assert_eq!(clock(120), "2:00");
        assert_eq!(clock(9), "0:09");
        assert_eq!(clock(0), "0:00");
    }

    #[test]
    fn a_code_is_shown_in_two_groups() {
        assert_eq!(grouped("482917"), "482 917");
        assert_eq!(grouped("12"), "12");
    }
}
