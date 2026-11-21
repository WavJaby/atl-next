// SPDX-License-Identifier: MIT
//! [`EditorText`]: the input method's copy of the editor's text around its selection, kept on the display thread so GTK's `retrieve-surrounding` is answered synchronously from it, as a native GTK
//! text widget answers from its own buffer. GTK 4.18 gtkimcontextwayland.c `text_input_done` runs the delete-surrounding and commit handlers, then `notify_im_change` emits retrieve-surrounding
//! and sends the result in the same `done`: an answer that has to come back from the editor (a binder round trip on the outbox) is always one edit late there.
//! Fed only by the editor's reports and by the edits ATL itself sends:
//! - a report replaces it: EditorInfo.initialSurroundingText at start, each getSurroundingText answer (both a SurroundingText: window `text` at UTF-16 `offset` in the editor, selection into
//!   `text`), with the composing span from the updateSelection that asked for it;
//! - an edit sent on the connection is applied here first, by BaseInputConnection's rules at the pin (`replaceText`, `deleteSurroundingTextInCodePoints`), so the copy holds the editor's text before
//!   the editor does. The next report then confirms or corrects it ([`EditorText::agrees`]); field.rs counts which.
//!
//! An editor change of its own (code-set text: TextView.setText does not restart input, Editor.sendUpdateSelection reports the moved selection; keys) reaches here as such a report.
//! Units: Android's UTF-16 throughout; the toolkit's form is made in [`EditorText::surrounding`] (units.rs).

use atl_platform_ui::Surrounding;

use crate::units;

/// The copy: a window of the editor's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorText {
    text: Vec<u16>,
    /// UTF-16 index of `text[0]` in the editor's text.
    offset: usize,
    /// Into `text`, start <= end (BaseInputConnection sorts; the direction is not reported).
    selection: (usize, usize),
    /// Into `text`, start < end; `None` = no composing span.
    composing: Option<(usize, usize)>,
}

/// Why a report cannot become a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportError {
    NegativeOffset(i32),
    Selection(i32, i32, usize),
}

impl EditorText {
    /// The editor's report: `text` at `offset`, selection `sel_start..sel_end` into it; `composing` = updateSelection's candidates (absolute in the editor, -1 = none). A composing span reaching outside
    /// the window is dropped: the copy cannot replace what it does not hold, and the next report corrects it.
    pub fn from_report(
        text: &str,
        sel_start: i32,
        sel_end: i32,
        offset: i32,
        composing: Option<(i32, i32)>,
    ) -> Result<Self, ReportError> {
        let text: Vec<u16> = text.encode_utf16().collect();
        let offset = usize::try_from(offset).map_err(|_| ReportError::NegativeOffset(offset))?;
        let index = |i: i32| usize::try_from(i).ok().filter(|&i| i <= text.len());
        let (Some(a), Some(b)) = (index(sel_start), index(sel_end)) else {
            return Err(ReportError::Selection(sel_start, sel_end, text.len()));
        };
        let composing = composing
            .and_then(|(s, e)| {
                let (s, e) = (s.min(e), s.max(e));
                let rel = |i: i32| {
                    usize::try_from(i)
                        .ok()?
                        .checked_sub(offset)
                        .filter(|&i| i <= text.len())
                };
                Some((rel(s)?, rel(e)?))
            })
            .filter(|(s, e)| s < e);
        Ok(EditorText {
            text,
            offset,
            selection: (a.min(b), a.max(b)),
            composing,
        })
    }

    /// commitText(text, 1) (`composing` false) / setComposingText(text, 1) (true): BaseInputConnection.replaceText replaces the composing span, or the selection when there is none, and puts the
    /// cursor right after the new text. The new text is the composing span when composing, unless empty (a zero-length SPAN_EXCLUSIVE_EXCLUSIVE span is not kept: no composing span).
    pub fn replace(&mut self, new: &str, composing: bool) {
        let (a, b) = self.composing.unwrap_or(self.selection);
        let new: Vec<u16> = new.encode_utf16().collect();
        let end = a + new.len();
        self.text.splice(a..b, new);
        self.selection = (end, end);
        self.composing = (composing && end > a).then_some((a, end));
    }

