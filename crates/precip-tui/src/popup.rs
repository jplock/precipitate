//! Popups: windows that open over a tab and take every key until they
//! close. They form a stack; the one on top gets the keys and is drawn
//! last. A popup turns a key into an [`Outcome`] and the app acts on it,
//! so the work a popup triggers lives with the rest of the app.

use crossterm::event::KeyCode;
use ratatui::widgets::ListState;

use crate::{
    ActionMenu, DISPOSITIONS, DatePicker, DependsPicker, DispositionPicker,
    ExportPrompt, Fields, HistoryView, HolidayPicker, MergePrompt, PairEditor,
    ReasonPicker, RulesEditor, WEEKDAY_ROWS, WeekdayPicker,
};

/// What a key did to the popup on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Handled inside the popup, or ignored.
    Stay,
    /// The popup is done without acting.
    Close,
    /// Act on the popup's selection or input.
    Submit,
    /// Change the selected value by this much.
    Adjust(i64),
    /// Remove the selected row.
    Remove,
}

/// Move a list selection one row with the arrow keys, within `len` rows.
/// Returns whether the key was an arrow.
fn step_list(state: &mut ListState, len: usize, code: KeyCode) -> bool {
    let i = state.selected().unwrap_or(0);
    if code == KeyCode::Down {
        state.select(Some((i + 1).min(len.saturating_sub(1))));
        true
    } else if code == KeyCode::Up {
        state.select(Some(i.saturating_sub(1)));
        true
    } else {
        false
    }
}

/// Type into a line of text. Returns whether the key changed it.
pub fn edit_text(text: &mut String, code: KeyCode) -> bool {
    if let KeyCode::Char(c) = code {
        text.push(c);
        true
    } else if code == KeyCode::Backspace {
        text.pop();
        true
    } else {
        false
    }
}

/// Keys for a form being filled in inside an editor row.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a form uses a few keys and ignores the rest of KeyCode"
)]
fn form_key(fields: &mut Fields, code: KeyCode) -> Option<Outcome> {
    match code {
        KeyCode::Esc => return None,
        KeyCode::Tab | KeyCode::Down => fields.next(),
        KeyCode::BackTab | KeyCode::Up => fields.prev(),
        KeyCode::Backspace => fields.pop(),
        KeyCode::Char(c) => fields.push(c),
        KeyCode::Enter => return Some(Outcome::Submit),
        _ => {}
    }
    Some(Outcome::Stay)
}

/// Keys shared by the rules and pair editors when no form is open.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "an editor uses a few keys and ignores the rest of KeyCode"
)]
fn editor_key(
    confirming_delete: &mut bool,
    step: impl FnOnce(isize),
    code: KeyCode,
) -> Outcome {
    match code {
        KeyCode::Esc if *confirming_delete => {
            *confirming_delete = false;
            Outcome::Stay
        }
        KeyCode::Esc => Outcome::Close,
        KeyCode::Down | KeyCode::Up => {
            step(if code == KeyCode::Down { 1 } else { -1 });
            *confirming_delete = false;
            Outcome::Stay
        }
        KeyCode::Left => Outcome::Adjust(-1),
        KeyCode::Right => Outcome::Adjust(1),
        KeyCode::Enter => Outcome::Submit,
        _ => Outcome::Stay,
    }
}

/// Generate the popup enum, a typed way to find each kind in the stack,
/// and the key dispatch.
macro_rules! popups {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        /// Every kind of popup.
        pub enum Popup {
            $($variant($ty)),+
        }

        /// The kind of a popup, for drawing in stack order.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PopupTag {
            $($variant),+
        }

        impl Popup {
            pub fn tag(&self) -> PopupTag {
                match self {
                    $(Self::$variant(_) => PopupTag::$variant),+
                }
            }

            pub fn key(&mut self, code: KeyCode) -> Outcome {
                match self {
                    $(Self::$variant(p) => p.key(code)),+
                }
            }
        }

        $(
            impl PopupKind for $ty {
                fn of(p: &Popup) -> Option<&Self> {
                    if let Popup::$variant(x) = p { Some(x) } else { None }
                }

                fn of_mut(p: &mut Popup) -> Option<&mut Self> {
                    if let Popup::$variant(x) = p { Some(x) } else { None }
                }

                fn into_popup(self) -> Popup {
                    Popup::$variant(self)
                }

                fn from_popup(p: Popup) -> Option<Self> {
                    if let Popup::$variant(x) = p { Some(x) } else { None }
                }
            }
        )+
    };
}

