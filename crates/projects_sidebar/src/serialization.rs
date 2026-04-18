use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::mode::WorkspaceMode;
use crate::transcript::{ToolDisplayKind, ToolStatus};

#[derive(Deserialize, Serialize, Default)]
pub(crate) struct SerializedProjectsSidebar {
    pub(crate) mode: WorkspaceMode,
    #[serde(default)]
    pub(crate) thread_groups: Vec<SerializedThreadGroup>,
    #[serde(default)]
    pub(crate) next_thread_number: usize,
    #[serde(default)]
    pub(crate) selected_model: Option<String>,
    #[serde(default)]
    pub(crate) selected_reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) selected_sandbox_policy: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SerializedThreadGroup {
    pub(crate) workspace_path: String,
    #[serde(default)]
    pub(crate) active_thread_id: Option<String>,
    pub(crate) threads: Vec<SerializedHarnessThread>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SerializedHarnessThread {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) provider_thread_id: Option<String>,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) messages: Vec<SerializedTranscriptMessage>,
    #[serde(default)]
    pub(crate) estimated_tokens: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SerializedTranscriptMessage {
    pub(crate) role: SerializedTranscriptRole,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) attachments: Vec<PathBuf>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SerializedTranscriptRole {
    User,
    Assistant,
    System,
    Tool {
        title: String,
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        tool_kind: SerializedToolKind,
        #[serde(default)]
        status: SerializedToolStatus,
    },
}

#[derive(Deserialize, Serialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SerializedToolKind {
    #[default]
    Other,
    Command,
    FileRead,
    FileChange,
    WebSearch,
    Reasoning,
}

impl SerializedToolKind {
    pub(crate) fn from_display(kind: ToolDisplayKind) -> Self {
        match kind {
            ToolDisplayKind::Command => Self::Command,
            ToolDisplayKind::FileRead => Self::FileRead,
            ToolDisplayKind::FileChange => Self::FileChange,
            ToolDisplayKind::WebSearch => Self::WebSearch,
            ToolDisplayKind::Reasoning => Self::Reasoning,
            ToolDisplayKind::Other => Self::Other,
        }
    }

    pub(crate) fn into_display(self) -> ToolDisplayKind {
        match self {
            Self::Command => ToolDisplayKind::Command,
            Self::FileRead => ToolDisplayKind::FileRead,
            Self::FileChange => ToolDisplayKind::FileChange,
            Self::WebSearch => ToolDisplayKind::WebSearch,
            Self::Reasoning => ToolDisplayKind::Reasoning,
            Self::Other => ToolDisplayKind::Other,
        }
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SerializedToolStatus {
    #[default]
    Completed,
    Running,
    Failed,
}

impl SerializedToolStatus {
    pub(crate) fn from_status(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Running => Self::Running,
            ToolStatus::Completed => Self::Completed,
            ToolStatus::Failed => Self::Failed,
        }
    }

    pub(crate) fn into_status(self) -> ToolStatus {
        match self {
            // Treat any restored "running" state as completed — we can't recover
            // a live run from a previous session.
            Self::Running | Self::Completed => ToolStatus::Completed,
            Self::Failed => ToolStatus::Failed,
        }
    }
}
