use gpui::{App, Context, Render, WeakEntity, Window};
use serde::{Deserialize, Serialize};
use ui::{TintColor, prelude::*};

use crate::sidebar_view::ProjectsSidebar;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum WorkspaceMode {
    #[default]
    Editor,
    Agents,
}

impl WorkspaceMode {
    pub(crate) fn toggled(self) -> Self {
        match self {
            WorkspaceMode::Editor => WorkspaceMode::Agents,
            WorkspaceMode::Agents => WorkspaceMode::Editor,
        }
    }
}

pub struct WorkspaceModeSwitcher {
    sidebar: WeakEntity<ProjectsSidebar>,
}

impl WorkspaceModeSwitcher {
    pub(crate) fn new(sidebar: WeakEntity<ProjectsSidebar>, cx: &mut Context<Self>) -> Self {
        if let Some(strong) = sidebar.upgrade() {
            cx.observe(&strong, |_, _, cx| cx.notify()).detach();
        }
        Self { sidebar }
    }

    fn current_mode(&self, cx: &App) -> WorkspaceMode {
        self.sidebar
            .upgrade()
            .map(|sidebar| sidebar.read(cx).mode())
            .unwrap_or_default()
    }

    fn set_mode(&self, mode: WorkspaceMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sidebar) = self.sidebar.upgrade() {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.set_mode(mode, window, cx);
            });
        }
    }
}

impl Render for WorkspaceModeSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.current_mode(cx);

        h_flex()
            .gap_1()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .child(
                Button::new("workspace-mode-editor", "Editor")
                    .style(ButtonStyle::Transparent)
                    .toggle_state(mode == WorkspaceMode::Editor)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_mode(WorkspaceMode::Editor, window, cx);
                    })),
            )
            .child(
                Button::new("workspace-mode-agents", "Agents")
                    .style(ButtonStyle::Transparent)
                    .toggle_state(mode == WorkspaceMode::Agents)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_mode(WorkspaceMode::Agents, window, cx);
                    })),
            )
    }
}
