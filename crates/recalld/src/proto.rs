//! The wire format: newline-delimited JSON, one message per line, both
//! directions (PROTOCOL.md).
//!
//! Everything here is pure. The transport is `server.rs`, the behaviour is
//! `service.rs`; keeping the encoding separate is what lets the protocol be
//! tested without a socket.

use serde_json::{Value, json};

/// The daemon's identity in the handshake.
pub fn daemon_id() -> String {
    format!("recalld/{}", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Echoed verbatim on the reply. Absent is allowed; the reply then carries
    /// `null`, which a client that did not ask for correlation can ignore.
    pub id: Value,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn param(&self, key: &str) -> Option<&Value> {
        self.params.get(key).filter(|v| !v.is_null())
    }

    pub fn i64(&self, key: &str) -> Result<i64, Error> {
        self.opt_i64(key)?
            .ok_or_else(|| Error::params(format!("{key} is required")))
    }

    pub fn opt_i64(&self, key: &str) -> Result<Option<i64>, Error> {
        match self.param(key) {
            None => Ok(None),
            Some(v) => v
                .as_i64()
                .map(Some)
                .ok_or_else(|| Error::params(format!("{key} must be an integer"))),
        }
    }

    pub fn str(&self, key: &str) -> Result<&str, Error> {
        self.opt_str(key)?
            .ok_or_else(|| Error::params(format!("{key} is required")))
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<&str>, Error> {
        match self.param(key) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(Some)
                .ok_or_else(|| Error::params(format!("{key} must be a string"))),
        }
    }

    pub fn opt_bool(&self, key: &str) -> Result<Option<bool>, Error> {
        match self.param(key) {
            None => Ok(None),
            Some(v) => v
                .as_bool()
                .map(Some)
                .ok_or_else(|| Error::params(format!("{key} must be true or false"))),
        }
    }

    pub fn usize_or(&self, key: &str, default: usize) -> Result<usize, Error> {
        match self.opt_i64(key)? {
            None => Ok(default),
            Some(v) if v >= 0 => Ok(v as usize),
            Some(_) => Err(Error::params(format!("{key} must not be negative"))),
        }
    }
}

/// What arrived on a line.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hello {
        proto: u64,
        client: String,
    },
    Request(Request),
    /// Not JSON, or JSON that is neither a hello nor a request.
    Malformed(String),
}

pub fn parse(line: &str) -> Incoming {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Incoming::Malformed(e.to_string()),
    };
    if let Some(hello) = value.get("hello") {
        let proto = hello.get("proto").and_then(Value::as_u64).unwrap_or(0);
        let client = hello
            .get("client")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        return Incoming::Hello { proto, client };
    }
    match value.get("method").and_then(Value::as_str) {
        Some(method) => Incoming::Request(Request {
            id: value.get("id").cloned().unwrap_or(Value::Null),
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        None => Incoming::Malformed("neither a hello nor a method".into()),
    }
}

/// A failed request. `code` is the stable, matchable half; `msg` is for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub msg: String,
}

