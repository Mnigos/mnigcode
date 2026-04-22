use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _,
    io::{BufReader, BufWriter},
};
use gpui::{BackgroundExecutor, SharedString};
use serde_json::{Value, json};
use smol::{
    channel::{Receiver, Sender},
    process::Command,
};
use std::{
    collections::HashMap,
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const APP_SERVER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

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

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HarnessSandboxPolicy {
    DangerFullAccess,
    WorkspaceWrite,
}

impl HarnessSandboxPolicy {
    pub fn serialization_key(&self) -> &'static str {
        match self {
            HarnessSandboxPolicy::DangerFullAccess => "dangerFullAccess",
            HarnessSandboxPolicy::WorkspaceWrite => "workspaceWrite",
        }
    }

    pub fn from_serialization_key(value: &str) -> Option<Self> {
        match value {
            "dangerFullAccess" => Some(HarnessSandboxPolicy::DangerFullAccess),
            "workspaceWrite" => Some(HarnessSandboxPolicy::WorkspaceWrite),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct HarnessSessionConfig {
    pub thread_id: HarnessThreadId,
    pub provider_thread_id: Option<String>,
    pub cwd: PathBuf,
    pub executor: BackgroundExecutor,
    pub model: String,
    pub approval_policy: HarnessApprovalPolicy,
    pub sandbox_policy: HarnessSandboxPolicy,
}

#[derive(Clone, Debug)]
pub struct HarnessTurnInput {
    pub input: String,
    pub skill_mentions: Vec<HarnessSkillMention>,
    pub model: String,
    pub reasoning_effort: String,
    pub approval_policy: HarnessApprovalPolicy,
    pub sandbox_policy: HarnessSandboxPolicy,
}

#[derive(Clone, Debug)]
pub struct HarnessSkillMention {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessSkillDefinition {
    pub name: SharedString,
    pub description: SharedString,
    pub path: Option<PathBuf>,
    pub source: HarnessSkillSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HarnessFileChange {
    pub(crate) path: SharedString,
    pub(crate) added_lines: usize,
    pub(crate) removed_lines: usize,
    pub(crate) unified_diff: Option<String>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum HarnessSkillSource {
    Local,
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
        file_changes: Vec<HarnessFileChange>,
    },
    TokensUsed {
        thread_id: HarnessThreadId,
        total_tokens: usize,
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
    McpToolCall,
    Other(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessToolPhase {
    Start,
    Update,
    End,
    Failed,
}

pub async fn run_codex_app_server_session(
    config: HarnessSessionConfig,
    turns: Receiver<HarnessTurnInput>,
    updates: Sender<HarnessTurnUpdate>,
) {
    if let Err(error) = run_codex_app_server_session_impl(config.clone(), turns, &updates).await {
        send_update(
            &updates,
            HarnessTurnUpdate::Failed {
                thread_id: config.thread_id,
                message: error.to_string().into(),
            },
        )
        .await;
    }
}

pub async fn load_codex_available_skills(
    cwd: PathBuf,
    executor: BackgroundExecutor,
) -> Result<Vec<HarnessSkillDefinition>> {
    let mut command = Command::new("codex");
    command
        .arg("app-server")
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start `codex app-server` in {}", cwd.display()))?;

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
    smol::spawn(log_app_server_stderr(stderr)).detach();

    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);
    let mut next_request_id: i64 = 1;

    let initialize_id = next_app_server_request_id(&mut next_request_id);
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
                "capabilities": {
                    "experimentalApi": true,
                },
            },
        }),
    )
    .await?;
    await_app_server_response(
        &mut child,
        &executor,
        "initialize response",
        read_until_app_server_response(&mut reader, &mut writer, initialize_id),
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

    let skills_id = next_app_server_request_id(&mut next_request_id);
    write_message(
        &mut writer,
        json!({
            "method": "skills/list",
            "id": skills_id,
            "params": {
                "cwds": [cwd.to_string_lossy()],
                "forceReload": false,
            },
        }),
    )
    .await?;
    let skills_result = await_app_server_response(
        &mut child,
        &executor,
        "skills/list response",
        read_until_app_server_response(&mut reader, &mut writer, skills_id),
    )
    .await?;

    let mut skills = Vec::new();
    append_codex_skills(&mut skills, &skills_result);

    child.kill().ok();
    deduplicate_skills(&mut skills);
    Ok(skills)
}

fn next_app_server_request_id(next_request_id: &mut i64) -> i64 {
    let request_id = *next_request_id;
    *next_request_id += 1;
    request_id
}

async fn log_app_server_stderr<Reader>(stderr: Reader)
where
    Reader: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let line = line.trim_end();
                if !line.is_empty() {
                    log::warn!("codex app-server stderr: {line}");
                }
            }
            Err(error) => {
                log::warn!("failed to read codex app-server stderr: {error}");
                break;
            }
        }
    }
}

async fn await_app_server_response<Fut, T>(
    child: &mut smol::process::Child,
    executor: &BackgroundExecutor,
    description: &'static str,
    future: Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    match smol::future::or(future, async {
        executor.timer(APP_SERVER_RESPONSE_TIMEOUT).await;
        Err(anyhow!(
            "timed out waiting for codex app-server {description} after {:?}",
            APP_SERVER_RESPONSE_TIMEOUT
        ))
    })
    .await
    {
        Ok(value) => Ok(value),
        Err(error) => {
            child.kill().ok();
            Err(error)
        }
    }
}

