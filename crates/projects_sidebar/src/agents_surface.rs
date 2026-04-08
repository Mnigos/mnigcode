use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, ExternalPaths, Focusable, MouseButton, ObjectFit,
    PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, Subscription, Window, deferred,
    img, px,
};
use language::language_settings::SoftWrap;
use menu::Confirm;
use smol::channel;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use ui::{CommonAnimationExt, Tooltip, WithScrollbar, prelude::*};
use workspace::{MultiWorkspace, MultiWorkspaceEvent};

use crate::CODEX_COMPOSER_KEY_CONTEXT;
use crate::harness::{
    HarnessApprovalPolicy, HarnessKind, HarnessRunStatus, HarnessSandboxPolicy, HarnessThreadId,
    HarnessToolPhase, HarnessTurnRequest, HarnessTurnUpdate, run_codex_app_server_turn,
};
use crate::helpers::{
    animated_thinking_label, attachment_display_name, attachment_icon, build_input_with_attachments,
    is_image_path, tool_summary_line, workspace_display_name, workspace_root_path,
    workspace_storage_key,
};
use crate::serialization::{
    SerializedHarnessThread, SerializedThreadGroup, SerializedToolKind, SerializedToolStatus,
    SerializedTranscriptMessage, SerializedTranscriptRole,
};
use crate::transcript::{
    HarnessThread, HarnessThreadSummary, ToolDisplayKind, ToolStatus, TranscriptMessage,
    TranscriptRole,
};

const CODEX_COMPOSER_WIDTH: Pixels = px(720.0);

