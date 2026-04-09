use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _,
    io::{BufReader, BufWriter},
};
use gpui::SharedString;
use serde_json::{Value, json};
use smol::{channel::Sender, process::Command};
use std::{path::PathBuf, process::Stdio};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HarnessThreadId(pub SharedString);

#[derive(Clone, Debug)]
pub enum HarnessKind {
    Codex,
}

#[derive(Clone, Debug)]
pub enum HarnessRunStatus {
    Idle,
    Connecting,
    Thinking,
    Running,
    Failed(SharedString),
}

impl HarnessRunStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            HarnessRunStatus::Connecting | HarnessRunStatus::Thinking | HarnessRunStatus::Running
        )
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum HarnessApprovalPolicy {
    Never,
    OnRequest,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum HarnessSandboxPolicy {
    DangerFullAccess,
    WorkspaceWrite,
}

#[derive(Clone, Debug)]
pub struct HarnessTurnRequest {
    pub thread_id: HarnessThreadId,
    pub provider_thread_id: Option<String>,
    pub cwd: PathBuf,
    pub input: String,
    pub model: String,
    pub reasoning_effort: String,
    pub approval_policy: HarnessApprovalPolicy,
    pub sandbox_policy: HarnessSandboxPolicy,
}

#[derive(Clone, Debug)]
pub enum HarnessTurnUpdate {
    Status {
        thread_id: HarnessThreadId,
        status: HarnessRunStatus,
    },
    ThreadReady {
        thread_id: HarnessThreadId,
        provider_thread_id: String,
    },
    AssistantDelta {
        thread_id: HarnessThreadId,
        delta: String,
    },
    ToolEvent {
        thread_id: HarnessThreadId,
        item_id: Option<String>,
        kind: HarnessToolKind,
        phase: HarnessToolPhase,
        title: SharedString,
        detail: SharedString,
    },
    Finished {
        thread_id: HarnessThreadId,
    },
    Failed {
        thread_id: HarnessThreadId,
        message: SharedString,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarnessToolKind {
    Command,
    FileRead,
    FileChange,
    WebSearch,
    Reasoning,
    Other(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessToolPhase {
    Start,
    Update,
    End,
}

pub async fn run_codex_app_server_turn(
    request: HarnessTurnRequest,
    updates: Sender<HarnessTurnUpdate>,
) {
    if let Err(error) = run_codex_app_server_turn_impl(request.clone(), updates.clone()).await {
        send_update(
            &updates,
            HarnessTurnUpdate::Failed {
                thread_id: request.thread_id,
                message: error.to_string().into(),
            },
        )
        .await;
    }
}

async fn run_codex_app_server_turn_impl(
    request: HarnessTurnRequest,
    updates: Sender<HarnessTurnUpdate>,
) -> Result<()> {
    send_status(
        &updates,
        request.thread_id.clone(),
        HarnessRunStatus::Connecting,
    )
    .await;

    let mut command = Command::new("codex");
    command
        .arg("app-server")
        .current_dir(&request.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start `codex app-server` in {}",
            request.cwd.display()
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .context("codex app-server stdin was not available")?;
    let stdout = child
        .stdout
        .take()
        .context("codex app-server stdout was not available")?;
    let stderr = child
        .stderr
        .take()
        .context("codex app-server stderr was not available")?;

    smol::spawn(forward_stderr(
        request.thread_id.clone(),
        stderr,
        updates.clone(),
    ))
    .detach();

    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);
    let mut next_request_id = 1;

    let initialize_id = next_request_id;
    next_request_id += 1;
    write_message(
        &mut writer,
        json!({
            "method": "initialize",
            "id": initialize_id,
            "params": {
                "clientInfo": {
                    "name": "mnig_code",
                    "title": "Mnig Code",
                    "version": "0.1.0",
                },
            },
        }),
    )
    .await?;
    read_until_response(
        &mut reader,
        &mut writer,
        initialize_id,
        &request.thread_id,
        &updates,
    )
    .await?;

    write_message(
        &mut writer,
        json!({
            "method": "initialized",
            "params": {},
        }),
    )
    .await?;

    let thread_open_id = next_request_id;
    next_request_id += 1;
    let thread_open_message = if let Some(provider_thread_id) = request.provider_thread_id.clone() {
        json!({
            "method": "thread/resume",
            "id": thread_open_id,
            "params": {
                "threadId": provider_thread_id,
                "cwd": request.cwd.to_string_lossy(),
                "model": request.model,
                "approvalPolicy": approval_policy_name(&request.approval_policy),
                "sandbox": sandbox_policy_name(&request.sandbox_policy),
                "serviceName": "mnig_code",
            },
        })
    } else {
        json!({
            "method": "thread/start",
            "id": thread_open_id,
            "params": {
                "cwd": request.cwd.to_string_lossy(),
                "model": request.model,
                "approvalPolicy": approval_policy_name(&request.approval_policy),
                "sandbox": sandbox_policy_name(&request.sandbox_policy),
                "serviceName": "mnig_code",
            },
        })
    };
    write_message(&mut writer, thread_open_message).await?;
    let thread_open_result = read_until_response(
        &mut reader,
        &mut writer,
        thread_open_id,
        &request.thread_id,
        &updates,
    )
    .await?;
    let provider_thread_id = read_thread_id(&thread_open_result)
        .or(request.provider_thread_id)
        .context("codex app-server did not return a thread id")?;

    send_update(
        &updates,
        HarnessTurnUpdate::ThreadReady {
            thread_id: request.thread_id.clone(),
            provider_thread_id: provider_thread_id.clone(),
        },
    )
    .await;
    send_status(
        &updates,
        request.thread_id.clone(),
        HarnessRunStatus::Thinking,
    )
    .await;

    let turn_start_id = next_request_id;
    write_message(
        &mut writer,
        json!({
            "method": "turn/start",
            "id": turn_start_id,
            "params": {
                "threadId": provider_thread_id,
                "input": [
                    {
                        "type": "text",
                        "text": request.input,
                    },
                ],
                "cwd": request.cwd.to_string_lossy(),
                "model": request.model,
                "effort": request.reasoning_effort,
                "approvalPolicy": approval_policy_name(&request.approval_policy),
                "sandboxPolicy": sandbox_policy_value(&request.sandbox_policy),
            },
        }),
    )
    .await?;
    read_until_response(
        &mut reader,
        &mut writer,
        turn_start_id,
        &request.thread_id,
        &updates,
    )
    .await?;
    send_status(
        &updates,
        request.thread_id.clone(),
        HarnessRunStatus::Running,
    )
    .await;

    read_until_turn_finished(&mut reader, &mut writer, &request.thread_id, &updates).await?;

    child.kill().ok();
    Ok(())
}

async fn read_until_response<Reader, Writer>(
    reader: &mut BufReader<Reader>,
    writer: &mut BufWriter<Writer>,
    expected_id: i64,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
) -> Result<Value>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(message) = read_message(reader).await? else {
            bail!("codex app-server exited before responding");
        };

        if let Some(result) =
            handle_protocol_message(message, writer, thread_id, updates, Some(expected_id)).await?
        {
            return Ok(result);
        }
    }
}

async fn read_until_turn_finished<Reader, Writer>(
    reader: &mut BufReader<Reader>,
    writer: &mut BufWriter<Writer>,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
) -> Result<()>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(message) = read_message(reader).await? else {
            bail!("codex app-server exited before the turn completed");
        };

        if let Some(method) = message.get("method").and_then(Value::as_str)
            && method == "turn/completed"
        {
            handle_protocol_message(message, writer, thread_id, updates, None).await?;
            return Ok(());
        }

        handle_protocol_message(message, writer, thread_id, updates, None).await?;
    }
}

async fn handle_protocol_message<Writer>(
    message: Value,
    writer: &mut BufWriter<Writer>,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
    expected_response_id: Option<i64>,
) -> Result<Option<Value>>
where
    Writer: AsyncWrite + Unpin,
{
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if let Some(request_id) = message.get("id").cloned() {
            respond_to_server_request(writer, request_id, method).await?;
            return Ok(None);
        }

        handle_notification(method, message.get("params"), thread_id, updates).await;
        return Ok(None);
    }

    if let Some(response_id) = message.get("id").and_then(Value::as_i64)
        && expected_response_id == Some(response_id)
    {
        if let Some(error) = message.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("codex app-server request failed");
            bail!("{message}");
        }

        return Ok(Some(message.get("result").cloned().ok_or_else(|| {
            anyhow!("codex app-server response did not include a result")
        })?));
    }

    Ok(None)
}