/// A type that is one kind of popup.
pub trait PopupKind: Sized {
    fn of(p: &Popup) -> Option<&Self>;
    fn of_mut(p: &mut Popup) -> Option<&mut Self>;
    fn into_popup(self) -> Popup;
    fn from_popup(p: Popup) -> Option<Self>;
}

popups! {
    Disposition(DispositionPicker),
    Date(DatePicker),
    History(HistoryView),
    Rules(RulesEditor),
    Pair(PairEditor),
    Weekdays(WeekdayPicker),
    Depends(DependsPicker),
    Holidays(HolidayPicker),
    Merge(MergePrompt),
    Reason(ReasonPicker),
    Export(ExportPrompt),
    Actions(ActionMenu),
}

/// The open popups, bottom first.
#[derive(Default)]
pub struct Popups(Vec<Popup>);

impl Popups {
    /// Open a popup on top of the others.
    pub fn open(&mut self, p: impl PopupKind) {
        self.0.push(p.into_popup());
    }

    /// The topmost popup of this kind.
    pub fn get<T: PopupKind>(&self) -> Option<&T> {
        self.0.iter().rev().find_map(T::of)
    }

    pub fn get_mut<T: PopupKind>(&mut self) -> Option<&mut T> {
        self.0.iter_mut().rev().find_map(T::of_mut)
    }

    /// Close the topmost popup of this kind and hand it back.
    pub fn close<T: PopupKind>(&mut self) -> Option<T> {
        let i = self.0.iter().rposition(|p| T::of(p).is_some())?;
        T::from_popup(self.0.remove(i))
    }

    /// Close the popup on top.
    pub fn pop(&mut self) {
        self.0.pop();
    }

    pub fn top_mut(&mut self) -> Option<&mut Popup> {
        self.0.last_mut()
    }

    /// The kind of each open popup, bottom first.
    pub fn tags(&self) -> Vec<PopupTag> {
        self.0.iter().map(Popup::tag).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl ActionMenu {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if code == KeyCode::Esc {
            return if self.confirming.take().is_none() {
                Outcome::Close
            } else {
                Outcome::Stay
            };
        }
        if step_list(&mut self.state, self.actions.len(), code) {
            self.confirming = None;
            return Outcome::Stay;
        }
        if code == KeyCode::Enter { Outcome::Submit } else { Outcome::Stay }
    }
}

impl ReasonPicker {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if let Some(text) = self.custom.as_mut() {
            if code == KeyCode::Esc {
                self.custom = None;
            } else if code == KeyCode::Enter {
                return Outcome::Submit;
            } else {
                edit_text(text, code);
            }
            return Outcome::Stay;
        }
        list_key(&mut self.state, Self::rows(), code)
    }
}

/// Keys for a plain list: arrows move, Enter submits, Esc closes.
fn list_key(state: &mut ListState, len: usize, code: KeyCode) -> Outcome {
    if code == KeyCode::Esc {
        Outcome::Close
    } else if code == KeyCode::Enter {
        Outcome::Submit
    } else {
        step_list(state, len, code);
        Outcome::Stay
    }
}

/// Keys for a line of text: typing edits, Enter submits, Esc closes.
fn text_key(text: &mut String, code: KeyCode) -> (Outcome, bool) {
    if code == KeyCode::Esc {
        (Outcome::Close, false)
    } else if code == KeyCode::Enter {
        (Outcome::Submit, false)
    } else {
        (Outcome::Stay, edit_text(text, code))
    }
}

impl ExportPrompt {
    fn key(&mut self, code: KeyCode) -> Outcome {
        let (outcome, edited) = text_key(&mut self.text, code);
        if edited {
            self.confirming_overwrite = false;
        }
        outcome
    }
}

impl MergePrompt {
    fn key(&mut self, code: KeyCode) -> Outcome {
        text_key(&mut self.text, code).0
    }
}

impl HolidayPicker {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if code == KeyCode::Char(' ') {
            if let Some(chosen) =
                self.state.selected().and_then(|i| self.chosen.get_mut(i))
            {
                *chosen = !*chosen;
            }
            return Outcome::Stay;
        }
        list_key(&mut self.state, self.holidays.len(), code)
    }
}