async fn run_codex_app_server_session_impl(
    config: HarnessSessionConfig,
    turns: Receiver<HarnessTurnInput>,
    updates: &Sender<HarnessTurnUpdate>,
) -> Result<()> {
    send_status(
        updates,
        config.thread_id.clone(),
        HarnessRunStatus::Connecting,
    )
    .await;

    let mut command = Command::new("codex");
    command
        .arg("app-server")
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start `codex app-server` in {}",
            config.cwd.display()
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
        config.thread_id.clone(),
        stderr,
        updates.clone(),
    ))
    .detach();

    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);
    let mut next_request_id: i64 = 1;

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
    await_app_server_response(
        &mut child,
        &config.executor,
        "initialize response",
        read_until_response(
            &mut reader,
            &mut writer,
            initialize_id,
            &config.thread_id,
            updates,
        ),
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
    let thread_open_message = if let Some(provider_thread_id) = config.provider_thread_id.clone() {
        json!({
            "method": "thread/resume",
            "id": thread_open_id,
            "params": {
                "threadId": provider_thread_id,
                "cwd": config.cwd.to_string_lossy(),
                "model": config.model,
                "approvalPolicy": approval_policy_name(&config.approval_policy),
                "sandboxPolicy": sandbox_policy_value(&config.sandbox_policy),
                "serviceName": "mnig_code",
            },
        })
    } else {
        json!({
            "method": "thread/start",
            "id": thread_open_id,
            "params": {
                "cwd": config.cwd.to_string_lossy(),
                "model": config.model,
                "approvalPolicy": approval_policy_name(&config.approval_policy),
                "sandboxPolicy": sandbox_policy_value(&config.sandbox_policy),
                "serviceName": "mnig_code",
            },
        })
    };
    write_message(&mut writer, thread_open_message).await?;
    let thread_open_result = await_app_server_response(
        &mut child,
        &config.executor,
        "thread open response",
        read_until_response(
            &mut reader,
            &mut writer,
            thread_open_id,
            &config.thread_id,
            updates,
        ),
    )
    .await?;
    let provider_thread_id = read_thread_id(&thread_open_result)
        .or(config.provider_thread_id.clone())
        .context("codex app-server did not return a thread id")?;
    let raw_session_path = read_thread_path(&thread_open_result);

    send_update(
        updates,
        HarnessTurnUpdate::ThreadReady {
            thread_id: config.thread_id.clone(),
            provider_thread_id: provider_thread_id.clone(),
        },
    )
    .await;

    while let Ok(turn) = turns.recv().await {
        send_status(
            updates,
            config.thread_id.clone(),
            HarnessRunStatus::Thinking,
        )
        .await;

        let turn_start_id = next_request_id;
        next_request_id += 1;
        let mut input = vec![json!({
            "type": "text",
            "text": turn.input,
            "text_elements": [],
        })];
        input.extend(turn.skill_mentions.into_iter().map(|skill| {
            json!({
                "type": "skill",
                "name": skill.name,
                "path": skill.path.to_string_lossy(),
            })
        }));

        write_message(
            &mut writer,
            json!({
                "method": "turn/start",
                "id": turn_start_id,
                "params": {
                    "threadId": provider_thread_id,
                    "input": input,
                    "cwd": config.cwd.to_string_lossy(),
                    "model": turn.model,
                    "effort": turn.reasoning_effort,
                    "approvalPolicy": approval_policy_name(&turn.approval_policy),
                    "sandboxPolicy": sandbox_policy_value(&turn.sandbox_policy),
                },
            }),
        )
        .await?;
        await_app_server_response(
            &mut child,
            &config.executor,
            "turn/start response",
            read_until_response(
                &mut reader,
                &mut writer,
                turn_start_id,
                &config.thread_id,
                updates,
            ),
        )
        .await?;
        send_status(updates, config.thread_id.clone(), HarnessRunStatus::Running).await;

        let raw_session_stream = raw_session_path.as_ref().map(|path| {
            let start_offset = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let (stop_tx, stop_rx) = smol::channel::bounded(1);
            let task = smol::spawn(stream_exec_command_outputs_from_session(
                path.clone(),
                start_offset,
                config.thread_id.clone(),
                config.executor.clone(),
                updates.clone(),
                stop_rx,
            ));
            (stop_tx, task)
        });

        let completed_thread_id =
            read_until_turn_finished(&mut reader, &mut writer, &config.thread_id, updates).await?;

        if let Some((stop, task)) = raw_session_stream {
            stop.send(()).await.ok();
            task.await;
        }

        send_update(
            updates,
            HarnessTurnUpdate::Finished {
                thread_id: completed_thread_id,
            },
        )
        .await;
    }

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

async fn read_until_app_server_response<Reader, Writer>(
    reader: &mut BufReader<Reader>,
    writer: &mut BufWriter<Writer>,
    expected_id: i64,
) -> Result<Value>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(message) = read_message(reader).await? else {
            bail!("codex app-server exited before responding");
        };

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(request_id) = message.get("id").cloned() {
                respond_to_server_request(writer, request_id, method).await?;
            }
            continue;
        }

        if let Some(response_id) = message.get("id").and_then(Value::as_i64)
            && response_id == expected_id
        {
            if let Some(error) = message.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex app-server request failed");
                bail!("{message}");
            }

            return message
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("codex app-server response did not include a result"));
        }
    }
}

async fn read_until_turn_finished<Reader, Writer>(
    reader: &mut BufReader<Reader>,
    writer: &mut BufWriter<Writer>,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
) -> Result<HarnessThreadId>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(message) = read_message(reader).await? else {
            bail!("codex app-server exited before the turn completed");
        };

        let is_turn_completed = matches!(
            message.get("method").and_then(Value::as_str),
            Some("turn/completed")
        );

        handle_protocol_message(message, writer, thread_id, updates, None).await?;

        if is_turn_completed {
            return Ok(thread_id.clone());
        }
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
        // Dispatch notifications first so method-driven updates (e.g.
        // turn/completed → Finished) are emitted even when the server sends
        // the method as a request that also requires a response.
        handle_notification(method, message.get("params"), thread_id, updates).await;

        if let Some(request_id) = message.get("id").cloned() {
            respond_to_server_request(writer, request_id, method).await?;
        }
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
            if let Some(total_tokens) = params.and_then(extract_total_tokens) {
                send_update(
                    updates,
                    HarnessTurnUpdate::TokensUsed {
                        thread_id: thread_id.clone(),
                        total_tokens,
                    },
                )
                .await;
            }
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
                        file_changes: event.file_changes,
                    },
                )
                .await;
            }
        }
        // Codex sometimes streams reasoning via top-level methods (e.g.
        // `reasoning/delta`, `thread/reasoning/delta`) that aren't in the
        // `item/...` namespace. Funnel those into a Reasoning ToolEvent so
        // the transcript can render them.
        other
            if other.contains("reasoning")
                || other.contains("thinking")
                || other.contains("thought") =>
        {
            let phase = if other.ends_with("/delta") || other.ends_with("/update") {
                HarnessToolPhase::Update
            } else if other.ends_with("/end")
                || other.ends_with("/completed")
                || other.ends_with("/done")
            {
                HarnessToolPhase::End
            } else {
                HarnessToolPhase::Start
            };
            let detail = params
                .and_then(|p| reasoning_text(p, p.get("item")))
                .unwrap_or_default()
                .into();
            send_update(
                updates,
                HarnessTurnUpdate::ToolEvent {
                    thread_id: thread_id.clone(),
                    item_id: params
                        .and_then(|p| p.get("id").or_else(|| p.get("itemId")))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    kind: HarnessToolKind::Reasoning,
                    phase,
                    title: "Reasoning".into(),
                    detail,
                    file_changes: Vec::new(),
                },
            )
            .await;
        }
        other => {
            log::debug!("unhandled codex notification: {other}");
        }
    }
}

