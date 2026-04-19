use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, ExternalPaths, Focusable, MouseButton,
    ObjectFit, PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, Subscription, Task,
    Window, deferred, img, px,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use language::language_settings::SoftWrap;
use menu::Confirm;
use smol::channel;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use ui::{
    CircularProgress, CommonAnimationExt, ContextMenu, ContextMenuEntry, PopoverMenu, TintColor,
    Tooltip, WithScrollbar, prelude::*,
};
use terminal_view::terminal_panel::Toggle as ToggleTerminalPanel;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, NewThread, ToggleWorkspaceMode};

use crate::COMPOSER_KEY_CONTEXT;
use crate::harness::{
    HarnessApprovalPolicy, HarnessKind, HarnessRunStatus, HarnessSandboxPolicy,
    HarnessSessionConfig, HarnessThreadId, HarnessToolPhase, HarnessTurnInput, HarnessTurnUpdate,
    run_codex_app_server_session,
};
use crate::helpers::{
    animated_thinking_label, attachment_display_name, attachment_icon, build_input_with_attachments,
    is_image_path, tool_summary_line, url_has_scheme, workspace_display_name,
    workspace_root_path, workspace_storage_key,
};
use crate::serialization::{
    SerializedHarnessThread, SerializedThreadGroup, SerializedToolKind, SerializedToolStatus,
    SerializedTranscriptMessage, SerializedTranscriptRole,
};
use crate::transcript::{
    HarnessThread, HarnessThreadSummary, ToolDisplayKind, ToolStatus, TranscriptMessage,
    TranscriptRole,
};

const COMPOSER_WIDTH: Pixels = px(720.0);

fn model_context_window(model: &str) -> usize {
    match model {
        "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.3-codex" => 256_000,
        "gpt-5.2" => 128_000,
        _ => 256_000,
    }
}

fn permission_trigger_presentation(
    policy: &HarnessSandboxPolicy,
) -> (&'static str, IconName, Color) {
    match policy {
        HarnessSandboxPolicy::WorkspaceWrite => {
            ("Default permissions", IconName::LockOutlined, Color::Muted)
        }
        HarnessSandboxPolicy::DangerFullAccess => {
            ("Full access", IconName::Warning, Color::Warning)
        }
    }
}

fn permission_options() -> [(HarnessSandboxPolicy, &'static str, IconName); 2] {
    [
        (
            HarnessSandboxPolicy::WorkspaceWrite,
            "Default permissions",
            IconName::LockOutlined,
        ),
        (
            HarnessSandboxPolicy::DangerFullAccess,
            "Full access",
            IconName::Warning,
        ),
    ]
}

const AVAILABLE_MODELS: &[(&str, &str)] = &[
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4-Mini"),
    ("gpt-5.3-codex", "GPT-5.3-Codex"),
    ("gpt-5.2", "GPT-5.2"),
];

const DEFAULT_MODEL: &str = "gpt-5.4";

const REASONING_EFFORTS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
];

const DEFAULT_REASONING_EFFORT: &str = "high";

struct CodexSessionHandle {
    turns: channel::Sender<HarnessTurnInput>,
    _session_task: Task<()>,
    _update_task: Task<()>,
}

pub(crate) enum AgentsSurfaceEvent {
    OpenedInEditor,
    ToggleModeRequested,
    NewThreadRequested,
    ToggleTerminalRequested,
}

fn format_token_count(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f32 / 1_000_000.0)
    } else if tokens >= 1000 {
        format!("{}k", tokens / 1000)
    } else {
        tokens.to_string()
    }
}

fn resolve_agent_link(
    workspace: &Entity<workspace::Workspace>,
    url: &str,
    cx: &App,
) -> Option<PathBuf> {
    let path = std::path::Path::new(url);

    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }

    let project = workspace.read(cx).project().clone();
    let worktrees: Vec<_> = project.read(cx).worktrees(cx).collect();

    // First try joining the link to each worktree root — this covers paths
    // that are already relative to a project root.
    for worktree in &worktrees {
        let abs_path = worktree.read(cx).abs_path();
        let candidate = abs_path.join(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Fallback: scan every tracked file and match on a trailing path segment.
    // Codex sometimes references files by a partial path like
    // `subdir/file.cpp` or just the basename.
    let needle = url.trim_start_matches('/');
    let needle_components: Vec<&str> = needle.split('/').filter(|s| !s.is_empty()).collect();
    if needle_components.is_empty() {
        return None;
    }

    for worktree in &worktrees {
        let worktree_ref = worktree.read(cx);
        for entry in worktree_ref.entries(true, 0) {
            if entry.is_dir() {
                continue;
            }
            let entry_unix = entry.path.as_unix_str();
            let entry_components: Vec<&str> = entry_unix.split('/').collect();
            if entry_components.len() < needle_components.len() {
                continue;
            }
            let tail = &entry_components[entry_components.len() - needle_components.len()..];
            if tail == needle_components.as_slice() {
                return Some(worktree_ref.absolutize(&entry.path));
            }
        }
    }

    None
}

fn should_skip_message(message: &TranscriptMessage) -> bool {
    match &message.role {
        TranscriptRole::Assistant if message.text.is_empty() => true,
        _ => false,
    }
}

fn format_thinking_duration(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        return "<1s".to_string();
    }
    let total_seconds = duration_ms / 1000;
    if total_seconds < 60 {
        format!("{total_seconds}s")
    } else {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes}m {seconds:02}s")
    }
}