impl Error {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
    pub fn params(msg: impl Into<String>) -> Self {
        Self::new("params", msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new("not_found", msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("internal", msg)
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::internal(format!("{e:#}"))
    }
}

fn line(value: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    bytes
}

pub fn welcome(seq: u64, schema: i64) -> Vec<u8> {
    line(json!({"welcome": {
        "proto": crate::bus::PROTO,
        "daemon": daemon_id(),
        "seq": seq,
        "schema": schema,
    }}))
}

/// A connection-fatal error. PROTOCOL.md spells this one `error`, not `err`:
/// it is about the connection, not about a request.
pub fn fatal(code: &str, msg: &str) -> Vec<u8> {
    line(json!({"error": {"code": code, "msg": msg}}))
}

pub fn ok(id: &Value, value: Value) -> Vec<u8> {
    line(json!({"id": id, "ok": value}))
}

pub fn err(id: &Value, e: &Error) -> Vec<u8> {
    line(json!({"id": id, "err": {"code": e.code, "msg": e.msg}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_json(bytes: Vec<u8>) -> Value {
        assert_eq!(bytes.last(), Some(&b'\n'), "every message is one line");
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn the_handshake_is_recognised_with_its_version() {
        let got = parse(r#"{"hello": {"proto": 1, "client": "nx-recall-gui/0.1"}}"#);
        assert_eq!(
            got,
            Incoming::Hello {
                proto: 1,
                client: "nx-recall-gui/0.1".into()
            }
        );
        // A hello without a client still identifies itself as a hello.
        assert!(matches!(
            parse(r#"{"hello":{"proto":2}}"#),
            Incoming::Hello { proto: 2, .. }
        ));
    }

    #[test]
    fn requests_carry_their_id_through_verbatim() {
        let Incoming::Request(req) =
            parse(r#"{"id": 7, "method": "search", "params": {"q": "portal world"}}"#)
        else {
            panic!("expected a request");
        };
        assert_eq!(req.id, json!(7));
        assert_eq!(req.method, "search");
        assert_eq!(req.str("q").unwrap(), "portal world");

        // A string id is as valid as a number; we never interpret it.
        let Incoming::Request(req) = parse(r#"{"id": "a", "method": "status"}"#) else {
            panic!("expected a request");
        };
        assert_eq!(req.id, json!("a"));
        assert_eq!(as_json(ok(&req.id, json!({})))["id"], json!("a"));
    }

    #[test]
    fn a_request_without_an_id_is_answered_with_null() {
        let Incoming::Request(req) = parse(r#"{"method": "status"}"#) else {
            panic!("expected a request");
        };
        assert_eq!(req.id, Value::Null);
        assert_eq!(as_json(ok(&req.id, json!({})))["id"], Value::Null);
    }

    #[test]
    fn garbage_is_reported_rather_than_panicking() {
        assert!(matches!(parse("not json at all"), Incoming::Malformed(_)));
        assert!(matches!(parse("{}"), Incoming::Malformed(_)));
        assert!(matches!(parse("[]"), Incoming::Malformed(_)));
    }

    #[test]
    fn param_helpers_distinguish_missing_from_wrong() {
        let Incoming::Request(req) = parse(
            r#"{"id":1,"method":"m","params":{"n":5,"s":"x","b":true,"nothing":null,"bad":"nope"}}"#,
        ) else {
            panic!("expected a request");
        };
        assert_eq!(req.i64("n").unwrap(), 5);
        assert_eq!(req.opt_i64("nothing").unwrap(), None);
        assert_eq!(req.opt_str("s").unwrap(), Some("x"));
        assert_eq!(req.opt_bool("b").unwrap(), Some(true));
        assert_eq!(req.usize_or("n", 9).unwrap(), 5);
        assert_eq!(req.usize_or("absent", 9).unwrap(), 9);

        assert_eq!(req.i64("bad").unwrap_err().code, "params");
        assert_eq!(req.i64("absent").unwrap_err().code, "params");
        assert_eq!(req.str("n").unwrap_err().code, "params");
    }

    #[test]
    fn the_welcome_states_the_protocol_the_daemon_and_where_the_stream_is() {
        let w = as_json(welcome(41823, 3))["welcome"].clone();
        assert_eq!(w["proto"], 1);
        assert_eq!(w["seq"], 41823);
        assert_eq!(w["schema"], 3);
        assert!(w["daemon"].as_str().unwrap().starts_with("recalld/"));
    }

    #[test]
    fn errors_keep_their_code_matchable() {
        let e = Error::not_found("no speaker with id 12");
        let v = as_json(err(&json!(3), &e));
        assert_eq!(v["id"], 3);
        assert_eq!(v["err"]["code"], "not_found");
        assert_eq!(v["err"]["msg"], "no speaker with id 12");
        // The connection-fatal shape is a different key on purpose.
        assert_eq!(
            as_json(fatal("proto", "unsupported"))["error"]["code"],
            "proto"
        );
    }
}
