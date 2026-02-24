pub mod app;
pub mod events;
pub mod ui;

use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::Terminal;

use app::{update, Model};
use events::{CrosstermEventHandler, EventSource};
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
) -> io::Result<()> {
    while model.running {
        terminal
            .draw(|frame| view(model, frame))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let msg = events.next()?;
        let mut chained = update(model, msg);
        while let Some(next_msg) = chained {
            chained = update(model, next_msg);
        }
    }
    Ok(())
}

/// Public entrypoint. Sets up terminal, runs loop, restores terminal.
pub fn run_tui(dev_model: Option<&Path>) -> io::Result<()> {
    let mut model = load_model(dev_model)?;
    let events = CrosstermEventHandler::new(Duration::from_millis(200));
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut model, &events);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::Message;
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

        // run_loop will process Tick, Tick, then Quit sets running=false
        // and the loop exits
        let result = run_loop(&mut terminal, &mut model, &events);
        assert!(result.is_ok());
        assert_eq!(model.tick_count, 2);
        assert!(!model.running);
    }
}
