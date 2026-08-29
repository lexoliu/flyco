//! The subset of `codex app-server` JSON-RPC that flycod speaks.
//!
//! Framing is newline-delimited JSON with **no** `"jsonrpc":"2.0"` field.
//! Discriminate by field presence: `{id,method}` is a request, `{method}` a
//! notification, `{id,result}` a response, `{id,error}` an error. Request
//! ids are a string or an integer. Ground truth is
//! `docs/research/codex-app-server.md`, not the app-server README.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request identifier: a string or an integer, never a float.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id, which is what flycod mints.
    Number(u64),
    /// String id, which a server may use for its own requests.
    Text(String),
}

impl RequestId {
    /// A numeric id flycod minted.
    #[must_use]
    pub const fn number(n: u64) -> Self {
        Self::Number(n)
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One framed message on the app-server's stdio.
///
/// Serialized without a `jsonrpc` field, matching the app-server's wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Envelope {
    /// Client or server request that expects a response.
    Request {
        /// Correlation id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Parameters, omitted when empty.
        params: Value,
    },
    /// One-way notification.
    Notification {
        /// Method name.
        method: String,
        /// Parameters, omitted when empty.
        params: Value,
    },
    /// Successful response.
    Response {
        /// Correlation id of the request.
        id: RequestId,
        /// Result payload.
        result: Value,
    },
    /// Failed response.
    Error {
        /// Correlation id of the request.
        id: RequestId,
        /// The error.
        error: RpcError,
    },
}

impl Envelope {
    /// A request with the given method and params.
    #[must_use]
    pub fn request(id: RequestId, method: &'static str, params: Value) -> Self {
        Self::Request {
            id,
            method: method.to_owned(),
            params,
        }
    }

    /// A notification with the given method and params.
    #[must_use]
    pub fn notification(method: &'static str, params: Value) -> Self {
        Self::Notification {
            method: method.to_owned(),
            params,
        }
    }

    /// A successful response.
    #[must_use]
    pub const fn response(id: RequestId, result: Value) -> Self {
        Self::Response { id, result }
    }

    /// A JSON-RPC error response.
    #[must_use]
    pub const fn error_response(id: RequestId, code: i64, message: String) -> Self {
        Self::Error {
            id,
            error: RpcError {
                code,
                message,
                data: None,
            },
        }
    }

    /// The method name, when this frame is a request or notification.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } | Self::Error { .. } => None,
        }
    }
}

impl Serialize for Envelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Request { id, method, params } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "id".to_owned(),
                    serde_json::to_value(id).map_err(serde::ser::Error::custom)?,
                );
                map.insert("method".to_owned(), Value::String(method.clone()));
                if !params.is_null() {
                    map.insert("params".to_owned(), params.clone());
                }
                map.serialize(serializer)
            }
            Self::Notification { method, params } => {
                let mut map = serde_json::Map::new();
                map.insert("method".to_owned(), Value::String(method.clone()));
                if !params.is_null() {
                    map.insert("params".to_owned(), params.clone());
                }
                map.serialize(serializer)
            }
            Self::Response { id, result } => WireResponse {
                id: id.clone(),
                result: result.clone(),
            }
            .serialize(serializer),
            Self::Error { id, error } => WireError {
                id: id.clone(),
                error: error.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("app-server frames are JSON objects"))?;
        let id = obj.get("id").map(|id| serde_json::from_value(id.clone()));
        let method = obj.get("method").and_then(Value::as_str);
        if let Some(method) = method {
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            return match id {
                Some(Ok(id)) => Ok(Self::Request {
                    id,
                    method: method.to_owned(),
                    params,
                }),
                Some(Err(error)) => Err(serde::de::Error::custom(error)),
                None => Ok(Self::Notification {
                    method: method.to_owned(),
                    params,
                }),
            };
        }
        let id = id
            .ok_or_else(|| serde::de::Error::custom("a response must have an id"))?
            .map_err(serde::de::Error::custom)?;
        if let Some(result) = obj.get("result") {
            return Ok(Self::Response {
                id,
                result: result.clone(),
            });
        }
        if let Some(error) = obj.get("error") {
            let error = serde_json::from_value(error.clone()).map_err(serde::de::Error::custom)?;
            return Ok(Self::Error { id, error });
        }
        Err(serde::de::Error::custom(
            "an app-server frame must be a request, notification, result, or error",
        ))
    }
}

#[derive(Debug, Serialize)]
struct WireResponse {
    id: RequestId,
    result: Value,
}

