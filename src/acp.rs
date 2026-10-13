//! Codex transport used by the ChatGPT subscription provider (`openai-acp`).
//!
//! Codex owns ChatGPT authentication. Completions use app-server's ephemeral
//! threads because ACP sessions are persisted in the user's Codex history.
//! Termplete never reads or stores Codex's credentials.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs, path::PathBuf};

use serde_json::{json, Value};

use crate::provider::{ChatRequest, Role};
use crate::{Error, Result};

const CODEX_PACKAGE: &str = "@openai/codex";
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPLY_BYTES: usize = 128 * 1024;

/// Long-lived Codex process. Each completion has its own unsaved thread.
pub struct Client {
    process: Option<Process>,
    timeout: Duration,
}

impl Client {
    pub fn new(timeout: Duration) -> Self {
        Self {
            process: None,
            timeout: timeout.max(Duration::from_secs(1)),
        }
    }

    pub fn complete(&mut self, model: &str, request: &ChatRequest) -> Result<String> {
        let exited = self
            .process
            .as_mut()
            .is_some_and(|process| !matches!(process.child.try_wait(), Ok(None)));
        if exited {
            self.process.take();
        }
        if self.process.is_none() {
            self.process = Some(Process::start(self.timeout)?);
        }

        let result = self
            .process
            .as_mut()
            .expect("Codex process initialized")
            .complete(model, request);
        if result.is_err() {
            // A timed out or malformed protocol exchange may leave the stream
            // out of sync. Drop the child and start cleanly on the next request.
            self.process.take();
        }
        result
    }
}

/// Start Codex's browser login flow. OAuth state remains managed by Codex.
pub fn login() -> Result<()> {
    let status = codex_runtime()?
        .codex_command()
        .arg("login")
        .status()
        .map_err(|error| Error::Auth(format!("could not start ChatGPT sign-in: {error}")))?;
    if !status.success() {
        return Err(Error::Auth(format!("`codex login` exited with {status}")));
    }
    Ok(())
}

/// Ask Codex to clear its authentication.
pub fn logout() -> Result<()> {
    let status = codex_runtime()?
        .codex_command()
        .arg("logout")
        .status()
        .map_err(|error| Error::Auth(format!("could not start ChatGPT sign-out: {error}")))?;
    if !status.success() {
        return Err(Error::Auth(format!("`codex logout` exited with {status}")));
    }
    Ok(())
}

fn missing_binary(error: io::Error) -> Error {
    Error::Auth(format!(
        "could not start `codex` ({error}); install Node.js and npm, then retry ChatGPT sign-in"
    ))
}

/// Reuse an installed Codex CLI or install it into Termplete's cache on first
/// use. Codex manages authentication in its own process.
struct CodexRuntime {
    codex: PathBuf,
}

impl CodexRuntime {
    fn codex_command(&self) -> Command {
        Command::new(&self.codex)
    }

    fn completion_command(&self) -> Command {
        let mut command = self.codex_command();
        // Completion must not inherit integrations or executable lifecycle hooks.
        command.args([
            "app-server",
            "-c",
            "model_provider=\"openai\"",
            "-c",
            "features.apps=false",
            "-c",
            "features.plugins=false",
            "-c",
            "features.hooks=false",
            "-c",
            "notify=[]",
        ]);
        command.env_remove("OPENAI_API_KEY");
        command.env_remove("CODEX_API_KEY");
        command.env_remove("DEFAULT_AUTH_REQUEST");
        command
    }
}