    /// deleteSurroundingTextInCodePoints(before, after): BaseInputConnection widens the selection to cover the composing span, then deletes `before` code points before it and `after` after it; the
    /// span itself stays, shifted. Past the window's edge the count is clamped there (the editor may delete more: its next report corrects the copy).
    pub fn delete_code_points(&mut self, before: u32, after: u32) {
        let (mut a, mut b) = self.selection;
        if let Some((ca, cb)) = self.composing {
            a = a.min(ca);
            b = b.max(cb);
        }
        let start = step_back(&self.text, a, before);
        let end = step_forward(&self.text, b, after);
        self.text.drain(b..end);
        self.text.drain(start..a);
        let shift = a - start;
        self.selection = (self.selection.0 - shift, self.selection.1 - shift);
        self.composing = self.composing.map(|(s, e)| (s - shift, e - shift));
    }

    /// The toolkit's surrounding (text-input v3 set_surrounding_text: "excluding the preedit text ... replaced with a cursor"): the composing span is the desktop's preedit, so it is cut out and the
    /// cursor sits where it was; without one, cursor = selection end, anchor = start (units.rs `selection_to_bytes`). `None` when the window holds a lone surrogate (no UTF-8 form).
    pub fn surrounding(&self) -> Option<Surrounding> {
        let (shown, start, end) = match self.composing {
            Some((s, e)) => {
                let mut t = self.text[..s].to_vec();
                t.extend_from_slice(&self.text[e..]);
                (t, s, s)
            }
            None => (self.text.clone(), self.selection.0, self.selection.1),
        };
        let text = String::from_utf16(&shown).ok()?;
        let (cursor_byte, anchor_byte) = units::selection_to_bytes(&text, start, end).ok()?;
        Some(Surrounding {
            text,
            cursor_byte,
            anchor_byte,
        })
    }

    /// Both copies describe the same editor state: the same absolute selection and composing span, and the same text where their windows overlap (a report's window is centred on the new cursor,
    /// the copy's on an older one).
    pub fn agrees(&self, other: &EditorText) -> bool {
        let abs = |t: &EditorText, (s, e): (usize, usize)| (s + t.offset, e + t.offset);
        if abs(self, self.selection) != abs(other, other.selection)
            || self.composing.map(|c| abs(self, c)) != other.composing.map(|c| abs(other, c))
        {
            return false;
        }
        let lo = self.offset.max(other.offset);
        let hi = (self.offset + self.text.len()).min(other.offset + other.text.len());
        lo >= hi || self.text[lo - self.offset..hi - self.offset] == other.text[lo - other.offset..hi - other.offset]
    }

    /// The window's length and the absolute selection, for log lines (never the text: a field may hold a password).
    pub fn shape(&self) -> String {
        let (s, e) = self.selection;
        format!(
            "{} units at {} selection {}..{} composing {:?}",
            self.text.len(),
            self.offset,
            s + self.offset,
            e + self.offset,
            self.composing.map(|(s, e)| (s + self.offset, e + self.offset))
        )
    }
}

/// The index `n` code points before `from` (BaseInputConnection.findIndexBackward; a surrogate pair is one code point), clamped at 0.
fn step_back(text: &[u16], from: usize, n: u32) -> usize {
    let mut i = from;
    for _ in 0..n {
        if i == 0 {
            break;
        }
        i -= 1;
        if i > 0 && is_low(text[i]) && is_high(text[i - 1]) {
            i -= 1;
        }
    }
    i
}

/// The index `n` code points after `from` (findIndexForward), clamped at the end.
fn step_forward(text: &[u16], from: usize, n: u32) -> usize {
    let mut i = from;
    for _ in 0..n {
        if i >= text.len() {
            break;
        }
        i += if is_high(text[i]) && text.get(i + 1).is_some_and(|&u| is_low(u)) {
            2
        } else {
            1
        };
    }
    i
}

fn is_high(u: u16) -> bool {
    (0xd800..0xdc00).contains(&u)
}