struct ParsedToolEvent {
    item_id: Option<String>,
    kind: HarnessToolKind,
    phase: HarnessToolPhase,
    title: SharedString,
    detail: SharedString,
    file_changes: Vec<HarnessFileChange>,
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
    let item_type = item_payload
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str);
    if matches!(
        item_type,
        Some("agentMessage" | "agent_message" | "userMessage" | "user_message")
    ) {
        return None;
    }

    // `first` can be either a phase (modern: `item/added`) or a legacy kind
    // (`item/commandExecution/start`). Fall back to item.type when the URL
    // doesn't name the kind directly.
    let (kind_str, phase_str): (&str, &str) = if !second.is_empty() {
        (item_type.unwrap_or(first), second)
    } else if let Some(item_type) = item_type {
        (item_type, first)
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
        "mcpToolCall" | "mcp_tool_call" => HarnessToolKind::McpToolCall,
        other => HarnessToolKind::Other(other.to_string().into()),
    };

    let phase = resolve_tool_phase(phase_str, params);

    let item_id = extract_item_id(params);
    let detail = extract_tool_detail(&kind, phase_str, params);
    let title = tool_title(&kind, phase, params);
    let file_changes = extract_file_changes(&kind, params);

    Some(ParsedToolEvent {
        item_id,
        kind,
        phase,
        title,
        detail,
        file_changes,
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

fn resolve_tool_phase(phase_str: &str, params: Option<&Value>) -> HarnessToolPhase {
    match phase_str {
        "added" | "start" | "started" | "began" => return HarnessToolPhase::Start,
        "completed" | "end" | "ended" | "complete" | "finished" => {
            return HarnessToolPhase::End;
        }
        _ => {}
    }

    match extract_item_status(params) {
        Some("completed" | "complete" | "finished" | "done" | "succeeded" | "success") => {
            HarnessToolPhase::End
        }
        Some("failed" | "error" | "errored" | "cancelled" | "canceled" | "interrupted") => {
            HarnessToolPhase::Failed
        }
        Some("added" | "pending" | "queued" | "started" | "starting") => HarnessToolPhase::Start,
        _ => HarnessToolPhase::Update,
    }
}

fn extract_item_status(params: Option<&Value>) -> Option<&str> {
    let params = params?;

    params
        .get("item")
        .and_then(|item| item.get("status"))
        .and_then(Value::as_str)
        .or_else(|| params.get("status").and_then(Value::as_str))
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

    // Reasoning: the codex app-server has shipped several shapes over time,
    // so try every text-bearing field we know about. Never dump raw JSON for
    // reasoning — empty is fine, it'll stream in via a later event.
    if matches!(kind, HarnessToolKind::Reasoning) {
        if let Some(text) = reasoning_text(params, item_payload) {
            return text.into();
        }
        // Codex routinely emits reasoning items with no summary or content
        // (several gpt-5 variants never produce a reasoning trace for a given
        // effort level). The renderer surfaces these as a non-expandable
        // "Thought for Xs" row, so log at debug level rather than warn.
        if !matches!(phase_str, "added" | "start" | "started") {
            log::debug!(
                "reasoning event yielded no text: {}",
                serde_json::to_string(params).unwrap_or_default()
            );
        }
        return SharedString::default();
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
    // from there, so duplicating it into the body would produce a "$ cmd"
    // line twice. Output arrives either as a running stream of `delta`s or
    // (more commonly) as a single `aggregated_output`/`output` snapshot on
    // the completion event.
    if matches!(kind, HarnessToolKind::Command) {
        let command_text = command_string(params, item_payload);

        if let Some(snapshot) = command_output_snapshot(params, item_payload) {
            // Some servers echo the command itself back in `output`. Strip it
            // so the transcript doesn't render the command line twice.
            if let Some(command_text) = command_text.as_deref()
                && snapshot.trim() == command_text.trim()
            {
                return SharedString::default();
            }
            return snapshot.into();
        }
        if let Some(delta) = command_output_delta(params, item_payload).or_else(|| lookup("delta")) {
            return delta.into();
        }
        // Nothing useful on a pure "completed" event with no output field.
        let _ = phase_str;
        return SharedString::default();
    }

    if matches!(kind, HarnessToolKind::McpToolCall) {
        return extract_mcp_tool_arguments(params, item_payload)
            .and_then(|arguments| format_mcp_tool_arguments(arguments))
            .unwrap_or_default()
            .into();
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

fn extract_file_changes(
    kind: &HarnessToolKind,
    params: Option<&Value>,
) -> Vec<HarnessFileChange> {
    if !matches!(kind, HarnessToolKind::FileChange) {
        return Vec::new();
    }
    let Some(params) = params else {
        return Vec::new();
    };
    let item_payload = params.get("item");

    let changes = ["changes", "fileChanges", "file_changes"]
        .iter()
        .find_map(|key| {
            params
                .get(*key)
                .or_else(|| item_payload.and_then(|item| item.get(*key)))
        });

    let mut file_changes = match changes {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(path, value)| file_change_from_value(Some(path.as_str()), value))
            .collect::<Vec<_>>(),
        Some(Value::Array(array)) => array
            .iter()
            .filter_map(|value| file_change_from_value(None, value))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    if file_changes.is_empty()
        && let Some(change) = file_change_from_value(None, params)
            .or_else(|| item_payload.and_then(|item| file_change_from_value(None, item)))
    {
        file_changes.push(change);
    }

    file_changes
}

fn file_change_from_value(path_hint: Option<&str>, value: &Value) -> Option<HarnessFileChange> {
    let object = value.as_object();
    let lookup_string = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| object.and_then(|object| object.get(*key)).and_then(Value::as_str))
            .map(str::to_string)
    };
    let lookup_usize = |keys: &[&str]| -> Option<usize> {
        keys.iter()
            .find_map(|key| object.and_then(|object| object.get(*key)).and_then(Value::as_u64))
            .map(|value| value as usize)
    };

    let path = path_hint
        .map(str::to_string)
        .or_else(|| {
            lookup_string(&["path", "filePath", "file", "relativePath", "absolutePath"])
        })?;
    let unified_diff = lookup_string(&["unified_diff", "unifiedDiff", "diff", "patch"])
        .or_else(|| {
            let old_text = lookup_string(&["old_text", "oldText", "before"]);
            let new_text = lookup_string(&["new_text", "newText", "after"]);
            match (old_text, new_text) {
                (Some(old_text), Some(new_text)) if old_text != new_text => {
                    Some(fallback_unified_diff(&old_text, &new_text))
                }
                _ => None,
            }
        });

    let (diff_added, diff_removed) = unified_diff
        .as_deref()
        .map(count_unified_diff_lines)
        .unwrap_or_default();
    let added_lines = lookup_usize(&[
        "added_lines",
        "addedLines",
        "additions",
        "linesAdded",
        "insertions",
    ])
    .unwrap_or(diff_added);
    let removed_lines = lookup_usize(&[
        "removed_lines",
        "removedLines",
        "deletions",
        "linesRemoved",
        "deletedLines",
    ])
    .unwrap_or(diff_removed);

    Some(HarnessFileChange {
        path: path.into(),
        added_lines,
        removed_lines,
        unified_diff,
    })
}

fn count_unified_diff_lines(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn fallback_unified_diff(old_text: &str, new_text: &str) -> String {
    let old_line_count = old_text.lines().count().max(1);
    let new_line_count = new_text.lines().count().max(1);
    let mut diff = format!("@@ -1,{old_line_count} +1,{new_line_count} @@\n");
    for line in old_text.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new_text.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

fn command_string(params: &Value, item_payload: Option<&Value>) -> Option<String> {
    // String form: { "command": "ls -la" }
    if let Some(cmd) = params.get("command").and_then(Value::as_str).or_else(|| {
        item_payload
            .and_then(|item| item.get("command"))
            .and_then(Value::as_str)
    }) {
        return Some(cmd.to_string());
    }
    // Array form: { "command": ["ls", "-la"] }
    if let Some(array) = params.get("command").and_then(Value::as_array).or_else(|| {
        item_payload
            .and_then(|item| item.get("command"))
            .and_then(Value::as_array)
    }) {
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
    if let Some(input) = params.get("input").and_then(Value::as_str).or_else(|| {
        item_payload
            .and_then(|item| item.get("input"))
            .and_then(Value::as_str)
    }) {
        return Some(input.to_string());
    }
    None
}

fn command_output_snapshot(params: &Value, item_payload: Option<&Value>) -> Option<String> {
    for key in ["aggregated_output", "aggregatedOutput", "output"] {
        if let Some(value) = params.get(key).or_else(|| item_payload.and_then(|item| item.get(key)))
            && let Some(rendered) = render_command_output_value(value)
        {
            return Some(rendered);
        }
    }

    if let Some(value) = params
        .get("result")
        .or_else(|| item_payload.and_then(|item| item.get("result")))
        .and_then(render_command_output_value)
    {
        return Some(value);
    }

    let stdout = params
        .get("stdout")
        .or_else(|| item_payload.and_then(|item| item.get("stdout")))
        .and_then(render_command_output_value);
    let stderr = params
        .get("stderr")
        .or_else(|| item_payload.and_then(|item| item.get("stderr")))
        .and_then(render_command_output_value);

    match (stdout, stderr) {
        (Some(stdout), Some(stderr)) if stdout != stderr => Some(format!("{stdout}\n{stderr}")),
        (Some(stdout), _) => Some(stdout),
        (_, Some(stderr)) => Some(stderr),
        _ => None,
    }
}

fn command_output_delta(params: &Value, item_payload: Option<&Value>) -> Option<String> {
    for key in ["delta", "text"] {
        if let Some(value) = params.get(key).or_else(|| item_payload.and_then(|item| item.get(key)))
            && let Some(rendered) = render_command_output_value(value)
        {
            return Some(rendered);
        }
    }

    let delta_base64 = params
        .get("deltaBase64")
        .or_else(|| params.get("delta_base64"))
        .or_else(|| item_payload.and_then(|item| item.get("deltaBase64")))
        .or_else(|| item_payload.and_then(|item| item.get("delta_base64")))
        .and_then(Value::as_str)?;

    base64::engine::general_purpose::STANDARD
        .decode(delta_base64)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .filter(|text| !text.is_empty())
}

fn render_command_output_value(value: &Value) -> Option<String> {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Null => {}
            Value::String(text) if !text.is_empty() => out.push(text.clone()),
            Value::Array(values) => {
                for value in values {
                    collect(value, out);
                }
            }
            Value::Object(map) => {
                if let Some(delta_base64) = map
                    .get("deltaBase64")
                    .or_else(|| map.get("delta_base64"))
                    .and_then(Value::as_str)
                    && let Ok(bytes) =
                        base64::engine::general_purpose::STANDARD.decode(delta_base64)
                {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    if !text.is_empty() {
                        out.push(text);
                    }
                }

                for key in [
                    "result",
                    "aggregated_output",
                    "aggregatedOutput",
                    "output",
                    "stdout",
                    "stderr",
                    "text",
                    "delta",
                    "content",
                    "message",
                ] {
                    if let Some(value) = map.get(key) {
                        collect(value, out);
                    }
                }
            }
            _ => {}
        }
    }

    let mut output = Vec::new();
    collect(value, &mut output);

    let mut deduped = Vec::new();
    for piece in output {
        if deduped.last().map(String::as_str) != Some(piece.as_str()) {
            deduped.push(piece);
        }
    }

    if deduped.is_empty() {
        None
    } else {
        Some(deduped.join("\n"))
    }
}

fn extract_total_tokens(params: &Value) -> Option<usize> {
    let usage = params
        .get("usage")
        .or_else(|| params.get("tokenUsage"))
        .or_else(|| params.get("token_usage"))?;

    if let Some(total) = usage
        .get("totalTokens")
        .or_else(|| usage.get("total_tokens"))
        .or_else(|| usage.get("total"))
        .and_then(Value::as_u64)
    {
        return Some(total as usize);
    }

    // `inputTokens` already includes `cachedInputTokens` and `outputTokens`
    // already includes `reasoningTokens` in OpenAI's usage schema, so only
    // sum the two top-level buckets here when a pre-computed total is absent.
    let input = usage
        .get("inputTokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("outputTokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let sum = input + output;
    if sum > 0 { Some(sum as usize) } else { None }
}

enum RawSessionCommandEvent {
    Started { call_id: String, tool_name: String },
    Completed {
        call_id: String,
        tool_name: Option<String>,
        output: String,
    },
}

async fn stream_exec_command_outputs_from_session(
    session_path: PathBuf,
    mut offset: u64,
    thread_id: HarnessThreadId,
    executor: BackgroundExecutor,
    updates: Sender<HarnessTurnUpdate>,
    stop: Receiver<()>,
) {
    let mut pending_tool_names = HashMap::new();
    let mut partial = Vec::new();

    loop {
        let stop_requested = stop.try_recv().is_ok();
        drain_new_session_lines(
            &session_path,
            &mut offset,
            &thread_id,
            &updates,
            &mut pending_tool_names,
            &mut partial,
        )
        .await;
        if stop_requested {
            finalize_partial_command_record(
                &thread_id,
                &updates,
                &mut pending_tool_names,
                &mut partial,
            )
            .await;
            break;
        }

        executor.timer(Duration::from_millis(150)).await;
        if stop.try_recv().is_ok() {
            drain_new_session_lines(
                &session_path,
                &mut offset,
                &thread_id,
                &updates,
                &mut pending_tool_names,
                &mut partial,
            )
            .await;
            finalize_partial_command_record(
                &thread_id,
                &updates,
                &mut pending_tool_names,
                &mut partial,
            )
            .await;
            break;
        }
    }
}

async fn drain_new_session_lines(
    session_path: &Path,
    offset: &mut u64,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
    pending_tool_names: &mut HashMap<String, String>,
    partial: &mut Vec<u8>,
) {
    let session_path = session_path.to_path_buf();
    let read_path = session_path.clone();
    match smol::unblock({
        let offset = *offset;
        move || read_new_session_lines(&read_path, offset)
    })
    .await
    {
        Ok(Some((new_offset, bytes))) => {
            *offset = new_offset;
            partial.extend_from_slice(&bytes);

            while let Some(newline_index) = partial.iter().position(|byte| *byte == b'\n') {
                let line = partial.drain(..=newline_index).collect::<Vec<_>>();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }

                if let Some(event) = parse_raw_session_command_event(line) {
                    handle_parsed_command_event(
                        event,
                        thread_id,
                        updates,
                        pending_tool_names,
                    )
                    .await;
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            log::debug!(
                "failed to read raw codex session output from {}: {error}",
                session_path.display()
            );
        }
    }
}

async fn finalize_partial_command_record(
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
    pending_tool_names: &mut HashMap<String, String>,
    partial: &mut Vec<u8>,
) {
    if partial.is_empty() {
        return;
    }

    if let Some(event) = parse_raw_session_command_event(partial.as_slice()) {
        handle_parsed_command_event(event, thread_id, updates, pending_tool_names).await;
    }
    partial.clear();
}

async fn handle_parsed_command_event(
    event: RawSessionCommandEvent,
    thread_id: &HarnessThreadId,
    updates: &Sender<HarnessTurnUpdate>,
    pending_tool_names: &mut HashMap<String, String>,
) {
    match event {
        RawSessionCommandEvent::Started { call_id, tool_name } => {
            pending_tool_names.insert(call_id, tool_name);
        }
        RawSessionCommandEvent::Completed {
            call_id,
            tool_name,
            output,
        } => {
            let pending_tool_name = pending_tool_names.remove(&call_id);
            if pending_tool_name.as_deref() == Some("exec_command")
                || tool_name.as_deref() == Some("exec_command")
            {
                send_update(
                    updates,
                    HarnessTurnUpdate::ToolEvent {
                        thread_id: thread_id.clone(),
                        item_id: Some(call_id),
                        kind: HarnessToolKind::Command,
                        phase: HarnessToolPhase::End,
                        title: SharedString::default(),
                        detail: output.into(),
                        file_changes: Vec::new(),
                    },
                )
                .await;
            }
        }
    }
}

fn read_new_session_lines(path: &Path, offset: u64) -> Result<Option<(u64, Vec<u8>)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let file_len = file.metadata()?.len();
    if file_len <= offset {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let new_offset = file.stream_position()?;
    Ok(Some((new_offset, bytes)))
}

fn parse_raw_session_command_event(line: &[u8]) -> Option<RawSessionCommandEvent> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let kind = value.get("type").and_then(Value::as_str)?;
    let payload = value.get("payload")?;

    match kind {
        "response_item"
            if payload.get("type").and_then(Value::as_str) == Some("function_call") =>
        {
            Some(RawSessionCommandEvent::Started {
                call_id: payload.get("call_id").and_then(Value::as_str)?.to_string(),
                tool_name: payload.get("name").and_then(Value::as_str)?.to_string(),
            })
        }
        "response_item"
            if payload.get("type").and_then(Value::as_str) == Some("function_call_output") =>
        {
            Some(RawSessionCommandEvent::Completed {
                call_id: payload.get("call_id").and_then(Value::as_str)?.to_string(),
                tool_name: payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                output: extract_exec_command_output(
                    payload.get("output").and_then(Value::as_str)?,
                )?,
            })
        }
        "event_msg" if payload.get("type").and_then(Value::as_str) == Some("exec_command_end") => {
            let output = payload
                .get("formatted_output")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    payload
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                })
                .or_else(|| {
                    let stdout = payload
                        .get("stdout")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty());
                    let stderr = payload
                        .get("stderr")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty());
                    match (stdout, stderr) {
                        (Some(stdout), Some(stderr)) if stdout != stderr => {
                            Some(format!("{stdout}\n{stderr}"))
                        }
                        (Some(stdout), _) => Some(stdout.to_string()),
                        (_, Some(stderr)) => Some(stderr.to_string()),
                        _ => None,
                    }
                })?;

            Some(RawSessionCommandEvent::Completed {
                call_id: payload.get("call_id").and_then(Value::as_str)?.to_string(),
                tool_name: Some("exec_command".to_string()),
                output,
            })
        }
        _ => None,
    }
}

fn extract_exec_command_output(output: &str) -> Option<String> {
    let output = if let Some(index) = output.find("\nOutput:\n") {
        &output[index + "\nOutput:\n".len()..]
    } else {
        output
    };
    let output = output.trim_matches('\n');
    if output.is_empty() {
        None
    } else {
        Some(output.to_string())
    }
}

fn reasoning_text(params: &Value, item_payload: Option<&Value>) -> Option<String> {
    // The codex app-server has shipped reasoning payloads in many shapes
    // (summary arrays, content blocks with {type,text}, `parts`, raw deltas,
    // etc.). Rather than chase every variant, walk the payload recursively
    // and harvest every text-bearing string field we find. Keys named `id`,
    // `type`, `role`, `status`, etc. are skipped so we don't pollute the
    // reasoning pane with metadata.
    fn ignored_key(key: &str) -> bool {
        matches!(
            key,
            "id" | "type"
                | "role"
                | "status"
                | "itemId"
                | "item_id"
                | "threadId"
                | "thread_id"
                | "model"
                | "usage"
                | "encrypted_content"
                | "encryptedContent"
        )
    }

    fn text_keys() -> &'static [&'static str] {
        &[
            "summary",
            "content",
            "parts",
            "text",
            "value",
            "delta",
            "reasoning",
            "body",
            "message",
            "thought",
            "thoughts",
            "chunk",
        ]
    }

    fn collect_from(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) if !s.is_empty() => out.push(s.clone()),
            Value::Array(array) => {
                for element in array {
                    collect_from(element, out);
                }
            }
            Value::Object(map) => {
                for key in text_keys() {
                    if let Some(inner) = map.get(*key) {
                        collect_from(inner, out);
                    }
                }
                for (k, v) in map {
                    if ignored_key(k) || text_keys().contains(&k.as_str()) {
                        continue;
                    }
                    if matches!(v, Value::Array(_) | Value::Object(_)) {
                        collect_from(v, out);
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    if let Some(item) = item_payload {
        collect_from(item, &mut out);
    }
    // Also consult top-level params keys that carry deltas or text directly
    // (event shapes like `{ method: "item/reasoning/delta", params: { delta: "..." } }`).
    if let Value::Object(map) = params {
        for key in ["delta", "text", "reasoning", "thought"] {
            if let Some(value) = map.get(key) {
                collect_from(value, &mut out);
            }
        }
    }

    // Deduplicate consecutive identical chunks (some servers re-emit a running
    // summary with each update) while preserving order.
    let mut deduped: Vec<String> = Vec::with_capacity(out.len());
    for piece in out {
        if deduped.last().map(String::as_str) != Some(piece.as_str()) {
            deduped.push(piece);
        }
    }

    if deduped.is_empty() {
        None
    } else {
        Some(deduped.join("\n\n"))
    }
}

fn extract_mcp_tool_arguments<'a>(
    params: &'a Value,
    item_payload: Option<&'a Value>,
) -> Option<&'a Value> {
    let argument_keys = [
        "arguments",
        "args",
        "input",
        "params",
        "parameters",
        "toolInput",
        "tool_input",
    ];

    item_payload
        .and_then(|item| {
            argument_keys
                .iter()
                .find_map(|key| item.get(*key))
        })
        .or_else(|| {
            argument_keys
                .iter()
                .find_map(|key| params.get(*key))
        })
}

fn format_mcp_tool_arguments(arguments: &Value) -> Option<String> {
    match arguments {
        Value::Null => None,
        Value::String(arguments) => {
            let arguments = arguments.trim();
            if arguments.is_empty() {
                return None;
            }

            if let Ok(parsed_arguments) = serde_json::from_str::<Value>(arguments)
                && let Some(formatted_arguments) = format_mcp_tool_arguments(&parsed_arguments)
            {
                return Some(formatted_arguments);
            }

            Some(format!("Input: {arguments}"))
        }
        Value::Object(arguments) => {
            let mut fields = Vec::new();
            for (key, value) in arguments {
                collect_mcp_argument_fields(key, value, &mut fields);
            }

            if fields.is_empty() {
                None
            } else {
                Some(fields.join("\n"))
            }
        }
        Value::Array(arguments) if arguments.is_empty() => None,
        Value::Array(arguments) => Some(
            arguments
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    format!("{}: {}", index + 1, format_mcp_argument_value(value))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Some(format!("Input: {}", format_mcp_argument_value(arguments))),
    }
}

fn collect_mcp_argument_fields(prefix: &str, value: &Value, fields: &mut Vec<String>) {
    match value {
        Value::Null => {}
        Value::Object(object) if object.is_empty() => {}
        Value::Object(object) => {
            for (key, value) in object {
                let nested_prefix = format!("{prefix}.{key}");
                collect_mcp_argument_fields(&nested_prefix, value, fields);
            }
        }
        _ => fields.push(format!("{prefix}: {}", format_mcp_argument_value(value))),
    }
}

fn format_mcp_argument_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values)
            if values.iter().all(|value| {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }) => {
            values
                .iter()
                .map(format_mcp_argument_value)
                .collect::<Vec<_>>()
                .join(", ")
        }
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

fn mcp_tool_name(params: Option<&Value>) -> Option<(SharedString, SharedString)> {
    let params = params?;
    let item = params.get("item");

    let tool: Option<String> = item
        .and_then(|i| i.get("tool"))
        .and_then(Value::as_str)
        .or_else(|| item.and_then(|i| i.get("name")).and_then(Value::as_str))
        .or_else(|| item.and_then(|i| i.get("toolName")).and_then(Value::as_str))
        .or_else(|| params.get("tool").and_then(Value::as_str))
        .or_else(|| params.get("name").and_then(Value::as_str))
        .map(str::to_string);

    let server: Option<String> = item
        .and_then(|i| i.get("server"))
        .and_then(Value::as_str)
        .or_else(|| {
            item.and_then(|i| i.get("serverLabel"))
                .and_then(Value::as_str)
        })
        .or_else(|| params.get("server").and_then(Value::as_str))
        .or_else(|| params.get("serverLabel").and_then(Value::as_str))
        .map(str::to_string);

    match (server, tool) {
        (Some(server), Some(tool)) => Some((SharedString::from(server), SharedString::from(tool))),
        (None, Some(tool)) => Some(("MCP".into(), SharedString::from(tool))),
        (Some(server), None) => Some((SharedString::from(server), "tool call".into())),
        (None, None) => None,
    }
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
        HarnessToolKind::McpToolCall => "MCP tool call".into(),
        HarnessToolKind::Other(name) => name.clone(),
    };

    let Some(params) = params else {
        return base;
    };
    let item_payload = params.get("item");

    if matches!(kind, HarnessToolKind::McpToolCall) {
        if let Some((server, tool)) = mcp_tool_name(Some(params)) {
            return format!("{server}: {tool}").into();
        }
        return base;
    }

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
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get("path"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get("filePath"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get("file"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                item_payload
                    .and_then(|item| item.get("query"))
                    .and_then(Value::as_str)
            });
        if let Some(detail) = detail {
            return format!("{base}: {detail}").into();
        }
    }

    base
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;
    use smol::channel;

    use super::{
        HarnessToolKind, HarnessTurnUpdate, RawSessionCommandEvent,
        extract_exec_command_output, finalize_partial_command_record,
        parse_raw_session_command_event, parse_tool_event, read_new_session_lines,
    };

    #[test]
    fn formats_mcp_tool_arguments_as_fields() {
        let params = json!({
            "item": {
                "id": "call-1",
                "type": "mcpToolCall",
                "server": "linear-ludus",
                "tool": "get_issue",
                "arguments": {
                    "id": "RIG-435",
                    "includeRelations": true
                }
            }
        });

        let event = parse_tool_event("item/added", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::McpToolCall);
        assert_eq!(event.title.as_ref(), "linear-ludus: get_issue");
        assert!(event.detail.lines().any(|line| line == "id: RIG-435"));
        assert!(
            event
                .detail
                .lines()
                .any(|line| line == "includeRelations: true")
        );
    }

    #[test]
    fn formats_stringified_mcp_tool_arguments_as_fields() {
        let params = json!({
            "item": {
                "id": "call-1",
                "type": "mcpToolCall",
                "server": "linear-ludus",
                "tool": "list_issue_comments",
                "arguments": "{\"issueId\":\"RIG-435\",\"limit\":50}"
            }
        });

        let event = parse_tool_event("item/completed", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::McpToolCall);
        assert!(event.detail.lines().any(|line| line == "issueId: RIG-435"));
        assert!(event.detail.lines().any(|line| line == "limit: 50"));
    }

    #[test]
    fn treats_updated_completed_tool_items_as_terminal() {
        let params = json!({
            "item": {
                "id": "cmd-1",
                "type": "commandExecution",
                "status": "completed",
                "command": ["/bin/zsh", "-lc", "pwd"],
                "output": "/tmp/workspace"
            }
        });

        let event = parse_tool_event("item/updated", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::End);
        assert_eq!(event.title.as_ref(), "Run command: /bin/zsh -lc pwd");
        assert_eq!(event.detail.as_ref(), "/tmp/workspace");
    }

    #[test]
    fn treats_legacy_command_completed_events_as_terminal() {
        let params = json!({
            "item": {
                "id": "cmd-2",
                "type": "commandExecution",
                "status": "completed",
                "command": ["/bin/zsh", "-lc", "sed -n '1,20p' file.txt"],
                "aggregatedOutput": "hello"
            }
        });

        let event = parse_tool_event("item/commandExecution/completed", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::End);
        assert_eq!(
            event.title.as_ref(),
            "Run command: /bin/zsh -lc sed -n '1,20p' file.txt"
        );
        assert_eq!(event.detail.as_ref(), "hello");
    }

    #[test]
    fn treats_failed_tool_items_as_failed_terminal_phase() {
        let params = json!({
            "item": {
                "id": "cmd-3",
                "type": "commandExecution",
                "status": "failed",
                "command": ["/bin/zsh", "-lc", "exit 1"],
                "output": "failed"
            }
        });

        let event = parse_tool_event("item/updated", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::Failed);
        assert_eq!(event.title.as_ref(), "Run command: /bin/zsh -lc exit 1");
        assert_eq!(event.detail.as_ref(), "failed");
    }

    #[test]
    fn decodes_command_output_delta_base64() {
        let params = json!({
            "itemId": "cmd-4",
            "deltaBase64": "aGVsbG8K"
        });

        let event = parse_tool_event("item/commandExecution/outputDelta", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::Update);
        assert_eq!(event.detail.as_ref(), "hello\n");
    }

    #[test]
    fn renders_command_stdout_and_stderr_snapshots() {
        let params = json!({
            "item": {
                "id": "cmd-5",
                "type": "commandExecution",
                "status": "completed",
                "command": ["/bin/zsh", "-lc", "echo hi >&2"],
                "stdout": "stdout line",
                "stderr": "stderr line"
            }
        });

        let event = parse_tool_event("item/completed", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::End);
        assert_eq!(event.detail.as_ref(), "stdout line\nstderr line");
    }

    #[test]
    fn renders_command_output_from_nested_result_snapshot() {
        let params = json!({
            "item": {
                "id": "cmd-6",
                "type": "commandExecution",
                "status": "completed",
                "command": ["/bin/zsh", "-lc", "python3 - <<'PY'"],
                "result": {
                    "stdout": "Cost: $0.27732585\nDuration: 155780 ms",
                    "stderr": ""
                }
            }
        });

        let event = parse_tool_event("item/completed", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::Command);
        assert_eq!(event.phase, super::HarnessToolPhase::End);
        assert_eq!(event.detail.as_ref(), "Cost: $0.27732585\nDuration: 155780 ms");
    }

    #[test]
    fn extracts_file_change_summaries_from_changes_map() {
        let params = json!({
            "item": {
                "id": "edit-1",
                "type": "fileChange",
                "changes": {
                    "src/main.rs": {
                        "type": "update",
                        "unified_diff": "@@ -1,2 +1,2 @@\n fn main() {\n-    old();\n+    new();\n }\n"
                    }
                }
            }
        });

        let event = parse_tool_event("item/completed", Some(&params)).unwrap();

        assert_eq!(event.kind, HarnessToolKind::FileChange);
        assert_eq!(event.file_changes.len(), 1);
        assert_eq!(event.file_changes[0].path.as_ref(), "src/main.rs");
        assert_eq!(event.file_changes[0].added_lines, 1);
        assert_eq!(event.file_changes[0].removed_lines, 1);
    }

    #[test]
    fn parses_exec_command_output_from_raw_session_response_item() {
        let line = br#"{"timestamp":"2026-04-21T20:00:55.004Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_123","output":"Chunk ID: 2ba670\nWall time: 1.0019 seconds\nProcess running with session ID 6888\nOriginal token count: 2177\nOutput:\nhello\nworld\n"}}"#;

        let event = parse_raw_session_command_event(line).unwrap();

        match event {
            RawSessionCommandEvent::Completed {
                call_id,
                tool_name,
                output,
            } => {
                assert_eq!(call_id, "call_123");
                assert_eq!(tool_name, None);
                assert_eq!(output, "hello\nworld");
            }
            RawSessionCommandEvent::Started { .. } => panic!("expected completed event"),
        }
    }

    #[test]
    fn parses_exec_command_end_output_from_raw_session_event() {
        let line = br#"{"timestamp":"2026-04-21T20:06:06.945Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_456","aggregated_output":"hello\nworld\n","formatted_output":"","stdout":"","stderr":"","status":"completed"}}"#;

        let event = parse_raw_session_command_event(line).unwrap();

        match event {
            RawSessionCommandEvent::Completed {
                call_id,
                tool_name,
                output,
            } => {
                assert_eq!(call_id, "call_456");
                assert_eq!(tool_name.as_deref(), Some("exec_command"));
                assert_eq!(output, "hello\nworld\n");
            }
            RawSessionCommandEvent::Started { .. } => panic!("expected completed event"),
        }
    }

    #[test]
    fn read_new_session_lines_advances_by_consumed_bytes() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mnigcode-harness-offset-{}-{unique}.jsonl",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "hello world").unwrap();
        drop(file);

        let (new_offset, bytes) = read_new_session_lines(&path, 6)
            .unwrap()
            .expect("expected new bytes");

        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "world");
        assert_eq!(new_offset, 11);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn finalizes_partial_command_record_without_newline() {
        smol::block_on(async {
            let (updates_tx, updates_rx) = channel::unbounded();
            let mut pending_tool_names = std::collections::HashMap::new();
            let mut partial = br#"{"timestamp":"2026-04-21T20:06:06.945Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_456","aggregated_output":"hello\nworld\n","formatted_output":"","stdout":"","stderr":"","status":"completed"}}"#.to_vec();

            finalize_partial_command_record(
                &super::HarnessThreadId("thread-1".into()),
                &updates_tx,
                &mut pending_tool_names,
                &mut partial,
            )
            .await;

            match updates_rx.recv().await.unwrap() {
                HarnessTurnUpdate::ToolEvent {
                    item_id,
                    kind,
                    phase,
                    detail,
                    ..
                } => {
                    assert_eq!(item_id.as_deref(), Some("call_456"));
                    assert_eq!(kind, HarnessToolKind::Command);
                    assert_eq!(phase, super::HarnessToolPhase::End);
                    assert_eq!(detail.as_ref(), "hello\nworld\n");
                }
                other => panic!("unexpected update: {other:?}"),
            }
            assert!(partial.is_empty());
        });
    }

    #[test]
    fn strips_exec_command_wrapper_text() {
        assert_eq!(
            extract_exec_command_output(
                "Chunk ID: abc\nWall time: 0.001s\nOutput:\nhello\nworld\n"
            )
            .as_deref(),
            Some("hello\nworld")
        );
        assert_eq!(
            extract_exec_command_output("Chunk ID: abc\nWall time: 0.001s\nOutput:\n"),
            None
        );
    }
}

