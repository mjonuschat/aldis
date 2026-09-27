//! Moonraker's JSON-RPC framing for agents.

use serde_json::{Value, json};

use crate::agent::api::UpdateResponse;
use crate::agent::service::{AgentBackend, AgentService, ApiError, UpdateRequest};

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub enum Incoming {
    /// A request relayed from a frontend's `server.extensions.request`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Moonraker rejected our `server.connection.identify` call (e.g. already registered, or an
    /// auth failure): the agent is connected but will never actually appear to frontends.
    IdentifyError { message: String },
    /// Responses to our own calls and Moonraker's `notify_*` broadcasts.
    Ignored,
}

/// The fixed `id` used for the one `server.connection.identify` call made per connection, so its
/// reply (success or error) can be told apart from any other response.
pub const IDENTIFY_ID: u64 = 1;

pub fn parse_incoming(text: &str) -> Incoming {
    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(text) else {
        return Incoming::Ignored;
    };
    let error = message.remove("error");
    match (message.remove("method"), message.remove("id")) {
        (Some(Value::String(method)), Some(id)) if !id.is_null() => Incoming::Request {
            id,
            method,
            params: message.remove("params").unwrap_or(Value::Null),
        },
        (None, Some(id)) if id == json!(IDENTIFY_ID) && error.is_some() => {
            let error = error.expect("checked above");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string());
            Incoming::IdentifyError { message }
        }
        _ => Incoming::Ignored,
    }
}

pub fn identify_message(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "server.connection.identify",
        "params": {
            "client_name": "aldis",
            "version": env!("CARGO_PKG_VERSION"),
            "type": "agent",
            "url": "https://github.com/mjonuschat/aldis"
        }
    })
}

pub fn result_message(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_message(id: &Value, error: &ApiError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

pub fn event_message(payload: &UpdateResponse) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "connection.send_event",
        "params": { "event": "update_response", "data": payload }
    })
}

fn parse_update(params: Value) -> Result<UpdateRequest, ApiError> {
    serde_json::from_value(params).map_err(|_| {
        ApiError::new(
            INVALID_PARAMS,
            "invalid_request",
            "expected {\"all\": true} or {\"mcus\": [...]}",
            Value::Null,
        )
    })
}

fn unknown_method(method: &str) -> ApiError {
    ApiError::new(
        METHOD_NOT_FOUND,
        "unknown_method",
        format!("unknown method {method:?}"),
        Value::Null,
    )
}

/// Answers one relayed request. Results are always JSON objects: Moonraker treats a `null`
/// result as an agent error.
pub fn dispatch<B: AgentBackend>(
    service: &AgentService<B>,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    match method {
        "status" => Ok(serde_json::to_value(service.status()).expect("status serializes")),
        "update" => service.update(parse_update(params)?),
        other => Err(unknown_method(other)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn recognizes_relayed_requests_with_or_without_params() {
        match parse_incoming(r#"{"jsonrpc":"2.0","method":"status","id":7}"#) {
            Incoming::Request { id, method, params } => {
                assert_eq!(
                    (id, method.as_str(), params),
                    (json!(7), "status", Value::Null)
                );
            }
            _ => panic!("expected a request"),
        }
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","result":"ok","id":1}"#),
            Incoming::Ignored
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","method":"notify_proc_stat_update","params":[{}]}"#),
            Incoming::Ignored
        ));
        assert!(matches!(parse_incoming("not json"), Incoming::Ignored));
    }

    #[test]
    fn identifies_as_an_agent() {
        let message = identify_message(IDENTIFY_ID);
        assert_eq!(message["method"], "server.connection.identify");
        assert_eq!(message["params"]["client_name"], "aldis");
        assert_eq!(message["params"]["type"], "agent");
        assert_eq!(message["params"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn recognizes_a_rejected_identify_reply() {
        match parse_incoming(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"already registered and connected"}}"#,
        ) {
            Incoming::IdentifyError { message } => {
                assert_eq!(message, "already registered and connected");
            }
            _ => panic!("expected an identify error"),
        }
        // A response to some other call must not be mistaken for the identify error.
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"nope"}}"#),
            Incoming::Ignored
        ));
    }

    #[test]
    fn wraps_results_errors_and_events() {
        assert_eq!(
            result_message(&json!(3), json!({"a": 1})),
            json!({"jsonrpc":"2.0","id":3,"result":{"a":1}})
        );
        let error = ApiError::new(-32000, "busy", "update already running", Value::Null);
        assert_eq!(
            error_message(&json!(3), &error),
            json!({"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"update already running","data":{"reason":"busy"}}})
        );
    }

    #[test]
    fn rejects_unknown_methods_and_malformed_update_arguments() {
        assert_eq!(
            parse_update(json!({"mcus": "mcu"})).unwrap_err().data["reason"],
            "invalid_request"
        );
        assert_eq!(
            parse_update(json!({})).unwrap_err().data["reason"],
            "invalid_request"
        );
        assert_eq!(
            parse_update(Value::Null).unwrap_err().data["reason"],
            "invalid_request"
        );
        assert_eq!(unknown_method("reboot").code, -32601);
    }
}