fn codex_runtime() -> Result<CodexRuntime> {
    if let Some(codex) = find_on_path("codex") {
        return Ok(CodexRuntime { codex });
    }

    let prefix = dirs::cache_dir()
        .ok_or_else(|| Error::Auth("could not locate the TermPlete cache directory".into()))?
        .join("termplete")
        .join("codex-acp");
    let bin_dir = prefix.join("node_modules").join(".bin");
    let codex = bin_dir.join("codex");

    if !codex.is_file() {
        let npm = find_on_path("npm").ok_or_else(|| {
            Error::Auth(
                "`codex` is required but npm was not found. Install Node.js with npm to set up ChatGPT sign-in"
                    .into(),
            )
        })?;
        fs::create_dir_all(&prefix).map_err(|error| {
            Error::Auth(format!(
                "could not create the ChatGPT helper cache directory: {error}"
            ))
        })?;

        eprintln!("Installing ChatGPT Codex helper into {}", prefix.display());
        let status = Command::new(npm)
            .arg("install")
            .arg("--prefix")
            .arg(&prefix)
            .args([
                "--no-save",
                "--no-package-lock",
                "--no-audit",
                "--no-fund",
                CODEX_PACKAGE,
            ])
            .status()
            .map_err(|error| {
                Error::Auth(format!(
                    "could not run npm to install `{CODEX_PACKAGE}`: {error}"
                ))
            })?;
        if !status.success() {
            return Err(Error::Auth(format!(
                "npm could not install the ChatGPT Codex helper (exited with {status})"
            )));
        }
    }
    if !codex.is_file() {
        return Err(Error::Auth(
            "npm did not provide the required `codex` command".into(),
        ));
    }

    Ok(CodexRuntime { codex })
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<io::Result<Value>>,
    next_id: u64,
    timeout: Duration,
    models: Vec<Value>,
    mcp_servers: serde_json::Map<String, Value>,
}

impl Process {
    fn start(timeout: Duration) -> Result<Self> {
        Self::start_command(codex_runtime()?.completion_command(), timeout)
    }

