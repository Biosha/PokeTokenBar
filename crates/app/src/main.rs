//! `poketoken-app` — Phase 2: the GTK4/libadwaita window (the macOS menu-bar popover analog).
//!
//! The GUI is behind the `gui` feature (off by default) so `cargo build` / `cargo test` / CI stay
//! green without GTK dev headers. Enable with `--features gui` on a host with `libgtk-4-dev`.

#[cfg(feature = "gui")]
mod app;

#[cfg(feature = "gui")]
mod floating;

#[cfg(feature = "gui")]
mod sni;

#[cfg(feature = "gui")]
mod screenshot;

#[cfg(feature = "gui")]
mod style;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    {
        app::run()
    }
    #[cfg(not(feature = "gui"))]
    {
        eprintln!(
            "poketoken-app {} — GUI not compiled.\n\n\
             Build & run with:  cargo run -p poketoken-app --features gui\n\
             Requires on host:  sudo apt install libgtk-4-dev libadwaita-1-dev",
            env!("CARGO_PKG_VERSION")
        );
        Ok(())
    }
}
