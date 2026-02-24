use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub running: bool,
    pub tick_count: u64,
    #[serde(default)]
    pub show_debug: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            running: true,
            tick_count: 0,
            show_debug: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Tick,
    Quit,
    ToggleDebug,
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
}
