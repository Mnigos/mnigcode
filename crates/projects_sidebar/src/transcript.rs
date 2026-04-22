use gpui::SharedString;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;
use ui::IconName;

use crate::harness::{
    HarnessFileChange, HarnessKind, HarnessRunStatus, HarnessThreadId, HarnessToolKind,
};

#[derive(Clone)]
pub(crate) struct HarnessThread {
    pub(crate) id: HarnessThreadId,
    pub(crate) provider_thread_id: Option<String>,
    pub(crate) title: SharedString,
    pub(crate) cwd: PathBuf,
    pub(crate) harness_kind: HarnessKind,
    pub(crate) run_status: HarnessRunStatus,
    pub(crate) messages: Vec<TranscriptMessage>,
    pub(crate) tokens_used: usize,
    pub(crate) model_context_window: Option<usize>,
}

#[derive(Clone)]
pub struct HarnessThreadSummary {
    pub(crate) id: HarnessThreadId,
    pub(crate) title: SharedString,
    pub(crate) harness_kind: HarnessKind,
    pub(crate) run_status: HarnessRunStatus,
    pub(crate) is_active: bool,
}

#[derive(Clone)]
pub(crate) struct TranscriptMessage {
    pub(crate) role: TranscriptRole,
    pub(crate) text: String,
    pub(crate) attachments: Vec<PathBuf>,
    pub(crate) file_mentions: Vec<ResolvedFileMention>,
    pub(crate) file_changes: Vec<HarnessFileChange>,
    /// Wall-clock start time captured when the tool message first lands in
    /// the transcript. Only set for Tool messages; kept in-memory so the
    /// renderer can show "Thought for Xs" once the tool completes.
    pub(crate) started_at: Option<Instant>,
    /// Milliseconds elapsed between start and completion, populated when the
    /// tool transitions to a terminal status. Persisted so restored threads
    /// keep the elapsed label.
    pub(crate) duration_ms: Option<u64>,
}

impl TranscriptMessage {
    pub(crate) fn new(role: TranscriptRole, text: String) -> Self {
        Self {
            role,
            text,
            attachments: Vec::new(),
            file_mentions: Vec::new(),
            file_changes: Vec::new(),
            started_at: None,
            duration_ms: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedFileMention {
    pub(crate) source_range: Range<usize>,
    pub(crate) abs_path: PathBuf,
}

#[derive(Clone)]
pub(crate) enum TranscriptRole {
    User,
    Assistant,
    System,
    ChangeSummary,
    Tool {
        item_id: Option<String>,
        kind: ToolDisplayKind,
        status: ToolStatus,
        title: SharedString,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolDisplayKind {
    Command,
    FileRead,
    FileChange,
    WebSearch,
    Reasoning,
    McpToolCall,
    Other,
}

impl ToolDisplayKind {
    pub(crate) fn from_harness(kind: &HarnessToolKind) -> Self {
        match kind {
            HarnessToolKind::Command => Self::Command,
            HarnessToolKind::FileRead => Self::FileRead,
            HarnessToolKind::FileChange => Self::FileChange,
            HarnessToolKind::WebSearch => Self::WebSearch,
            HarnessToolKind::Reasoning => Self::Reasoning,
            HarnessToolKind::McpToolCall => Self::McpToolCall,
            HarnessToolKind::Other(_) => Self::Other,
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            ToolDisplayKind::Command => IconName::Terminal,
            ToolDisplayKind::FileRead => IconName::FileTextOutlined,
            ToolDisplayKind::FileChange => IconName::Pencil,
            ToolDisplayKind::WebSearch => IconName::MagnifyingGlass,
            ToolDisplayKind::Reasoning => IconName::ToolThink,
            ToolDisplayKind::McpToolCall => IconName::ToolHammer,
            ToolDisplayKind::Other => IconName::ToolHammer,
        }
    }

    pub(crate) fn body_is_monospace(self) -> bool {
        matches!(
            self,
            ToolDisplayKind::Command | ToolDisplayKind::FileRead | ToolDisplayKind::FileChange
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Running,
    Completed,
    Failed,
}
