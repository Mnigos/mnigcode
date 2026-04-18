use gpui::SharedString;
use std::path::PathBuf;
use ui::IconName;

use crate::harness::{HarnessKind, HarnessRunStatus, HarnessThreadId, HarnessToolKind};

#[derive(Clone)]
pub(crate) struct HarnessThread {
    pub(crate) id: HarnessThreadId,
    pub(crate) provider_thread_id: Option<String>,
    pub(crate) title: SharedString,
    pub(crate) cwd: PathBuf,
    pub(crate) harness_kind: HarnessKind,
    pub(crate) run_status: HarnessRunStatus,
    pub(crate) messages: Vec<TranscriptMessage>,
    pub(crate) estimated_tokens_used: usize,
    pub(crate) has_reported_tokens: bool,
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
}

#[derive(Clone)]
pub(crate) enum TranscriptRole {
    User,
    Assistant,
    System,
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
