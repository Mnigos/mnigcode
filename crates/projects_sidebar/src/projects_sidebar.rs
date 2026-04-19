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
use workspace::{NewThread, ToggleWorkspaceMode, Workspace};

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

    // Also register action handlers at the Workspace level. Actions bubble up
    // the focus chain, and when focus is inside a Pane (e.g. a terminal tab)
    // the chain reaches Workspace before MultiWorkspace, so we need a handler
    // here too — otherwise the keypress matches the binding but bubbles past
    // the MultiWorkspace handler without firing.
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut gpui::Context<Workspace>| {
            workspace
                .register_action(|workspace, _: &ToggleWorkspaceMode, window, cx| {
                    let Some(multi_workspace) =
                        workspace.multi_workspace().and_then(|mw| mw.upgrade())
                    else {
                        return;
                    };
                    multi_workspace.update(cx, |mw, cx| {
                        if let Some(sidebar) = mw.sidebar() {
                            sidebar.toggle_workspace_mode(window, cx);
                        }
                    });
                })
                .register_action(|workspace, _: &NewThread, window, cx| {
                    let Some(multi_workspace) =
                        workspace.multi_workspace().and_then(|mw| mw.upgrade())
                    else {
                        return;
                    };
                    multi_workspace.update(cx, |mw, cx| {
                        if let Some(sidebar) = mw.sidebar() {
                            sidebar.new_thread_in_active_project(window, cx);
                        }
                    });
                });
        },
    )
    .detach();
}
