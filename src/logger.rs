use std::sync::{Arc, Mutex};

use crate::ipc::MainEvent;
use crate::server::EventBus;

/// Replayed to each new WS client; older lines are dropped past this.
const HISTORY_LIMIT: usize = 256 * 1024;

// Not actually "remote" (matching the real app's RemoteLogger.ts naming) —
// just an in-memory buffer new WS clients get replayed on connect, plus a
// live MAIN->CLIENT::log-entry broadcast to whoever's already connected.
pub struct Logger {
    history: Mutex<String>,
    events: Arc<EventBus>,
}

impl Logger {
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            history: Mutex::new(String::new()),
            events,
        }
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
        self.events.broadcast(&MainEvent::LogEntry { message: line });
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
        let logger = Logger::new(Arc::new(EventBus::new().0));
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
