//! The pairing dialog: choose, show a code, or type one. It draws what the service last said and
//! says what the person asked for. Talking to the service is the caller's job.

use super::dialog;
use super::theme::{line, Type, ACCENT, BG, COUNTDOWN_HEIGHT, DANGER, TEXT, TEXT_DIM};
use super::widgets::ButtonKind;
use crate::secure::pairing::CODE_SECONDS;
use eframe::egui::{
    self, text::CCursor, text::CCursorRange, vec2, Align, Event, FontId, Frame, Galley, Id, Layout, Margin, Rect, Sense,
    Ui, WidgetInfo, WidgetType,
};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const CODE_DIGITS: usize = 6;
/// Six digits and the space between the two groups.
const CODE_CHARS: usize = CODE_DIGITS + 1;
/// The padding above and below the code, shown or typed.
const CODE_SHOW_PADDING: i8 = 18;
const CODE_ENTER_PADDING: i8 = 14;
/// How the service words a code that didn't match. Other failures are shown as it words them.
const WRONG_CODE: &str = "The code was wrong.";

/// What the entering side says when its code didn't match `other`'s.
pub fn wrong_code_text(other: &str) -> String {
    format!("A wrong code can't be used again. Show a new one on {other} and try once more.")
}

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
    pub other_address: Option<&'a str>,
    /// An action is with the service and hasn't been answered yet.
    pub working: bool,
    /// How long ago the status's `seconds_left` was counted.
    pub left_since_poll: Duration,
}

/// What the person has typed or chosen in the window.
#[derive(Default)]
pub struct PairingInput {
    /// They chose to type a code. The service has nothing to say about that until they send it.
    pub entering: bool,
    pub code: String,
    /// Why the service wouldn't start what was asked, worded for the person.
    pub refusal: Option<String>,
    /// Give the code box the keyboard again, as after a failure.
    pub refocus: bool,
    focused: bool,
    /// The failure on show has cleared the code box already.
    failure_seen: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairingRequest {
    Show,
    Enter(String),
    /// Stop whatever is going on, and close the window.
    Cancel,
}

impl PairingInput {
    pub fn ready(&self) -> bool {
        code_digits(&self.code).len() == CODE_DIGITS
    }
}

/// What the code box shows for `typed`: its ASCII digits, to six, with a space once there is a
/// fourth. A space after three would put itself back each time Backspace took it away.
pub fn code_shown(typed: &str) -> String {
    let digits: String = typed.chars().filter(char::is_ascii_digit).take(CODE_DIGITS).collect();
    match digits.split_at_checked(3) {
        Some((head, tail)) if !tail.is_empty() => format!("{head} {tail}"),
        _ => digits,
    }
}

/// The digits of a code as the box shows it.
pub fn code_digits(shown: &str) -> String {
    shown.chars().filter(char::is_ascii_digit).collect()
}

/// A code as a screen reader should say it: one digit at a time.
pub fn spoken_code(code: &str) -> String {
    let digits: Vec<String> = code_digits(code).chars().map(String::from).collect();
    digits.join(" ")
}

enum Screen<'a> {
    Choice,
    Showing { code: &'a str, left: f32 },
    /// Typing a code, with what went wrong with the last one.
    Entering { failure: Option<String> },
    /// The showing side, as the service words what went wrong.
    Failed(&'a str),
    Paired,
}

fn screen<'a>(view: &PairingView<'a>, input: &PairingInput) -> Screen<'a> {
    let state = view.status.map_or("", |s| s.state.as_str());
    let error = view.status.and_then(|s| s.error.as_deref()).unwrap_or("This didn't work.");
    match (state, view.status.and_then(|s| s.code.as_deref())) {
        ("paired", _) => Screen::Paired,
        _ if view.status.is_some_and(|s| s.peer_name.is_some()) => Screen::Paired,
        ("failed", _) if input.entering => {
            let failure = if error.starts_with(WRONG_CODE) { wrong_code_text(view.other) } else { error.to_string() };
            Screen::Entering { failure: Some(failure) }
        }
        ("failed", _) => Screen::Failed(error),
        ("showing", Some(code)) => {
            let polled = view.status.and_then(|s| s.seconds_left).unwrap_or(0) as f32;
            Screen::Showing { code, left: (polled - view.left_since_poll.as_secs_f32()).max(0.0) }
        }
        ("entering", _) => Screen::Entering { failure: None },
        _ if input.entering => Screen::Entering { failure: None },
        _ => Screen::Choice,
    }
}

