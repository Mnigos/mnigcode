use anyhow::Result;
use editor::{
    CompletionContext, CompletionProvider, ContextMenuOptions, Editor, FoldPlaceholder, ToOffset,
    display_map::{Crease, CreaseId},
};
use fuzzy::PathMatch;
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, ExternalPaths, Focusable,
    ListAlignment, ListSizingBehavior, ListState, MouseButton, ObjectFit, PathPromptOptions,
    Pixels, Render, SharedString, Subscription, Task, WeakEntity, Window, deferred, img, list, px,
};
use language::{ToPoint, language_settings::SoftWrap};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use menu::Confirm;
use project::{
    Candidates, Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    PathMatchCandidateSet, lsp_store::CompletionDocumentation,
};
use smol::channel;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    sync::atomic::AtomicBool,
    time::Duration,
};
use terminal_view::terminal_panel::Toggle as ToggleTerminalPanel;
use ui::{
    ButtonLike, ButtonStyle, CircularProgress, CommonAnimationExt, ContextMenu, ContextMenuEntry,
    PopoverMenu, TintColor, Tooltip, WithScrollbar, prelude::*,
};
use url::Url;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, NewThread, ToggleWorkspaceMode};

use crate::COMPOSER_KEY_CONTEXT;
use crate::harness::{
    HarnessApprovalPolicy, HarnessKind, HarnessRunStatus, HarnessSandboxPolicy,
    HarnessSessionConfig, HarnessSkillDefinition as SkillDefinition, HarnessSkillMention,
    HarnessThreadId, HarnessToolPhase, HarnessTurnInput, HarnessTurnUpdate,
    load_codex_available_skills, run_codex_app_server_session,
};
use crate::helpers::{
    animated_thinking_label, attachment_display_name, attachment_icon,
    build_input_with_attachments, is_image_path, tool_summary_line, url_has_scheme,
    workspace_display_name, workspace_root_path, workspace_storage_key,
};
use crate::serialization::{
    SerializedHarnessThread, SerializedThreadGroup, SerializedToolKind, SerializedToolStatus,
    SerializedTranscriptMessage, SerializedTranscriptRole,
};
use crate::transcript::{
    HarnessThread, HarnessThreadSummary, ResolvedFileMention, ToolDisplayKind, ToolStatus,
    TranscriptMessage, TranscriptRole,
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

const REASONING_EFFORTS: &[(&str, &str)] =
    &[("low", "Low"), ("medium", "Medium"), ("high", "High")];

const DEFAULT_REASONING_EFFORT: &str = "high";

struct CodexSessionHandle {
    turns: channel::Sender<HarnessTurnInput>,
    _session_task: Task<()>,
    _update_task: Task<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileMentionQuery {
    source_range: Range<usize>,
    query: Option<String>,
}

impl FileMentionQuery {
    fn try_parse(line: &str, offset_to_line: usize) -> Option<Self> {
        let mut mention_start = None;
        for (index, _) in line.rmatch_indices('@') {
            if !is_mention_boundary(line[..index].chars().last()) {
                continue;
            }

            mention_start = Some(index);
            break;
        }

        let mention_start = mention_start?;
        let rest = &line[mention_start + 1..];

        if rest.is_empty() {
            return Some(Self {
                source_range: mention_start + offset_to_line..mention_start + 1 + offset_to_line,
                query: None,
            });
        }

        if rest.starts_with(char::is_whitespace) {
            return None;
        }

        if let Some(stripped) = rest.strip_prefix('"') {
            let mut escaped = false;
            let mut query = String::new();
            let mut consumed = 1usize;

            for character in stripped.chars() {
                consumed += character.len_utf8();
                if escaped {
                    query.push(character);
                    escaped = false;
                    continue;
                }

                match character {
                    '\\' => escaped = true,
                    '"' => {
                        return Some(Self {
                            source_range: mention_start + offset_to_line
                                ..mention_start + consumed + 1 + offset_to_line,
                            query: Some(query),
                        });
                    }
                    _ => query.push(character),
                }
            }

            return Some(Self {
                source_range: mention_start + offset_to_line
                    ..mention_start + consumed + 1 + offset_to_line,
                query: if query.is_empty() { None } else { Some(query) },
            });
        }

        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = rest[..token_end]
            .trim_end_matches(|character: char| ",;:!?)]}".contains(character))
            .to_string();

        Some(Self {
            source_range: mention_start + offset_to_line
                ..mention_start + token_end + 1 + offset_to_line,
            query: if token.is_empty() { None } else { Some(token) },
        })
    }
}

#[derive(Clone, Debug)]
struct FileMentionMatch {
    path_match: PathMatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillMentionQuery {
    source_range: Range<usize>,
    query: Option<String>,
}

impl SkillMentionQuery {
    fn try_parse(line: &str, offset_to_line: usize, trigger: char) -> Option<Self> {
        let mut mention_start = None;
        for (index, _) in line.rmatch_indices(trigger) {
            if !is_mention_boundary(line[..index].chars().last()) {
                continue;
            }
            mention_start = Some(index);
            break;
        }

        let mention_start = mention_start?;
        let rest = &line[mention_start + 1..];

        if rest.starts_with(char::is_whitespace) {
            return None;
        }

        if rest.is_empty() {
            return Some(Self {
                source_range: mention_start + offset_to_line..mention_start + 1 + offset_to_line,
                query: None,
            });
        }

        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];

        Some(Self {
            source_range: mention_start + offset_to_line
                ..mention_start + token_end + 1 + offset_to_line,
            query: if token.is_empty() {
                None
            } else {
                Some(token.to_string())
            },
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ComposerQuery {
    File(FileMentionQuery),
    Skill(SkillMentionQuery),
}

impl ComposerQuery {
    fn try_parse(line: &str, offset_to_line: usize) -> Option<Self> {
        let candidates = [
            line.contains('$')
                .then(|| SkillMentionQuery::try_parse(line, offset_to_line, '$'))
                .flatten()
                .map(Self::Skill),
            line.contains('/')
                .then(|| SkillMentionQuery::try_parse(line, offset_to_line, '/'))
                .flatten()
                .map(Self::Skill),
            line.contains('@')
                .then(|| FileMentionQuery::try_parse(line, offset_to_line))
                .flatten()
                .map(Self::File),
        ];

        candidates
            .into_iter()
            .flatten()
            .max_by_key(|query| (query.source_range().end, query.source_range().start))
    }

    fn source_range(&self) -> &Range<usize> {
        match self {
            Self::File(q) => &q.source_range,
            Self::Skill(q) => &q.source_range,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileMentionSpan {
    source_range: Range<usize>,
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillMentionSpan {
    source_range: Range<usize>,
    name: String,
    path: Option<PathBuf>,
}

struct ComposerFileCompletionProvider {
    multi_workspace: WeakEntity<MultiWorkspace>,
    surface: WeakEntity<AgentsSurface>,
    editor: WeakEntity<Editor>,
}

impl ComposerFileCompletionProvider {
    fn new(
        multi_workspace: WeakEntity<MultiWorkspace>,
        surface: WeakEntity<AgentsSurface>,
        editor: WeakEntity<Editor>,
    ) -> Self {
        Self {
            multi_workspace,
            surface,
            editor,
        }
    }

    fn search_skills(&self, query: String, cx: &mut App) -> Task<Vec<SkillDefinition>> {
        let Some(surface) = self.surface.upgrade() else {
            return Task::ready(Vec::new());
        };

        let (skills, cwd, should_load) = surface.read(cx).available_skills_for_completion(cx);
        if !should_load {
            return cx.spawn(async move |cx| filter_skill_definitions(skills, query, cx).await);
        }

        let Some(cwd) = cwd else {
            return Task::ready(Vec::new());
        };
        surface.update(cx, |surface, cx| surface.refresh_available_skills(cx));
        let surface = self.surface.clone();

        cx.spawn(async move |cx| {
            loop {
                let Some(load_state) = cx
                    .update(|cx| {
                        let Some(surface) = surface.upgrade() else {
                            return None;
                        };

                        Some(surface.read_with(cx, |surface, _| {
                            if surface.available_skills_cwd.as_ref() != Some(&cwd) {
                                return None;
                            }

                            if surface.skills_refresh_task.is_some() {
                                return Some(None);
                            }

                            Some(Some(surface.available_skills.clone()))
                        }))
                    })
                    .flatten()
                else {
                    return Vec::new();
                };

                if let Some(skills) = load_state {
                    return filter_skill_definitions(skills, query, cx).await;
                }

                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        })
    }

    fn completion_for_skill(
        skill: &SkillDefinition,
        source_range: Range<language::Anchor>,
        surface: WeakEntity<AgentsSurface>,
        editor: WeakEntity<Editor>,
        _cx: &mut App,
    ) -> Completion {
        let uri = acp_thread::MentionUri::Skill {
            name: skill.name.to_string(),
            description: skill.description.to_string(),
        };
        let mut mention_uri = uri.to_uri();
        if let Some(path) = &skill.path {
            mention_uri
                .query_pairs_mut()
                .append_pair("path", &path.to_string_lossy());
        }
        let mention_text = format!("[@{}]({})", uri.name(), mention_uri);
        let new_text = format!("{} ", mention_text);
        let content_len = new_text.len().saturating_sub(1);
        let mention_start = source_range.start;
        let label: SharedString = skill.name.clone();
        let tooltip: SharedString = skill.description.clone();

        Completion {
            replace_range: source_range,
            new_text,
            label: language::CodeLabel::plain(skill.name.to_string(), None),
            documentation: Some(CompletionDocumentation::MultiLinePlainText(tooltip.clone())),
            source: CompletionSource::Custom,
            icon_path: Some(IconName::Box.path().into()),
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: Some(Arc::new(
                move |_intent: project::CompletionIntent, window: &mut Window, cx: &mut App| {
                    let surface = surface.clone();
                    let editor_weak = editor.clone();
                    let label = label.clone();
                    let tooltip = tooltip.clone();
                    let start = mention_start;

                    window.defer(cx, move |window, cx| {
                        let Some(editor) = editor_weak.upgrade() else {
                            return;
                        };

                        if let Some(crease_id) = insert_skill_mention_crease(
                            start,
                            content_len,
                            label,
                            tooltip,
                            editor.clone(),
                            window,
                            cx,
                        ) {
                            if let Some(surface) = surface.upgrade() {
                                surface.update(cx, |this, _cx| {
                                    this.mention_crease_ids.push(crease_id);
                                });
                            }
                        }
                    });
                    false
                },
            )),
        }
    }

    fn workspace(&self, cx: &App) -> Option<Entity<workspace::Workspace>> {
        self.multi_workspace
            .upgrade()
            .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
    }

    fn search_files(
        &self,
        query: String,
        cancellation_flag: Arc<AtomicBool>,
        cx: &mut App,
    ) -> Task<Vec<FileMentionMatch>> {
        let Some(workspace) = self.workspace(cx) else {
            return Task::ready(Vec::new());
        };

        let workspace = workspace.read(cx);
        let relative_to = workspace
            .recent_navigation_history_iter(cx)
            .next()
            .map(|(project_path, _)| project_path.path);
        let worktrees = workspace.visible_worktrees(cx).collect::<Vec<_>>();
        let candidate_sets = worktrees
            .into_iter()
            .map(|worktree| {
                let worktree = worktree.read(cx);
                PathMatchCandidateSet {
                    snapshot: worktree.snapshot(),
                    include_ignored: worktree.root_entry().is_some_and(|entry| entry.is_ignored),
                    include_root_name: false,
                    candidates: Candidates::Files,
                }
            })
            .collect::<Vec<_>>();

        let executor = cx.background_executor().clone();
        cx.foreground_executor().spawn(async move {
            fuzzy::match_path_sets(
                candidate_sets.as_slice(),
                query.as_str(),
                &relative_to,
                false,
                100,
                &cancellation_flag,
                executor,
            )
            .await
            .into_iter()
            .map(|path_match| FileMentionMatch { path_match })
            .collect()
        })
    }

    fn completion_for_match(
        file_match: FileMentionMatch,
        source_range: Range<language::Anchor>,
        workspace: &Entity<workspace::Workspace>,
        surface: WeakEntity<AgentsSurface>,
        editor: WeakEntity<Editor>,
        cx: &mut App,
    ) -> Completion {
        let display_path = mention_display_path(workspace, &file_match.path_match, cx);
        let mention_label: SharedString = file_match
            .path_match
            .path
            .file_name()
            .unwrap_or(file_match.path_match.path.as_unix_str())
            .to_string()
            .into();
        let tooltip: SharedString = display_path.clone().into();
        let project_path = project::ProjectPath {
            worktree_id: project::WorktreeId::from_usize(file_match.path_match.worktree_id),
            path: file_match.path_match.path.clone(),
        };
        let abs_path = workspace
            .read(cx)
            .project()
            .read(cx)
            .absolute_path(&project_path, cx);
        let new_text = format!("{} ", format_file_mention(&display_path));
        let content_len = new_text.len().saturating_sub(1);
        let mention_start = source_range.start;

        Completion {
            replace_range: source_range,
            new_text,
            label: language::CodeLabel::plain(display_path, None),
            documentation: None,
            source: CompletionSource::Custom,
            icon_path: Some(IconName::File.path().into()),
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: abs_path.map(|abs_path| {
                Arc::new(
                    move |_intent: project::CompletionIntent, window: &mut Window, cx: &mut App| {
                        let surface = surface.clone();
                        let editor = editor.clone();
                        let abs_path = abs_path.clone();
                        let mention_label = mention_label.clone();
                        let tooltip = tooltip.clone();
                        let start = mention_start;

                        window.defer(cx, move |window, cx| {
                            let Some(editor) = editor.upgrade() else {
                                return;
                            };
                            let Some(surface) = surface.upgrade() else {
                                return;
                            };
                            let workspace = surface.read(cx).workspace.clone();

                            if let Some(crease_id) = insert_file_mention_crease(
                                start,
                                content_len,
                                mention_label,
                                tooltip,
                                abs_path,
                                editor,
                                workspace.downgrade(),
                                window,
                                cx,
                            ) {
                                surface.update(cx, |this, _cx| {
                                    this.mention_crease_ids.push(crease_id)
                                });
                            }
                        });
                        false
                    },
                )
                    as Arc<
                        dyn Fn(project::CompletionIntent, &mut Window, &mut App) -> bool
                            + Send
                            + Sync,
                    >
            }),
        }
    }
}

impl CompletionProvider for ComposerFileCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<language::Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let parsed = buffer.update(cx, |buffer, _cx| {
            let position = buffer_position.to_point(buffer);
            let line_start = language::Point::new(position.row, 0);
            let offset_to_line = buffer.point_to_offset(line_start);
            let mut lines = buffer.text_for_range(line_start..position).lines();
            let line = lines.next()?;
            ComposerQuery::try_parse(line, offset_to_line)
        });

        let Some(parsed) = parsed else {
            return Task::ready(Ok(Vec::new()));
        };

        let snapshot = buffer.read(cx).snapshot();
        let source_range = snapshot.anchor_before(parsed.source_range().start)
            ..snapshot.anchor_after(parsed.source_range().end);
        let surface = self.surface.clone();
        let editor = self.editor.clone();

        match parsed {
            ComposerQuery::File(file_query) => {
                let Some(workspace) = self.workspace(cx) else {
                    return Task::ready(Ok(Vec::new()));
                };
                let query = file_query.query.unwrap_or_default();
                let search_task = self.search_files(query, Arc::new(AtomicBool::default()), cx);

                cx.spawn(async move |_, cx| {
                    let matches = search_task.await;
                    let completions = cx.update(|cx| {
                        matches
                            .into_iter()
                            .map(|file_match| {
                                ComposerFileCompletionProvider::completion_for_match(
                                    file_match,
                                    source_range.clone(),
                                    &workspace,
                                    surface.clone(),
                                    editor.clone(),
                                    cx,
                                )
                            })
                            .collect::<Vec<_>>()
                    });

                    Ok(vec![CompletionResponse {
                        completions,
                        display_options: CompletionDisplayOptions {
                            dynamic_width: true,
                        },
                        is_incomplete: true,
                    }])
                })
            }
            ComposerQuery::Skill(skill_query) => {
                let query = skill_query.query.unwrap_or_default();
                let search_task = self.search_skills(query, cx);

                cx.spawn(async move |_, cx| {
                    let skills = search_task.await;
                    let completions = cx.update(|cx| {
                        skills
                            .iter()
                            .map(|skill| {
                                Self::completion_for_skill(
                                    skill,
                                    source_range.clone(),
                                    surface.clone(),
                                    editor.clone(),
                                    cx,
                                )
                            })
                            .collect::<Vec<_>>()
                    });

                    Ok(vec![CompletionResponse {
                        completions,
                        display_options: CompletionDisplayOptions {
                            dynamic_width: true,
                        },
                        is_incomplete: true,
                    }])
                })
            }
        }
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<language::Buffer>,
        position: language::Anchor,
        _text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let buffer = buffer.read(cx);
        let position = position.to_point(buffer);
        let line_start = language::Point::new(position.row, 0);
        let offset_to_line = buffer.point_to_offset(line_start);
        let mut lines = buffer.text_for_range(line_start..position).lines();
        lines
            .next()
            .and_then(|line| ComposerQuery::try_parse(line, offset_to_line))
            .map(|query| {
                query.source_range().start <= offset_to_line + position.column as usize
                    && query.source_range().end >= offset_to_line + position.column as usize
            })
            .unwrap_or(false)
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
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

async fn filter_skill_definitions(
    skills: Vec<SkillDefinition>,
    query: String,
    cx: &mut AsyncApp,
) -> Vec<SkillDefinition> {
    if query.is_empty() {
        return skills;
    }

    let candidates: Vec<_> = skills
        .iter()
        .enumerate()
        .map(|(id, skill)| fuzzy::StringMatchCandidate::new(id, &skill.name))
        .collect();

    fuzzy::match_strings(
        &candidates,
        &query,
        false,
        true,
        100,
        &Arc::new(AtomicBool::default()),
        cx.background_executor().clone(),
    )
    .await
    .into_iter()
    .map(|mat| skills[mat.candidate_id].clone())
    .collect()
}

fn is_mention_boundary(previous_character: Option<char>) -> bool {
    previous_character.is_none_or(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '{' | '<' | '\'' | '"')
    })
}

fn format_file_mention(path: &str) -> String {
    if path.chars().any(char::is_whitespace) || path.contains('"') {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        format!("@\"{escaped}\"")
    } else {
        format!("@{path}")
    }
}

fn parse_file_mention_spans(text: &str) -> Vec<FileMentionSpan> {
    let mut mentions = Vec::new();
    let characters: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0usize;

    while index < characters.len() {
        let (at_byte, character) = characters[index];
        if character != '@' {
            index += 1;
            continue;
        }

        let previous = if index == 0 {
            None
        } else {
            Some(characters[index - 1].1)
        };
        if !is_mention_boundary(previous) {
            index += 1;
            continue;
        }

        let next_index = index + 1;
        let Some((mention_start, next_character)) = characters.get(next_index).copied() else {
            index += 1;
            continue;
        };
        if next_character.is_whitespace() {
            index += 1;
            continue;
        }

        if next_character == '"' {
            let mut mention = String::new();
            let mut escaped = false;
            let mut cursor = next_index + 1;
            let mut closed_quote = false;

            while cursor < characters.len() {
                let (current_byte, current) = characters[cursor];
                if escaped {
                    mention.push(current);
                    escaped = false;
                    cursor += 1;
                    continue;
                }

                match current {
                    '\\' => {
                        escaped = true;
                        cursor += 1;
                    }
                    '"' => {
                        if !mention.is_empty() {
                            mentions.push(FileMentionSpan {
                                source_range: at_byte..current_byte + current.len_utf8(),
                                path: mention.clone(),
                            });
                        }
                        closed_quote = true;
                        index = cursor + 1;
                        break;
                    }
                    _ => {
                        mention.push(current);
                        cursor += 1;
                    }
                }
            }

            if !closed_quote {
                if !mention.is_empty() {
                    mentions.push(FileMentionSpan {
                        source_range: at_byte..text.len(),
                        path: mention,
                    });
                }
                break;
            }

            continue;
        }

        let mut end_byte = text.len();
        let mut cursor = next_index;
        while cursor < characters.len() {
            let (current_byte, current) = characters[cursor];
            if current.is_whitespace() {
                end_byte = current_byte;
                break;
            }
            cursor += 1;
        }

        let mention = text[mention_start..end_byte]
            .trim_end_matches(|current: char| ",;:!?)]}".contains(current));
        if !mention.is_empty() {
            mentions.push(FileMentionSpan {
                source_range: at_byte..mention_start + mention.len(),
                path: mention.to_string(),
            });
        }
        index = cursor.max(index + 1);
    }

    mentions
}

fn parse_file_mentions(text: &str) -> Vec<String> {
    parse_file_mention_spans(text)
        .into_iter()
        .map(|mention| mention.path)
        .collect()
}

fn parse_skill_mention_spans(
    text: &str,
    path_style: util::paths::PathStyle,
) -> Vec<SkillMentionSpan> {
    let mut mentions = Vec::new();
    let mut cursor = 0usize;

    while let Some(relative_start) = text[cursor..].find("[@") {
        let mention_start = cursor + relative_start;
        let Some(relative_link_separator) = text[mention_start + 2..].find("](") else {
            break;
        };
        let url_start = mention_start + 2 + relative_link_separator + 2;
        let Some(relative_url_end) = text[url_start..].find(')') else {
            break;
        };
        let url_end = url_start + relative_url_end;
        let url = &text[url_start..url_end];
        cursor = url_end + 1;

        let Ok(acp_thread::MentionUri::Skill { name, .. }) =
            acp_thread::MentionUri::parse(url, path_style)
        else {
            continue;
        };

        let path = Url::parse(url).ok().and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "path")
                .map(|(_, value)| PathBuf::from(value.into_owned()))
        });

        mentions.push(SkillMentionSpan {
            source_range: mention_start..url_end + 1,
            name,
            path,
        });
    }

    mentions
}

fn sanitize_skill_mentions(text: &str, path_style: util::paths::PathStyle) -> String {
    let mentions = parse_skill_mention_spans(text, path_style);
    if mentions.is_empty() {
        return text.to_string();
    }

    let mut sanitized = String::new();
    let mut cursor = 0usize;

    for mention in mentions {
        if mention.source_range.start > cursor {
            sanitized.push_str(&text[cursor..mention.source_range.start]);
        }
        sanitized.push('$');
        sanitized.push_str(&mention.name);
        cursor = mention.source_range.end;
    }

    if cursor < text.len() {
        sanitized.push_str(&text[cursor..]);
    }

    sanitized
}

fn resolve_file_mention_spans(
    workspace: &Entity<workspace::Workspace>,
    text: &str,
    cx: &App,
) -> Vec<ResolvedFileMention> {
    parse_file_mention_spans(text)
        .into_iter()
        .filter_map(|mention| {
            resolve_agent_link(workspace, std::path::Path::new(&mention.path), cx).map(|abs_path| {
                ResolvedFileMention {
                    source_range: mention.source_range,
                    abs_path,
                }
            })
        })
        .collect()
}

fn mention_display_path(
    workspace: &Entity<workspace::Workspace>,
    path_match: &PathMatch,
    cx: &App,
) -> String {
    let include_root_name = workspace.read(cx).visible_worktrees(cx).count() > 1;
    if include_root_name
        && let Some(worktree) = workspace
            .read(cx)
            .project()
            .read(cx)
            .worktree_for_id(project::WorktreeId::from_usize(path_match.worktree_id), cx)
    {
        format!(
            "{}/{}",
            worktree.read(cx).root_name().as_unix_str(),
            path_match.path.as_unix_str()
        )
    } else {
        path_match.path.as_unix_str().to_string()
    }
}

fn insert_file_mention_crease(
    anchor: language::Anchor,
    content_len: usize,
    label: SharedString,
    tooltip: SharedString,
    abs_path: PathBuf,
    editor: Entity<Editor>,
    workspace: WeakEntity<workspace::Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Option<CreaseId> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let start = snapshot.anchor_in_excerpt(anchor)?.bias_right(&snapshot);
        let end = snapshot.anchor_before(start.to_offset(&snapshot) + content_len);

        let placeholder = FoldPlaceholder {
            render: render_file_mention_pill(
                label.clone(),
                tooltip.clone(),
                abs_path.clone(),
                workspace.clone(),
            ),
            merge_adjacent: false,
            ..Default::default()
        };

        let crease = Crease::Inline {
            range: start..end,
            placeholder,
            render_toggle: None,
            render_trailer: None,
            metadata: None,
        };

        let ids = editor.insert_creases(vec![crease.clone()], cx);
        editor.fold_creases(vec![crease], false, window, cx);
        ids.first().copied()
    })
}

fn render_file_mention_pill(
    label: SharedString,
    tooltip: SharedString,
    abs_path: PathBuf,
    workspace: WeakEntity<workspace::Workspace>,
) -> Arc<
    dyn Send
        + Sync
        + Fn(editor::display_map::FoldId, Range<editor::Anchor>, &mut App) -> AnyElement,
> {
    Arc::new(move |_fold_id, _fold_range, _cx| {
        file_mention_pill_element(
            SharedString::from(format!("composer-file-mention-{}", abs_path.display())),
            label.clone(),
            tooltip.clone(),
            abs_path.clone(),
            workspace.clone(),
        )
    })
}

fn file_mention_pill_element(
    id: SharedString,
    label: SharedString,
    tooltip: SharedString,
    abs_path: PathBuf,
    workspace: WeakEntity<workspace::Workspace>,
) -> AnyElement {
    ButtonLike::new(id)
        .style(ButtonStyle::Tinted(TintColor::Accent))
        .tooltip(Tooltip::text(tooltip))
        .when_some(workspace.upgrade(), |this, workspace| {
            let abs_path = abs_path.clone();
            this.on_click(move |_event, window, cx| {
                open_workspace_path(&workspace, abs_path.clone(), window, cx);
            })
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Default),
        )
        .into_any_element()
}

fn open_workspace_path(
    workspace: &Entity<workspace::Workspace>,
    abs_path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
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
}

fn insert_skill_mention_crease(
    anchor: language::Anchor,
    content_len: usize,
    label: SharedString,
    tooltip: SharedString,
    editor: Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
) -> Option<CreaseId> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let start = snapshot.anchor_in_excerpt(anchor)?.bias_right(&snapshot);
        let end = snapshot.anchor_before(start.to_offset(&snapshot) + content_len);

        let placeholder = FoldPlaceholder {
            render: render_skill_mention_pill(label.clone(), tooltip),
            merge_adjacent: false,
            ..Default::default()
        };

        let crease = Crease::Inline {
            range: start..end,
            placeholder,
            render_toggle: None,
            render_trailer: None,
            metadata: None,
        };

        let ids = editor.insert_creases(vec![crease.clone()], cx);
        editor.fold_creases(vec![crease], false, window, cx);
        ids.first().copied()
    })
}

fn render_skill_mention_pill(
    label: SharedString,
    tooltip: SharedString,
) -> Arc<
    dyn Send
        + Sync
        + Fn(editor::display_map::FoldId, Range<editor::Anchor>, &mut App) -> AnyElement,
> {
    Arc::new(move |_fold_id, _fold_range, _cx| {
        ButtonLike::new(SharedString::from(format!("skill-mention-{}", label)))
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Compact)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Icon::new(IconName::Box)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    ),
            )
            .tooltip(Tooltip::text(tooltip.clone()))
            .into_any_element()
    })
}

