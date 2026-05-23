//! ratatui-based UI rendering pipeline.
//!
//! This module replaces the hand-rolled stdout paint paths in
//! `crate::ui::renderer` with a ratatui Widget + Buffer model. The
//! migration is staged — old `draw_*` paths in `renderer.rs` keep
//! working while individual regions are ported here. The final
//! integration phase (see beads dirge-eu3) wires the main loop to
//! `terminal.draw(|f| renderer.render(f))` and deletes the legacy
//! paint code.
//!
//! Phase 1 (this commit) introduces `Layout` — the single source of
//! truth for region geometry. Every widget takes a `ratatui::Rect`
//! computed by `Layout::new(cols, rows, input_rows)` so that
//! alignment bugs caused by per-callsite column math become
//! impossible.

// Layout types are consumed by future migration phases (top frame
// widget, chat region, side panels, bottom strip). The dead-code
// warning will go away as those phases land.
#[allow(dead_code)]
pub mod layout;
