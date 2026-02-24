pub mod app;
pub mod client;
pub mod events;
pub mod ui;

use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::Terminal;

use app::{update, Command, Message, Model};
use client::{daemon_worker, DaemonClient};
use events::{CrosstermEventHandler, EventReceiver, EventSource};
use ui::view;

/// Load model from JSON file (dev mode) or create default.
/// When loading from file, auto-enables debug panel.
pub fn load_model(dev_model: Option<&Path>) -> io::Result<Model> {
    match dev_model {
        None => Ok(Model::default()),
        Some(path) => {
            let contents = std::fs::read_to_string(path)?;
            let mut model: Model = serde_json::from_str(&contents)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            model.show_debug = true;
            Ok(model)
        }
    }
}

/// Main event loop. Generic over backend for testability.
pub fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    model: &mut Model,
    events: &dyn EventSource,
    commands: &tokio::sync::mpsc::Sender<Command>,
) -> io::Result<()> {
    // Auto-fetch status on startup
    update(model, Message::FetchStatus);
    send_pending_command(model, commands);

    while model.running {
        terminal
            .draw(|frame| view(model, frame))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let msg = events.next()?;
        let mut chained = update(model, msg);
        while let Some(next_msg) = chained {
            chained = update(model, next_msg);
        }

        send_pending_command(model, commands);
    }
    Ok(())
}

fn send_pending_command(model: &mut Model, commands: &tokio::sync::mpsc::Sender<Command>) {
    let cmd = std::mem::take(&mut model.pending_command);
    if cmd != Command::None {
        let is_status = matches!(cmd, Command::FetchStatus { .. });
        if let Err(e) = commands.try_send(cmd) {
            if let Some(id) = model.pending_request_id.take() {
                let err_msg = format!("command channel: {e}");
                if is_status {
                    update(
                        model,
                        Message::StatusResult {
                            request_id: id,
                            result: Err(err_msg),
                        },
                    );
                } else {
                    update(
                        model,
                        Message::PingResult {
                            request_id: id,
                            result: Err(err_msg),
                        },
                    );
                }
            }
        }
    }
}

/// Public entrypoint. Sets up terminal, runs loop, restores terminal.
pub fn run_tui(dev_model: Option<&Path>, socket_path: &Path) -> io::Result<()> {
    let mut model = load_model(dev_model)?;

    let (msg_tx, msg_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);

    let _keyboard = CrosstermEventHandler::new(Duration::from_millis(200), msg_tx.clone());

    let client = DaemonClient::new(socket_path.to_owned(), Duration::from_secs(2));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    rt.spawn(daemon_worker(client, cmd_rx, msg_tx));

    let events = EventReceiver::new(msg_rx);
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::{DaemonStatus, Message};
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_model_none_uses_default() {
        let model = load_model(None).unwrap();
        assert_eq!(model, Model::default());
    }

    #[test]
    fn load_model_dev_valid_json() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, r#"{{"running":true,"tick_count":99}}"#).unwrap();
        let model = load_model(Some(file.path())).unwrap();
        assert_eq!(model.tick_count, 99);
        assert!(model.show_debug);
    }

    #[test]
    fn load_model_dev_missing_file() {
        let result = load_model(Some(Path::new("/nonexistent")));
        assert!(result.is_err());
    }

    #[test]
    fn load_model_dev_invalid_json() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "not json").unwrap();
        let result = load_model(Some(file.path()));
        assert!(result.is_err());
    }

    struct TestEventSource {
        messages: RefCell<VecDeque<Message>>,
    }

    impl TestEventSource {
        fn new(msgs: Vec<Message>) -> Self {
            Self {
                messages: RefCell::new(msgs.into()),
            }
        }
    }

    impl EventSource for TestEventSource {
        fn next(&self) -> io::Result<Message> {
            self.messages
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no more messages"))
        }
    }

    #[test]
    fn run_loop_processes_messages() {
        let events = TestEventSource::new(vec![
            Message::Tick,
            Message::Tick,
            Message::Quit,
        ]);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut model = Model::default();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);

        let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
        assert!(result.is_ok());
        assert_eq!(model.tick_count, 2);
        assert!(!model.running);
    }

    #[test]
    fn run_loop_auto_fetch_on_startup() {
        // Just Quit — but auto-fetch should have sent a FetchStatus command
        let events = TestEventSource::new(vec![Message::Quit]);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut model = Model::default();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);

        let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
        assert!(result.is_ok());

        // The first command should be FetchStatus from auto-fetch
        let cmd = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd, Command::FetchStatus { request_id: 0 });
    }

    #[test]
    fn run_loop_status_sends_command() {
        let events = TestEventSource::new(vec![
            Message::FetchStatus,
            Message::StatusResult {
                request_id: 1,
                result: Ok(crate::status::StatusReport {
                    schema_version: 1,
                    mount_point: "/mnt/storage".to_owned(),
                    status_code: crate::status::StatusCode::Healthy,
                    status: "healthy".to_owned(),
                    total_devices: Some(2),
                    present_count: Some(2),
                    missing_count: Some(0),
                    profile: Some("RAID1".to_owned()),
                    capacity: None,
                    last_scrub: None,
                    disks: vec![],
                }),
            },
            Message::Quit,
        ]);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut model = Model::default();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);

        let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
        assert!(result.is_ok());

        // First command: auto-fetch (id=0)
        let cmd0 = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd0, Command::FetchStatus { request_id: 0 });

        // Second command: user-triggered FetchStatus (id=1)
        let cmd1 = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd1, Command::FetchStatus { request_id: 1 });

        assert_eq!(model.daemon_status, DaemonStatus::Ok);
        assert!(model.status_report.is_some());
    }

    #[test]
    fn run_loop_ping_sends_command() {
        let events = TestEventSource::new(vec![
            Message::Ping,
            Message::PingResult {
                request_id: 1, // id=0 is auto-fetch
                result: Ok(()),
            },
            Message::Quit,
        ]);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut model = Model::default();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);

        let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
        assert!(result.is_ok());

        // First command: auto-fetch (id=0)
        let cmd0 = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd0, Command::FetchStatus { request_id: 0 });

        // Second command: user ping (id=1)
        let cmd1 = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd1, Command::Ping { request_id: 1 });

        assert_eq!(model.daemon_status, DaemonStatus::Ok);
    }

    #[test]
    fn run_loop_ping_error() {
        let events = TestEventSource::new(vec![
            Message::Ping,
            Message::PingResult {
                request_id: 1, // id=0 is auto-fetch
                result: Err("connection refused".to_string()),
            },
            Message::Quit,
        ]);
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut model = Model::default();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);

        let result = run_loop(&mut terminal, &mut model, &events, &cmd_tx);
        assert!(result.is_ok());

        assert_eq!(
            model.daemon_status,
            DaemonStatus::Error("connection refused".to_string())
        );
    }
}
