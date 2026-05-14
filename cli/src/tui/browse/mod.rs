//! Browse tab state, key routing, and rendering for raw command output.
//!
//! Kept under `tui` because Browse is now one top-level tab in the live
//! dashboard, not an independent command runtime.

pub(crate) mod keymap;
pub(crate) mod state;
pub(crate) mod view;

pub(crate) use state::{BrowseFocus, BrowseState};