    fn start_command(mut command: Command, timeout: Duration) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(missing_binary)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Provider("could not open Codex stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Provider("could not open Codex stdout".into()))?;
        let messages = message_reader(stdout);
        let mut process = Self {
            child,
            stdin,
            messages,
            next_id: 1,
            timeout: timeout.max(Duration::from_secs(1)),
            models: Vec::new(),
            mcp_servers: serde_json::Map::new(),
        };
        process.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "termplete",
                    "title": "Termplete",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }),
        )?;
        process.send(&json!({ "method": "initialized" }))?;
        let config = process.request("config/read", json!({ "includeLayers": false }))?;
        if config
            .pointer("/config/notify")
            .and_then(Value::as_array)
            .is_none_or(|notify| !notify.is_empty())
            || ["apps", "plugins", "hooks"].into_iter().any(|feature| {
                config
                    .pointer(&format!("/config/features/{feature}"))
                    .and_then(Value::as_bool)
                    != Some(false)
            })
        {
            return Err(Error::Provider(
                "Codex did not confirm disabled apps, plugins, hooks and notifications; update Codex before using ChatGPT completions".into(),
            ));
        }
        if let Some(servers) = config.pointer("/config/mcp_servers") {
            let servers = servers.as_object().ok_or_else(|| {
                Error::Provider("Codex returned an invalid MCP configuration".into())
            })?;
            for name in servers.keys() {
                process
                    .mcp_servers
                    .insert(name.clone(), json!({ "enabled": false }));
            }
        }
        let mut cursor = Value::Null;
        loop {
            let catalog = process.request(
                "model/list",
                json!({
                    "includeHidden": true, "cursor": cursor, "limit": 100
                }),
            )?;
            let models = catalog
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Provider("Codex returned an invalid model catalog".into()))?;
            process.models.extend(models.iter().cloned());
            let next = catalog.get("nextCursor").cloned().unwrap_or(Value::Null);
            if next.is_null() {
                break;
            }
            if next == cursor {
                return Err(Error::Provider(
                    "Codex returned a repeated model catalog cursor".into(),
                ));
            }
            cursor = next;
        }
        Ok(process)
    }

    fn complete(&mut self, model: &str, request: &ChatRequest) -> Result<String> {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let mut params = json!({
            "cwd": cwd,
            "modelProvider": "openai",
            "ephemeral": true,
            "sandbox": "read-only",
            "approvalPolicy": "on-request",
            "baseInstructions": "You provide terminal command completions. Never use tools, execute commands, inspect files or change files. Reply only with the requested completion text.",
            "config": {
                "notify": [],
                "mcp_servers": self.mcp_servers,
                "features.apps": false,
                "features.plugins": false,
                "features.hooks": false,
                "features.shell_tool": false,
                "features.unified_exec": false,
                "features.multi_agent": false,
                "web_search": "disabled"
            }
        });
        if !model.trim().is_empty() {
            params["model"] = json!(model);
        }
        let session = self.request("thread/start", params)?;
        let thread_id = session
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Provider("Codex returned no thread id".into()))?
            .to_string();
        let result = (|| {
            // Never send terminal context unless the server confirms that it
            // will not persist this thread. Older helpers fail closed here.
            if session
                .pointer("/thread/ephemeral")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err(Error::Provider(
                    "Codex did not confirm an ephemeral thread; update Codex before using ChatGPT completions".into()
                ));
            }
            let mut cursor = Value::Null;
            loop {
                let inventory = self.request(
                    "mcpServerStatus/list",
                    json!({ "threadId": thread_id, "cursor": cursor, "limit": 100 }),
                )?;
                let servers = inventory
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        Error::Provider("Codex returned an invalid MCP inventory".into())
                    })?;
                if servers.iter().any(|server| {
                    server.get("runtimeStatus").and_then(Value::as_str) != Some("disabled")
                }) {
                    return Err(Error::Provider(
                        "Codex did not disable all MCP integrations; terminal context was withheld"
                            .into(),
                    ));
                }
                let next = inventory.get("nextCursor").cloned().unwrap_or(Value::Null);
                if next.is_null() {
                    break;
                }
                if next == cursor {
                    return Err(Error::Provider(
                        "Codex returned a repeated MCP inventory cursor".into(),
                    ));
                }
                cursor = next;
            }
            let selected = session
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Provider("Codex returned no selected model".into()))?;
            let effort = self.minimum_effort(selected)?;
            let mut turn = json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": completion_prompt(request), "text_elements": [] }],
                "summary": "none"
            });
            if let Some(effort) = effort {
                turn["effort"] = json!(effort);
            }
            // Read the start response and all notifications in the same loop:
            // short completions can emit text before the start response.
            let id = self.reserve_id();
            self.send(&json!({ "id": id, "method": "turn/start", "params": turn }))?;
            let deadline = Instant::now() + self.timeout;
            let mut deltas = String::new();
            let mut final_text = String::new();
            loop {
                let message = self.receive(deadline)?;
                if is_response_for(&message, id) {
                    response_result(&message)?;
                    continue;
                }
                self.handle_server_request(&message)?;
                if message.pointer("/params/threadId").and_then(Value::as_str) != Some(&thread_id) {
                    continue;
                }
                match message.get("method").and_then(Value::as_str) {
                    Some("item/agentMessage/delta") => {
                        if let Some(text) = message.pointer("/params/delta").and_then(Value::as_str)
                        {
                            deltas.push_str(text);
                        }
                    }
                    Some("item/completed") => {
                        let item = &message["params"]["item"];
                        if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                            && item.get("phase").and_then(Value::as_str) != Some("commentary")
                        {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                if !final_text.is_empty() {
                                    final_text.push('\n');
                                }
                                final_text.push_str(text);
                            }
                        }
                    }
                    Some("turn/completed") => {
                        let turn = &message["params"]["turn"];
                        if turn.get("status").and_then(Value::as_str) != Some("completed") {
                            let detail = turn
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("completion failed or was interrupted");
                            return Err(Error::Provider(format!("Codex: {detail}")));
                        }
                        let reply = if final_text.is_empty() {
                            &deltas
                        } else {
                            &final_text
                        };
                        let reply = reply.trim();
                        if reply.is_empty() {
                            return Err(Error::Provider(
                                "Codex returned no completion text".into(),
                            ));
                        }
                        return Ok(reply.to_string());
                    }
                    _ => {}
                }
                if deltas.len() > MAX_REPLY_BYTES || final_text.len() > MAX_REPLY_BYTES {
                    return Err(Error::Provider(
                        "Codex reply exceeded the size limit".into(),
                    ));
                }
            }
        })();
        if result.is_ok() {
            // Release in-memory state; this does not archive or delete a chat.
            let _ = self.request("thread/unsubscribe", json!({ "threadId": thread_id }));
        }
        result
    }

    fn minimum_effort(&self, selected: &str) -> Result<Option<&str>> {
        let model = self
            .models
            .iter()
            .find(|model| {
                model.get("model").and_then(Value::as_str) == Some(selected)
                    || model.get("id").and_then(Value::as_str) == Some(selected)
            })
            .ok_or_else(|| {
                Error::Provider(format!(
                    "Codex did not advertise reasoning controls for `{selected}`"
                ))
            })?;
        let supported = model
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Provider("invalid Codex reasoning options".into()))?;
        if supported.is_empty() {
            return Ok(None);
        }
        ["none", "low", "minimal"]
            .into_iter()
            .find(|effort| {
                supported.iter().any(|option| {
                    option.get("reasoningEffort").and_then(Value::as_str) == Some(*effort)
                })
            })
            .map(Some)
            .ok_or_else(|| {
                Error::Provider(
                    "this Codex model cannot disable reasoning or use Low; choose another model"
                        .into(),
                )
            })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.reserve_id();
        self.send(&json!({
            "id": id,
            "method": method,
            "params": params
        }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let message = self.receive(deadline)?;
            if is_response_for(&message, id) {
                return response_result(&message);
            }
            self.handle_server_request(&message)?;
        }
    }

    fn reserve_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| Error::Provider(format!("could not encode Codex message: {error}")))?;
        self.stdin
            .write_all(&bytes)
            .and_then(|()| self.stdin.write_all(b"\n"))
            .and_then(|()| self.stdin.flush())
            .map_err(|error| Error::Provider(format!("could not write to Codex: {error}")))
    }

    fn receive(&self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Provider(format!(
                "Codex timed out after {} seconds",
                self.timeout.as_secs()
            )));
        }
        match self.messages.recv_timeout(remaining) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => Err(Error::Provider(format!(
                "could not read from Codex: {error}"
            ))),
            Err(RecvTimeoutError::Timeout) => Err(Error::Provider(format!(
                "Codex timed out after {} seconds",
                self.timeout.as_secs()
            ))),
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Provider("Codex closed its protocol stream".into()))
            }
        }
    }

    fn handle_server_request(&mut self, message: &Value) -> Result<()> {
        let (Some(id), Some(method)) = (
            message.get("id"),
            message.get("method").and_then(Value::as_str),
        ) else {
            return Ok(());
        };
        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                Some(json!({ "decision": "decline" }))
            }
            "item/permissions/requestApproval" => {
                Some(json!({ "permissions": {}, "scope": "turn" }))
            }
            "item/tool/requestUserInput" | "tool/requestUserInput" => {
                Some(json!({ "answers": {} }))
            }
            "mcpServer/elicitation/request" => {
                Some(json!({ "action": "decline", "content": null }))
            }
            "item/tool/call" => Some(json!({ "success": false, "contentItems": [] })),
            _ => None,
        };
        if let Some(result) = result {
            self.send(&json!({ "id": id, "result": result }))
        } else {
            self.send(&json!({
                "id": id,
                "error": { "code": -32601, "message": "method not supported by termplete" }
            }))
        }
    }
}

