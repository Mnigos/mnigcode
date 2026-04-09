use gpui::{
    AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Pixels, Render, SharedString, Subscription, WeakEntity, Window, px,
};
use std::{
    collections::HashSet,
    path::PathBuf,
};
use ui::{CommonAnimationExt, Tooltip, prelude::*};
use workspace::{MultiWorkspace, MultiWorkspaceEvent, OpenMode, SidebarEvent, SidebarSide};

use crate::agents_surface::AgentsSurface;
use crate::harness::{HarnessKind, HarnessRunStatus};
use crate::helpers::{path_display_label, paths_storage_key};
use crate::mode::{WorkspaceMode, WorkspaceModeSwitcher};
use crate::serialization::SerializedProjectsSidebar;
use crate::transcript::HarnessThreadSummary;

const DEFAULT_WIDTH: Pixels = px(280.0);

struct ProjectListEntry {
    storage_key: String,
    display_name: SharedString,
    display_path: SharedString,
    paths: Vec<PathBuf>,
    loaded_workspace: Option<Entity<workspace::Workspace>>,
}

pub struct ProjectsSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    agents_surface: Entity<AgentsSurface>,
    mode_switcher: Entity<WorkspaceModeSwitcher>,
    width: Pixels,
    focus_handle: FocusHandle,
    mode: WorkspaceMode,
    subscribed_workspaces: HashSet<EntityId>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectsSidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let agents_surface = cx.new(|cx| AgentsSurface::new(multi_workspace.clone(), window, cx));
        let weak_self = cx.weak_entity();
        let mode_switcher = cx.new(|cx| WorkspaceModeSwitcher::new(weak_self, cx));

        cx.subscribe_in(
            &multi_workspace,
            window,
            |this, _multi_workspace, event: &MultiWorkspaceEvent, window, cx| match event {
                MultiWorkspaceEvent::ActiveWorkspaceChanged => {
                    this.refresh_workspace_subscriptions(window, cx);
                    cx.notify();
                }
                MultiWorkspaceEvent::WorkspaceAdded(workspace) => {
                    this.subscribe_to_workspace(workspace, window, cx);
                    cx.notify();
                }
                MultiWorkspaceEvent::WorkspaceRemoved(_) => {
                    cx.notify();
                }
            },
        )
        .detach();

        let workspaces = multi_workspace
            .read(cx)
            .workspaces()
            .cloned()
            .collect::<Vec<_>>();

        let mut this = Self {
            multi_workspace: multi_workspace.downgrade(),
            agents_surface,
            mode_switcher,
            width: DEFAULT_WIDTH,
            focus_handle: cx.focus_handle(),
            mode: WorkspaceMode::Editor,
            subscribed_workspaces: HashSet::new(),
            _subscriptions: Vec::new(),
        };

        for workspace in &workspaces {
            this.subscribe_to_workspace(workspace, window, cx);
        }

        this
    }

    pub(crate) fn mode(&self) -> WorkspaceMode {
        self.mode
    }

    pub(crate) fn set_mode(
        &mut self,
        mode: WorkspaceMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = mode;

        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let agents_surface: AnyView = self.agents_surface.clone().into();
            multi_workspace.update(cx, |multi_workspace, cx| match mode {
                WorkspaceMode::Editor => {
                    multi_workspace.set_center_surface(None, cx);
                    multi_workspace.focus_active_workspace(window, cx);
                }
                WorkspaceMode::Agents => {
                    multi_workspace.set_center_surface(Some(agents_surface), cx);
                }
            });
        }

        if mode == WorkspaceMode::Agents {
            let editor = self.agents_surface.read(cx).composer_editor().clone();
            editor.read(cx).focus_handle(cx).focus(window, cx);
        }

        cx.emit(SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn active_workspace(&self, cx: &App) -> Option<Entity<workspace::Workspace>> {
        self.multi_workspace
            .upgrade()
            .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
    }

    fn refresh_workspace_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let workspaces = multi_workspace
            .read(cx)
            .workspaces()
            .cloned()
            .collect::<Vec<_>>();

        for workspace in &workspaces {
            self.subscribe_to_workspace(workspace, window, cx);
        }
    }

    fn subscribe_to_workspace(
        &mut self,
        workspace: &Entity<workspace::Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.subscribed_workspaces.insert(workspace.entity_id()) {
            return;
        }

        let subscription = cx.subscribe(
            workspace,
            |_this, _workspace, _event: &workspace::Event, cx| {
                cx.notify();
            },
        );
        self._subscriptions.push(subscription);
    }

    fn activate_project(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace, window, cx);
            });
        }

        if self.mode == WorkspaceMode::Editor {
            self.set_mode(WorkspaceMode::Editor, window, cx);
        }
    }

    fn add_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project folder".into()),
        });

        let multi_workspace = self.multi_workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let paths = match receiver.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };
            if paths.is_empty() {
                return;
            }

            let Some(multi_workspace) = multi_workspace.upgrade() else {
                return;
            };

            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace
                        .open_project(paths, OpenMode::Activate, window, cx)
                        .detach_and_log_err(cx);
                })
                .ok();
        })
        .detach();
    }

    fn open_project_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace
                .open_project(paths, OpenMode::Activate, window, cx)
                .detach_and_log_err(cx);
        });
    }

    fn activate_thread(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        thread: HarnessThreadSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_project(workspace.clone(), window, cx);
        self.set_mode(WorkspaceMode::Agents, window, cx);

        self.agents_surface.update(cx, |agents_surface, cx| {
            agents_surface.activate_thread(workspace, thread.id, cx);
        });
    }

    fn start_thread(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_project(workspace.clone(), window, cx);
        self.set_mode(WorkspaceMode::Agents, window, cx);

        self.agents_surface.update(cx, |agents_surface, cx| {
            agents_surface.start_thread(workspace, cx);
        });
    }

    fn render_project_row(
        &self,
        row_index: usize,
        storage_key: String,
        display_name: SharedString,
        display_path: SharedString,
        paths: Vec<PathBuf>,
        loaded_workspace: Option<Entity<workspace::Workspace>>,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let threads = self
            .agents_surface
            .read(cx)
            .thread_summaries_for_workspace_path(&storage_key);
        let has_running_thread = threads.iter().any(|thread| thread.run_status.is_active());

        let mut thread_elements = Vec::new();
        for thread in threads {
            thread_elements.push(self.render_thread_row(
                loaded_workspace.clone(),
                paths.clone(),
                thread,
                cx,
            ));
        }

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .id(("projects-sidebar-project", row_index))
                    .h(px(34.0))
                    .w_full()
                    .cursor_pointer()
                    .rounded_md()
                    .px_2()
                    .gap_2()
                    .border_1()
                    .border_color(if is_active {
                        cx.theme().colors().border_focused
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if is_active {
                        cx.theme().colors().element_active
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener({
                        let loaded_workspace = loaded_workspace.clone();
                        let paths = paths.clone();
                        move |this, _, window, cx| {
                            if let Some(workspace) = loaded_workspace.clone() {
                                this.activate_project(workspace, window, cx);
                            } else {
                                this.open_project_paths(paths.clone(), window, cx);
                            }
                        }
                    }))
                    .child(Icon::new(IconName::Folder).size(IconSize::Small))
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(Label::new(display_name).truncate())
                            .child(
                                Label::new(display_path)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .when(has_running_thread, |this| {
                        this.child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Accent)
                                .with_rotate_animation(2),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .pl_4()
                    .children(thread_elements)
                    .child(self.render_new_thread_row(row_index, loaded_workspace, paths, cx)),
            )
            .into_any_element()
    }

    fn render_thread_row(
        &self,
        loaded_workspace: Option<Entity<workspace::Workspace>>,
        paths: Vec<PathBuf>,
        thread: HarnessThreadSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_running = thread.run_status.is_active();
        let is_selected = self.mode == WorkspaceMode::Agents && thread.is_active;
        let row_title = thread.title.clone();
        let harness_label: SharedString = match &thread.run_status {
            HarnessRunStatus::Failed(message) => message.clone(),
            _ => match thread.harness_kind {
                HarnessKind::Codex => "Codex app-server".into(),
            },
        };
        let icon = if is_running {
            Icon::new(IconName::LoadCircle)
                .size(IconSize::XSmall)
                .color(Color::Accent)
                .with_rotate_animation(2)
                .into_any_element()
        } else if let HarnessRunStatus::Failed(_) = &thread.run_status {
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Warning)
                .into_any_element()
        } else {
            Icon::new(IconName::Thread)
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        };

        h_flex()
            .id(format!("projects-sidebar-thread-{}", thread.id.0))
            .h(px(28.0))
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .gap_2()
            .bg(if is_selected {
                cx.theme().colors().element_active
            } else {
                gpui::transparent_black()
            })
            .tooltip(Tooltip::text(harness_label))
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(workspace) = loaded_workspace.clone() {
                    this.activate_thread(workspace, thread.clone(), window, cx);
                } else {
                    this.open_project_paths(paths.clone(), window, cx);
                }
            }))
            .child(icon)
            .child(
                Label::new(row_title)
                    .size(LabelSize::Small)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_new_thread_row(
        &self,
        row_index: usize,
        loaded_workspace: Option<Entity<workspace::Workspace>>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(("projects-sidebar-new-thread", row_index))
            .h(px(28.0))
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .gap_2()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .tooltip(Tooltip::text("Start a Codex thread in this project"))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(workspace) = loaded_workspace.clone() {
                    this.start_thread(workspace, window, cx);
                } else {
                    this.open_project_paths(paths.clone(), window, cx);
                }
            }))
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new("New thread")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_add_project_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("projects-sidebar-add-project")
            .h(px(32.0))
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .gap_2()
            .items_center()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .tooltip(Tooltip::text("Add a project folder"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.add_new_project(window, cx);
            }))
            .child(
                Icon::new(IconName::FolderPlus)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new("Add project").size(LabelSize::Small))
    }
}

