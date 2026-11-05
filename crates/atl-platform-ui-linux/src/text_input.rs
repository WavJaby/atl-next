// SPDX-License-Identifier: MIT
//! [`GtkTextInput`]: a window's `TextInput` on a `GtkIMMulticontext` whose client widget is the window's `SceneView`. GTK picks the IM module (Wayland: text-input v3; X11/IBus: its own), so this file speaks GtkIMContext only.
//! Every call ends with `mirror::resync`: the IM module's Wayland requests (a new registry at the first focus_in, text-input state) are flushed only from GLib's prepare, which a call from the Looper outside a mirror pass never reaches (measured 2026-11-06: without it the text-input manager is never bound).
//! Units pass through as GTK has them (UTF-8 + byte indices for surrounding, chars for preedit cursor and delete); converting to Android's is atl-ime's.
//! Keys are not filtered here yet: a desktop IM that composes from key presses (IBus/fcitx over GTK's key path) needs `filter_keypress` on the window's key controller (IME plan step 7). The Wayland input-method path (compositor-side IME) does not.

use std::cell::RefCell;
use std::rc::Rc;

use atl_platform_ui_api::{
    ContentHints, ContentPurpose, PhysRect, Surrounding, TextInput, TextInputEvent,
};
use gtk4::prelude::*;
use gtk4::{InputHints, InputPurpose, gdk};

use crate::mirror;
use crate::view::SceneView;
use crate::window::Slot;

/// One window's IM context.
pub struct GtkTextInput {
    context: gtk4::IMMulticontext,
    /// For the content's scale: the cursor rect arrives in physical pixels, GTK wants the widget's logical ones.
    view: SceneView,
    /// The last surrounding set; GTK asks for it again (`retrieve-surrounding`) whenever its IM module wants it.
    surrounding: Rc<RefCell<Surrounding>>,
    on_event: Rc<Slot<dyn FnMut(TextInputEvent)>>,
}

/// GTK's surrounding call takes `int` byte indices; a text past `i32::MAX` bytes is clamped (no field holds 2 GiB).
fn index(byte: usize) -> i32 {
    i32::try_from(byte).unwrap_or(i32::MAX)
}

fn send_surrounding(context: &gtk4::IMMulticontext, s: &Surrounding) {
    context.set_surrounding_with_selection(&s.text, index(s.cursor_byte), index(s.anchor_byte));
}

impl GtkTextInput {
    pub(crate) fn new(view: &SceneView) -> Self {
        let context = gtk4::IMMulticontext::new();
        context.set_client_widget(Some(view));
        let on_event: Rc<Slot<dyn FnMut(TextInputEvent)>> = Rc::new(Slot::new());
        let surrounding = Rc::new(RefCell::new(Surrounding::default()));

        let slot = Rc::clone(&on_event);
        context.connect_preedit_changed(move |c| {
            let (text, _attrs, cursor) = c.preedit_string();
            let event = TextInputEvent::Preedit {
                text: text.to_string(),
                cursor_chars: usize::try_from(cursor).unwrap_or(0),
            };
            slot.call(|f| f(event));
        });
        let slot = Rc::clone(&on_event);
        context.connect_preedit_end(move |_| slot.call(|f| f(TextInputEvent::PreeditEnd)));
        let slot = Rc::clone(&on_event);
        context.connect_commit(move |_, text| {
            slot.call(|f| f(TextInputEvent::Commit(text.to_owned())));
        });
        let slot = Rc::clone(&on_event);
        context.connect_delete_surrounding(move |_, offset, n| {
            let event = TextInputEvent::DeleteSurrounding {
                offset_chars: offset,
                n_chars: u32::try_from(n).unwrap_or(0),
            };
            slot.call(|f| f(event));
            true
        });
        let cached = Rc::clone(&surrounding);
        context.connect_retrieve_surrounding(move |c| {
            send_surrounding(c, &cached.borrow());
            true
        });
        Self {
            context,
            view: view.clone(),
            surrounding,
            on_event,
        }
    }
}

fn purpose(p: ContentPurpose) -> InputPurpose {
    match p {
        ContentPurpose::FreeForm => InputPurpose::FreeForm,
        ContentPurpose::Alpha => InputPurpose::Alpha,
        ContentPurpose::Digits => InputPurpose::Digits,
        ContentPurpose::Number => InputPurpose::Number,
        ContentPurpose::Phone => InputPurpose::Phone,
        ContentPurpose::Url => InputPurpose::Url,
        ContentPurpose::Email => InputPurpose::Email,
        ContentPurpose::Name => InputPurpose::Name,
        ContentPurpose::Password => InputPurpose::Password,
        ContentPurpose::Pin => InputPurpose::Pin,
        ContentPurpose::Terminal => InputPurpose::Terminal,
    }
}

fn hints(h: ContentHints) -> InputHints {
    const MAP: [(ContentHints, InputHints); 12] = [
        (ContentHints::SPELLCHECK, InputHints::SPELLCHECK),
        (ContentHints::NO_SPELLCHECK, InputHints::NO_SPELLCHECK),
        (ContentHints::WORD_COMPLETION, InputHints::WORD_COMPLETION),
        (ContentHints::LOWERCASE, InputHints::LOWERCASE),
        (ContentHints::UPPERCASE_CHARS, InputHints::UPPERCASE_CHARS),
        (ContentHints::UPPERCASE_WORDS, InputHints::UPPERCASE_WORDS),
        (
            ContentHints::UPPERCASE_SENTENCES,
            InputHints::UPPERCASE_SENTENCES,
        ),
        (ContentHints::INHIBIT_OSK, InputHints::INHIBIT_OSK),
        (ContentHints::VERTICAL_WRITING, InputHints::VERTICAL_WRITING),
        (ContentHints::EMOJI, InputHints::EMOJI),
        (ContentHints::NO_EMOJI, InputHints::NO_EMOJI),
        (ContentHints::PRIVATE, InputHints::PRIVATE),
    ];
    MAP.iter()
        .filter(|(ours, _)| h.contains(*ours))
        .fold(InputHints::NONE, |acc, (_, gtk)| acc | *gtk)
}

impl TextInput for GtkTextInput {
    fn focus_in(&self) {
        self.context.focus_in();
        mirror::resync();
    }

    fn focus_out(&self) {
        self.context.focus_out();
        mirror::resync();
    }

    fn reset(&self) {
        self.context.reset();
        mirror::resync();
    }

    fn set_content(&self, p: ContentPurpose, h: ContentHints) {
        self.context.set_input_purpose(purpose(p));
        self.context.set_input_hints(hints(h));
        mirror::resync();
    }

    fn set_surrounding(&self, surrounding: &Surrounding) {
        *self.surrounding.borrow_mut() = surrounding.clone();
        send_surrounding(&self.context, surrounding);
        mirror::resync();
    }

    fn set_cursor_rect(&self, rect: PhysRect) {
        let scale = self.view.scale();
        let (x, y) = (scale.to_logical(rect.x), scale.to_logical(rect.y));
        // outward rounding: the logical rect covers the physical one
        let (right, bottom) = (
            scale.to_logical(rect.x + rect.width).ceil(),
            scale.to_logical(rect.y + rect.height).ceil(),
        );
        let (x, y) = (x.floor(), y.floor());
        self.context.set_cursor_location(&gdk::Rectangle::new(
            x as i32,
            y as i32,
            (right - x) as i32,
            (bottom - y) as i32,
        ));
        mirror::resync();
    }

    fn on_events<F: FnMut(TextInputEvent) + 'static>(&self, on_event: F) {
        self.on_event.set(Box::new(on_event));
    }
}