fn is_response_for(message: &Value, id: u64) -> bool {
    message.get("method").is_none() && message.get("id") == Some(&json!(id))
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn message_reader(stdout: std::process::ChildStdout) -> Receiver<io::Result<Value>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let result = read_frame(&mut reader);
            let done = result.is_err();
            if sender.send(result).is_err() || done {
                break;
            }
        }
    });
    receiver
}

fn read_frame(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut bytes = Vec::new();
    // Codex stdio uses one JSON-RPC message per line. Bound each read before
    // allocating so a broken helper cannot grow the buffer indefinitely.
    use std::io::Read;
    reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Codex server closed stdout",
        ));
    }
    if bytes.len() > MAX_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Codex frame size or missing newline",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn response_result(message: &Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex request failed");
        return Err(Error::Provider(format!(
            "OpenAI Codex request failed ({code}): {detail}"
        )));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| Error::Provider("Codex returned an invalid response".into()))
}

fn completion_prompt(request: &ChatRequest) -> String {
    let mut prompt = String::from(
        "You provide terminal command completions. Do not run commands, inspect files,",
    );
    prompt.push_str(
        " use tools, or change files. Reply only with the requested completion text.\n\n",
    );
    for message in &request.messages {
        let role = match message.role {
            Role::System => "Instructions",
            Role::User => "Request",
            Role::Assistant => "Previous response",
        };
        prompt.push_str(role);
        prompt.push_str(":\n");
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    const MOCK: &str = r#"
import json, sys, time
scenario, trace = sys.argv[1:]
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    with open(trace, 'a') as log:
        log.write(json.dumps(message) + '\n')
    method = message.get('method')
    if method == 'initialize':
        result = {}
    elif method == 'config/read':
        result = {'config': {'notify': [], 'features': dict.fromkeys(['apps', 'plugins', 'hooks'], False),
                            'mcp_servers': {'local.fixture': {'enabled': True}}}}
        if scenario == 'unsafe-config':
            result['config']['features']['apps'] = True
        if scenario == 'unsafe-notify':
            result['config']['notify'] = ['unused-fixture']
    elif method == 'model/list':
        result = {'data': [{'model': 'mock-model', 'supportedReasoningEfforts': [
            {'reasoningEffort': 'high'}, {'reasoningEffort': 'none'}]}]}
    elif method == 'thread/start':
        assert message['params']['config']['mcp_servers']['local.fixture']['enabled'] is False
        assert message['params']['sandbox'] == 'read-only'
        assert message['params']['ephemeral'] is True
        assert message['params']['approvalPolicy'] == 'on-request'
        assert message['params']['config']['notify'] == []
        result = {'thread': {'id': 'mock-thread', 'ephemeral': scenario != 'persisted'},
                  'model': 'mock-model'}
    elif method == 'mcpServerStatus/list':
        result = {'data': [{'runtimeStatus': 'ready' if scenario == 'active-mcp' else 'disabled'}]}
    elif method == 'turn/start':
        assert message['params']['effort'] == 'none'
        if scenario == 'timeout':
            time.sleep(5)
            continue
        if scenario == 'malformed':
            print('invalid-json', flush=True)
            continue
        if scenario == 'error':
            send({'id': message['id'], 'error': {'code': -1, 'message': 'synthetic failure'}})
            continue
        send({'method': 'item/agentMessage/delta', 'params': {'threadId': 'mock-thread', 'delta': 'git log'}})
        for identifier, request in enumerate([
            'item/commandExecution/requestApproval', 'item/fileChange/requestApproval',
            'item/tool/call', 'tool/requestUserInput', 'mcpServer/elicitation/request'], 100):
            send({'id': identifier, 'method': request, 'params': {}})
        result = {}
        send({'method': 'turn/completed', 'params': {'threadId': 'mock-thread', 'turn': {'status': 'completed'}}})
    elif method == 'thread/unsubscribe':
        result = {}
    else:
        continue
    send({'id': message['id'], 'result': result})
"#;

    fn mock(scenario: &str, trace: &std::path::Path) -> Result<Process> {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", MOCK, scenario]).arg(trace);
        Process::start_command(command, Duration::from_secs(1))
    }

    #[test]
    fn completion_handles_early_text_and_declines_tool_requests() {
        let trace = tempfile::NamedTempFile::new().unwrap();
        let mut process = mock("success", trace.path()).unwrap();
        let request = ChatRequest::new("mock-model", vec![Message::user("git l")]);
        assert_eq!(process.complete("mock-model", &request).unwrap(), "git log");
        drop(process);
        let messages: Vec<Value> = fs::read_to_string(trace.path())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for (id, result) in [
            (100, json!({ "decision": "decline" })),
            (101, json!({ "decision": "decline" })),
            (102, json!({ "success": false, "contentItems": [] })),
            (103, json!({ "answers": {} })),
            (104, json!({ "action": "decline", "content": null })),
        ] {
            assert!(messages
                .iter()
                .any(|message| message["id"] == id && message["result"] == result));
        }
    }

    #[test]
    fn unsafe_threads_never_receive_terminal_context() {
        for scenario in ["persisted", "active-mcp", "unsafe-config", "unsafe-notify"] {
            let trace = tempfile::NamedTempFile::new().unwrap();
            let request = ChatRequest::new("mock-model", vec![Message::user("CONTEXT_SENTINEL")]);
            if let Ok(mut process) = mock(scenario, trace.path()) {
                assert!(
                    process.complete("mock-model", &request).is_err(),
                    "{scenario}"
                );
            }
            assert!(
                !fs::read_to_string(trace.path())
                    .unwrap()
                    .contains("CONTEXT_SENTINEL"),
                "{scenario}"
            );
        }
    }

    #[test]
    fn failed_protocol_exchange_drops_the_child_for_retry() {
        for scenario in ["timeout", "malformed", "error"] {
            let trace = tempfile::NamedTempFile::new().unwrap();
            let mut client = Client::new(Duration::from_secs(1));
            client.process = Some(mock(scenario, trace.path()).unwrap());
            let request = ChatRequest::new("mock-model", vec![Message::user("git l")]);
            assert!(
                client.complete("mock-model", &request).is_err(),
                "{scenario}"
            );
            assert!(client.process.is_none(), "{scenario}");
        }
        for frame in [
            b"invalid-json\n".as_slice(),
            b"{}".as_slice(),
            b"".as_slice(),
        ] {
            assert!(read_frame(&mut io::Cursor::new(frame)).is_err());
        }
    }
}
