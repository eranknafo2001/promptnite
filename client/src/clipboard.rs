//! Copy, cut and paste in the text boxes.
//!
//! Every text box goes through [`show`]. On the desktop that is all there is
//! to it: bevy_egui talks to the system clipboard itself.
//!
//! The browser needs more care, because a page only gets at the clipboard in
//! narrow ways, and narrower still inside the frame itch.io puts the game in:
//!
//! - Pasting only works through the browser's own `paste` event. The window
//!   library cancels every key press so the browser leaves the keys to the
//!   game, which also cancels that event. So the clipboard shortcuts are
//!   kept away from it (`listen`).
//! - Copying has to put the text on the clipboard while the browser is still
//!   handling the key press or its `copy` event. egui only knows a frame
//!   later what was copied, which is too late, and the write it then tries is
//!   refused inside itch.io's frame. So the selected text is remembered here
//!   after every frame, ready to hand over on the spot.

use bevy::prelude::*;
use bevy_egui::egui;

pub fn plugin(app: &mut App) {
    #[cfg(target_arch = "wasm32")]
    web::plugin(app);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = app;
}

/// Draws a text box whose selection can be copied and cut, and that can be
/// pasted into.
pub fn show(ui: &mut egui::Ui, text_edit: egui::TextEdit<'_>) -> egui::Response {
    let output = text_edit.show(ui);
    #[cfg(target_arch = "wasm32")]
    if output.response.has_focus() {
        let selected = output
            .cursor_range
            .map_or("", |range| range.slice_str(output.galley.text()));
        web::SELECTED.with_borrow_mut(|text| selected.clone_into(text));
    }
    output.response.response
}

/// What a key press asks of the clipboard.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shortcut {
    Copy,
    Cut,
    Paste,
}

/// The modifier keys held during a key press.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Default)]
struct Held {
    ctrl: bool,
    meta: bool,
    shift: bool,
    alt: bool,
}