impl EventEmitter<SidebarEvent> for ProjectsSidebar {}

impl Focusable for ProjectsSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl workspace::Sidebar for ProjectsSidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(DEFAULT_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, cx: &App) -> bool {
        self.agents_surface
            .read(cx)
            .threads_by_path
            .values()
            .flat_map(|threads| threads.iter())
            .any(|thread| thread.run_status.is_active())
    }

    fn side(&self, _cx: &App) -> SidebarSide {
        SidebarSide::Left
    }

    fn is_threads_list_view_active(&self) -> bool {
        self.mode == WorkspaceMode::Agents
    }

    fn top_bar_view(&self, _cx: &App) -> Option<AnyView> {
        Some(self.mode_switcher.clone().into())
    }

    fn serialized_state(&self, cx: &App) -> Option<String> {
        let agents_surface = self.agents_surface.read(cx);
        let serialized = SerializedProjectsSidebar {
            mode: self.mode,
            thread_groups: agents_surface.serialize_threads(),
            next_thread_number: agents_surface.next_thread_number(),
            selected_model: Some(agents_surface.selected_model().to_string()),
            selected_reasoning_effort: Some(
                agents_surface.selected_reasoning_effort().to_string(),
            ),
        };
        serde_json::to_string(&serialized).ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match serde_json::from_str::<SerializedProjectsSidebar>(state) {
            Ok(serialized) => {
                let SerializedProjectsSidebar {
                    mode,
                    thread_groups,
                    next_thread_number,
                    selected_model,
                    selected_reasoning_effort,
                } = serialized;
                self.mode = mode;
                self.agents_surface.update(cx, |agents_surface, _| {
                    agents_surface.restore_threads(thread_groups);
                    agents_surface.set_next_thread_number(next_thread_number);
                    if let Some(model) = selected_model {
                        agents_surface.set_selected_model(model);
                    }
                    if let Some(effort) = selected_reasoning_effort {
                        agents_surface.set_selected_reasoning_effort(effort);
                    }
                });
                cx.defer_in(window, move |this, window, cx| {
                    this.set_mode(mode, window, cx);
                });
            }
            Err(error) => log::warn!("failed to restore projects sidebar state: {error}"),
        }
    }
}