impl DependsPicker {
    fn key(&mut self, code: KeyCode) -> Outcome {
        list_key(&mut self.state, self.candidates.len(), code)
    }
}

impl WeekdayPicker {
    /// Esc and Enter both save: the ticks are the answer.
    fn key(&mut self, code: KeyCode) -> Outcome {
        if code == KeyCode::Char(' ') {
            self.toggle_selected();
            return Outcome::Stay;
        }
        if code == KeyCode::Esc {
            return Outcome::Submit;
        }
        list_key(&mut self.state, WEEKDAY_ROWS.len(), code)
    }
}

impl PairEditor {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if let Some(fields) = self.input.as_mut() {
            return form_key(fields, code).unwrap_or_else(|| {
                self.input = None;
                Outcome::Stay
            });
        }
        let mut confirming = self.confirming_delete;
        let outcome = editor_key(&mut confirming, |d| self.step(d), code);
        self.confirming_delete = confirming;
        outcome
    }
}

impl RulesEditor {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if let Some(fields) = self.input.as_mut() {
            return form_key(fields, code).unwrap_or_else(|| {
                self.input = None;
                Outcome::Stay
            });
        }
        if code == KeyCode::Backspace || code == KeyCode::Delete {
            return Outcome::Remove;
        }
        let mut confirming = self.confirming_delete;
        let outcome = editor_key(&mut confirming, |d| self.step(d), code);
        self.confirming_delete = confirming;
        outcome
    }
}

impl HistoryView {
    fn key(&mut self, code: KeyCode) -> Outcome {
        if code == KeyCode::Esc || code == KeyCode::Enter {
            Outcome::Close
        } else {
            step_list(&mut self.state, self.events.len(), code);
            Outcome::Stay
        }
    }
}

impl DatePicker {
    fn key(&mut self, code: KeyCode) -> Outcome {
        list_key(&mut self.state, self.dates.len(), code)
    }
}

impl DispositionPicker {
    fn key(&mut self, code: KeyCode) -> Outcome {
        list_key(&mut self.state, DISPOSITIONS.len(), code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> HistoryView {
        HistoryView { events: Vec::new(), state: ListState::default() }
    }

    fn merge() -> MergePrompt {
        MergePrompt { alias: "a".into(), text: String::new() }
    }

    #[test]
    fn the_stack_finds_closes_and_orders_popups() {
        let mut s = Popups::default();
        assert!(s.is_empty());
        s.open(history());
        s.open(merge());
        assert_eq!(s.tags(), [PopupTag::History, PopupTag::Merge]);
        assert!(s.get::<HistoryView>().is_some(), "found below the top");
        assert!(s.get::<DependsPicker>().is_none());
        assert!(s.close::<HistoryView>().is_some());
        assert_eq!(s.tags(), [PopupTag::Merge]);
        assert!(s.close::<HistoryView>().is_none());
    }

    #[test]
    fn the_top_popup_gets_the_key() {
        let mut s = Popups::default();
        s.open(merge());
        let top = s.top_mut().unwrap();
        assert_eq!(top.key(KeyCode::Char('x')), Outcome::Stay);
        assert_eq!(top.key(KeyCode::Enter), Outcome::Submit);
        assert_eq!(top.key(KeyCode::Esc), Outcome::Close);
        assert_eq!(s.get::<MergePrompt>().unwrap().text, "x");
    }

    #[test]
    fn lists_stay_in_range() {
        let mut state = ListState::default();
        state.select(Some(0));
        assert!(step_list(&mut state, 2, KeyCode::Up));
        assert_eq!(state.selected(), Some(0));
        step_list(&mut state, 2, KeyCode::Down);
        step_list(&mut state, 2, KeyCode::Down);
        assert_eq!(state.selected(), Some(1));
        assert!(!step_list(&mut state, 2, KeyCode::Left));
        let mut empty = ListState::default();
        step_list(&mut empty, 0, KeyCode::Down);
        assert_eq!(
            empty.selected(),
            Some(0),
            "an empty list does not underflow"
        );
    }

    #[test]
    fn export_typing_clears_a_pending_overwrite() {
        let mut p = ExportPrompt {
            kind: crate::ExportKind::Schedule,
            text: "out.csv".into(),
            confirming_overwrite: true,
        };
        assert_eq!(p.key(KeyCode::Backspace), Outcome::Stay);
        assert!(!p.confirming_overwrite);
        assert_eq!(p.text, "out.cs");
    }
}