fn should_show_role_header(
    index: usize,
    message: &TranscriptMessage,
    all: &[TranscriptMessage],
) -> bool {
    // Tool messages render their own header; this helper only governs the
    // User/Assistant/System bubble label.
    if matches!(message.role, TranscriptRole::Tool { .. }) {
        return true;
    }
    // Scan backwards past intervening tool messages: an assistant message
    // that follows another assistant's chunk (with tool calls in between)
    // is part of the same Codex response, so suppress the duplicate label.
    for prev in all[..index].iter().rev() {
        match (&message.role, &prev.role) {
            (TranscriptRole::Assistant, TranscriptRole::Tool { .. }) => continue,
            (TranscriptRole::Assistant, TranscriptRole::Assistant) => return false,
            _ => return true,
        }
    }
    true
}

fn reset_or_create_markdown(
    cache: &mut HashMap<(SharedString, usize), (SharedString, Entity<Markdown>)>,
    key: (SharedString, usize),
    source: SharedString,
    cx: &mut App,
) -> Entity<Markdown> {
    match cache.get_mut(&key) {
        Some(entry) => {
            if entry.0 != source {
                entry.0 = source.clone();
                entry.1.update(cx, |md, cx| md.reset(source, cx));
            }
            entry.1.clone()
        }
        None => {
            let entity = cx.new(|cx| Markdown::new(source.clone(), None, None, cx));
            cache.insert(key, (source, entity.clone()));
            entity
        }
    }
}

pub struct AgentsSurface {
    workspace: Entity<workspace::Workspace>,
    composer_editor: Entity<Editor>,
    pub(crate) active_thread_by_path: HashMap<String, HarnessThreadId>,
    pub(crate) threads_by_path: HashMap<String, Vec<HarnessThread>>,
    next_thread_number: usize,
    pending_attachments: Vec<PathBuf>,
    previewing_attachment: Option<PathBuf>,
    selected_model: String,
    selected_reasoning_effort: String,
    selected_sandbox_policy: HarnessSandboxPolicy,
    codex_sessions: HashMap<HarnessThreadId, CodexSessionHandle>,
    expanded_tool_messages: HashSet<(SharedString, usize)>,
    markdown_cache: HashMap<(SharedString, usize), (SharedString, Entity<Markdown>)>,
    transcript_scroll_handle: ScrollHandle,
    /// When true, new transcript content pins the scroll position to the
    /// bottom. Turns off once the user scrolls up and re-engages when they
    /// return to the bottom.
    stick_to_bottom: bool,
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
                "Ask anything, @ to add files, / for commands",
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
            selected_model: DEFAULT_MODEL.to_string(),
            selected_reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            selected_sandbox_policy: HarnessSandboxPolicy::DangerFullAccess,
            codex_sessions: HashMap::new(),
            expanded_tool_messages: HashSet::new(),
            markdown_cache: HashMap::new(),
            transcript_scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
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
            estimated_tokens_used: 0,
            has_reported_tokens: false,
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

    fn is_turn_running(&self, cx: &App) -> bool {
        self.active_thread(cx)
            .is_some_and(|thread| thread.run_status.is_active())
    }

    fn stop_active_turn(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_path) = workspace_storage_key(&self.workspace, cx) else {
            return;
        };
        let Some(thread_id) = self.active_thread_by_path.get(&workspace_path).cloned() else {
            return;
        };

        // Drop the active thread's codex session to kill its child process
        // and cancel the in-flight turn. Other threads' sessions are left
        // untouched so concurrent runs keep going. The next submit on this
        // thread will start a fresh session via thread/resume.
        self.codex_sessions.remove(&thread_id);

        if let Some(thread) = self.thread_mut(&thread_id) {
            thread.run_status = HarnessRunStatus::Idle;
            for message in thread.messages.iter_mut() {
                if let TranscriptRole::Tool { status, .. } = &mut message.role {
                    if *status == ToolStatus::Running {
                        *status = ToolStatus::Failed;
                    }
                }
            }
        }
        cx.notify();
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_attachments = !self.pending_attachments.is_empty();
        let text = self.composer_editor.read(cx).text(cx).trim().to_string();

        if text.is_empty() && !has_attachments {
            return;
        }

        let workspace = self.workspace.clone();
        let Some(workspace_path) = workspace_storage_key(&workspace, cx) else {
            return;
        };
        let thread_id = if let Some(existing) = self.active_thread_by_path.get(&workspace_path) {
            existing.clone()
        } else if let Some(new_id) = self.start_thread(workspace, cx) {
            new_id
        } else {
            return;
        };