fn append_codex_skills(skills: &mut Vec<HarnessSkillDefinition>, result: &Value) {
    let Some(entries) = result.get("data").and_then(Value::as_array) else {
        return;
    };

    for entry in entries {
        let Some(skill_entries) = entry.get("skills").and_then(Value::as_array) else {
            continue;
        };

        for skill in skill_entries {
            if skill
                .get("enabled")
                .and_then(Value::as_bool)
                .is_some_and(|enabled| !enabled)
            {
                continue;
            }

            let Some(name) = skill.get("name").and_then(Value::as_str) else {
                continue;
            };
            let description = skill
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                .unwrap_or("Codex skill");
            let path = skill.get("path").and_then(Value::as_str).map(PathBuf::from);

            skills.push(HarnessSkillDefinition {
                name: name.to_string().into(),
                description: description.to_string().into(),
                path,
                source: HarnessSkillSource::Local,
            });
        }
    }
}

fn deduplicate_skills(skills: &mut Vec<HarnessSkillDefinition>) {
    let mut seen = std::collections::HashSet::new();
    skills.retain(|skill| seen.insert((skill.source, skill.name.to_string(), skill.path.clone())));
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

fn read_thread_path(result: &Value) -> Option<PathBuf> {
    result
        .get("thread")
        .and_then(|thread| thread.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn approval_policy_name(policy: &HarnessApprovalPolicy) -> &'static str {
    match policy {
        HarnessApprovalPolicy::Never => "never",
        HarnessApprovalPolicy::OnRequest => "on-request",
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