fn resolve_agent_link(
    workspace: &Entity<workspace::Workspace>,
    path: &std::path::Path,
    cx: &App,
) -> Option<PathBuf> {
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }

    let project = workspace.read(cx).project().clone();
    let worktrees: Vec<_> = project.read(cx).worktrees(cx).collect();
    let needle = path.to_string_lossy();
    let needle = needle.trim_start_matches('/');
    let needle_components: Vec<&str> = needle.split('/').filter(|s| !s.is_empty()).collect();

    if let Some((root_name, relative_components)) = needle_components.split_first() {
        for worktree in &worktrees {
            let worktree_ref = worktree.read(cx);
            if worktree_ref.root_name().as_unix_str() != *root_name {
                continue;
            }

            let candidate = if relative_components.is_empty() {
                worktree_ref.abs_path().to_path_buf()
            } else {
                worktree_ref
                    .abs_path()
                    .join(relative_components.iter().collect::<PathBuf>())
            };
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    for worktree in &worktrees {
        let candidate = worktree.read(cx).abs_path().join(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }

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

fn open_agent_link(
    url: &str,
    workspace_handle: &WeakEntity<workspace::Workspace>,
    cwd: Option<&PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if url_has_scheme(url) {
        cx.open_url(url);
        return false;
    }

    let Some(workspace) = workspace_handle.upgrade() else {
        return false;
    };

    let parsed = util::paths::PathWithPosition::parse_str(url);

    let abs_path = if parsed.path.is_absolute() {
        resolve_agent_link(&workspace, &parsed.path, cx)
    } else if let Some(cwd) = cwd {
        let candidate = cwd.join(&parsed.path);
        if candidate.exists() {
            Some(candidate)
        } else {
            resolve_agent_link(&workspace, &parsed.path, cx)
        }
    } else {
        resolve_agent_link(&workspace, &parsed.path, cx)
    };

    let Some(abs_path) = abs_path else {
        log::warn!("open_agent_link: could not resolve {url:?} to any file");
        return false;
    };

    let project = workspace.read(cx).project().clone();
    let project_path = project.read(cx).find_project_path(&abs_path, cx);
    let row = parsed.row;

    let item = workspace.update(cx, |workspace, cx| {
        if let Some(project_path) = project_path {
            workspace.open_path(project_path, None, true, window, cx)
        } else {
            workspace.open_abs_path(abs_path, Default::default(), window, cx)
        }
    });

    if let Some(row) = row {
        window
            .spawn(cx, async move |cx| {
                let Some(editor) = item.await?.downcast::<editor::Editor>() else {
                    return anyhow::Ok(());
                };
                editor
                    .update_in(cx, |editor, window, cx| {
                        let point = language::Point::new(row.saturating_sub(1), 0);
                        editor.change_selections(
                            editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges(vec![point..point]),
                        );
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    } else {
        item.detach_and_log_err(cx);
    }

    true
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

enum TranscriptViewEvent {
    OpenedInEditor,
    PreviewRequested(PathBuf),
}

struct TranscriptView {
    workspace: WeakEntity<workspace::Workspace>,
    active_thread: Option<Arc<HarnessThread>>,
    expanded_tool_messages: HashSet<(SharedString, usize)>,
    markdown_cache: HashMap<(SharedString, usize), (SharedString, Entity<Markdown>)>,
    list_state: ListState,
}

impl TranscriptView {
    fn new(workspace: Entity<workspace::Workspace>) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(2048.0));
        list_state.set_follow_mode(gpui::FollowMode::Tail);
        Self {
            workspace: workspace.downgrade(),
            active_thread: None,
            expanded_tool_messages: HashSet::default(),
            markdown_cache: HashMap::default(),
            list_state,
        }
    }

    fn sync_state(
        &mut self,
        workspace: Entity<workspace::Workspace>,
        thread: Option<HarnessThread>,
        cx: &mut Context<Self>,
    ) {
        let previous_thread_id = self.active_thread.as_ref().map(|thread| thread.id.clone());
        let next_thread_id = thread.as_ref().map(|thread| thread.id.clone());
        let previous_count = self.item_count();

        self.workspace = workspace.downgrade();
        self.active_thread = thread.map(Arc::new);

        if previous_thread_id != next_thread_id {
            self.expanded_tool_messages.clear();
            self.markdown_cache.clear();
            self.list_state.reset(self.item_count());
            self.list_state.set_follow_mode(gpui::FollowMode::Tail);
            self.list_state.scroll_to_end();
        } else {
            let item_count = self.item_count();
            if previous_count != item_count {
                if item_count > previous_count {
                    self.list_state
                        .splice(previous_count..previous_count, item_count - previous_count);
                } else {
                    self.list_state.splice(item_count..previous_count, 0);
                }
            }
            self.list_state.remeasure();
        }

        self.prune_markdown_cache();
        cx.notify();
    }

    fn item_count(&self) -> usize {
        let Some(thread) = self.active_thread.as_ref() else {
            return 0;
        };

        thread.messages.len() + usize::from(thread.run_status.is_active())
    }

    fn prune_markdown_cache(&mut self) {
        let Some(thread) = self.active_thread.as_ref() else {
            self.markdown_cache.clear();
            return;
        };

        let thread_id = &thread.id.0;
        self.markdown_cache.retain(|(cached_thread_id, index), _| {
            cached_thread_id == thread_id && *index < thread.messages.len()
        });
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

    fn request_preview(&self, path: PathBuf, cx: &mut Context<Self>) {
        cx.emit(TranscriptViewEvent::PreviewRequested(path));
    }

    fn pin_to_bottom(&mut self) {
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        self.list_state.scroll_to_end();
    }

    fn render_status_indicator(
        &self,
        thread: &HarnessThread,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .gap_2()
            .items_center()
            .py_2()
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
            .into_any_element()
    }

    fn render_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(thread) = self.active_thread.clone() else {
            return div().into_any_element();
        };

        let content = if index == thread.messages.len() {
            self.render_status_indicator(&thread, cx)
        } else {
            let Some(message) = thread.messages.get(index) else {
                return div().into_any_element();
            };

            if should_skip_message(message) {
                return div().into_any_element();
            }

            let show_header = should_show_role_header(index, message, &thread.messages);
            self.render_message(thread.id.0.clone(), index, message, show_header, window, cx)
        };

        h_flex()
            .w_full()
            .justify_center()
            .child(div().w(COMPOSER_WIDTH).py_2().child(content))
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
        let (label, label_color, background): (SharedString, Color, gpui::Hsla) = match &message
            .role
        {
            TranscriptRole::User => ("You".into(), Color::Muted, colors.element_background),
            TranscriptRole::Assistant => ("Codex".into(), Color::Muted, colors.editor_background),
            TranscriptRole::System => ("System".into(), Color::Warning, colors.element_hover),
            TranscriptRole::Tool { .. } => unreachable!(),
        };

        let skip_body = matches!(message.role, TranscriptRole::User)
            && message.text.is_empty()
            && !message.attachments.is_empty();
        let file_mentions = if matches!(message.role, TranscriptRole::User) {
            message.file_mentions.clone()
        } else {
            Vec::new()
        };

        let body: AnyElement = if skip_body {
            div().into_any_element()
        } else if is_assistant && !message.text.is_empty() {
            let source: SharedString = message.text.clone().into();
            let cache_key = (thread_id.clone(), index);
            let markdown_entity =
                reset_or_create_markdown(&mut self.markdown_cache, cache_key, source, cx);
            let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
            let workspace_handle = self.workspace.clone();
            let cwd = self.active_thread.as_ref().map(|t| t.cwd.clone());
            let this_weak = cx.entity().downgrade();
            MarkdownElement::new(markdown_entity, style)
                .on_url_click(move |url, window, cx| {
                    if open_agent_link(url.as_ref(), &workspace_handle, cwd.as_ref(), window, cx) {
                        if let Some(view) = this_weak.upgrade() {
                            view.update(cx, |_, cx| {
                                cx.emit(TranscriptViewEvent::OpenedInEditor);
                            });
                        }
                    }
                })
                .into_any_element()
        } else {
            let text_color = match &message.role {
                TranscriptRole::System => Color::Warning,
                _ => Color::Default,
            };
            if matches!(message.role, TranscriptRole::User) && !file_mentions.is_empty() {
                self.render_user_message_body(index, &message.text, &file_mentions, text_color, cx)
            } else {
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
            }
        };

        let attachments = if matches!(message.role, TranscriptRole::User) {
            let mentioned_paths = file_mentions
                .iter()
                .map(|mention| mention.abs_path.clone())
                .collect::<HashSet<_>>();
            message
                .attachments
                .iter()
                .filter(|attachment| !mentioned_paths.contains(*attachment))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let attachments_row = if !attachments.is_empty() {
            Some(self.render_message_attachments(&thread_id, index, &attachments, cx))
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

        let is_file_tool = matches!(
            kind,
            ToolDisplayKind::FileRead | ToolDisplayKind::FileChange
        );
        let detail_element: Option<AnyElement> = match (is_reasoning, &summary) {
            (false, Some((_, Some(detail)))) if is_file_tool => {
                let workspace = self.workspace.clone();
                let file_path = detail.clone();
                let cwd = self.active_thread.as_ref().map(|t| t.cwd.clone());
                Some(
                    div()
                        .id(("tool-file-link", index))
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.8))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(move |_this, _, window, cx| {
                            if open_agent_link(
                                file_path.as_ref(),
                                &workspace,
                                cwd.as_ref(),
                                window,
                                cx,
                            ) {
                                cx.emit(TranscriptViewEvent::OpenedInEditor);
                            }
                        }))
                        .child(
                            Label::new(detail.clone())
                                .size(LabelSize::Small)
                                .color(Color::Accent)
                                .truncate(),
                        )
                        .into_any_element(),
                )
            }
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
            .when(chevron_icon.is_none(), |this| this.child(div().w(px(12.0))))
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
                let md_entity =
                    reset_or_create_markdown(&mut self.markdown_cache, cache_key, source, cx);
                let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                let workspace_handle = self.workspace.clone();
                let cwd = self.active_thread.as_ref().map(|t| t.cwd.clone());
                let this_weak = cx.entity().downgrade();
                div()
                    .ml(px(20.0))
                    .text_color(Color::Muted.color(cx))
                    .child(MarkdownElement::new(md_entity, style).on_url_click(
                        move |url, window, cx| {
                            if open_agent_link(
                                url.as_ref(),
                                &workspace_handle,
                                cwd.as_ref(),
                                window,
                                cx,
                            ) {
                                if let Some(view) = this_weak.upgrade() {
                                    view.update(cx, |_, cx| {
                                        cx.emit(TranscriptViewEvent::OpenedInEditor);
                                    });
                                }
                            }
                        },
                    ))
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
                            this.request_preview(clickable_path.clone(), cx);
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

    fn render_user_message_body(
        &self,
        message_index: usize,
        text: &str,
        mentions: &[ResolvedFileMention],
        text_color: Color,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut children = Vec::new();
        let mut cursor = 0usize;

        for (mention_index, mention) in mentions.iter().enumerate() {
            if mention.source_range.start < cursor || mention.source_range.end > text.len() {
                continue;
            }

            if cursor < mention.source_range.start {
                children.push(
                    div()
                        .child(SharedString::from(
                            text[cursor..mention.source_range.start].to_string(),
                        ))
                        .into_any_element(),
                );
            }

            let label = attachment_display_name(&mention.abs_path);
            let tooltip: SharedString = mention.abs_path.to_string_lossy().to_string().into();
            children.push(file_mention_pill_element(
                SharedString::from(format!(
                    "message-file-mention-{message_index}-{mention_index}-{}",
                    mention.abs_path.display()
                )),
                label,
                tooltip,
                mention.abs_path.clone(),
                self.workspace.clone(),
            ));
            cursor = mention.source_range.end;
        }

        if cursor < text.len() {
            children.push(
                div()
                    .child(SharedString::from(text[cursor..].to_string()))
                    .into_any_element(),
            );
        }

        h_flex()
            .text_color(text_color.color(cx))
            .text_sm()
            .whitespace_normal()
            .items_center()
            .flex_wrap()
            .children(children)
            .into_any_element()
    }
}

impl Render for TranscriptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_thread.is_none() {
            return div().flex_1().into_any_element();
        }

        let list_state = self.list_state.clone();

        v_flex()
            .id("agent-transcript-container")
            .flex_1()
            .w_full()
            .relative()
            .overflow_hidden()
            .child(
                list(self.list_state.clone(), cx.processor(Self::render_item))
                    .with_sizing_behavior(ListSizingBehavior::Auto)
                    .size_full(),
            )
            .vertical_scrollbar_for(&list_state, _window, cx)
            .into_any_element()
    }
}

impl EventEmitter<TranscriptViewEvent> for TranscriptView {}

pub struct AgentsSurface {
    workspace: Entity<workspace::Workspace>,
    composer_editor: Entity<Editor>,
    mention_crease_ids: Vec<CreaseId>,
    transcript_view: Entity<TranscriptView>,
    pub(crate) active_thread_by_path: HashMap<String, HarnessThreadId>,
    pub(crate) threads_by_path: HashMap<String, Vec<HarnessThread>>,
    next_thread_number: usize,
    pending_attachments: Vec<PathBuf>,
    previewing_attachment: Option<PathBuf>,
    selected_model: String,
    selected_reasoning_effort: String,
    selected_sandbox_policy: HarnessSandboxPolicy,
    available_skills: Vec<SkillDefinition>,
    available_skills_loaded: bool,
    available_skills_cwd: Option<PathBuf>,
    skills_refresh_task: Option<Task<()>>,
    codex_sessions: HashMap<HarnessThreadId, CodexSessionHandle>,
    streaming_notify_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

const STREAMING_NOTIFY_INTERVAL: Duration = Duration::from_millis(16);

impl AgentsSurface {
    pub(crate) fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = multi_workspace.read(cx).workspace().clone();
        let transcript_view = cx.new(|_| TranscriptView::new(workspace.clone()));
        let surface = cx.weak_entity();

        let composer_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor.set_placeholder_text(
                "Ask anything, @ to add files, $ or / for skills",
                window,
                cx,
            );
            editor.set_show_indent_guides(false, cx);
            editor.set_cursor_blink(false, cx);
            editor.set_show_completions_on_input(Some(true));
            editor.set_context_menu_options(ContextMenuOptions {
                min_entries_visible: 12,
                max_entries_visible: 12,
                placement: None,
            });
            editor
        });
        let completion_provider = Rc::new(ComposerFileCompletionProvider::new(
            multi_workspace.downgrade(),
            surface,
            composer_editor.downgrade(),
        ));
        composer_editor.update(cx, |editor, _cx| {
            editor.set_completion_provider(Some(completion_provider))
        });

        let active_workspace_subscription = cx.subscribe_in(
            &multi_workspace,
            window,
            |this, multi_workspace, event: &MultiWorkspaceEvent, _window, cx| {
                if matches!(event, MultiWorkspaceEvent::ActiveWorkspaceChanged) {
                    this.workspace = multi_workspace.read(cx).workspace().clone();
                    this.sync_transcript_view(cx);
                    this.refresh_available_skills(cx);
                    cx.notify();
                }
            },
        );

        let transcript_subscription = cx.subscribe_in(
            &transcript_view,
            window,
            |this, _, event: &TranscriptViewEvent, _window, cx| match event {
                TranscriptViewEvent::OpenedInEditor => {
                    cx.emit(AgentsSurfaceEvent::OpenedInEditor);
                }
                TranscriptViewEvent::PreviewRequested(path) => {
                    this.preview_attachment(path.clone(), cx);
                }
            },
        );

        let composer_subscription = cx.subscribe_in(
            &composer_editor,
            window,
            |this, editor, event: &editor::EditorEvent, _window, cx| {
                if !matches!(event, editor::EditorEvent::Edited { .. }) {
                    return;
                }

                if this.mention_crease_ids.is_empty() {
                    return;
                }

                let should_clear_mentions = editor.read(cx).text(cx).trim().is_empty();
                if should_clear_mentions {
                    let crease_ids = std::mem::take(&mut this.mention_crease_ids);
                    editor.update(cx, |editor, cx| {
                        editor.remove_creases(crease_ids, cx);
                    });
                }
            },
        );

        let mut this = Self {
            workspace,
            composer_editor,
            mention_crease_ids: Vec::new(),
            transcript_view,
            active_thread_by_path: HashMap::default(),
            threads_by_path: HashMap::default(),
            next_thread_number: 1,
            pending_attachments: Vec::new(),
            previewing_attachment: None,
            selected_model: DEFAULT_MODEL.to_string(),
            selected_reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            selected_sandbox_policy: HarnessSandboxPolicy::DangerFullAccess,
            available_skills: Vec::new(),
            available_skills_loaded: false,
            available_skills_cwd: None,
            skills_refresh_task: None,
            codex_sessions: HashMap::new(),
            streaming_notify_task: None,
            _subscriptions: vec![
                active_workspace_subscription,
                transcript_subscription,
                composer_subscription,
            ],
        };
        this.sync_transcript_view(cx);
        this.refresh_available_skills(cx);
        this
    }

    /// Coalesce rapid streaming updates into at most one re-render per
    /// STREAMING_NOTIFY_INTERVAL. Use for AssistantDelta/ToolEvent paths that
    /// would otherwise fire `cx.notify()` once per token.
    fn schedule_streaming_notify(&mut self, cx: &mut Context<Self>) {
        if self.streaming_notify_task.is_some() {
            return;
        }
        self.streaming_notify_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(STREAMING_NOTIFY_INTERVAL)
                .await;
            this.update(cx, |this, cx| {
                this.streaming_notify_task = None;
                this.sync_transcript_view(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn sync_transcript_view(&mut self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let thread = self.active_thread(cx).cloned();
        self.transcript_view.update(cx, |transcript_view, cx| {
            transcript_view.sync_state(workspace, thread, cx);
        });
    }

    fn current_cwd(&self, cx: &App) -> Option<PathBuf> {
        workspace_root_path(&self.workspace, cx).or_else(|| std::env::current_dir().ok())
    }

    fn available_skills_for_completion(
        &self,
        cx: &App,
    ) -> (Vec<SkillDefinition>, Option<PathBuf>, bool) {
        let cwd = self.current_cwd(cx);
        let stale = cwd.as_ref() != self.available_skills_cwd.as_ref();
        let skills = if stale {
            Vec::new()
        } else {
            self.available_skills.clone()
        };
        let should_load =
            stale || (!self.available_skills_loaded && self.skills_refresh_task.is_none());
        (skills, cwd, should_load)
    }

    fn refresh_available_skills(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.current_cwd(cx) else {
            return;
        };

        if self.skills_refresh_task.is_some() && self.available_skills_cwd.as_ref() == Some(&cwd) {
            return;
        }

        self.available_skills_cwd = Some(cwd.clone());
        self.available_skills_loaded = false;
        let loaded_cwd = cwd.clone();
        self.skills_refresh_task = Some(cx.spawn(async move |this, cx| {
            let load_task =
                cx.background_spawn(async move { load_codex_available_skills(cwd).await });
            let result = load_task.await;
            this.update(cx, |this, cx| {
                this.skills_refresh_task = None;
                match result {
                    Ok(skills) if this.available_skills_cwd.as_ref() == Some(&loaded_cwd) => {
                        this.available_skills = skills;
                        this.available_skills_loaded = true;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("failed to load Codex skills: {error}");
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn notify_thread_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_transcript_view(cx);
        cx.notify();
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
        self.notify_thread_changed(cx);
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
        self.notify_thread_changed(cx);
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
        self.notify_thread_changed(cx);
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mentioned_attachments = self.resolve_mentioned_attachments(cx);
        let has_attachments =
            !self.pending_attachments.is_empty() || !mentioned_attachments.is_empty();
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
            self.notify_thread_changed(cx);
            return;
        }

        self.composer_editor.update(cx, |editor, cx| {
            if !self.mention_crease_ids.is_empty() {
                editor.remove_creases(std::mem::take(&mut self.mention_crease_ids), cx);
            }
            editor.clear(window, cx);
        });
        let mut attachments = std::mem::take(&mut self.pending_attachments);
        for mentioned_path in mentioned_attachments {
            if !attachments.iter().any(|path| path == &mentioned_path) {
                attachments.push(mentioned_path);
            }
        }

        // Sending a new message re-engages autoscroll so the user always sees
        // their own turn land at the bottom even if they were scrolled up to
        // reread earlier context.
        self.transcript_view.update(cx, |transcript_view, _| {
            transcript_view.pin_to_bottom();
        });

        let Some(turn_input) = self.prepare_turn_input(thread_id.clone(), text, attachments, cx)
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
            self.notify_thread_changed(cx);
        }
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
        let path_style = self.workspace.read(cx).path_style(cx);
        let skill_mentions = self.resolve_skill_mentions(&text, cx);
        let sanitized_text = sanitize_skill_mentions(&text, path_style);
        let combined_input = build_input_with_attachments(&sanitized_text, &attachments);
        let file_mentions = resolve_file_mention_spans(&self.workspace, &sanitized_text, cx);
        let thread = self.thread_mut(&thread_id)?;

        if thread.messages.is_empty() {
            let attachment_fallback = attachments
                .first()
                .map(|first| attachment_display_name(first).to_string());
            let title_source: &str = if !sanitized_text.is_empty() {
                sanitized_text.as_str()
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

        // Pre-turn estimate is only used until codex reports real usage via
        // turn/completed; once we have real numbers we stop accumulating
        // guesses.
        if !thread.has_reported_tokens {
            thread.estimated_tokens_used += combined_input.len() / 4;
        }
        thread.messages.push(TranscriptMessage {
            role: TranscriptRole::User,
            text: sanitized_text,
            attachments,
            file_mentions,
            started_at: None,
            duration_ms: None,
        });
        thread.run_status = HarnessRunStatus::Connecting;
        self.notify_thread_changed(cx);

        Some(HarnessTurnInput {
            input: combined_input,
            skill_mentions,
            model: self.selected_model.clone(),
            reasoning_effort: self.selected_reasoning_effort.clone(),
            approval_policy: HarnessApprovalPolicy::Never,
            sandbox_policy: self.selected_sandbox_policy.clone(),
        })
    }

    fn resolve_mentioned_attachments(&self, cx: &App) -> Vec<PathBuf> {
        let composer_text = self.composer_editor.read(cx).text(cx);
        let composer_text =
            sanitize_skill_mentions(&composer_text, self.workspace.read(cx).path_style(cx));
        parse_file_mentions(&composer_text)
            .into_iter()
            .filter_map(|mention| {
                resolve_agent_link(&self.workspace, std::path::Path::new(&mention), cx)
            })
            .fold(Vec::new(), |mut attachments, path| {
                if !attachments.iter().any(|existing| existing == &path) {
                    attachments.push(path);
                }
                attachments
            })
    }

    fn resolve_skill_mentions(&self, text: &str, cx: &App) -> Vec<HarnessSkillMention> {
        parse_skill_mention_spans(text, self.workspace.read(cx).path_style(cx))
            .into_iter()
            .filter_map(|mention| {
                let path = mention.path.or_else(|| {
                    self.available_skills
                        .iter()
                        .find(|skill| skill.name.as_ref() == mention.name && skill.path.is_some())
                        .and_then(|skill| skill.path.clone())
                })?;
                Some(HarnessSkillMention {
                    name: mention.name,
                    path,
                })
            })
            .fold(Vec::new(), |mut mentions, mention| {
                if !mentions
                    .iter()
                    .any(|existing| existing.name == mention.name && existing.path == mention.path)
                {
                    mentions.push(mention);
                }
                mentions
            })
    }

    fn apply_turn_update(&mut self, update: HarnessTurnUpdate, cx: &mut Context<Self>) {
        // Streaming updates (assistant deltas, tool events) can fire once per
        // token. Coalesce those into a throttled re-render; all other updates
        // are infrequent enough that an immediate notify keeps the UI snappy.
        let is_streaming = matches!(
            &update,
            HarnessTurnUpdate::AssistantDelta { .. } | HarnessTurnUpdate::ToolEvent { .. }
        );

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
                            if *status == ToolStatus::Running && next_status != ToolStatus::Running
                            {
                                transitioned_to_terminal = true;
                            }
                            *status = next_status;
                        }
                        if transitioned_to_terminal
                            && message.duration_ms.is_none()
                            && let Some(started) = message.started_at.take()
                        {
                            message.duration_ms = Some(
                                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                            );
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

        if is_streaming {
            self.schedule_streaming_notify(cx);
        } else {
            // Also flush any pending throttled notify so this immediate
            // notify subsumes it.
            self.streaming_notify_task = None;
            self.notify_thread_changed(cx);
        }
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

    pub(crate) fn restore_threads(
        &mut self,
        groups: Vec<SerializedThreadGroup>,
        cx: &mut Context<Self>,
    ) {
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
                    .map(|message| {
                        let role = match message.role {
                            SerializedTranscriptRole::User => TranscriptRole::User,
                            SerializedTranscriptRole::Assistant => TranscriptRole::Assistant,
                            SerializedTranscriptRole::System => TranscriptRole::System,
                            SerializedTranscriptRole::Tool {
                                title,
                                item_id,
                                tool_kind,
                                status,
                            } => {
                                let mut kind = tool_kind.into_display();
                                let mut display_title: SharedString = title.into();
                                if kind == ToolDisplayKind::Other
                                    && display_title.as_ref() == "mcpToolCall"
                                {
                                    kind = ToolDisplayKind::McpToolCall;
                                    display_title = "MCP tool call".into();
                                }
                                TranscriptRole::Tool {
                                    item_id,
                                    kind,
                                    status: status.into_status(),
                                    title: display_title,
                                }
                            }
                        };
                        let text = message.text;
                        let file_mentions = if matches!(role, TranscriptRole::User) {
                            resolve_file_mention_spans(&self.workspace, &text, cx)
                        } else {
                            Vec::new()
                        };

                        TranscriptMessage {
                            role,
                            text,
                            attachments: message.attachments,
                            file_mentions,
                            started_at: None,
                            duration_ms: message.duration_ms,
                        }
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

        self.sync_transcript_view(cx);
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = workspace_display_name(&self.workspace, cx);
        let has_active_thread = self.active_thread(cx).is_some();
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
            .child(if has_active_thread {
                self.transcript_view.clone().into_any_element()
            } else {
                self.render_welcome(project_name, cx).into_any_element()
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
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.dismiss_preview(cx);
                                            }),
                                        ),
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
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
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
                    },
                ))
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

#[cfg(test)]
mod tests {
    use util::paths::PathStyle;

    use super::{
        ComposerQuery, FileMentionQuery, FileMentionSpan, SkillMentionQuery, SkillMentionSpan,
        format_file_mention, parse_file_mention_spans, parse_file_mentions,
        parse_skill_mention_spans, sanitize_skill_mentions,
    };

    #[test]
    fn parses_plain_file_mentions() {
        assert_eq!(
            parse_file_mentions("please check @src/main.rs and @crates/app/lib.rs"),
            vec!["src/main.rs", "crates/app/lib.rs"]
        );
    }

    #[test]
    fn parses_quoted_file_mentions() {
        assert_eq!(
            parse_file_mentions("review @\"src/my file.rs\" next"),
            vec!["src/my file.rs"]
        );
    }

    #[test]
    fn parses_file_mention_source_ranges() {
        assert_eq!(
            parse_file_mention_spans("explain @src/main.rs, then @\"src/my file.rs\""),
            vec![
                FileMentionSpan {
                    source_range: 8..20,
                    path: "src/main.rs".to_string(),
                },
                FileMentionSpan {
                    source_range: 27..44,
                    path: "src/my file.rs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn ignores_email_addresses() {
        assert!(parse_file_mentions("hello test@example.com").is_empty());
    }

    #[test]
    fn formats_mentions_with_quotes_when_needed() {
        assert_eq!(format_file_mention("src/main.rs"), "@src/main.rs");
        assert_eq!(format_file_mention("src/my file.rs"), "@\"src/my file.rs\"");
    }

    #[test]
    fn parses_active_mention_query() {
        assert_eq!(
            FileMentionQuery::try_parse("Look at @src/main.rs", 0),
            Some(FileMentionQuery {
                source_range: 8..20,
                query: Some("src/main.rs".to_string()),
            })
        );
        assert_eq!(
            FileMentionQuery::try_parse("Look at @", 0),
            Some(FileMentionQuery {
                source_range: 8..9,
                query: None,
            })
        );
        assert_eq!(
            FileMentionQuery::try_parse("Look at @\"src/my file.rs", 0),
            Some(FileMentionQuery {
                source_range: 8..24,
                query: Some("src/my file.rs".to_string()),
            })
        );
    }

    #[test]
    fn prefers_active_trigger_over_earlier_mentions() {
        assert_eq!(
            ComposerQuery::try_parse("@src/main.rs $linear", 0),
            Some(ComposerQuery::Skill(SkillMentionQuery {
                source_range: 13..20,
                query: Some("linear".to_string()),
            }))
        );
        assert_eq!(
            ComposerQuery::try_parse("/linear @src/main.rs", 0),
            Some(ComposerQuery::File(FileMentionQuery {
                source_range: 8..20,
                query: Some("src/main.rs".to_string()),
            }))
        );
    }

    #[test]
    fn parses_skill_mention_spans() {
        assert_eq!(
            parse_skill_mention_spans(
                "Use [@Linear Workflow](zed:///agent/skill/Linear%20Workflow?description=Linear+MCP+integration&path=%2Ftmp%2Fskill%2FSKILL.md)",
                PathStyle::local(),
            ),
            vec![SkillMentionSpan {
                source_range: 4..126,
                name: "Linear Workflow".to_string(),
                path: Some("/tmp/skill/SKILL.md".into()),
            }]
        );
    }

    #[test]
    fn sanitizes_skill_mentions_for_display() {
        assert_eq!(
            sanitize_skill_mentions(
                "Use [@Linear Workflow](zed:///agent/skill/Linear%20Workflow?description=Linear+MCP+integration) next",
                PathStyle::local(),
            ),
            "Use $Linear Workflow next"
        );
    }
}