async fn handle_notification(
    method: &str,
    params: Option<&Value>,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
) {
    match method {
        "thread/started" => {
            if let Some(provider_thread_id) = params
                .and_then(|params| params.get("thread"))
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
            {
                send_update(
                    updates,
                    HarnessTurnUpdate::ThreadReady {
                        thread_id: thread_id.clone(),
                        provider_thread_id: provider_thread_id.to_string(),
                    },
                )
                .await;
            }
        }
        "item/agentMessage/delta" => {
            if let Some(delta) = params
                .and_then(|params| params.get("delta"))
                .and_then(Value::as_str)
            {
                send_update(
                    updates,
                    HarnessTurnUpdate::AssistantDelta {
                        thread_id: thread_id.clone(),
                        delta: delta.to_string(),
                    },
                )
                .await;
            }
        }
        "turn/started" => {
            send_status(updates, thread_id.clone(), HarnessRunStatus::Running).await;
        }
        "turn/completed" => {
            send_update(
                updates,
                HarnessTurnUpdate::Finished {
                    thread_id: thread_id.clone(),
                },
            )
            .await;
        }
        "error" => {
            let message = params
                .and_then(|params| params.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("codex app-server reported an error");
            send_update(
                updates,
                HarnessTurnUpdate::Failed {
                    thread_id: thread_id.clone(),
                    message: message.to_string().into(),
                },
            )
            .await;
        }
        other if other.starts_with("item/") => {
            if let Some(event) = parse_tool_event(other, params) {
                send_update(
                    updates,
                    HarnessTurnUpdate::ToolEvent {
                        thread_id: thread_id.clone(),
                        item_id: event.item_id,
                        kind: event.kind,
                        phase: event.phase,
                        title: event.title,
                        detail: event.detail,
                    },
                )
                .await;
            }
        }
        _ => {}
    }
}

struct ParsedToolEvent {
    item_id: Option<String>,
    kind: HarnessToolKind,
    phase: HarnessToolPhase,
    title: SharedString,
    detail: SharedString,
}

fn parse_tool_event(method: &str, params: Option<&Value>) -> Option<ParsedToolEvent> {
    let stripped = method.strip_prefix("item/")?;
    let mut parts = stripped.splitn(2, '/');
    let first = parts.next()?;
    let second = parts.next().unwrap_or("");

    // Approval requests are handled separately by respond_to_server_request.
    if first == "requestApproval" || second == "requestApproval" {
        return None;
    }

    // Skip the `agentMessage` transport — it's handled upstream in the main
    // dispatcher as `item/agentMessage/delta` so we never reach here for it, but
    // the modern app-server also sends `item/added` / `item/updated` /
    // `item/completed` events with `params.item.type == "agentMessage"` that we
    // *should* drop so we don't double-render assistant text as a tool block.
    let item_payload = params.and_then(|params| params.get("item"));
    let item_type = item_payload.and_then(|item| item.get("type")).and_then(Value::as_str);
    if item_type == Some("agentMessage") || item_type == Some("agent_message") {
        return None;
    }

    // `first` can be either a phase (modern: `item/added`) or a legacy kind
    // (`item/commandExecution/start`). Fall back to item.type when the URL
    // doesn't name the kind directly.
    let (kind_str, phase_str): (&str, &str) = if let Some(item_type) = item_type {
        (item_type, first)
    } else if !second.is_empty() {
        (first, second)
    } else {
        // No kind source at all — best effort, use the URL segment as the kind
        // with an unspecified phase.
        (first, "")
    };

    let kind = match kind_str {
        "commandExecution" | "command_execution" | "command" => HarnessToolKind::Command,
        "fileRead" | "file_read" | "read" => HarnessToolKind::FileRead,
        "fileChange" | "file_change" | "edit" | "write" => HarnessToolKind::FileChange,
        "webSearch" | "web_search" => HarnessToolKind::WebSearch,
        "reasoning" => HarnessToolKind::Reasoning,
        other => HarnessToolKind::Other(other.to_string().into()),
    };

    let phase = match phase_str {
        "added" | "start" | "started" | "began" => HarnessToolPhase::Start,
        "completed" | "end" | "ended" | "complete" | "finished" => HarnessToolPhase::End,
        _ => HarnessToolPhase::Update,
    };

    let item_id = extract_item_id(params);
    let detail = extract_tool_detail(&kind, phase_str, params);
    let title = tool_title(&kind, phase, params);

    Some(ParsedToolEvent {
        item_id,
        kind,
        phase,
        title,
        detail,
    })
}

fn extract_item_id(params: Option<&Value>) -> Option<String> {
    let params = params?;
    params
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("itemId")
                .or_else(|| params.get("item_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| params.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

fn extract_tool_detail(
    kind: &HarnessToolKind,
    phase_str: &str,
    params: Option<&Value>,
) -> SharedString {
    let Some(params) = params else {
        return SharedString::default();
    };
    let item_payload = params.get("item");

    // Reasoning: pull text from `item.summary` (array of strings), or the older
    // `delta` / `text` shapes. Never dump raw JSON for reasoning — empty is
    // fine, it'll stream in later.
    if matches!(kind, HarnessToolKind::Reasoning) {
        if let Some(summary) = item_payload
            .and_then(|item| item.get("summary"))
            .and_then(Value::as_array)
        {
            let joined = summary
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            return joined.into();
        }
        if let Some(content) = item_payload
            .and_then(|item| item.get("content"))
            .and_then(Value::as_array)
        {
            let joined = content
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            return joined.into();
        }
        return params
            .get("delta")
            .and_then(Value::as_str)
            .or_else(|| params.get("text").and_then(Value::as_str))
            .map(String::from)
            .unwrap_or_default()
            .into();
    }

    let lookup = |key: &str| -> Option<String> {
        params
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get(key))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
    };

    // For commands, only put stdout/output in the detail — the command itself
    // lives in the title ("Run command: ...") and the renderer extracts it
    // from there, so duplicating it here causes the "$ cmd" line to appear
    // twice.
    if matches!(kind, HarnessToolKind::Command) {
        if let Some(output) = lookup("output")
            .or_else(|| lookup("stdout"))
            .or_else(|| lookup("text"))
            .or_else(|| lookup("delta"))
        {
            return output.into();
        }
        return SharedString::default();
    }

    // Generic file/search/other extraction.
    if let Some(path) = lookup("path")
        .or_else(|| lookup("filePath"))
        .or_else(|| lookup("file"))
    {
        return path.into();
    }
    if let Some(query) = lookup("query") {
        return query.into();
    }
    if let Some(text) = lookup("delta")
        .or_else(|| lookup("output"))
        .or_else(|| lookup("text"))
        .or_else(|| lookup("stdout"))
    {
        return text.into();
    }

    // Phase-end events with no useful payload → nothing to show.
    if phase_str == "end"
        || phase_str == "ended"
        || phase_str == "complete"
        || phase_str == "completed"
        || phase_str == "finished"
    {
        return SharedString::default();
    }

    // Everything else we don't understand yet: leave empty rather than dumping
    // raw JSON at the user.
    SharedString::default()
}

fn command_string(params: &Value, item_payload: Option<&Value>) -> Option<String> {
    // String form: { "command": "ls -la" }
    if let Some(cmd) = params
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| {
            item_payload
                .and_then(|item| item.get("command"))
                .and_then(Value::as_str)
        })
    {
        return Some(cmd.to_string());
    }
    // Array form: { "command": ["ls", "-la"] }
    if let Some(array) = params
        .get("command")
        .and_then(Value::as_array)
        .or_else(|| {
            item_payload
                .and_then(|item| item.get("command"))
                .and_then(Value::as_array)
        })
    {
        let joined = array
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    // Zed-style command execution: { "item": { "input": "..." } }
    if let Some(input) = params
        .get("input")
        .and_then(Value::as_str)
        .or_else(|| {
            item_payload
                .and_then(|item| item.get("input"))
                .and_then(Value::as_str)
        })
    {
        return Some(input.to_string());
    }
    None
}

fn tool_title(
    kind: &HarnessToolKind,
    _phase: HarnessToolPhase,
    params: Option<&Value>,
) -> SharedString {
    let base: SharedString = match kind {
        HarnessToolKind::Command => "Run command".into(),
        HarnessToolKind::FileRead => "Read file".into(),
        HarnessToolKind::FileChange => "Edit file".into(),
        HarnessToolKind::WebSearch => "Web search".into(),
        HarnessToolKind::Reasoning => "Reasoning".into(),
        HarnessToolKind::Other(name) => name.clone(),
    };

    let Some(params) = params else {
        return base;
    };
    let item_payload = params.get("item");

    // Commands: "Run command: ls -la"
    if matches!(kind, HarnessToolKind::Command) {
        if let Some(cmd) = command_string(params, item_payload) {
            let one_line = cmd.lines().next().unwrap_or("").trim();
            if !one_line.is_empty() {
                return format!("{base}: {one_line}").into();
            }
        }
        return base;
    }

    // File ops / web search: "Read file: path/to/file" or "Web search: query"
    if matches!(
        kind,
        HarnessToolKind::FileRead | HarnessToolKind::FileChange | HarnessToolKind::WebSearch
    ) {
        let detail = params
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| params.get("filePath").and_then(Value::as_str))
            .or_else(|| params.get("file").and_then(Value::as_str))
            .or_else(|| params.get("query").and_then(Value::as_str))
            .or_else(|| item_payload.and_then(|item| item.get("path")).and_then(Value::as_str))
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get("filePath"))
                    .and_then(Value::as_str)
            })
            .or_else(|| item_payload.and_then(|item| item.get("file")).and_then(Value::as_str))
            .or_else(|| item_payload.and_then(|item| item.get("query")).and_then(Value::as_str));
        if let Some(detail) = detail {
            return format!("{base}: {detail}").into();
        }
    }

    base
}