impl Render for ProjectsSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_workspace = self.active_workspace(cx);
        let project_entries: Vec<ProjectListEntry> = self
            .multi_workspace
            .upgrade()
            .map(|multi_workspace| {
                multi_workspace
                    .read(cx)
                    .project_groups(cx)
                    .map(|(key, workspaces)| {
                        let paths: Vec<PathBuf> = key
                            .path_list()
                            .paths()
                            .iter()
                            .cloned()
                            .collect();
                        let storage_key = paths_storage_key(&paths);
                        let display_name = key.display_name();
                        let display_path = paths
                            .first()
                            .map(|path| path_display_label(path.as_ref()))
                            .unwrap_or_else(|| "No folder opened".into());
                        let loaded_workspace = workspaces.into_iter().next();
                        ProjectListEntry {
                            storage_key,
                            display_name,
                            display_path,
                            paths,
                            loaded_workspace,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut project_elements = Vec::new();
        for (index, entry) in project_entries.into_iter().enumerate() {
            let is_active = entry.loaded_workspace.as_ref().is_some_and(|workspace| {
                active_workspace
                    .as_ref()
                    .is_some_and(|active| active == workspace)
            });
            project_elements.push(self.render_project_row(
                index,
                entry.storage_key,
                entry.display_name,
                entry.display_path,
                entry.paths,
                entry.loaded_workspace,
                is_active,
                cx,
            ));
        }

        v_flex()
            .key_context("ProjectsSidebar")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .p_3()
            .gap_3()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(Label::new("Mnig Code").size(LabelSize::Large))
                    .child(
                        Label::new("Projects and Codex threads")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(self.render_add_project_row(cx))
            .child(
                v_flex()
                    .gap_2()
                    .overflow_hidden()
                    .children(project_elements),
            )
    }
}