        // If the thread is already running a turn, surface a system message
        // and keep the composer intact so the user can retry after it
        // finishes rather than losing what they typed.
        if self
            .thread_mut(&thread_id)
            .is_some_and(|thread| thread.run_status.is_active())
        {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.messages.push(TranscriptMessage::new(
                    TranscriptRole::System,
                    "Agent is already working on this thread. Please wait for this turn to finish."
                        .to_string(),
                ));
            }
            cx.notify();
            return;
        }

        self.composer_editor.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });
        let attachments = std::mem::take(&mut self.pending_attachments);

        // Sending a new message re-engages autoscroll so the user always sees
        // their own turn land at the bottom even if they were scrolled up to
        // reread earlier context.
        self.stick_to_bottom = true;

        let Some(turn_input) =
            self.prepare_turn_input(thread_id.clone(), text, attachments, cx)
        else {
            return;
        };

        let Some(session_sender) = self.ensure_codex_session(&thread_id, cx) else {
            return;
        };
        if session_sender.try_send(turn_input).is_err() {
            if let Some(thread) = self.thread_mut(&thread_id) {
                thread.run_status = HarnessRunStatus::Failed("codex session closed".into());
                thread.messages.push(TranscriptMessage::new(
                    TranscriptRole::System,
                    "Agent session closed unexpectedly. Please try again.".to_string(),
                ));
            }
            self.codex_sessions.remove(&thread_id);
        }

        cx.notify();
    }

    fn ensure_codex_session(
        &mut self,
        thread_id: &HarnessThreadId,
        cx: &mut Context<Self>,
    ) -> Option<channel::Sender<HarnessTurnInput>> {
        if let Some(session) = self.codex_sessions.get(thread_id) {
            if !session.turns.is_closed() {
                return Some(session.turns.clone());
            }
            self.codex_sessions.remove(thread_id);
        }

        let thread = self
            .threads_by_path
            .values()
            .flat_map(|threads| threads.iter())
            .find(|thread| &thread.id == thread_id)?;

        let config = HarnessSessionConfig {
            thread_id: thread_id.clone(),
            provider_thread_id: thread.provider_thread_id.clone(),
            cwd: thread.cwd.clone(),
            model: self.selected_model.clone(),
            approval_policy: HarnessApprovalPolicy::Never,
            sandbox_policy: self.selected_sandbox_policy.clone(),
        };

        let (turns_sender, turns_receiver) = channel::unbounded();
        let (updates_sender, updates_receiver) = channel::unbounded();
        let session_task = cx.background_spawn(run_codex_app_server_session(
            config,
            turns_receiver,
            updates_sender,
        ));
        let update_task = cx.spawn(async move |this, cx| {
            while let Ok(update) = updates_receiver.recv().await {
                if let Err(error) = this.update(cx, |this, cx| {
                    this.apply_turn_update(update, cx);
                }) {
                    log::debug!("failed to apply harness update: {error}");
                    break;
                }
            }
        });

        self.codex_sessions.insert(
            thread_id.clone(),
            CodexSessionHandle {
                turns: turns_sender.clone(),
                _session_task: session_task,
                _update_task: update_task,
            },
        );
        Some(turns_sender)
    }

    fn prepare_turn_input(
        &mut self,
        thread_id: HarnessThreadId,
        text: String,
        attachments: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Option<HarnessTurnInput> {
        let thread = self.thread_mut(&thread_id)?;

        if thread.messages.is_empty() {
            let attachment_fallback = attachments
                .first()
                .map(|first| attachment_display_name(first).to_string());
            let title_source: &str = if !text.is_empty() {
                text.as_str()
            } else if let Some(fallback) = attachment_fallback.as_deref() {
                fallback
            } else {
                "New thread"
            };
            thread.title = title_source
                .lines()
                .next()
                .unwrap_or("New thread")
                .chars()
                .take(48)
                .collect::<String>()
                .into();
        }

        let combined_input = build_input_with_attachments(&text, &attachments);

        // Pre-turn estimate is only used until codex reports real usage via
        // turn/completed; once we have real numbers we stop accumulating
        // guesses.
        if !thread.has_reported_tokens {
            thread.estimated_tokens_used += combined_input.len() / 4;
        }
        thread.messages.push(TranscriptMessage {
            role: TranscriptRole::User,
            text,
            attachments,
            started_at: None,
            duration_ms: None,
        });
        thread.run_status = HarnessRunStatus::Connecting;
        cx.notify();

        Some(HarnessTurnInput {
            input: combined_input,
            model: self.selected_model.clone(),
            reasoning_effort: self.selected_reasoning_effort.clone(),
            approval_policy: HarnessApprovalPolicy::Never,
            sandbox_policy: self.selected_sandbox_policy.clone(),
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
                    if !thread.has_reported_tokens {
                        thread.estimated_tokens_used += delta.len() / 4;
                    }
                    match thread.messages.last_mut() {
                        Some(message) if matches!(message.role, TranscriptRole::Assistant) => {
                            message.text.push_str(&delta);
                        }
                        _ => thread
                            .messages
                            .push(TranscriptMessage::new(TranscriptRole::Assistant, delta)),
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
                        let mut transitioned_to_terminal = false;
                        if let TranscriptRole::Tool {
                            status,
                            title: existing_title,
                            ..
                        } = &mut message.role
                        {
                            if !title.is_empty() {
                                *existing_title = title.clone();
                            }
                            let next_status = match phase {
                                HarnessToolPhase::End => ToolStatus::Completed,
                                _ => ToolStatus::Running,
                            };
                            if *status == ToolStatus::Running
                                && next_status != ToolStatus::Running
                            {
                                transitioned_to_terminal = true;
                            }
                            *status = next_status;
                        }
                        if transitioned_to_terminal
                            && message.duration_ms.is_none()
                            && let Some(started) = message.started_at.take()
                        {
                            message.duration_ms =
                                Some(started.elapsed().as_millis().min(u128::from(u64::MAX))
                                    as u64);
                        }
                        if !detail.is_empty() {
                            // Codex can ship tool bodies either as pure deltas
                            // or as cumulative snapshots. Reasoning frequently
                            // arrives as rolling snapshots; command output
                            // typically arrives as a single completion-time
                            // snapshot after any streamed deltas. In both
                            // cases, if the new payload already contains the
                            // existing body, treat it as a snapshot and
                            // replace rather than double-append; otherwise
                            // treat it as a delta.
                            let use_snapshot_merge = matches!(
                                display_kind,
                                ToolDisplayKind::Reasoning | ToolDisplayKind::Command
                            );
                            if use_snapshot_merge
                                && detail.as_ref().starts_with(message.text.as_str())
                                && detail.len() >= message.text.len()
                            {
                                message.text = detail.to_string();
                            } else if matches!(display_kind, ToolDisplayKind::Reasoning) {
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
                        let mut new_message = TranscriptMessage::new(
                            TranscriptRole::Tool {
                                item_id: item_id.clone(),
                                kind: display_kind,
                                status,
                                title,
                            },
                            detail.to_string(),
                        );
                        // Only stamp a start time when the tool is actually
                        // starting — if we get here on a terminal phase with
                        // no prior Running record, we have no duration to
                        // derive.
                        if status == ToolStatus::Running {
                            new_message.started_at = Some(std::time::Instant::now());
                        }
                        thread.messages.push(new_message);
                    }
                }
            }
            HarnessTurnUpdate::TokensUsed {
                thread_id,
                total_tokens,
            } => {
                if let Some(thread) = self.thread_mut(&thread_id) {
                    thread.estimated_tokens_used = total_tokens;
                    thread.has_reported_tokens = true;
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
                    thread.messages.push(TranscriptMessage::new(
                        TranscriptRole::System,
                        format!("Agent failed: {message}"),
                    ));
                }
            }
        }

        cx.notify();
    }

    fn handle_agent_url_click(
        &mut self,
        url: &str,
        workspace_handle: &gpui::WeakEntity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = workspace_handle.upgrade() else {
            if url_has_scheme(url) {
                cx.open_url(url);
            } else {
                log::warn!("agent link clicked but workspace is gone: {url}");
            }
            return;
        };

        let Some(abs_path) = resolve_agent_link(&workspace, url, cx) else {
            if url_has_scheme(url) {
                cx.open_url(url);
            } else {
                log::warn!("no file in project matched link: {url}");
                if let Some(thread) = self.active_thread_mut(cx) {
                    thread.messages.push(TranscriptMessage::new(
                        TranscriptRole::System,
                        format!("Could not find file for link: {url}"),
                    ));
                    cx.notify();
                }
            }
            return;
        };

        // Prefer opening through find_project_path so the file lands in the
        // existing worktree's pane with the correct project metadata; fall
        // back to open_abs_path for files outside any worktree.
        let project = workspace.read(cx).project().clone();
        let project_path = project.read(cx).find_project_path(&abs_path, cx);

        workspace.update(cx, |workspace, cx| {
            if let Some(project_path) = project_path {
                workspace
                    .open_path(project_path, None, true, window, cx)
                    .detach_and_log_err(cx);
            } else {
                workspace
                    .open_abs_path(abs_path.clone(), Default::default(), window, cx)
                    .detach_and_log_err(cx);
            }
        });

        cx.emit(AgentsSurfaceEvent::OpenedInEditor);
    }

    fn active_thread_mut(&mut self, cx: &App) -> Option<&mut HarnessThread> {
        let workspace_path = workspace_storage_key(&self.workspace, cx)?;
        let thread_id = self.active_thread_by_path.get(&workspace_path).cloned()?;
        self.thread_mut(&thread_id)
    }

    fn thread_mut(&mut self, thread_id: &HarnessThreadId) -> Option<&mut HarnessThread> {
        self.threads_by_path
            .values_mut()
            .flat_map(|threads| threads.iter_mut())
            .find(|thread| &thread.id == thread_id)
    }

    fn prune_markdown_cache(&mut self) {
        let mut valid = HashSet::new();
        for threads in self.threads_by_path.values() {
            for thread in threads {
                for index in 0..thread.messages.len() {
                    valid.insert((thread.id.0.clone(), index));
                }
            }
        }
        self.markdown_cache.retain(|key, _| valid.contains(key));
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
                                    attachments: message.attachments.clone(),
                                    duration_ms: message.duration_ms,
                                })
                                .collect(),
                            estimated_tokens: Some(thread.estimated_tokens_used),
                            has_reported_tokens: thread.has_reported_tokens,
                        })
                        .collect(),
                })
            })
            .collect()
    }

    pub(crate) fn restore_threads(&mut self, groups: Vec<SerializedThreadGroup>) {
        self.threads_by_path.clear();
        self.active_thread_by_path.clear();
        self.codex_sessions.clear();
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
                        attachments: message.attachments,
                        started_at: None,
                        duration_ms: message.duration_ms,
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
                    estimated_tokens_used: serialized.estimated_tokens.unwrap_or(0),
                    has_reported_tokens: serialized.has_reported_tokens,
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

        self.prune_markdown_cache();
    }

    pub(crate) fn next_thread_number(&self) -> usize {
        self.next_thread_number
    }

    pub(crate) fn set_next_thread_number(&mut self, value: usize) {
        if value > self.next_thread_number {
            self.next_thread_number = value;
        }
    }

    pub(crate) fn selected_model(&self) -> &str {
        &self.selected_model
    }

    pub(crate) fn set_selected_model(&mut self, model: String) {
        self.selected_model = model;
    }

    pub(crate) fn selected_reasoning_effort(&self) -> &str {
        &self.selected_reasoning_effort
    }

    pub(crate) fn set_selected_reasoning_effort(&mut self, effort: String) {
        self.selected_reasoning_effort = effort;
    }

    pub(crate) fn selected_sandbox_policy(&self) -> &HarnessSandboxPolicy {
        &self.selected_sandbox_policy
    }

    pub(crate) fn set_selected_sandbox_policy(&mut self, policy: HarnessSandboxPolicy) {
        self.selected_sandbox_policy = policy;
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
            .key_context(COMPOSER_KEY_CONTEXT)
            .on_action(cx.listener(Self::send_message))
            .on_action(cx.listener(|_this, _: &ToggleWorkspaceMode, _window, cx| {
                cx.emit(AgentsSurfaceEvent::ToggleModeRequested);
            }))
            .on_action(cx.listener(|_this, _: &NewThread, _window, cx| {
                cx.emit(AgentsSurfaceEvent::NewThreadRequested);
            }))
            .on_action(cx.listener(|_this, _: &ToggleTerminalPanel, _window, cx| {
                cx.emit(AgentsSurfaceEvent::ToggleTerminalRequested);
            }))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.editor_background)
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
        &mut self,
        thread: &HarnessThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Decide whether to keep the transcript pinned to the bottom. GPUI
        // applies wheel deltas to the scroll offset before invoking our
        // re-render, so the offset we read here already reflects the user's
        // latest scroll position.
        //
        // - If the user was being auto-scrolled and has now scrolled up, drop
        //   out of stick mode so we don't fight their input.
        // - If they scroll back to the bottom, re-engage stick mode so new
        //   messages keep appearing without extra clicks.
        let offset_y = self.transcript_scroll_handle.offset().y;
        let max_offset_y = self.transcript_scroll_handle.max_offset().y;
        let at_bottom =
            max_offset_y <= px(0.0) || (offset_y + max_offset_y).abs() <= px(4.0);
        if self.stick_to_bottom != at_bottom {
            self.stick_to_bottom = at_bottom;
        }
        if self.stick_to_bottom {
            self.transcript_scroll_handle.scroll_to_bottom();
        }

        let thread_id = thread.id.0.clone();
        let mut messages = Vec::new();
        for (index, message) in thread.messages.iter().enumerate() {
            if should_skip_message(message) {
                continue;
            }
            let show_header = should_show_role_header(index, message, &thread.messages);
            messages.push(self.render_message(
                thread_id.clone(),
                index,
                message,
                show_header,
                window,
                cx,
            ));
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
                        HarnessRunStatus::Connecting => "Connecting…",
                        HarnessRunStatus::Thinking => "Thinking…",
                        HarnessRunStatus::Running => "Working…",
                        _ => "Working…",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
        });

        v_flex()
            .id("agent-transcript-container")
            .flex_1()
            .w_full()
            .relative()
            .overflow_hidden()
            .child(
                v_flex()
                    .id("agent-transcript-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.transcript_scroll_handle)
                    .items_center()
                    .child(
                        v_flex()
                            .w(COMPOSER_WIDTH)
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
        &mut self,
        thread_id: SharedString,
        index: usize,
        message: &TranscriptMessage,
        show_header: bool,
        window: &mut Window,
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
                message.duration_ms,
                window,
                cx,
            );
        }

        let colors = cx.theme().colors();
        let is_assistant = matches!(message.role, TranscriptRole::Assistant);
        let (label, label_color, background): (SharedString, Color, gpui::Hsla) =
            match &message.role {
                TranscriptRole::User => ("You".into(), Color::Muted, colors.element_background),
                TranscriptRole::Assistant => {
                    ("Codex".into(), Color::Muted, colors.editor_background)
                }
                TranscriptRole::System => {
                    ("System".into(), Color::Warning, colors.element_hover)
                }
                TranscriptRole::Tool { .. } => unreachable!(),
            };

        let skip_body = matches!(message.role, TranscriptRole::User)
            && message.text.is_empty()
            && !message.attachments.is_empty();

        let body: AnyElement = if skip_body {
            div().into_any_element()
        } else if is_assistant && !message.text.is_empty() {
            let source: SharedString = message.text.clone().into();
            let cache_key = (thread_id.clone(), index);
            let markdown_entity = reset_or_create_markdown(
                &mut self.markdown_cache,
                cache_key,
                source,
                cx,
            );
            let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
            let workspace_handle = self.workspace.downgrade();
            let this_weak = cx.entity().downgrade();
            MarkdownElement::new(markdown_entity, style)
                .on_url_click(move |url, window, cx| {
                    if let Some(this) = this_weak.upgrade() {
                        this.update(cx, |this, cx| {
                            this.handle_agent_url_click(
                                url.as_ref(),
                                &workspace_handle,
                                window,
                                cx,
                            );
                        });
                    }
                })
                .into_any_element()
        } else {
            let text_color = match &message.role {
                TranscriptRole::System => Color::Warning,
                _ => Color::Default,
            };
            div()
                .text_color(text_color.color(cx))
                .text_sm()
                .whitespace_normal()
                .child(if message.text.is_empty() {
                    SharedString::from(" ")
                } else {
                    SharedString::from(message.text.clone())
                })
                .into_any_element()
        };

        let attachments_row = if matches!(message.role, TranscriptRole::User)
            && !message.attachments.is_empty()
        {
            Some(self.render_message_attachments(
                &thread_id,
                index,
                &message.attachments,
                cx,
            ))
        } else {
            None
        };

        v_flex()
            .id(("harness-message", index))
            .w_full()
            .gap_1()
            .rounded_lg()
            .bg(background)
            .p_3()
            .when(show_header, |this| {
                this.child(Label::new(label).size(LabelSize::Small).color(label_color))
            })
            .children(attachments_row)
            .child(body)
            .into_any_element()
    }

    fn render_tool_message(
        &mut self,
        thread_id: SharedString,
        index: usize,
        kind: ToolDisplayKind,
        status: ToolStatus,
        title: &SharedString,
        body: &str,
        duration_ms: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_reasoning = matches!(kind, ToolDisplayKind::Reasoning);
        let key = (thread_id, index);
        let expanded = self.expanded_tool_messages.contains(&key);
        let has_body = !body.trim().is_empty();
        let is_running = status == ToolStatus::Running;
        let is_expandable = match kind {
            ToolDisplayKind::Reasoning => has_body || is_running,
            ToolDisplayKind::Command => true,
            _ => has_body || is_running,
        };
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
                let label_text: SharedString = match duration_ms {
                    Some(ms) => format!("Thought for {}", format_thinking_duration(ms)).into(),
                    None => "Thought".into(),
                };
                Label::new(label_text)
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
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
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
            let body_element: AnyElement = if matches!(kind, ToolDisplayKind::Command) {
                self.render_command_body(title, body, status, cx)
            } else if has_body && is_reasoning {
                let source: SharedString = body.to_string().into();
                let cache_key = (key.0.clone(), key.1);
                let md_entity = reset_or_create_markdown(
                    &mut self.markdown_cache,
                    cache_key,
                    source,
                    cx,
                );
                let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                div()
                    .ml(px(20.0))
                    .text_color(Color::Muted.color(cx))
                    .child(MarkdownElement::new(md_entity, style))
                    .into_any_element()
            } else if has_body && kind.body_is_monospace() {
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
            } else if has_body {
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
            } else if is_running {
                div()
                    .ml(px(20.0))
                    .px_3()
                    .py_2()
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
        title: &SharedString,
        body: &str,
        status: ToolStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        // The command string lives in the title ("Run command: {cmd}").
        // The body contains only stdout/output.
        let command_text: SharedString = title
            .split_once(": ")
            .map(|(_, cmd)| cmd.to_string())
            .unwrap_or_default()
            .into();
        let output_text: Option<SharedString> = if body.trim().is_empty() {
            None
        } else {
            Some(SharedString::from(body.to_string()))
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
            );

        if !command_text.is_empty() {
            card = card.child(
                div()
                    .px_3()
                    .pt_2()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(Color::Default.color(cx))
                    .whitespace_normal()
                    .child(SharedString::from(format!("$ {command_text}"))),
            );
        }

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
        let is_running = self.is_turn_running(cx);
        let attachments_element = if self.pending_attachments.is_empty() {
            None
        } else {
            Some(self.render_attachments(cx))
        };
        let drop_overlay = self.render_drop_overlay(cx);
        let colors = cx.theme().colors();

        v_flex()
            .id("agent-composer")
            .w(COMPOSER_WIDTH)
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
                        IconButton::new("agent-composer-attach", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Attach files or images"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_attachments(window, cx);
                            })),
                    )
                    .child(self.render_model_selector(cx))
                    .child(self.render_reasoning_selector(cx))
                    .child(div().flex_1())
                    .children(self.render_context_indicator(cx))
                    .child(
                        IconButton::new("agent-composer-mic", IconName::Mic)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Voice input")),
                    )
                    .child(if is_running {
                        IconButton::new("agent-composer-stop", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Error))
                            .tooltip(Tooltip::text("Stop generation"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stop_active_turn(cx);
                            }))
                            .into_any_element()
                    } else {
                        IconButton::new("agent-composer-send", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Filled)
                            .tooltip(Tooltip::text("Send message"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_composer(window, cx);
                            }))
                            .into_any_element()
                    }),
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
                    .id(("agent-attachment", index))
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
                        IconButton::new(("agent-attachment-remove", index), IconName::Close)
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

    fn render_message_attachments(
        &self,
        thread_id: &SharedString,
        message_index: usize,
        attachments: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let previews: Vec<AnyElement> = attachments
            .iter()
            .enumerate()
            .map(|(attachment_index, path)| {
                let display_name = attachment_display_name(path);
                let full_path: SharedString = path.to_string_lossy().to_string().into();
                let is_image = is_image_path(path);
                let element_id = SharedString::from(format!(
                    "message-attachment-{}-{}-{}",
                    thread_id, message_index, attachment_index
                ));
                let element_id = ElementId::Name(element_id);
                let clickable_path = path.clone();

                if is_image {
                    h_flex()
                        .id(element_id)
                        .w(px(96.0))
                        .h(px(96.0))
                        .flex_shrink_0()
                        .overflow_hidden()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .cursor_pointer()
                        .tooltip(Tooltip::text(full_path))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.preview_attachment(clickable_path.clone(), cx);
                        }))
                        .child(
                            img(path.clone())
                                .object_fit(ObjectFit::Cover)
                                .w_full()
                                .h_full(),
                        )
                        .into_any_element()
                } else {
                    h_flex()
                        .id(element_id)
                        .h(px(28.0))
                        .gap_1p5()
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background)
                        .items_center()
                        .tooltip(Tooltip::text(full_path))
                        .child(
                            Icon::new(attachment_icon(path))
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(display_name)
                                .size(LabelSize::Small)
                                .color(Color::Default)
                                .truncate(),
                        )
                        .into_any_element()
                }
            })
            .collect();

        h_flex()
            .gap_2()
            .flex_wrap()
            .children(previews)
            .into_any_element()
    }

    fn render_drop_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        // Fully opaque fill so the composer's contents are clearly replaced by
        // the drop prompt — a subtle tint behind input text is easy to miss.
        let overlay_background = cx.theme().colors().element_selected;
        let border_focused = cx.theme().colors().border_focused;
        div()
            .invisible()
            .absolute()
            .inset_0()
            .size_full()
            .rounded_xl()
            .bg(overlay_background)
            .border_2()
            .border_color(border_focused)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .drag_over::<ExternalPaths>(|this, _, _, _| this.visible())
            .child(
                Icon::new(IconName::Download)
                    .size(IconSize::Medium)
                    .color(Color::Accent),
            )
            .child(
                Label::new("Drop files here to attach")
                    .size(LabelSize::Default)
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
                    .id("agent-attachment-preview-overlay")
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
                            .id("agent-attachment-preview-card")
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
                                            "agent-attachment-preview-close",
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

    fn render_model_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_label: SharedString = AVAILABLE_MODELS
            .iter()
            .find(|(id, _)| *id == self.selected_model)
            .map(|(_, label)| label.to_string())
            .unwrap_or_else(|| self.selected_model.clone())
            .into();

        let this = cx.entity().downgrade();
        PopoverMenu::new("agent-model-selector")
            .anchor(gpui::Corner::TopLeft)
            .trigger(
                Button::new("agent-composer-model", current_label)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                let current = this.upgrade()?.read(cx).selected_model.clone();
                Some(ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                    for &(model_id, label) in AVAILABLE_MODELS {
                        let this = this.clone();
                        let model_id = model_id.to_string();
                        let is_selected = current == model_id;
                        menu = menu.toggleable_entry(
                            label,
                            is_selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                if let Some(this) = this.upgrade() {
                                    this.update(cx, |this, cx| {
                                        this.selected_model = model_id.clone();
                                        cx.notify();
                                    });
                                }
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_reasoning_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_label: SharedString = REASONING_EFFORTS
            .iter()
            .find(|(id, _)| *id == self.selected_reasoning_effort)
            .map(|(_, label)| label.to_string())
            .unwrap_or_else(|| self.selected_reasoning_effort.clone())
            .into();

        let this = cx.entity().downgrade();
        PopoverMenu::new("agent-reasoning-selector")
            .anchor(gpui::Corner::TopLeft)
            .trigger(
                Button::new("agent-composer-reasoning", current_label)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                let current = this.upgrade()?.read(cx).selected_reasoning_effort.clone();
                Some(ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                    for &(effort_id, label) in REASONING_EFFORTS {
                        let this = this.clone();
                        let effort_id = effort_id.to_string();
                        let is_selected = current == effort_id;
                        menu = menu.toggleable_entry(
                            label,
                            is_selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                if let Some(this) = this.upgrade() {
                                    this.update(cx, |this, cx| {
                                        this.selected_reasoning_effort = effort_id.clone();
                                        cx.notify();
                                    });
                                }
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_context_indicator(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.active_thread(cx)?;
        if thread.estimated_tokens_used == 0 {
            return None;
        }

        let max_tokens = model_context_window(&self.selected_model) as f32;
        let used = thread.estimated_tokens_used as f32;
        let ratio = (used / max_tokens).clamp(0.0, 1.0);
        // Round up so that the moment the user spends any tokens, the bar
        // advertises at least 1%. Zero is only shown when the thread is empty.
        let percentage = (ratio * 100.0).ceil().min(100.0) as u32;
        let used_label = format_token_count(thread.estimated_tokens_used);
        let max_label = format_token_count(model_context_window(&self.selected_model));
        let bar_color = if percentage >= 85 {
            cx.theme().status().warning
        } else {
            cx.theme().status().info
        };
        // Ensure the progress arc is visually perceptible even when usage is
        // still a tiny fraction of the window (new threads, first tokens).
        let display_value = (ratio.max(0.03) * max_tokens).min(max_tokens);
        let tooltip_text =
            format!("{percentage}% context used ({used_label} / {max_label} tokens)");

        Some(
            h_flex()
                .id("agent-context-indicator")
                .gap_1p5()
                .items_center()
                .tooltip(Tooltip::text(tooltip_text))
                .child(
                    CircularProgress::new(display_value, max_tokens, px(14.0), cx)
                        .stroke_width(px(2.0))
                        .progress_color(bar_color),
                )
                .child(
                    Label::new(format!("{used_label} / {max_label}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }

    fn render_run_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w(COMPOSER_WIDTH)
            .px_2()
            .gap_3()
            .items_center()
            .child(
                Button::new("agent-env-selector", "Local")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .child(self.render_permission_selector(cx))
            .child(div().flex_1())
            .child(self.render_branch_selector(cx))
    }

    fn render_permission_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (trigger_label, trigger_icon, trigger_color) =
            permission_trigger_presentation(&self.selected_sandbox_policy);

        let this = cx.entity().downgrade();
        PopoverMenu::new("agent-permission-selector")
            .anchor(gpui::Corner::TopLeft)
            .trigger(
                Button::new("agent-composer-permission", trigger_label)
                    .label_size(LabelSize::Small)
                    .color(trigger_color)
                    .start_icon(
                        Icon::new(trigger_icon)
                            .size(IconSize::XSmall)
                            .color(trigger_color),
                    )
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                let current = this.upgrade()?.read(cx).selected_sandbox_policy.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for (policy, label, icon) in permission_options() {
                        let this = this.clone();
                        let is_selected = current == policy;
                        let entry = ContextMenuEntry::new(label)
                            .icon(icon)
                            .toggleable(IconPosition::End, is_selected)
                            .handler(move |_window, cx| {
                                if let Some(this) = this.upgrade() {
                                    let policy = policy.clone();
                                    this.update(cx, |this, cx| {
                                        this.selected_sandbox_policy = policy;
                                        cx.notify();
                                    });
                                }
                            });
                        menu.push_item(entry);
                    }
                    menu
                }))
            })
    }

    fn render_branch_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.workspace.read(cx).project().clone();
        let repo = project.read(cx).active_repository(cx);

        let (branch_label, branch_list) = if let Some(repo) = repo.as_ref() {
            let snapshot = repo.read(cx).snapshot();
            let label: SharedString = snapshot
                .branch
                .as_ref()
                .map(|b| SharedString::from(b.name().to_string()))
                .unwrap_or_else(|| "no branch".into());
            let list: Vec<(String, bool)> = snapshot
                .branch_list
                .iter()
                .map(|b| (b.name().to_string(), b.is_head))
                .collect();
            (label, list)
        } else {
            ("no branch".into(), Vec::new())
        };

        let repo_weak = repo.map(|r| r.downgrade());

        PopoverMenu::new("agent-branch-selector")
            .anchor(gpui::Corner::TopRight)
            .trigger(
                Button::new("agent-branch-btn", branch_label)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle),
            )
            .menu(move |window, cx| {
                let repo_weak = repo_weak.clone()?;
                let repo = repo_weak.upgrade()?;
                let current_name = repo
                    .read(cx)
                    .snapshot()
                    .branch
                    .as_ref()
                    .map(|b| b.name().to_string());
                Some(ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                    for (name, _is_head) in &branch_list {
                        let is_selected = current_name.as_deref() == Some(name.as_str());
                        let repo_weak = repo_weak.clone();
                        let branch_name = name.clone();
                        menu = menu.toggleable_entry(
                            name.clone(),
                            is_selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                if let Some(repo) = repo_weak.upgrade() {
                                    repo.update(cx, |repo, _cx| {
                                        let _ = repo.change_branch(branch_name.clone());
                                    });
                                }
                            },
                        );
                    }
                    menu
                }))
            })
    }
}

impl EventEmitter<AgentsSurfaceEvent> for AgentsSurface {}