/// 94 seconds as 1:34.
fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Draws the dialog. `None` when the person asked for nothing this frame.
pub fn window(ctx: &egui::Context, view: &PairingView, input: &mut PairingInput, opacity: f32) -> Option<PairingRequest> {
    let screen = screen(view, input);
    match &screen {
        Screen::Entering { failure: Some(_) } if !input.failure_seen => {
            input.failure_seen = true;
            input.code.clear();
            input.refocus = true;
        }
        Screen::Entering { failure: Some(_) } => {}
        _ => input.failure_seen = false,
    }
    let title = format!("Pair with {}", view.other);
    let shown = dialog::show(ctx, Id::new("pairing window"), &title, opacity, |ui| match screen {
        Screen::Choice => choice(ui, view, input),
        Screen::Showing { code, left } => showing(ui, view, code, left),
        Screen::Entering { failure } => entering(ui, view, input, failure.as_deref()),
        Screen::Failed(error) => {
            dialog::text(ui, error, Type::Body, DANGER);
            working_line(ui, view);
            refusal_line(ui, input);
            let mut request = None;
            dialog::buttons(ui, |ui| {
                request = cancel(ui, true);
                if dialog::button(ui, "Show a new code", ButtonKind::Primary, !view.working).clicked() {
                    request = Some(PairingRequest::Show);
                }
            });
            request
        }
        Screen::Paired => {
            let name = view.status.and_then(|s| s.peer_name.as_deref()).unwrap_or(view.other);
            dialog::text(ui, &format!("Paired with {name}"), Type::Body, TEXT);
            dialog::text(ui, "Everything between these devices is encrypted.", Type::Caption, TEXT_DIM);
            None
        }
    });
    if shown.escaped {
        return Some(PairingRequest::Cancel);
    }
    shown.inner
}

/// The Cancel button, which has the keyboard when the dialog opens with nothing to type in.
fn cancel(ui: &mut Ui, nothing_to_type: bool) -> Option<PairingRequest> {
    let button = dialog::button(ui, "Cancel", ButtonKind::Secondary, true);
    if nothing_to_type {
        dialog::focus_on_open(ui, &button);
    }
    button.clicked().then_some(PairingRequest::Cancel)
}

fn working_line(ui: &mut Ui, view: &PairingView) {
    if view.working {
        dialog::text(ui, "Still working on it.", Type::Caption, TEXT_DIM);
    }
}

fn refusal_line(ui: &mut Ui, input: &PairingInput) {
    if let Some(why) = &input.refusal {
        dialog::text(ui, why, Type::Body, DANGER);
    }
}

fn choice(ui: &mut Ui, view: &PairingView, input: &mut PairingInput) -> Option<PairingRequest> {
    let mut request = None;
    let sentence = "Pairing lets these two devices trust each other. You only do this once, and it needs mesh-kvm open on both.";
    dialog::text(ui, sentence, Type::Body, TEXT);
    if let Some(address) = view.other_address {
        dialog::text(ui, &format!("Other device's address: {address}"), Type::Caption, TEXT_DIM);
    }
    working_line(ui, view);
    refusal_line(ui, input);
    dialog::buttons(ui, |ui| {
        request = cancel(ui, true);
        if dialog::button(ui, "Enter a code", ButtonKind::Secondary, !view.working).clicked() {
            input.entering = true;
        }
        if dialog::button(ui, "Show a code", ButtonKind::Primary, !view.working).clicked() {
            request = Some(PairingRequest::Show);
        }
    });
    request
}

