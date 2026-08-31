//! A minimal blocking client for the control socket.
//!
//! This is what `recalld pause` talks through (DESIGN §8: the pause surfaces
//! are the tray, the GUI, and this, for scripts). It is deliberately small —
//! the GUI speaks the protocol itself — but it is a real client, so the CLI
//! exercises the same handshake every other client has to get right.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
    pub welcome: Value,
}

impl Client {
    /// Connect and complete the handshake.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).with_context(|| {
            format!(
                "connecting to {} (is the daemon running? `recalld run`)",
                path.display()
            )
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            welcome: Value::Null,
        };
        client.send(&json!({"hello": {
            "proto": crate::bus::PROTO,
            "client": format!("recalld-cli/{}", env!("CARGO_PKG_VERSION")),
        }}))?;
        let reply = client.read()?;
        if let Some(error) = reply.get("error") {
            bail!("the daemon refused the handshake: {error}");
        }
        client.welcome = reply
            .get("welcome")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("expected a welcome, got {reply}"))?;
        Ok(client)
    }

    /// One request, one terminal reply. Events that arrive in between are
    /// skipped: a request/response caller is not subscribed to anything.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            let reply = self.read()?;
            if reply.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(err) = reply.get("err") {
                bail!(
                    "{method} failed ({}): {}",
                    err["code"].as_str().unwrap_or("?"),
                    err["msg"].as_str().unwrap_or("")
                );
            }
            return Ok(reply.get("ok").cloned().unwrap_or(Value::Null));
        }
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        Ok(())
    }

    fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("the daemon closed the connection");
        }
        Ok(serde_json::from_str(&line)?)
    }
}
