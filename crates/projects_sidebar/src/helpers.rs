use gpui::{Animation, AnimationExt, App, Entity, IntoElement, SharedString};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use ui::{IconName, prelude::*};

use crate::transcript::ToolDisplayKind;

pub(crate) fn workspace_display_name(
    workspace: &Entity<workspace::Workspace>,
    cx: &App,
) -> SharedString {
    let root_paths = workspace.read(cx).root_paths(cx);
    root_paths
        .first()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(|name| SharedString::from(name.to_string()))
        .unwrap_or_else(|| "Untitled project".into())
}

pub(crate) fn path_display_label(path: &Path) -> SharedString {
    path.to_string_lossy().to_string().into()
}

pub(crate) fn workspace_root_path(
    workspace: &Entity<workspace::Workspace>,
    cx: &App,
) -> Option<PathBuf> {
    workspace
        .read(cx)
        .root_paths(cx)
        .first()
        .map(|path| path.as_ref().to_path_buf())
}

pub(crate) fn url_has_scheme(url: &str) -> bool {
    // Treat anything with a `scheme://` prefix or a `mailto:`/`tel:`-style
    // prefix as an actual URL. Bare paths like `WebViewSupport.cpp` should
    // not be handed to the OS `open` command.
    if let Some((scheme, _)) = url.split_once(':') {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && !scheme.contains('/')
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    } else {
        false
    }
}

pub(crate) fn workspace_storage_key(
    workspace: &Entity<workspace::Workspace>,
    cx: &App,
) -> Option<String> {
    let paths = workspace.read(cx).root_paths(cx);
    if paths.is_empty() {
        return None;
    }
    Some(paths_storage_key(&paths))
}

pub(crate) fn paths_storage_key<P: AsRef<Path>>(paths: &[P]) -> String {
    let mut joined = String::new();
    for (index, path) in paths.iter().enumerate() {
        if index > 0 {
            joined.push('|');
        }
        joined.push_str(&path.as_ref().to_string_lossy());
    }
    joined
}

/// Splits a tool message title into a primary label (e.g. "Run command") and an
/// optional one-line detail (e.g. "ls -la"). Used to render the collapsed
/// header as `[icon] [primary] [detail]` without showing the full body.
pub(crate) fn tool_summary_line(
    title: &SharedString,
    kind: ToolDisplayKind,
) -> (SharedString, Option<SharedString>) {
    if let Some((primary, detail)) = title.split_once(": ") {
        let primary: SharedString = primary.trim().to_string().into();
        let detail: SharedString = detail.trim().to_string().into();
        return (primary, Some(detail));
    }

    let default_primary: SharedString = match kind {
        ToolDisplayKind::Command => "Run command".into(),
        ToolDisplayKind::FileRead => "Read file".into(),
        ToolDisplayKind::FileChange => "Edit file".into(),
        ToolDisplayKind::WebSearch => "Web search".into(),
        ToolDisplayKind::Reasoning => "Thought".into(),
        ToolDisplayKind::McpToolCall => "MCP tool call".into(),
        ToolDisplayKind::Other => title.clone(),
    };

    (default_primary, None)
}

pub(crate) fn animated_thinking_label() -> impl IntoElement {
    let frames: Vec<&'static str> = vec!["Thinking", "Thinking.", "Thinking..", "Thinking..."];
    Label::new(frames[0])
        .size(LabelSize::Small)
        .color(Color::Muted)
        .with_animation(
            "harness-thinking",
            Animation::new(Duration::from_millis(1200)).repeat(),
            move |mut label, delta| {
                let frame_index = (delta * frames.len() as f32) as usize % frames.len();
                label.set_text(frames[frame_index]);
                label
            },
        )
}

pub(crate) fn build_input_with_attachments(text: &str, attachments: &[PathBuf]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }

    let mut combined = text.to_string();
    if !combined.is_empty() {
        combined.push_str("\n\n");
    }
    combined.push_str("Attached files:");
    for path in attachments {
        combined.push_str("\n- ");
        combined.push_str(&path.to_string_lossy());
    }
    combined
}

pub(crate) fn attachment_display_name(path: &Path) -> SharedString {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| SharedString::from(name.to_string()))
        .unwrap_or_else(|| SharedString::from(path.to_string_lossy().to_string()))
}

pub(crate) fn attachment_icon(path: &Path) -> IconName {
    if is_image_path(path) {
        IconName::Image
    } else {
        IconName::File
    }
}

pub(crate) fn is_image_path(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    matches!(
        extension.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico")
    )
}