fn showing(ui: &mut Ui, view: &PairingView, code: &str, left: f32) -> Option<PairingRequest> {
    let sentence = format!("Open mesh-kvm on {}, choose Enter a code, and type:", view.other);
    dialog::text(ui, &sentence, Type::Body, TEXT);
    let shown = code_shown(code);
    let display = Frame::new().fill(BG).inner_margin(Margin::symmetric(0, CODE_SHOW_PADDING)).show(ui, |ui| {
        ui.vertical_centered(|ui| ui.label(Type::CodeShow.job(&shown, ACCENT)));
    });
    display.response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, spoken_code(code)));
    // The bar sits right under the code, and the words right under the bar.
    let gap = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), COUNTDOWN_HEIGHT), Sense::hover());
    ui.painter().rect_filled(bar, 0.0, BG);
    let share = (left / CODE_SECONDS as f32).clamp(0.0, 1.0);
    ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * share, bar.height())), 0.0, ACCENT);
    ui.add_space(gap);
    ui.spacing_mut().item_spacing.y = gap;
    let row = FontId::monospace(12.0);
    let left = format!("{} LEFT, ONE TRY", clock(left.ceil() as u64));
    let waiting = format!("WAITING FOR {}", view.other.to_uppercase());
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(left).font(row.clone()).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(egui::RichText::new(waiting).font(row).color(TEXT_DIM));
        });
    });
    let mut request = None;
    dialog::buttons(ui, |ui| request = cancel(ui, true));
    request
}