/// Which clipboard shortcut a key press is, if any. `key` and `code` are the
/// browser's: what the key types, and where it sits on the keyboard.
#[cfg(any(target_arch = "wasm32", test))]
fn shortcut(key: &str, code: &str, held: Held, mac: bool) -> Option<Shortcut> {
    // AltGr counts as Ctrl + Alt on Windows, and types a character.
    if held.alt {
        return None;
    }
    let command = if mac {
        held.meta && !held.ctrl
    } else {
        held.ctrl && !held.meta
    };
    // The letter the key types. Keyboards without Latin letters (Russian,
    // Greek…) go by where the key sits instead, as browsers do.
    let letter = if key.len() == 1 && key.is_ascii() {
        key.to_ascii_lowercase()
    } else {
        code.strip_prefix("Key")
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    match (letter.as_str(), command, held.shift) {
        // With Shift these open the browser's developer tools.
        ("c", true, false) => Some(Shortcut::Copy),
        ("x", true, false) => Some(Shortcut::Cut),
        // With Shift: paste without formatting, the same thing here.
        ("v", true, _) => Some(Shortcut::Paste),
        // The older shortcuts of Windows and Linux.
        _ if mac || held.meta => None,
        _ => match (key, held.ctrl, held.shift) {
            ("Insert", true, false) => Some(Shortcut::Copy),
            ("Insert", false, true) => Some(Shortcut::Paste),
            ("Delete", false, true) => Some(Shortcut::Cut),
            _ => None,
        },
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};

    use bevy::prelude::*;
    use bevy_egui::{
        EguiClipboard, EguiGlobalSettings, EguiInputSet, EguiPreUpdateSet, PrimaryEguiContext,
        egui, input::EguiInputEvent, web_clipboard::WebClipboardEvent,
    };
    use web_sys::wasm_bindgen::{JsCast, JsValue, closure::Closure};

    use super::{Held, Shortcut, shortcut};

    thread_local! {
        /// The text selected in the box being typed in, as of the last frame.
        /// The browser's event handlers run between frames and read it there.
        pub static SELECTED: RefCell<String> = const { RefCell::new(String::new()) };
        /// The selection went to the clipboard as a cut and is still to be
        /// deleted.
        static CUT: Cell<bool> = const { Cell::new(false) };
    }

    pub fn plugin(app: &mut App) {
        app.add_systems(Startup, listen)
            .add_systems(First, forget_selection)
            .add_systems(
                PreUpdate,
                feed_egui
                    .in_set(EguiPreUpdateSet::ProcessInput)
                    .in_set(EguiInputSet::ReadBevyMessages),
            );
    }

    /// Hooks into the browser's key presses and clipboard events.
    fn listen(mut settings: ResMut<EguiGlobalSettings>) {
        // `feed_egui` takes the place of bevy_egui's own handling.
        settings
            .input_system_settings
            .run_write_web_clipboard_messages_system = false;

        let Some(window) = web_sys::window() else {
            return;
        };
        let Some(document) = window.document() else {
            return;
        };
        let mac = crate::ui::is_mac();

        // On the window, and before anything inside it hears of the key
        // press: the window library listens on the canvas.
        let ignore = Closure::<dyn FnMut(JsValue)>::new(|_| {});
        let key_down = Closure::<dyn FnMut(_)>::new(move |event: web_sys::KeyboardEvent| {
            let held = Held {
                ctrl: event.ctrl_key(),
                meta: event.meta_key(),
                shift: event.shift_key(),
                alt: event.alt_key(),
            };
            let Some(shortcut) = shortcut(&event.key(), &event.code(), held, mac) else {
                return;
            };
            // Unseen by the window library, the key press is not
            // cancelled, and the browser goes on to its `copy`, `cut` or
            // `paste` event.
            event.stop_propagation();
            if shortcut == Shortcut::Paste {
                return;
            }
            let text = SELECTED.with_borrow(String::clone);
            if text.is_empty() {
                return;
            }
            // Not every browser sends `copy` and `cut` events when the
            // page itself has nothing selected, so also write directly.
            // Where that is refused (itch.io's frame), the event below
            // does the job. Pages not served over https have no clipboard.
            let clipboard = web_sys::window().map(|window| window.navigator().clipboard());
            if let Some(clipboard) = clipboard.filter(|clipboard| !clipboard.is_undefined()) {
                let _ = clipboard.write_text(&text).catch(&ignore);
            }
            if shortcut == Shortcut::Cut {
                CUT.set(true);
            }
        });
        let listening = window.add_event_listener_with_callback_and_bool(
            "keydown",
            key_down.as_ref().unchecked_ref(),
            true,
        );
        if listening.is_err() {
            warn!("clipboard: could not listen for key presses");
        }
        // Listening for as long as the page lives.
        key_down.forget();

        // Also reached from the browser's Edit menu.
        let copy = Closure::<dyn FnMut(_)>::new(|event: web_sys::ClipboardEvent| {
            let text = SELECTED.with_borrow(String::clone);
            if text.is_empty() {
                return;
            }
            let written = event
                .clipboard_data()
                .is_some_and(|data| data.set_data("text/plain", &text).is_ok());
            if written {
                // Otherwise the browser copies its own (empty) selection.
                event.prevent_default();
                if event.type_() == "cut" {
                    CUT.set(true);
                }
            }
        });
        for kind in ["copy", "cut"] {
            if document
                .add_event_listener_with_callback(kind, copy.as_ref().unchecked_ref())
                .is_err()
            {
                warn!("clipboard: could not listen for {kind} events");
            }
        }
        copy.forget();
    }

    /// Each frame starts with nothing selected; the box being typed in, if
    /// there is one, says otherwise when it is drawn.
    fn forget_selection() {
        SELECTED.with_borrow_mut(String::clear);
    }

    /// Passes what the browser handed over on to egui.
    fn feed_egui(
        clipboard: Res<EguiClipboard>,
        context: Query<Entity, With<PrimaryEguiContext>>,
        mut events: MessageWriter<EguiInputEvent>,
    ) {
        let Ok(context) = context.single() else {
            return;
        };
        // The text is on the clipboard already. Telling egui to cut would
        // have it copy the text again, too late to be allowed; backspace
        // deletes the selection just the same. Before any paste: a cut and a
        // paste within one frame are a cut followed by a paste.
        if CUT.take() {
            for pressed in [true, false] {
                events.write(EguiInputEvent {
                    context,
                    event: egui::Event::Key {
                        key: egui::Key::Backspace,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                });
            }
        }
        // bevy_egui still listens for the events; only pastes carry anything.
        while let Some(event) = clipboard.try_receive_clipboard_event() {
            if let WebClipboardEvent::Paste(text) = event {
                events.write(EguiInputEvent {
                    context,
                    event: egui::Event::Paste(text),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Held = Held {
        ctrl: false,
        meta: false,
        shift: false,
        alt: false,
    };
    const CTRL: Held = Held { ctrl: true, ..NONE };
    const META: Held = Held { meta: true, ..NONE };
    const SHIFT: Held = Held {
        shift: true,
        ..NONE
    };

    #[test]
    fn command_is_ctrl_or_the_mac_command_key() {
        assert_eq!(shortcut("c", "KeyC", CTRL, false), Some(Shortcut::Copy));
        assert_eq!(shortcut("x", "KeyX", CTRL, false), Some(Shortcut::Cut));
        assert_eq!(shortcut("v", "KeyV", CTRL, false), Some(Shortcut::Paste));
        assert_eq!(shortcut("v", "KeyV", META, true), Some(Shortcut::Paste));
        assert_eq!(shortcut("v", "KeyV", CTRL, true), None);
        assert_eq!(shortcut("v", "KeyV", META, false), None);
        assert_eq!(shortcut("v", "KeyV", NONE, false), None);
    }

    #[test]
    fn typing_is_left_alone() {
        // AltGr on Windows.
        let alt_gr = Held { alt: true, ..CTRL };
        assert_eq!(shortcut("v", "KeyV", alt_gr, false), None);
        // Opens the developer tools.
        let shifted = Held {
            shift: true,
            ..CTRL
        };
        assert_eq!(shortcut("C", "KeyC", shifted, false), None);
        assert_eq!(shortcut("V", "KeyV", shifted, false), Some(Shortcut::Paste));
        assert_eq!(shortcut("Delete", "Delete", NONE, false), None);
    }

    #[test]
    fn layouts_go_by_letter_then_by_position() {
        // Dvorak: the key in C's place types "j".
        assert_eq!(shortcut("j", "KeyC", CTRL, false), None);
        assert_eq!(shortcut("c", "KeyI", CTRL, false), Some(Shortcut::Copy));
        // Russian: the key in V's place types "м".
        assert_eq!(shortcut("м", "KeyV", CTRL, false), Some(Shortcut::Paste));
    }

    #[test]
    fn older_shortcuts_outside_the_mac() {
        assert_eq!(
            shortcut("Insert", "Insert", CTRL, false),
            Some(Shortcut::Copy)
        );
        assert_eq!(
            shortcut("Insert", "Insert", SHIFT, false),
            Some(Shortcut::Paste)
        );
        assert_eq!(
            shortcut("Delete", "Delete", SHIFT, false),
            Some(Shortcut::Cut)
        );
        assert_eq!(shortcut("Delete", "Delete", SHIFT, true), None);
    }
}