pub struct AgentsSurface {
    workspace: Entity<workspace::Workspace>,
    composer_editor: Entity<Editor>,
    pub(crate) active_thread_by_path: HashMap<String, HarnessThreadId>,
    pub(crate) threads_by_path: HashMap<String, Vec<HarnessThread>>,
    next_thread_number: usize,
    pending_attachments: Vec<PathBuf>,
    previewing_attachment: Option<PathBuf>,
    expanded_tool_messages: HashSet<(SharedString, usize)>,
    transcript_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl AgentsSurface {
    pub(crate) fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = multi_workspace.read(cx).workspace().clone();

        let composer_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor.set_placeholder_text(
                "Ask Codex anything, @ to add files, / for commands",
                window,
                cx,
            );
            editor.set_show_indent_guides(false, cx);
            editor
        });

        let active_workspace_subscription = cx.subscribe_in(
            &multi_workspace,
            window,
            |this, multi_workspace, event: &MultiWorkspaceEvent, _window, cx| {
                if matches!(event, MultiWorkspaceEvent::ActiveWorkspaceChanged) {
                    this.workspace = multi_workspace.read(cx).workspace().clone();
                    cx.notify();
                }
            },
        );

        Self {
            workspace,
            composer_editor,
            active_thread_by_path: HashMap::default(),
            threads_by_path: HashMap::default(),
            next_thread_number: 1,
            pending_attachments: Vec::new(),
            previewing_attachment: None,
            expanded_tool_messages: HashSet::new(),
            transcript_scroll_handle: ScrollHandle::new(),
            _subscriptions: vec![active_workspace_subscription],
        }
    }

    fn add_attachments<I>(&mut self, paths: I, cx: &mut Context<Self>)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let mut added = false;
        for path in paths {
            if !self
                .pending_attachments
                .iter()
                .any(|existing| existing == &path)
            {
                self.pending_attachments.push(path);
                added = true;
            }
        }
        if added {
            cx.notify();
        }
    }

    fn preview_attachment(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.previewing_attachment = Some(path);
        cx.notify();
    }

    fn dismiss_preview(&mut self, cx: &mut Context<Self>) {
        if self.previewing_attachment.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn composer_editor(&self) -> &Entity<Editor> {
        &self.composer_editor
    }

    fn toggle_tool_expansion(
        &mut self,
        thread_id: SharedString,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let key = (thread_id, index);
        if !self.expanded_tool_messages.remove(&key) {
            self.expanded_tool_messages.insert(key);
        }
        cx.notify();
    }

    fn pick_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            let paths = match receiver.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };

            this.update(cx, |this, cx| {
                this.add_attachments(paths, cx);
            })
            .ok();
        })
        .detach();
    }

    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_attachments.len() {
            self.pending_attachments.remove(index);
            cx.notify();
        }
    }

    fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer_editor
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    pub(crate) fn thread_summaries_for_workspace_path(
        &self,
        workspace_path: &str,
    ) -> Vec<HarnessThreadSummary> {
        let active_thread_id = self.active_thread_by_path.get(workspace_path);
        self.threads_by_path
            .get(workspace_path)
            .map(|threads| {
                threads
                    .iter()
                    .map(|thread| HarnessThreadSummary {
                        id: thread.id.clone(),
                        title: thread.title.clone(),
                        harness_kind: thread.harness_kind.clone(),
                        run_status: thread.run_status.clone(),
                        is_active: active_thread_id == Some(&thread.id),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn activate_thread(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        thread_id: HarnessThreadId,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_path) = workspace_storage_key(&workspace, cx) else {
            return;
        };
        self.workspace = workspace;
        self.active_thread_by_path.insert(workspace_path, thread_id);
        cx.notify();
    }

    pub(crate) fn start_thread(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        cx: &mut Context<Self>,
    ) -> Option<HarnessThreadId> {
        let workspace_path = workspace_storage_key(&workspace, cx)?;
        self.workspace = workspace.clone();
        let thread_id = HarnessThreadId(format!("thread-{}", self.next_thread_number).into());
        self.next_thread_number += 1;

        let thread = HarnessThread {
            id: thread_id.clone(),
            provider_thread_id: None,
            title: "New thread".into(),
            cwd: workspace_root_path(&workspace, cx)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            harness_kind: HarnessKind::Codex,
            run_status: HarnessRunStatus::Idle,
            messages: Vec::new(),
        };

        self.threads_by_path
            .entry(workspace_path.clone())
            .or_default()
            .push(thread);
        self.active_thread_by_path
            .insert(workspace_path, thread_id.clone());
        cx.notify();
        Some(thread_id)
    }

    fn send_message(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_composer(window, cx);
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_attachments = !self.pending_attachments.is_empty();

        let text = self.composer_editor.update(cx, |editor, cx| {
            let text = editor.text(cx).trim().to_string();
            if !text.is_empty() || has_attachments {
                editor.clear(window, cx);
            }
            text
        });

        if text.is_empty() && !has_attachments {
            return;
        }

        let attachments = std::mem::take(&mut self.pending_attachments);
        let combined_input = build_input_with_attachments(&text, &attachments);

        let workspace = self.workspace.clone();
        let Some(workspace_path) = workspace_storage_key(&workspace, cx) else {
            return;
        };
        let thread_id = if let Some(existing) = self.active_thread_by_path.get(&workspace_path) {
            existing.clone()
        } else if let Some(new_id) = self.start_thread(workspace.clone(), cx) {
            new_id
        } else {
            return;
        };

        let Some(request) = self.prepare_turn_request(thread_id.clone(), combined_input, cx) else {
            return;
        };

        let (updates_sender, updates_receiver) = channel::unbounded();
        cx.background_spawn(run_codex_app_server_turn(request, updates_sender))
            .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(update) = updates_receiver.recv().await {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.apply_turn_update(update, cx);
                }) {
                    log::debug!("failed to apply harness update: {error}");
                    break;
                }
            }
        })
        .detach();

        cx.notify();
    }

    fn prepare_turn_request(
        &mut self,
        thread_id: HarnessThreadId,
        text: String,
        cx: &mut Context<Self>,
    ) -> Option<HarnessTurnRequest> {
        let thread = self.thread_mut(&thread_id)?;
        if thread.run_status.is_active() {
            thread.messages.push(TranscriptMessage {
                role: TranscriptRole::System,
                text:
                    "Codex is already working on this thread. Please wait for this turn to finish."
                        .to_string(),
            });
            cx.notify();
            return None;
        }

        if thread.messages.is_empty() {
            thread.title = text
                .lines()
                .next()
                .unwrap_or("New thread")
                .chars()
                .take(48)
                .collect::<String>()
                .into();
        }

        thread.messages.push(TranscriptMessage {
            role: TranscriptRole::User,
            text: text.clone(),
        });
        thread.messages.push(TranscriptMessage {
            role: TranscriptRole::Assistant,
            text: String::new(),
        });
        thread.run_status = HarnessRunStatus::Connecting;

        Some(HarnessTurnRequest {
            thread_id,
            provider_thread_id: thread.provider_thread_id.clone(),
            cwd: thread.cwd.clone(),
            input: text,
            model: "gpt-5.4".to_string(),
            reasoning_effort: "high".to_string(),
            approval_policy: HarnessApprovalPolicy::Never,
            sandbox_policy: HarnessSandboxPolicy::DangerFullAccess,
        })
    }

    fn apply_turn_update(&mut self, update: HarnessTurnUpdate, cx: &mut Context<Self>) {
        match update {
            HarnessTurnUpdate::Status { thread_id, status } => {
                if let Some(thread) = self.thread_mut(&thread_id)
                    && !matches!(thread.run_status, HarnessRunStatus::Failed(_))
                {
                    thread.run_status = status;
                }
            }
            HarnessTurnUpdate::ThreadReady {
                thread_id,
                provider_thread_id,
            } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.provider_thread_id = Some(provider_thread_id);
                }
            }
            HarnessTurnUpdate::AssistantDelta { thread_id, delta } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.run_status = HarnessRunStatus::Running;
                    match thread.messages.last_mut() {
                        Some(message) if matches!(message.role, TranscriptRole::Assistant) => {
                            message.text.push_str(&delta);
                        }
                        _ => thread.messages.push(TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: delta,
                        }),
                    }
                }
            }
            HarnessTurnUpdate::ToolEvent {
                thread_id,
                item_id,
                kind,
                phase,
                title,
                detail,
            } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.run_status = HarnessRunStatus::Running;
                    let display_kind = ToolDisplayKind::from_harness(&kind);

                    let existing = thread.messages.iter_mut().rev().find(|message| {
                        match (&message.role, item_id.as_ref()) {
                            (
                                TranscriptRole::Tool {
                                    item_id: Some(existing_id),
                                    ..
                                },
                                Some(new_id),
                            ) => existing_id == new_id,
                            (
                                TranscriptRole::Tool {
                                    item_id: None,
                                    kind: existing_kind,
                                    title: existing_title,
                                    status,
                                    ..
                                },
                                None,
                            ) => {
                                *existing_kind == display_kind
                                    && existing_title == &title
                                    && *status == ToolStatus::Running
                            }
                            _ => false,
                        }
                    });

                    if let Some(message) = existing {
                        if let TranscriptRole::Tool {
                            status,
                            title: existing_title,
                            ..
                        } = &mut message.role
                        {
                            if !title.is_empty() {
                                *existing_title = title.clone();
                            }
                            *status = match phase {
                                HarnessToolPhase::End => ToolStatus::Completed,
                                _ => ToolStatus::Running,
                            };
                        }
                        if !detail.is_empty() {
                            if matches!(display_kind, ToolDisplayKind::Reasoning) {
                                message.text.push_str(&detail);
                            } else {
                                if !message.text.is_empty() {
                                    message.text.push('\n');
                                }
                                message.text.push_str(&detail);
                            }
                        }
                    } else {
                        let status = match phase {
                            HarnessToolPhase::End => ToolStatus::Completed,
                            _ => ToolStatus::Running,
                        };
                        thread.messages.push(TranscriptMessage {
                            role: TranscriptRole::Tool {
                                item_id: item_id.clone(),
                                kind: display_kind,
                                status,
                                title,
                            },
                            text: detail.to_string(),
                        });
                    }
                }
            }
            HarnessTurnUpdate::Finished { thread_id } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.run_status = HarnessRunStatus::Idle;
                    for message in thread.messages.iter_mut() {
                        if let TranscriptRole::Tool { status, .. } = &mut message.role {
                            if *status == ToolStatus::Running {
                                *status = ToolStatus::Completed;
                            }
                        }
                    }
                }
            }
            HarnessTurnUpdate::Failed { thread_id, message } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.run_status = HarnessRunStatus::Failed(message.clone());
                    for transcript_message in thread.messages.iter_mut() {
                        if let TranscriptRole::Tool { status, .. } = &mut transcript_message.role {
                            if *status == ToolStatus::Running {
                                *status = ToolStatus::Failed;
                            }
                        }
                    }
                    thread.messages.push(TranscriptMessage {
                        role: TranscriptRole::System,
                        text: format!("Codex failed: {message}"),
                    });
                }
            }
        }

        cx.notify();
    }

    fn thread_mut(&mut self, thread_id: &HarnessThreadId) -> Option<&mut HarnessThread> {
        self.threads_by_path
            .values_mut()
            .flat_map(|threads| threads.iter_mut())
            .find(|thread| &thread.id == thread_id)
    }

    fn active_thread(&self, cx: &App) -> Option<&HarnessThread> {
        let workspace_path = workspace_storage_key(&self.workspace, cx)?;
        let thread_id = self.active_thread_by_path.get(&workspace_path)?;
        self.threads_by_path
            .get(&workspace_path)?
            .iter()
            .find(|thread| &thread.id == thread_id)
    }

    pub(crate) fn serialize_threads(&self) -> Vec<SerializedThreadGroup> {
        self.threads_by_path
            .iter()
            .filter_map(|(workspace_path, threads)| {
                if threads.is_empty() {
                    return None;
                }
                Some(SerializedThreadGroup {
                    workspace_path: workspace_path.clone(),
                    active_thread_id: self
                        .active_thread_by_path
                        .get(workspace_path)
                        .map(|id| id.0.to_string()),
                    threads: threads
                        .iter()
                        .map(|thread| SerializedHarnessThread {
                            id: thread.id.0.to_string(),
                            provider_thread_id: thread.provider_thread_id.clone(),
                            title: thread.title.to_string(),
                            cwd: thread.cwd.clone(),
                            messages: thread
                                .messages
                                .iter()
                                .map(|message| SerializedTranscriptMessage {
                                    role: match &message.role {
                                        TranscriptRole::User => SerializedTranscriptRole::User,
                                        TranscriptRole::Assistant => {
                                            SerializedTranscriptRole::Assistant
                                        }
                                        TranscriptRole::System => SerializedTranscriptRole::System,
                                        TranscriptRole::Tool {
                                            item_id,
                                            kind,
                                            status,
                                            title,
                                        } => SerializedTranscriptRole::Tool {
                                            title: title.to_string(),
                                            item_id: item_id.clone(),
                                            tool_kind: SerializedToolKind::from_display(*kind),
                                            status: SerializedToolStatus::from_status(*status),
                                        },
                                    },
                                    text: message.text.clone(),
                                })
                                .collect(),
                        })
                        .collect(),
                })
            })
            .collect()
    }

    pub(crate) fn restore_threads(&mut self, groups: Vec<SerializedThreadGroup>) {
        self.threads_by_path.clear();
        self.active_thread_by_path.clear();
        let mut max_thread_number: usize = 0;

        for group in groups {
            let mut threads = Vec::with_capacity(group.threads.len());
            for serialized in group.threads {
                if let Some(number) = serialized
                    .id
                    .strip_prefix("thread-")
                    .and_then(|tail| tail.parse::<usize>().ok())
                {
                    if number > max_thread_number {
                        max_thread_number = number;
                    }
                }

                let messages = serialized
                    .messages
                    .into_iter()
                    .map(|message| TranscriptMessage {
                        role: match message.role {
                            SerializedTranscriptRole::User => TranscriptRole::User,
                            SerializedTranscriptRole::Assistant => TranscriptRole::Assistant,
                            SerializedTranscriptRole::System => TranscriptRole::System,
                            SerializedTranscriptRole::Tool {
                                title,
                                item_id,
                                tool_kind,
                                status,
                            } => TranscriptRole::Tool {
                                item_id,
                                kind: tool_kind.into_display(),
                                status: status.into_status(),
                                title: title.into(),
                            },
                        },
                        text: message.text,
                    })
                    .collect();

                threads.push(HarnessThread {
                    id: HarnessThreadId(serialized.id.into()),
                    provider_thread_id: serialized.provider_thread_id,
                    title: serialized.title.into(),
                    cwd: serialized.cwd,
                    harness_kind: HarnessKind::Codex,
                    run_status: HarnessRunStatus::Idle,
                    messages,
                });
            }

            if !threads.is_empty() {
                if let Some(active) = group.active_thread_id {
                    self.active_thread_by_path
                        .insert(group.workspace_path.clone(), HarnessThreadId(active.into()));
                }
                self.threads_by_path.insert(group.workspace_path, threads);
            }
        }

        if self.next_thread_number <= max_thread_number {
            self.next_thread_number = max_thread_number + 1;
        }
    }

    pub(crate) fn next_thread_number(&self) -> usize {
        self.next_thread_number
    }

    pub(crate) fn set_next_thread_number(&mut self, value: usize) {
        if value > self.next_thread_number {
            self.next_thread_number = value;
        }
    }
}

