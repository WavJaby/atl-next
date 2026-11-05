// SPDX-License-Identifier: MIT
//! One focused GtkText; prints `FOCUS` once it has focus, `PREEDIT <text>` on `preedit-changed`, `TEXT <text>` on every buffer change (a committed string lands here). Line-buffered stdout for the gate.
use gtk4::prelude::*;
use std::io::Write;

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn main() -> gtk4::glib::ExitCode {
    let app = gtk4::Application::builder()
        .application_id("org.atl.ImeEntry")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        let text = gtk4::Text::new();
        text.connect_preedit_changed(|_, p| say(&format!("PREEDIT {p}")));
        text.connect_changed(|t| say(&format!("TEXT {}", t.text())));
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_enter(|_| say("FOCUS"));
        text.add_controller(focus);
        let win = gtk4::ApplicationWindow::builder()
            .application(app)
            .default_width(320)
            .default_height(80)
            .child(&text)
            .build();
        win.present();
        text.grab_focus();
    });
    app.run_with_args::<&str>(&[])
}
