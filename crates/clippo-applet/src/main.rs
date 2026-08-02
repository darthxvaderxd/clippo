//! `clippo-applet` — libcosmic panel applet: search, pins, images and live updates.
//!
//! Placeholder — filled in at M5 (applet); see `docs/ROADMAP.md`.
//!
//! It depends on `clippo-ipc` already, which is where the signatures live: the
//! popup will read `clippo_ipc::ClippoProxy::search` and redraw on
//! `receive_history_changed`, and will not restate a member of either.

fn main() {
    println!("clippo-applet: not implemented yet (arrives in M5 — see docs/ROADMAP.md)");
    println!(
        "It will talk to clippod at {} on the session bus.",
        clippo_ipc::BUS_NAME
    );
}
