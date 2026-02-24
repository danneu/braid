use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DaemonStatus {
    #[default]
    Idle,
    Requesting,
    Ok,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Command {
    #[default]
    None,
    Ping { request_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub running: bool,
    pub tick_count: u64,
    #[serde(default)]
    pub show_debug: bool,
    #[serde(skip)]
    pub daemon_status: DaemonStatus,
    #[serde(skip)]
    pub pending_command: Command,
    #[serde(skip)]
    pub next_request_id: u64,
    #[serde(skip)]
    pub pending_request_id: Option<u64>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            running: true,
            tick_count: 0,
            show_debug: false,
            daemon_status: DaemonStatus::default(),
            pending_command: Command::default(),
            next_request_id: 0,
            pending_request_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Tick,
    Quit,
    ToggleDebug,
    Ping,
    PingResult {
        request_id: u64,
        result: Result<(), String>,
    },
}

pub fn update(model: &mut Model, msg: Message) -> Option<Message> {
    match msg {
        Message::Tick => {
            model.tick_count += 1;
        }
        Message::Quit => {
            model.running = false;
        }
        Message::ToggleDebug => {
            model.show_debug = !model.show_debug;
        }
        Message::Ping => {
            let id = model.next_request_id;
            model.next_request_id += 1;
            model.pending_request_id = Some(id);
            model.daemon_status = DaemonStatus::Requesting;
            model.pending_command = Command::Ping { request_id: id };
        }
        Message::PingResult { request_id, result } => {
            if model.pending_request_id == Some(request_id) {
                model.pending_request_id = None;
                model.daemon_status = match result {
                    Ok(()) => DaemonStatus::Ok,
                    Err(e) => DaemonStatus::Error(e),
                };
            }
            // else: stale response, ignore
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_running() {
        let model = Model::default();
        assert!(model.running);
        assert_eq!(model.tick_count, 0);
        assert!(!model.show_debug);
    }

    #[test]
    fn tick_increments_counter() {
        let mut model = Model::default();
        update(&mut model, Message::Tick);
        update(&mut model, Message::Tick);
        update(&mut model, Message::Tick);
        assert_eq!(model.tick_count, 3);
        assert!(model.running);
    }

    #[test]
    fn quit_stops_running() {
        let mut model = Model::default();
        update(&mut model, Message::Quit);
        assert!(!model.running);
    }

    #[test]
    fn toggle_debug() {
        let mut model = Model::default();
        assert!(!model.show_debug);
        update(&mut model, Message::ToggleDebug);
        assert!(model.show_debug);
        update(&mut model, Message::ToggleDebug);
        assert!(!model.show_debug);
    }

    #[test]
    fn model_round_trips_json() {
        let model = Model::default();
        let json = serde_json::to_string(&model).unwrap();
        let deserialized: Model = serde_json::from_str(&json).unwrap();
        assert_eq!(model, deserialized);
    }

    #[test]
    fn ping_sets_pending_command() {
        let mut model = Model::default();
        update(&mut model, Message::Ping);
        assert_eq!(model.daemon_status, DaemonStatus::Requesting);
        assert_eq!(
            model.pending_command,
            Command::Ping { request_id: 0 }
        );
        assert_eq!(model.pending_request_id, Some(0));
        assert_eq!(model.next_request_id, 1);
    }

    #[test]
    fn ping_result_ok() {
        let mut model = Model::default();
        update(&mut model, Message::Ping);
        update(
            &mut model,
            Message::PingResult {
                request_id: 0,
                result: Ok(()),
            },
        );
        assert_eq!(model.daemon_status, DaemonStatus::Ok);
        assert_eq!(model.pending_request_id, None);
    }

    #[test]
    fn ping_result_error() {
        let mut model = Model::default();
        update(&mut model, Message::Ping);
        update(
            &mut model,
            Message::PingResult {
                request_id: 0,
                result: Err("connection refused".to_string()),
            },
        );
        assert_eq!(
            model.daemon_status,
            DaemonStatus::Error("connection refused".to_string())
        );
        assert_eq!(model.pending_request_id, None);
    }

    #[test]
    fn stale_ping_result_ignored() {
        let mut model = Model::default();
        // Send two pings — second overwrites first's request_id
        update(&mut model, Message::Ping); // id=0
        update(&mut model, Message::Ping); // id=1
        assert_eq!(model.pending_request_id, Some(1));
        assert_eq!(model.daemon_status, DaemonStatus::Requesting);

        // Response to first ping (stale) — should be ignored
        update(
            &mut model,
            Message::PingResult {
                request_id: 0,
                result: Ok(()),
            },
        );
        assert_eq!(model.daemon_status, DaemonStatus::Requesting);
        assert_eq!(model.pending_request_id, Some(1));

        // Response to second ping (current) — should be accepted
        update(
            &mut model,
            Message::PingResult {
                request_id: 1,
                result: Ok(()),
            },
        );
        assert_eq!(model.daemon_status, DaemonStatus::Ok);
        assert_eq!(model.pending_request_id, None);
    }
}
