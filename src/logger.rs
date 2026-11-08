use std::sync::{Arc, Mutex};

use crate::ipc::MainEvent;
use crate::server::EventBus;

/// Replayed to each new WS client; older lines are dropped past this.
const HISTORY_LIMIT: usize = 256 * 1024;

// Not actually "remote" (matching the real app's RemoteLogger.ts naming) —
// just an in-memory buffer new WS clients get replayed on connect, plus a
// live MAIN->CLIENT::log-entry broadcast to whoever's already connected.
// One logger for the whole host, attached to every game's bus: host-level
// errors (portal, input) aren't specific to either game.
#[derive(Default)]
pub struct Logger {
    history: Mutex<String>,
    buses: Mutex<Vec<Arc<EventBus>>>,
}

impl Logger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&self, events: Arc<EventBus>) {
        self.buses.lock().unwrap().push(events);
    }

    pub fn history(&self) -> String {
        self.history.lock().unwrap().clone()
    }

    /// Same line format as RemoteLogger.ts (`[time] message\n`): the
    /// renderer appends each entry to its Log tab verbatim, so the newline
    /// is what separates live entries.
    pub fn write(&self, message: &str) {
        let time = gtk4::glib::DateTime::now_local()
            .and_then(|now| now.format("%X"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        let line = format!("[{time}] {message}\n");
        {
            let mut history = self.history.lock().unwrap();
            history.push_str(&line);
            if history.len() > HISTORY_LIMIT {
                let excess = history.len() - HISTORY_LIMIT;
                // Cut at a line boundary so the replay never starts mid-line.
                let cut = history[excess..].find('\n').map_or(history.len(), |i| excess + i + 1);
                history.drain(..cut);
            }
        }
        let event = MainEvent::LogEntry { message: line };
        let buses = self.buses.lock().unwrap().clone();
        for bus in buses {
            bus.broadcast(&event);
        }
    }

    /// stderr + the in-app Log tab.
    pub fn error(&self, message: &str) {
        eprintln!("{message}");
        self.write(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_capped_at_a_line_boundary() {
        let logger = Logger::new();
        let line = "x".repeat(1000);
        for _ in 0..300 {
            logger.write(&line);
        }
        let history = logger.history();
        assert!(history.len() <= HISTORY_LIMIT);
        assert!(history.starts_with('[') && history.ends_with("x\n"));
        assert_eq!(history.lines().count(), history.matches('\n').count());
    }
}
