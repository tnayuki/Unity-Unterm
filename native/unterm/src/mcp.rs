//! In-process MCP server hosted in the native plugin so it survives Unity C#
//! domain reloads. There is no transport and no port: the control-protocol driver
//! dispatches the agent's `mcp_message` requests straight into this server
//! in-process (see [`McpDispatcher`]), so MCP needs neither Node nor an HTTP
//! socket.
//!
//! Tool definitions and execution are supplied by the Unity (C#) side: C# sets
//! the tool list via [`McpDispatcher::set_tools`] and drains/answers tool calls
//! via [`McpDispatcher::next_call`] / [`McpDispatcher::respond`]. This server only
//! forwards `tools/call` to C# through a pending queue.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-06-18";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

struct PendingCall {
    id: u64,
    name: String,
    args: Value,
}

struct Shared {
    tools: Mutex<Value>, // JSON array for tools/list
    pending: Mutex<VecDeque<PendingCall>>,
    responders: Mutex<HashMap<u64, Sender<String>>>,
    next_id: AtomicU64,
}

/// The process-global MCP state. It lives for the life of the loaded plugin image
/// — so the tool catalog and any in-flight calls survive C# domain reloads — and
/// is reached only through a cloneable [`McpDispatcher`].
fn shared() -> &'static Arc<Shared> {
    static S: OnceLock<Arc<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        Arc::new(Shared {
            tools: Mutex::new(json!([])),
            pending: Mutex::new(VecDeque::new()),
            responders: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    })
}

/// A transport-free handle onto the in-process MCP server's queue. Clone-able and
/// `Send`, it lets the control-protocol driver dispatch standard MCP JSON-RPC into
/// the pending queue the Unity side drains. It holds only an `Arc`, so it can
/// safely outlive a session (a blocked `tools/call` just times out).
#[derive(Clone)]
pub struct McpDispatcher {
    shared: Arc<Shared>,
}

/// A handle onto the process-global in-process MCP server (lazily created).
pub fn dispatcher() -> McpDispatcher {
    McpDispatcher {
        shared: shared().clone(),
    }
}

impl McpDispatcher {
    /// Dispatch one MCP JSON-RPC message, returning the JSON-RPC response, or
    /// `None` for an id-less notification (which needs no reply). `tools/call`
    /// blocks until the Unity side answers (or the call times out).
    pub fn dispatch(&self, msg: &Value) -> Option<Value> {
        dispatch(msg, &self.shared)
    }

    /// Replace the advertised tool list (JSON array of {name,description,inputSchema}).
    pub fn set_tools(&self, tools_json: &str) {
        if let Ok(v) = serde_json::from_str::<Value>(tools_json) {
            *self.shared.tools.lock().unwrap() = v;
        }
    }

    /// Pop the next queued tool call as `{id, name, args}` JSON, or None.
    pub fn next_call(&self) -> Option<String> {
        let mut q = self.shared.pending.lock().unwrap();
        q.pop_front()
            .map(|c| json!({ "id": c.id, "name": c.name, "args": c.args }).to_string())
    }

    /// Answer a tool call by id with a result JSON string (the MCP tool result).
    pub fn respond(&self, id: u64, result_json: &str) {
        if let Some(tx) = self.shared.responders.lock().unwrap().remove(&id) {
            let _ = tx.send(result_json.to_string());
        }
    }
}

/// Handle one MCP JSON-RPC request, returning the JSON-RPC response (success or
/// error envelope), or `None` for an id-less notification. The control-protocol
/// driver is the only caller (the agent's `mcp_message` requests).
fn dispatch(msg: &Value, shared: &Arc<Shared>) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Notifications (no id) need no response.
    id.as_ref()?;

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "unterm-unity", "version": "0.1.0" },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": shared.tools.lock().unwrap().clone() }),
        "tools/call" => call_tool(&msg["params"], shared),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "unknown method" }
            }));
        }
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Forward a tools/call to the Unity side and block until it answers.
fn call_tool(params: &Value, shared: &Arc<Shared>) -> Value {
    let name = params["name"].as_str().unwrap_or("").to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel::<String>();
    shared.responders.lock().unwrap().insert(id, tx);
    shared
        .pending
        .lock()
        .unwrap()
        .push_back(PendingCall { id, name, args });

    match rx.recv_timeout(CALL_TIMEOUT) {
        Ok(result) => {
            serde_json::from_str::<Value>(&result).unwrap_or_else(|_| text_result(&result, false))
        }
        Err(_) => {
            shared.responders.lock().unwrap().remove(&id);
            text_result("tool call timed out or the editor was unavailable", true)
        }
    }
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}