fn is_low(u: u16) -> bool {
    (0xdc00..0xe000).contains(&u)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values counted by hand from the encodings and AOSP's rules, never from the code under test.
    // "typed from code" = 15 ASCII (15 units, 15 bytes); か な 仮 名 = 1 UTF-16 unit and 3 UTF-8 bytes each; 😀 U+1F600 = 2 units, 4 bytes.
    const INITIAL: &str = "typed from code";

    fn report(text: &str, sel: i32, composing: Option<(i32, i32)>) -> EditorText {
        EditorText::from_report(text, sel, sel, 0, composing).unwrap()
    }

    fn shown(t: &EditorText) -> (String, usize, usize) {
        let s = t.surrounding().unwrap();
        (s.text, s.cursor_byte, s.anchor_byte)
    }

    /// The ime gate's sequence: setText, preedit かな, commit 仮名 (with an empty preedit in the same done, GTK applies the commit first), delete one char.
    #[test]
    fn the_gate_sequence() {
        let mut t = report(INITIAL, 15, None);
        t.replace("かな", true);
        // the composing span 15..17 is the preedit: cut out, cursor where it starts (byte 15)
        assert_eq!(shown(&t), (INITIAL.to_owned(), 15, 15));
        assert!(t.agrees(&report("typed from codeかな", 17, Some((15, 17)))));
        t.replace("仮名", false);
        // 15 + 2 x 3 bytes = 21
        assert_eq!(shown(&t), ("typed from code仮名".to_owned(), 21, 21));
        t.replace("", true);
        assert_eq!(shown(&t), ("typed from code仮名".to_owned(), 21, 21));
        assert!(t.agrees(&report("typed from code仮名", 17, None)));
        t.delete_code_points(1, 0);
        assert_eq!(shown(&t), ("typed from code仮".to_owned(), 18, 18));
    }

    /// setComposingText("") removes the composing text and leaves none.
    #[test]
    fn empty_preedit_removes_the_composing_text() {
        let mut t = report(INITIAL, 15, None);
        t.replace("かな", true);
        t.replace("", true);
        assert_eq!(t, report(INITIAL, 15, None));
    }

    /// No composing span: the selection is replaced; the cursor lands after the new text.
    #[test]
    fn commit_replaces_the_selection() {
        // "typed" = units 0..5 selected
        let mut t = EditorText::from_report(INITIAL, 0, 5, 0, None).unwrap();
        t.replace("x", false);
        assert_eq!(shown(&t), ("x from code".to_owned(), 1, 1));
    }

    /// A surrogate pair is one code point; the delete widens over the composing span and shifts it.
    #[test]
    fn delete_counts_code_points_around_the_composing_span() {
        // "a😀bか" with か (unit 4) composing, cursor after it (5): delete 1 before -> 'b' goes
        let mut t = report("a😀bか", 5, Some((4, 5)));
        t.delete_code_points(1, 0);
        assert_eq!(t, report("a😀か", 4, Some((3, 4))));
        // 1 more before: the emoji's two units
        t.delete_code_points(1, 0);
        assert_eq!(t, report("aか", 2, Some((1, 2))));
        // more than the window holds: clamped at its start
        t.delete_code_points(5, 0);
        assert_eq!(t, report("か", 1, Some((0, 1))));
    }

    /// Windows at different offsets agree where they overlap; a different absolute selection or text there does not.
    #[test]
    fn agreement_over_the_overlap() {
        // the editor holds "0123456789", cursor at 5; one window is units 2..8, the other 4..10
        let a = EditorText::from_report("234567", 3, 3, 2, None).unwrap();
        let b = EditorText::from_report("456789", 1, 1, 4, None).unwrap();
        assert!(a.agrees(&b));
        let moved = EditorText::from_report("456789", 2, 2, 4, None).unwrap();
        assert!(!a.agrees(&moved));
        let edited = EditorText::from_report("45x789", 1, 1, 4, None).unwrap();
        assert!(!a.agrees(&edited));
        assert!(!a.agrees(&EditorText::from_report("234567", 3, 3, 2, Some((3, 5))).unwrap()));
    }

    /// Reports the copy cannot hold; a composing span outside the window is dropped, -1 candidates are none.
    #[test]
    fn reports() {
        assert_eq!(
            EditorText::from_report("ab", 0, 3, 0, None),
            Err(ReportError::Selection(0, 3, 2))
        );
        assert_eq!(
            EditorText::from_report("ab", 0, 0, -1, None),
            Err(ReportError::NegativeOffset(-1))
        );
        // window "cd" at 2; composing 0..1 lies before it
        assert_eq!(
            EditorText::from_report("cd", 0, 0, 2, Some((0, 1))).unwrap().composing,
            None
        );
        assert_eq!(report("ab", 1, Some((-1, -1))).composing, None);
        assert_eq!(
            EditorText::from_report("cd", 0, 0, 2, Some((3, 2))).unwrap().composing,
            Some((0, 1))
        );
    }
}
