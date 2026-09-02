//! The sockets a room's tests watch it through.
//!
//! Cloudflare's runtime hands a Durable Object hibernating sockets, and what
//! a room *did* to one is the thing the runtime cannot supply back: a
//! broadcast, a targeted forward and a refusal are told apart only by which
//! socket was written to and with what. So the sockets here are flyco's —
//! they record every frame — and everything behind them is skyzen's.
//!
//! Shared by the session room's tests and the host room's, because both ask
//! the same question of the same trait.

use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use skyzen::durable::{
    DurableConnectionsInner, DurableObjectError, WebSocketConnection, WebSocketConnectionInner,
};

/// One thing the room did to a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// A text frame, with the tag of the socket it went to.
    Text {
        /// Role tag of the receiving socket.
        to: String,
        /// The frame.
        text: String,
    },
    /// A close, with the tag of the socket it went to.
    Closed {
        /// Role tag of the receiving socket.
        to: String,
        /// Close code.
        code: u16,
    },
}

/// A fake hibernating socket.
///
/// Sends go out on a channel — append-only, drained at the end of a test,
/// no shared mutable state. The attachment is the one thing that must be
/// *read back* (it is how the room remembers a daemon was greeted), and the
/// `WebSocketConnectionInner` trait takes `&self` and is `Send + Sync`, so
/// interior mutability behind a lock is structurally required rather than
/// chosen.
#[derive(Debug, Clone)]
pub struct FakeSocket {
    pub tags: Vec<String>,
    pub sent: Sender<Sent>,
    pub attachment: Arc<RwLock<Option<Vec<u8>>>>,
}

impl FakeSocket {
    pub fn tag(&self) -> String {
        self.tags
            .first()
            .cloned()
            .unwrap_or_else(|| "untagged".to_owned())
    }
}

impl WebSocketConnectionInner for FakeSocket {
    fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
        self.sent
            .send(Sent::Text {
                to: self.tag(),
                text: text.to_owned(),
            })
            .map_err(|error| DurableObjectError::WebSocket(error.to_string()))
    }

    fn send_binary(&self, _data: &[u8]) -> Result<(), DurableObjectError> {
        Err(DurableObjectError::WebSocket(
            "the flyco relay never sends binary frames".to_owned(),
        ))
    }

    fn close(&self, code: u16, _reason: &str) -> Result<(), DurableObjectError> {
        self.sent
            .send(Sent::Closed {
                to: self.tag(),
                code,
            })
            .map_err(|error| DurableObjectError::WebSocket(error.to_string()))
    }

    fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
        Ok(self.tags.clone())
    }

    fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
        Ok(self.attachment.read().expect("attachment lock").clone())
    }

    fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        *self.attachment.write().expect("attachment lock") = Some(data.to_vec());
        Ok(())
    }
}

/// The sockets attached to one room.
#[derive(Debug, Clone, Default)]
pub struct FakeConnections {
    pub sockets: Vec<FakeSocket>,
}

impl DurableConnectionsInner for FakeConnections {
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(self
            .sockets
            .iter()
            .cloned()
            .map(|socket| WebSocketConnection::new(Box::new(socket)))
            .collect())
    }

    fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(self
            .sockets
            .iter()
            .filter(|socket| socket.tags.iter().any(|value| value == tag))
            .cloned()
            .map(|socket| WebSocketConnection::new(Box::new(socket)))
            .collect())
    }

    fn set_auto_response(&self, _request: &str, _response: &str) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
        Box::new(self.clone())
    }
}