#[derive(Debug, Serialize)]
struct WireError {
    id: RequestId,
    error: RpcError,
}

/// Method names flycod sends or handles.
pub mod method {
    /// Client → server handshake request.
    pub const INITIALIZE: &str = "initialize";
    /// Client → server handshake notification after [`INITIALIZE`].
    pub const INITIALIZED: &str = "initialized";
    /// Open a new thread.
    pub const THREAD_START: &str = "thread/start";
    /// Resume an existing thread.
    pub const THREAD_RESUME: &str = "thread/resume";
    /// Open a turn with a user message.
    pub const TURN_START: &str = "turn/start";
    /// Interrupt the in-flight turn.
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    /// Server → client: a command wants permission.
    pub const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
    /// Server → client: a file change wants permission.
    pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
    /// Server → client: a permissions grant wants permission.
    pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
    /// Server → client: `ChatGPT` tokens must be refreshed.
    pub const AUTH_REFRESH: &str = "account/chatgptAuthTokens/refresh";
    /// Notification: a turn began.
    pub const TURN_STARTED: &str = "turn/started";
    /// Notification: a turn ended.
    pub const TURN_COMPLETED: &str = "turn/completed";
    /// Notification: incremental assistant text.
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    /// Notification: an item started.
    pub const ITEM_STARTED: &str = "item/started";
    /// Notification: an item completed.
    pub const ITEM_COMPLETED: &str = "item/completed";
    /// Notification: token usage snapshot.
    pub const TOKEN_USAGE: &str = "thread/tokenUsage/updated";
    /// Notification: a turn-level error, possibly retried internally.
    pub const ERROR: &str = "error";
}

/// `initialize.clientInfo`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

/// `initialize.capabilities`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Experimental API surface. Flycod stays on the stable methods.
    pub experimental_api: bool,
}

/// Params for `initialize`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Who is speaking.
    pub client_info: ClientInfo,
    /// What this client supports.
    pub capabilities: ClientCapabilities,
}

/// Params for `thread/start` and `thread/resume`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadParams {
    /// Working directory the agent operates in.
    pub cwd: String,
    /// Approval policy, kebab-case: `untrusted` | `on-request` | `never`.
    pub approval_policy: String,
    /// Sandbox mode, kebab-case: `read-only` | `workspace-write` | `danger-full-access`.
    pub sandbox: String,
    /// Model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Thread to resume. Only on `thread/resume`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A text user-input item for `turn/start`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// Plain text.
    Text {
        /// The message.
        text: String,
        /// Structured elements; flycod sends none.
        text_elements: [(); 0],
    },
}

/// Params for `turn/start`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    /// Thread this turn belongs to.
    pub thread_id: String,
    /// User input items.
    pub input: Vec<UserInput>,
}

/// Params for `turn/interrupt`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    /// Thread whose in-flight turn is interrupted.
    pub thread_id: String,
}

/// An approval decision flycod returns to the app-server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    /// Allow this invocation.
    Accept,
    /// Refuse this invocation; the turn continues.
    Decline,
    /// Abort the turn.
    Cancel,
}

/// Response body for command/file-change approvals.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ApprovalDecisionBody {
    /// The decision.
    pub decision: ApprovalDecision,
}

#[cfg(test)]
mod tests {
    use super::{Envelope, RequestId, method};
    use serde_json::json;

    #[test]
    fn a_request_omits_the_jsonrpc_field() {
        let frame = Envelope::request(RequestId::number(1), method::INITIALIZE, json!({}));
        let value = serde_json::to_value(&frame).expect("serialize");
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], method::INITIALIZE);
    }

    #[test]
    fn a_notification_has_no_id() {
        let frame = Envelope::notification(method::INITIALIZED, serde_json::Value::Null);
        let encoded = serde_json::to_string(&frame).expect("serialize");
        assert_eq!(encoded, r#"{"method":"initialized"}"#);
    }

    #[test]
    fn a_server_request_round_trips_a_string_id() {
        let raw = json!({"id":"approval-1","method":"item/commandExecution/requestApproval","params":{"command":"ls"}});
        let frame: Envelope = serde_json::from_value(raw.clone()).expect("deserialize");
        match &frame {
            Envelope::Request { id, method, .. } => {
                assert_eq!(id, &RequestId::Text("approval-1".to_owned()));
                assert_eq!(method, super::method::COMMAND_APPROVAL);
            }
            other => panic!("expected a request, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&frame).expect("serialize"), raw);
    }
}
