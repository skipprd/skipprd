use tokio::sync::broadcast;

use crate::ws::api_gen::src::models as api;

/// Internal pub-sub for runtime events.
///
/// We intentionally emit the same typed messages we send over WS (`api::ServerMessage`),
/// so sinks (WS writer, terminal renderer, headless runner) stay consistent.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<api::ServerMessage>,
}

impl EventHub {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel::<api::ServerMessage>(capacity.max(16));
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<api::ServerMessage> {
        self.tx.subscribe()
    }

    pub fn emit(&self, msg: api::ServerMessage) {
        let _ = self.tx.send(msg);
    }
}