impl Render for AgentsSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = workspace_display_name(&self.workspace, cx);
        let active_thread = self.active_thread(cx).cloned();
        let titlebar_item = self.workspace.read(cx).titlebar_item();
        let preview_overlay = self.render_attachment_preview(cx);
        let colors = cx.theme().colors();

        v_flex()
            .id("agents-surface")
            .key_context(CODEX_COMPOSER_KEY_CONTEXT)
            .on_action(cx.listener(Self::send_message))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.editor_background)
            .on_mouse_down(MouseButton::Left, |_, window, _cx| {
                window.blur();
            })
            .children(titlebar_item)
            .child(
                h_flex()
                    .h(px(36.0))
                    .px_4()
                    .items_center()
                    .child(Label::new("New thread").size(LabelSize::Small)),
            )
            .child(match active_thread {
                Some(thread) => self.render_transcript(&thread, window, cx),
                None => self.render_welcome(project_name, cx).into_any_element(),
            })
            .child(
                v_flex()
                    .items_center()
                    .pb_5()
                    .gap_2()
                    .child(self.render_composer(cx))
                    .child(self.render_run_controls(cx)),
            )
            .children(preview_overlay)
            .into_any_element()
    }
}

impl AgentsSurface {
    fn render_transcript(
        &self,
        thread: &HarnessThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let thread_id = thread.id.0.clone();
        let mut messages = Vec::new();
        for (index, message) in thread.messages.iter().enumerate() {
            messages.push(self.render_message(thread_id.clone(), index, message, cx));
        }

        let status_indicator = thread.run_status.is_active().then(|| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(
                    Label::new(match thread.run_status {
                        HarnessRunStatus::Connecting => "Connecting to Codex",
                        HarnessRunStatus::Thinking => "Codex is thinking",
                        HarnessRunStatus::Running => "Codex is working",
                        _ => "Codex is working",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
        });

        v_flex()
            .id("codex-transcript-container")
            .flex_1()
            .w_full()
            .relative()
            .overflow_hidden()
            .child(
                v_flex()
                    .id("codex-transcript-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.transcript_scroll_handle)
                    .items_center()
                    .child(
                        v_flex()
                            .w(CODEX_COMPOSER_WIDTH)
                            .gap_4()
                            .py_8()
                            .children(messages)
                            .children(status_indicator),
                    ),
            )
            .vertical_scrollbar_for(&self.transcript_scroll_handle, window, cx)
            .into_any_element()
    }

    fn render_message(
        &self,
        thread_id: SharedString,
        index: usize,
        message: &TranscriptMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let TranscriptRole::Tool {
            kind,
            status,
            title,
            ..
        } = &message.role
        {
            return self.render_tool_message(
                thread_id,
                index,
                *kind,
                *status,
                title,
                &message.text,
                cx,
            );
        }

        let colors = cx.theme().colors();
        let (label, label_color, text_color, background): (SharedString, Color, Color, gpui::Hsla) =
            match &message.role {
                TranscriptRole::User => (
                    "You".into(),
                    Color::Muted,
                    Color::Default,
                    colors.element_background,
                ),
                TranscriptRole::Assistant => (
                    "Codex".into(),
                    Color::Muted,
                    Color::Default,
                    colors.editor_background,
                ),
                TranscriptRole::System => (
                    "System".into(),
                    Color::Warning,
                    Color::Warning,
                    colors.element_hover,
                ),
                TranscriptRole::Tool { .. } => unreachable!(),
            };

        v_flex()
            .id(("harness-message", index))
            .w_full()
            .gap_1()
            .rounded_lg()
            .bg(background)
            .p_3()
            .child(Label::new(label).size(LabelSize::Small).color(label_color))
            .child(
                div()
                    .text_color(text_color.color(cx))
                    .text_sm()
                    .whitespace_normal()
                    .child(if message.text.is_empty() {
                        SharedString::from(" ")
                    } else {
                        SharedString::from(message.text.clone())
                    }),
            )
            .into_any_element()
    }

    fn render_tool_message(
        &self,
        thread_id: SharedString,
        index: usize,
        kind: ToolDisplayKind,
        status: ToolStatus,
        title: &SharedString,
        body: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_reasoning = matches!(kind, ToolDisplayKind::Reasoning);
        let key = (thread_id, index);
        let expanded = self.expanded_tool_messages.contains(&key);
        let has_body = !body.trim().is_empty();
        let is_running = status == ToolStatus::Running;
        // While the tool is running we always allow expanding so the user can
        // watch streaming output as it arrives. After it finishes we only show
        // the chevron when there's actually something to look at.
        let is_expandable = has_body || is_running;
        let colors = cx.theme().colors();

        let summary = if is_reasoning {
            None
        } else {
            Some(tool_summary_line(title, kind))
        };

        let chevron_icon = if !is_expandable {
            None
        } else if expanded {
            Some(IconName::ChevronDown)
        } else {
            Some(IconName::ChevronRight)
        };

        let kind_icon_color = match status {
            ToolStatus::Failed => Color::Error,
            _ if is_reasoning => Color::Muted,
            _ => Color::Muted,
        };

        let title_element: AnyElement = if is_reasoning {
            if status == ToolStatus::Running {
                animated_thinking_label().into_any_element()
            } else {
                Label::new("Thought")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }
        } else {
            let (label_text, label_color) = if let Some((primary, _)) = &summary {
                (primary.clone(), Color::Default)
            } else {
                (title.clone(), Color::Default)
            };
            Label::new(label_text)
                .size(LabelSize::Small)
                .color(label_color)
                .into_any_element()
        };

        let detail_element: Option<AnyElement> = match (is_reasoning, &summary) {
            (false, Some((_, Some(detail)))) => Some(
                Label::new(detail.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate()
                    .into_any_element(),
            ),
            _ => None,
        };

        let header_key = key.clone();
        let header = h_flex()
            .id(("harness-tool-header", index))
            .w_full()
            .gap_1p5()
            .items_center()
            .when(is_expandable, |this| {
                this.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_tool_expansion(header_key.0.clone(), header_key.1, cx);
                    }))
            })
            .when_some(chevron_icon, |this, icon_name| {
                this.child(
                    Icon::new(icon_name)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(chevron_icon.is_none(), |this| {
                this.child(div().w(px(12.0)))
            })
            .child(
                Icon::new(kind.icon())
                    .size(IconSize::XSmall)
                    .color(kind_icon_color),
            )
            .child(title_element)
            .when_some(detail_element, |this, element| {
                this.child(div().min_w_0().flex_1().child(element))
            })
            .when(status == ToolStatus::Running && !is_reasoning, |this| {
                this.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::XSmall)
                        .color(Color::Accent)
                        .with_rotate_animation(2),
                )
            });

        let mut row = v_flex().id(("harness-tool-row", index)).w_full().gap_1();
        row = row.child(header);

        if expanded {
            let body_element: AnyElement = if has_body {
                if matches!(kind, ToolDisplayKind::Command) {
                    self.render_command_body(body, status, cx)
                } else if kind.body_is_monospace() {
                    div()
                        .ml(px(20.0))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(colors.element_background)
                        .border_1()
                        .border_color(colors.border)
                        .text_xs()
                        .text_color(Color::Muted.color(cx))
                        .font_buffer(cx)
                        .whitespace_normal()
                        .child(SharedString::from(body.to_string()))
                        .into_any_element()
                } else {
                    div()
                        .ml(px(20.0))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(colors.element_background)
                        .border_1()
                        .border_color(colors.border)
                        .text_sm()
                        .text_color(Color::Muted.color(cx))
                        .whitespace_normal()
                        .child(SharedString::from(body.to_string()))
                        .into_any_element()
                }
            } else if is_running {
                div()
                    .ml(px(20.0))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(colors.element_background)
                    .border_1()
                    .border_color(colors.border)
                    .text_sm()
                    .text_color(Color::Muted.color(cx))
                    .child(SharedString::from("Waiting for output…"))
                    .into_any_element()
            } else {
                div().into_any_element()
            };
            row = row.child(body_element);
        }

        row.into_any_element()
    }

    fn render_command_body(
        &self,
        body: &str,
        status: ToolStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        // The harness formats command details as `$ {cmd}\n{stdout}`. Split on
        // the first line so we can render the shell prompt and the output as
        // separate blocks.
        let mut lines = body.splitn(2, '\n');
        let first_line = lines.next().unwrap_or("");
        let remainder = lines.next().unwrap_or("");

        let (command_text, output_text): (SharedString, Option<SharedString>) =
            if let Some(stripped) = first_line.strip_prefix("$ ") {
                let command: SharedString = stripped.to_string().into();
                let output = if remainder.trim().is_empty() {
                    None
                } else {
                    Some(SharedString::from(remainder.to_string()))
                };
                (command, output)
            } else {
                (first_line.to_string().into(), {
                    if remainder.trim().is_empty() {
                        None
                    } else {
                        Some(SharedString::from(remainder.to_string()))
                    }
                })
            };

        let mut card = v_flex()
            .ml(px(20.0))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .bg(colors.element_background)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        Label::new("Shell")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(Color::Default.color(cx))
                    .whitespace_normal()
                    .child(SharedString::from(format!("$ {command_text}"))),
            );

        if let Some(output) = output_text {
            card = card.child(
                div()
                    .px_3()
                    .pt_1()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(Color::Muted.color(cx))
                    .whitespace_normal()
                    .child(output),
            );
        }

        let (status_icon, status_label, status_color) = match status {
            ToolStatus::Completed => (IconName::Check, "Success", Color::Success),
            ToolStatus::Failed => (IconName::XCircle, "Failed", Color::Error),
            ToolStatus::Running => (IconName::LoadCircle, "Running", Color::Accent),
        };

        card = card.child(
            h_flex()
                .px_3()
                .py_1p5()
                .gap_1()
                .justify_end()
                .child(
                    Icon::new(status_icon)
                        .size(IconSize::XSmall)
                        .color(status_color),
                )
                .child(
                    Label::new(status_label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );

        card.into_any_element()
    }

    fn render_welcome(
        &self,
        project_name: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();

        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .size(px(72.0))
                    .rounded_full()
                    .border_1()
                    .border_color(colors.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::XLarge)
                            .color(Color::Muted),
                    ),
            )
            .child(Label::new("Let's build").size(LabelSize::Large))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(project_name)
                            .size(LabelSize::Large)
                            .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let attachments_element = if self.pending_attachments.is_empty() {
            None
        } else {
            Some(self.render_attachments(cx))
        };
        let drop_overlay = self.render_drop_overlay(cx);
        let colors = cx.theme().colors();

        v_flex()
            .id("codex-composer")
            .w(CODEX_COMPOSER_WIDTH)
            .relative()
            .rounded_xl()
            .border_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.focus_composer(window, cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.border_color(cx.theme().colors().border_focused)
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let new_paths: Vec<PathBuf> = paths
                    .paths()
                    .iter()
                    .filter(|path| path.is_file())
                    .cloned()
                    .collect();
                if !new_paths.is_empty() {
                    this.add_attachments(new_paths, cx);
                }
            }))
            .child(drop_overlay)
            .children(attachments_element)
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(self.composer_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .pb_2()
                    .gap_1()
                    .items_center()
                    .child(
                        IconButton::new("codex-composer-attach", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Attach files or images"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_attachments(window, cx);
                            })),
                    )
                    .child(
                        Button::new("codex-composer-model", "GPT-5.4")
                            .label_size(LabelSize::Small)
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                            .style(ButtonStyle::Subtle),
                    )
                    .child(
                        Button::new("codex-composer-reasoning", "High")
                            .label_size(LabelSize::Small)
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                            .style(ButtonStyle::Subtle),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("codex-composer-mic", IconName::Mic)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Voice input")),
                    )
                    .child(
                        IconButton::new("codex-composer-send", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Filled)
                            .tooltip(Tooltip::text("Send message"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_composer(window, cx);
                            })),
                    ),
            )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> AnyElement {
        let border_color = cx.theme().colors().border;
        let chip_background = cx.theme().colors().element_background;
        let chips: Vec<AnyElement> = self
            .pending_attachments
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let name = attachment_display_name(path);
                let full_path: SharedString = path.to_string_lossy().to_string().into();
                let is_image = is_image_path(path);

                let thumbnail: AnyElement = if is_image {
                    div()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex_shrink_0()
                        .overflow_hidden()
                        .rounded_sm()
                        .child(
                            img(path.clone())
                                .object_fit(ObjectFit::Cover)
                                .w_full()
                                .h_full(),
                        )
                        .into_any_element()
                } else {
                    Icon::new(attachment_icon(path))
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element()
                };

                let clickable_path = path.clone();
                let chip = h_flex()
                    .id(("codex-attachment", index))
                    .h(px(28.0))
                    .gap_1p5()
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(border_color)
                    .bg(chip_background)
                    .items_center()
                    .tooltip(Tooltip::text(full_path))
                    .when(is_image, |this| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview_attachment(clickable_path.clone(), cx);
                            }))
                    })
                    .child(thumbnail)
                    .child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .color(Color::Default)
                            .truncate(),
                    )
                    .child(
                        IconButton::new(("codex-attachment-remove", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Remove attachment"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(index, cx);
                            })),
                    );

                chip.into_any_element()
            })
            .collect();

        h_flex()
            .px_3()
            .pt_3()
            .pb_1()
            .gap_2()
            .flex_wrap()
            .children(chips)
            .into_any_element()
    }

    fn render_drop_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let drop_target_background = cx.theme().colors().drop_target_background;
        let border_focused = cx.theme().colors().border_focused;
        div()
            .invisible()
            .absolute()
            .inset_0()
            .size_full()
            .rounded_xl()
            .bg(drop_target_background)
            .border_2()
            .border_color(border_focused)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .drag_over::<ExternalPaths>(|this, _, _, _| this.visible())
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(
                Label::new("Drop files here to attach")
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .into_any_element()
    }

    fn render_attachment_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.previewing_attachment.as_ref()?;
        let colors = cx.theme().colors();
        let display_name = attachment_display_name(path);
        let preview_path = path.clone();

        Some(
            deferred(
                div()
                    .id("codex-attachment-preview-overlay")
                    .absolute()
                    .inset_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::black().opacity(0.75))
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dismiss_preview(cx);
                        }),
                    )
                    .child(
                        v_flex()
                            .id("codex-attachment-preview-card")
                            .w(px(960.0))
                            .gap_2()
                            .rounded_xl()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.panel_background)
                            .p_3()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Label::new(display_name)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        IconButton::new(
                                            "codex-attachment-preview-close",
                                            IconName::Close,
                                        )
                                        .icon_size(IconSize::Small)
                                        .style(ButtonStyle::Subtle)
                                        .tooltip(Tooltip::text("Close preview"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.dismiss_preview(cx);
                                        })),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(640.0))
                                    .overflow_hidden()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        img(preview_path)
                                            .object_fit(ObjectFit::Contain)
                                            .w_full()
                                            .h_full(),
                                    ),
                            ),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn render_run_controls(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w(CODEX_COMPOSER_WIDTH)
            .px_2()
            .gap_3()
            .items_center()
            .child(
                Button::new("codex-env-selector", "Local")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .child(
                Button::new("codex-permission-selector", "Full access")
                    .label_size(LabelSize::Small)
                    .color(Color::Warning)
                    .start_icon(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .child(div().flex_1())
            .child(
                Button::new("codex-branch-selector", "main")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
    }
}