async fn respond_to_server_request<Writer>(
    writer: &mut BufWriter<Writer>,
    request_id: Value,
    method: &str,
) -> Result<()>
where
    Writer: AsyncWrite + Unpin,
{
    let result = match method {
        "item/commandExecution/requestApproval"
        | "item/fileRead/requestApproval"
        | "item/fileChange/requestApproval" => json!({
            "decision": "acceptForSession",
        }),
        _ => {
            write_message(
                writer,
                json!({
                    "id": request_id,
                    "error": {
                        "code": -32601,
                        "message": format!("Unsupported server request: {method}"),
                    },
                }),
            )
            .await?;
            return Ok(());
        }
    };

    write_message(
        writer,
        json!({
            "id": request_id,
            "result": result,
        }),
    )
    .await
}

async fn read_message<Reader>(reader: &mut BufReader<Reader>) -> Result<Option<Value>>
where
    Reader: AsyncRead + Unpin,
{
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line).await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    Ok(Some(serde_json::from_str(&line)?))
}

async fn write_message<Writer>(writer: &mut BufWriter<Writer>, message: Value) -> Result<()>
where
    Writer: AsyncWrite + Unpin,
{
    writer.write_all(message.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn forward_stderr<Stderr>(
    thread_id: HarnessThreadId,
    stderr: Stderr,
    updates: Sender<HarnessTurnUpdate>,
) where
    Stderr: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    while let Ok(bytes_read) = reader.read_line(&mut line).await {
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if !trimmed.is_empty() {
            log::debug!("codex app-server stderr: {trimmed}");
        }
        line.clear();
    }

    send_status(&updates, thread_id, HarnessRunStatus::Idle).await;
}

async fn send_status(
    updates: &Sender<HarnessTurnUpdate>,
    thread_id: HarnessThreadId,
    status: HarnessRunStatus,
) {
    send_update(updates, HarnessTurnUpdate::Status { thread_id, status }).await;
}

async fn send_update(updates: &Sender<HarnessTurnUpdate>, update: HarnessTurnUpdate) {
    updates.send(update).await.ok();
}

fn read_thread_id(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .or_else(|| result.get("threadId").and_then(Value::as_str))
        .map(str::to_string)
}

fn approval_policy_name(policy: &HarnessApprovalPolicy) -> &'static str {
    match policy {
        HarnessApprovalPolicy::Never => "never",
        HarnessApprovalPolicy::OnRequest => "on-request",
    }
}

fn sandbox_policy_name(policy: &HarnessSandboxPolicy) -> &'static str {
    match policy {
        HarnessSandboxPolicy::DangerFullAccess => "danger-full-access",
        HarnessSandboxPolicy::WorkspaceWrite => "workspace-write",
    }
}

fn sandbox_policy_value(policy: &HarnessSandboxPolicy) -> Value {
    match policy {
        HarnessSandboxPolicy::DangerFullAccess => json!({
            "type": "dangerFullAccess",
        }),
        HarnessSandboxPolicy::WorkspaceWrite => json!({
            "type": "workspaceWrite",
        }),
    }
}
