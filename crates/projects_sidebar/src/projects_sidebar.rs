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
use workspace::{NewThread, ToggleWorkspaceMode};

pub use sidebar_view::ProjectsSidebar;

pub(crate) const COMPOSER_KEY_CONTEXT: &str = "AgentComposer";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, Some("AgentComposer > Editor")),
        KeyBinding::new(
            "alt-enter",
            editor::actions::Newline,
            Some("AgentComposer > Editor"),
        ),
        // Register without a context predicate so the shortcuts match
        // regardless of which focusable element is currently active. The
        // action itself still routes up to the MultiWorkspace handler.
        KeyBinding::new("ctrl-alt-m", ToggleWorkspaceMode, None),
        KeyBinding::new("ctrl-alt-n", NewThread, None),
    ]);
}
