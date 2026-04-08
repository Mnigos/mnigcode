mod agents_surface;
mod harness;
mod helpers;
mod mode;
mod serialization;
mod sidebar_view;
mod transcript;

use editor;
use gpui::{App, KeyBinding};
use menu::Confirm;

pub use sidebar_view::ProjectsSidebar;

pub(crate) const CODEX_COMPOSER_KEY_CONTEXT: &str = "CodexComposer";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, Some("CodexComposer > Editor")),
        KeyBinding::new(
            "alt-enter",
            editor::actions::Newline,
            Some("CodexComposer > Editor"),
        ),
    ]);
}
