use std::sync::mpsc;

use crate::tui::event::Event;
use crate::tui::state::CmdId;

pub fn spawn(_id: CmdId, _cmd: &str, _tx: &mpsc::Sender<Event>) {
    todo!("will be wired when first command is needed")
}