fn entering(ui: &mut Ui, view: &PairingView, input: &mut PairingInput, failure: Option<&str>) -> Option<PairingRequest> {
    let mut request = None;
    let label = dialog::text(ui, &format!("Type the code {} is showing.", view.other), Type::Body, TEXT);
    let id = Id::new("pairing code");
    // A typed or pasted code is kept to its digits before the box takes it, so stray characters
    // never use up its seven places.
    if ui.memory(|m| m.has_focus(id)) {
        ui.input_mut(|i| {
            for event in &mut i.events {
                if let Event::Text(text) | Event::Paste(text) = event {
                    text.retain(|c| c.is_ascii_digit());
                }
            }
        });
    }
    let mut layouter = |ui: &Ui, text: &str, _wrap: f32| -> Arc<Galley> { ui.fonts(|f| f.layout_job(Type::CodeEnter.job(text, TEXT))) };
    let edit = egui::TextEdit::singleline(&mut input.code)
        .id(id)
        .frame(false)
        .char_limit(CODE_CHARS)
        .horizontal_align(Align::Center)
        .desired_width(f32::INFINITY)
        .layouter(&mut layouter);
    let boxed = Frame::new()
        .fill(BG)
        .stroke(line(1.0, ACCENT))
        .inner_margin(Margin::symmetric(0, CODE_ENTER_PADDING))
        .show(ui, |ui| ui.add_enabled(!view.working, edit));
    let field = boxed.inner.labelled_by(label.id);
    if !input.focused || input.refocus {
        input.focused = true;
        input.refocus = false;
        field.request_focus();
    }
    let shown = code_shown(&input.code);
    if shown != input.code {
        input.code = shown;
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            let end = CCursor::new(input.code.chars().count());
            state.cursor.set_char_range(Some(CCursorRange::one(end)));
            state.store(ui.ctx(), id);
        }
    }
    let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let hint = "Use this device's own keyboard. The devices can't share one until they're paired.";
    dialog::text(ui, hint, Type::Caption, TEXT_DIM);
    if let Some(failure) = failure {
        dialog::text(ui, failure, Type::Body, DANGER);
    }
    refusal_line(ui, input);
    dialog::buttons(ui, |ui| {
        request = cancel(ui, false);
        let ready = input.ready() && !view.working;
        let label = if view.working { "Pairing\u{2026}" } else { "Pair" };
        let pair = dialog::button(ui, label, ButtonKind::Primary, ready);
        if ready && (pair.clicked() || submitted) {
            request = Some(PairingRequest::Enter(code_digits(&input.code)));
        }
    });
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::theme;
    use eframe::egui::{self, Event, Key, PointerButton, Pos2, RawInput, Rect};

    fn status(json: &str) -> PairingStatus {
        serde_json::from_str(json).unwrap()
    }

    struct Drawn {
        text: String,
        texts: Vec<(String, Rect)>,
        request: Option<PairingRequest>,
        area: Option<Rect>,
    }

    fn one(ctx: &egui::Context, view: &PairingView, input: &mut PairingInput, size: [f32; 2], events: Vec<Event>) -> Drawn {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size.into())), events, ..Default::default() };
        let mut request = None;
        let output = ctx.run(raw, |ctx| request = window(ctx, view, input, 1.0));
        let (mut text, mut texts) = (String::new(), Vec::new());
        for clipped in &output.shapes {
            if let egui::Shape::Text(shape) = &clipped.shape {
                text.push_str(shape.galley.text());
                text.push('\n');
                texts.push((shape.galley.text().to_string(), shape.visual_bounding_rect()));
            }
        }
        Drawn { text, texts, request, area: ctx.memory(|m| m.area_rect(egui::Id::new("pairing window"))) }
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        ctx
    }

    fn settle(ctx: &egui::Context, view: &PairingView, input: &mut PairingInput, size: [f32; 2]) -> Drawn {
        for _ in 0..2 {
            one(ctx, view, input, size, vec![]);
        }
        one(ctx, view, input, size, vec![])
    }

    fn draw(view: &PairingView, input: &mut PairingInput) -> Drawn {
        settle(&context(), view, input, [1280.0, 760.0])
    }

    /// Clicks the button labelled `label` and returns what the window asked for.
    fn click(ctx: &egui::Context, view: &PairingView, input: &mut PairingInput, label: &str) -> Option<PairingRequest> {
        let drawn = settle(ctx, view, input, [1280.0, 760.0]);
        let at = drawn.texts.iter().find(|(t, _)| t == label).unwrap_or_else(|| panic!("{label}\n{}", drawn.text)).1.center();
        let press = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Default::default() };
        let mut asked = None;
        for events in [vec![Event::PointerMoved(at)], vec![press(true)], vec![press(false)]] {
            asked = asked.or(one(ctx, view, input, [1280.0, 760.0], events).request);
        }
        asked
    }

    fn view(status: Option<&PairingStatus>) -> PairingView<'_> {
        PairingView {
            status,
            other: "Laptop",
            other_address: Some("192.168.8.210"),
            working: false,
            left_since_poll: Duration::ZERO,
        }
    }

    fn has(text: &str, lines: &[&str]) {
        for line in lines {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn the_code_box_formats_typing_and_pasting() {
        assert_eq!(code_shown("482"), "482");
        assert_eq!(code_shown("4829"), "482 9");
        assert_eq!(code_shown("482917"), "482 917");
        assert_eq!(code_shown("482 917"), "482 917");
        assert_eq!(code_shown("48a2-9 17x"), "482 917");
        assert_eq!(code_shown("4829179999"), "482 917");
        assert_eq!(code_shown("\u{664}\u{665}12"), "12", "only ASCII digits");
        assert_eq!(code_shown(""), "");
        assert_eq!(code_digits("482 917"), "482917");
        assert!(code_shown("482 917").chars().count() <= 7);
        let mut input = PairingInput { code: code_shown("48291"), ..Default::default() };
        assert!(!input.ready());
        input.code = code_shown("482917");
        assert!(input.ready());
    }

    #[test]
    fn backspace_goes_back_through_the_space() {
        // "482 9" with its last character removed is "482 ", which shows as "482"; removing again gives "48".
        assert_eq!(code_shown("482 "), "482");
        assert_eq!(code_shown("48"), "48");
    }

    #[test]
    fn a_code_is_spoken_digit_by_digit() {
        assert_eq!(spoken_code("482917"), "4 8 2 9 1 7");
        assert_eq!(spoken_code("482 917"), "4 8 2 9 1 7");
    }

    #[test]
    fn showing_draws_the_board() {
        let showing = status(r#"{"state":"showing","code":"482917","seconds_left":94}"#);
        let drawn = draw(&view(Some(&showing)), &mut PairingInput::default());
        has(
            &drawn.text,
            &["PAIR WITH LAPTOP", "Open mesh-kvm on Laptop, choose Enter a code, and type:", "482 917", "1:34 LEFT, ONE TRY", "WAITING FOR LAPTOP", "Cancel"],
        );
        assert!(!drawn.text.contains("\nPair\n"), "{}", drawn.text);
        // Between polls the time left keeps going down.
        let mut later = view(Some(&showing));
        later.left_since_poll = Duration::from_millis(4500);
        assert!(draw(&later, &mut PairingInput::default()).text.contains("1:30 LEFT, ONE TRY"));
    }

    #[test]
    fn entering_draws_the_board_and_pair_waits_for_six_digits() {
        let mut debian = view(None);
        debian.other = "Debian";
        let mut input = PairingInput { entering: true, ..Default::default() };
        let drawn = draw(&debian, &mut input);
        has(
            &drawn.text,
            &[
                "PAIR WITH DEBIAN",
                "Type the code Debian is showing.",
                "Use this device's own keyboard. The devices can't share one until they're paired.",
                "Cancel",
                "Pair",
            ],
        );
        let ctx = context();
        input.code = "48291".into();
        assert_eq!(click(&ctx, &debian, &mut input, "Pair"), None);
        input.code = "482917".into();
        assert_eq!(click(&ctx, &debian, &mut input, "Pair"), Some(PairingRequest::Enter("482917".into())));
    }

    #[test]
    fn the_code_box_has_the_keyboard_and_shows_what_is_typed_as_two_groups() {
        let ctx = context();
        let mut input = PairingInput { entering: true, ..Default::default() };
        let entering = view(None);
        settle(&ctx, &entering, &mut input, [1280.0, 760.0]);
        assert!(ctx.memory(|m| m.has_focus(egui::Id::new("pairing code"))));
        one(&ctx, &entering, &mut input, [1280.0, 760.0], vec![Event::Text("4829".into())]);
        assert_eq!(input.code, "482 9");
        one(&ctx, &entering, &mut input, [1280.0, 760.0], vec![Event::Paste("17x99".into())]);
        assert_eq!(input.code, "482 917");
        let enter = Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
        let asked = one(&ctx, &entering, &mut input, [1280.0, 760.0], vec![enter]).request;
        assert_eq!(asked, Some(PairingRequest::Enter("482917".into())));
    }

    #[test]
    fn a_wrong_code_is_said_in_the_dialog_and_the_box_is_cleared_for_another_go() {
        let failed = status(r#"{"state":"failed","error":"The code was wrong. Show a new code and try again."}"#);
        let mut input = PairingInput { entering: true, code: "482 917".into(), ..Default::default() };
        let drawn = draw(&view(Some(&failed)), &mut input);
        has(&drawn.text, &["A wrong code can't be used again. Show a new one on Laptop and try once more.", "Cancel", "Pair"]);
        assert_eq!(input.code, "");
        // A new code can be typed while the failure is still on show.
        input.code = "1".into();
        draw(&view(Some(&failed)), &mut input);
        assert_eq!(input.code, "1");
    }

    #[test]
    fn other_failures_show_the_services_own_sentence() {
        let ran_out = status(r#"{"state":"failed","error":"The code ran out. Show a new one."}"#);
        let mut input = PairingInput { entering: true, code: "482 917".into(), ..Default::default() };
        let drawn = draw(&view(Some(&ran_out)), &mut input);
        has(&drawn.text, &["The code ran out. Show a new one.", "Pair"]);
        assert!(!drawn.text.contains("can't be used again"), "{}", drawn.text);
        assert_eq!(input.code, "");
        // The showing side offers a new code.
        let ctx = context();
        let mut showing_side = PairingInput::default();
        let drawn = settle(&ctx, &view(Some(&ran_out)), &mut showing_side, [1280.0, 760.0]);
        has(&drawn.text, &["The code ran out. Show a new one.", "Cancel", "Show a new code"]);
        assert_eq!(click(&ctx, &view(Some(&ran_out)), &mut showing_side, "Show a new code"), Some(PairingRequest::Show));
    }

    #[test]
    fn pairing_says_so_and_cancel_still_works() {
        let ctx = context();
        let mut busy = view(None);
        busy.working = true;
        let mut input = PairingInput { entering: true, code: "482 917".into(), ..Default::default() };
        let drawn = settle(&ctx, &busy, &mut input, [1280.0, 760.0]);
        assert!(drawn.text.contains("Pairing\u{2026}"), "{}", drawn.text);
        assert_eq!(click(&ctx, &busy, &mut input, "Pairing\u{2026}"), None, "it is disabled");
        assert_eq!(click(&ctx, &busy, &mut input, "Cancel"), Some(PairingRequest::Cancel));
    }

    #[test]
    fn the_choice_offers_show_and_enter_a_code() {
        let ctx = context();
        let mut input = PairingInput::default();
        let drawn = settle(&ctx, &view(None), &mut input, [1280.0, 760.0]);
        has(
            &drawn.text,
            &[
                "PAIR WITH LAPTOP",
                "Pairing lets these two devices trust each other. You only do this once, and it needs mesh-kvm open on both.",
                "Other device's address: 192.168.8.210",
                "Cancel",
                "Enter a code",
                "Show a code",
            ],
        );
        assert!(!drawn.text.contains(" here"), "{}", drawn.text);
        assert_eq!(drawn.request, None);
        // With nothing to type in, Cancel has the keyboard.
        let enter = Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
        assert_eq!(one(&ctx, &view(None), &mut input, [1280.0, 760.0], vec![enter]).request, Some(PairingRequest::Cancel));
        let mut unknown = view(None);
        unknown.other_address = None;
        assert!(!draw(&unknown, &mut PairingInput::default()).text.contains("Other device's address"));
        assert_eq!(click(&ctx, &view(None), &mut input, "Show a code"), Some(PairingRequest::Show));
        assert_eq!(click(&ctx, &view(None), &mut input, "Enter a code"), None);
        assert!(input.entering);
    }

    #[test]
    fn paired_says_so_with_no_button() {
        let paired = status(r#"{"state":"paired","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#);
        let drawn = draw(&view(Some(&paired)), &mut PairingInput::default());
        has(&drawn.text, &["Paired with Laptop", "Everything between these devices is encrypted."]);
        for button in ["Done", "Cancel", "Close"] {
            assert!(!drawn.text.contains(button), "{button}\n{}", drawn.text);
        }
    }

    #[test]
    fn the_dialog_fits_the_smallest_window() {
        let showing = status(r#"{"state":"showing","code":"482917","seconds_left":94}"#);
        let wrong = status(r#"{"state":"failed","error":"The code was wrong. Show a new code and try again."}"#);
        let ran_out = status(r#"{"state":"failed","error":"The code ran out. Show a new one."}"#);
        let paired = status(r#"{"state":"paired","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#);
        let size = [760.0, 560.0];
        for status in [None, Some(&showing), Some(&wrong), Some(&ran_out), Some(&paired)] {
            for (entering, working) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut input = PairingInput { entering, refusal: Some("The code is 6 digits.".into()), ..Default::default() };
                let mut v = view(status);
                v.working = working;
                let area = settle(&context(), &v, &mut input, size).area.expect("the dialog is up");
                assert!(Rect::from_min_size(Pos2::ZERO, size.into()).contains_rect(area), "{area:?}");
            }
        }
    }

    #[test]
    fn escape_cancels() {
        let esc = || Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
        for entering in [false, true] {
            let ctx = context();
            let mut input = PairingInput { entering, ..Default::default() };
            settle(&ctx, &view(None), &mut input, [1280.0, 760.0]);
            let asked = one(&ctx, &view(None), &mut input, [1280.0, 760.0], vec![esc()]).request;
            assert_eq!(asked, Some(PairingRequest::Cancel));
        }
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
        assert_eq!(code_shown("482917"), "482 917");
        assert_eq!(code_shown("12"), "12");
    }
}
