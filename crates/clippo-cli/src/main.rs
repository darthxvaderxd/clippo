//! `clippo` — thin D-Bus client over `com.nilfactor.Clippo`:
//! `list|search|copy|pin|rm|clear|pause|reveal`.
//!
//! Placeholder. The daemon and the interface it will call are in; the commands
//! are not. See `docs/ROADMAP.md`.
//!
//! It depends on `clippo-ipc` already, which is where the signatures live:
//! `clippo_ipc::ClippoProxy` is the entire client, and this crate will not
//! restate a single member of it.

fn main() {
    println!("clippo: not implemented yet (arrives with the CLI — see docs/ROADMAP.md).");
    println!();
    println!(
        "clippod serves {} at {}. Until this binary exists, busctl will do:",
        clippo_ipc::BUS_NAME,
        clippo_ipc::OBJECT_PATH
    );
    println!(
        "  busctl --user call {} {} {} List uu 10 0",
        clippo_ipc::BUS_NAME,
        clippo_ipc::OBJECT_PATH,
        clippo_ipc::INTERFACE_NAME
    );
}
