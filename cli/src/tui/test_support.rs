//! Shared TUI snapshot helpers so `view` and `browse` tests render and assert
//! through one canonical path, keeping their `.snap` output byte-identical.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Shared so `view` and `browse` snapshots assert through one path; trims
/// trailing per-line whitespace so insta diffs stay clean (styles/colors are
/// dropped -- text only).
pub(crate) fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        let trimmed = out.trim_end();
        out.truncate(trimmed.len());
        out.push('\n');
    }
    out
}

/// Forces `prepend_module_to_snapshot => false` so snapshot files are named
/// after the test alone (`snapshot_with_pool.snap`), not
/// `braid_cli__tui__..__snapshot_with_pool.snap`. Shared so every view test
/// asserts through the same settings; a bare `insta::assert_snapshot!` would
/// reintroduce the prefix.
macro_rules! snap {
    ($value:expr) => {
        insta::with_settings!({ prepend_module_to_snapshot => false }, {
            insta::assert_snapshot!($value);
        });
    };
}
pub(crate) use snap;
