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
use terminal_view::terminal_panel::Toggle as ToggleTerminalPanel;
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
        // regardless of which focusable element is currently active.
        KeyBinding::new("ctrl-alt-m", ToggleWorkspaceMode, None),
        KeyBinding::new("ctrl-alt-n", NewThread, None),
        // Make ctrl-` toggle the terminal panel even when the agents
        // surface is showing (Workspace context doesn't apply there).
        KeyBinding::new("ctrl-`", ToggleTerminalPanel, Some("AgentComposer")),
    ]);

}
