//! Hidden `--screenshot <DIR>` self-test: render one PNG per page (Home / Shop / Bag /
//! Collection / Settings) and exit.
//!
//! It is strictly read-only — [`crate::app::refresh`] is called with `write = false`, so the
//! day-counter mutation (`day_delta` → `add_tokens` → `save`) is skipped and the real state
//! file is never touched. It runs under a dedicated **non-unique** application id so it can be
//! invoked while the normal single-instance window is active, and it starts no tray or
//! recurring timers: a single main-loop timer drives a small settle-then-capture state machine.
//!
//! Capture path (GTK 4.14, no `gtk_snapshot_replay`): `widget.snapshot` → `Snapshot::to_node`
//! → `gsk::RenderNode::draw` onto a cairo `ImageSurface` → `write_to_png`. If GTK refuses a
//! snapshot because a layout pass is still pending (no *current* allocation for the frame),
//! the capture is retried a few ticks later before the page is reported as failed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

/// Dedicated id for the self-test instance: non-unique, so it never clashes with (or is
/// forwarded to) a running normal instance.
const APP_ID: &str = "io.github.poketoken.screenshot";

/// Window size used for the captures (popover-like proportions, matching the macOS panel).
const WIN_W: i32 = 380;
const WIN_H: i32 = 660;

/// The pages, in capture order.
const PAGES: [&str; 5] = [
    crate::app::PAGE_HOME,
    crate::app::PAGE_SHOP,
    crate::app::PAGE_BAG,
    crate::app::PAGE_COLLECTION,
    crate::app::PAGE_SETTINGS,
];

/// Upper bound to wait for the sprite worker (cached on disk for the real state, so this is a
/// backstop for the no-cache / offline case — the emoji fallback is captured instead).
const SPRITE_DEADLINE: Duration = Duration::from_secs(12);
/// Minimum time the settled window is shown before the first capture (layout + image decode).
const SETTLE_GRACE: Duration = Duration::from_millis(500);

/// Build the window, do one read-only refresh, and drive the capture state machine on the
/// main loop. Returns once every page has been written (or the deadline forces a stop).
pub fn run(out_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let out_dir = out_dir.to_path_buf();
    app.connect_activate(move |app| {
        let ui = crate::app::build_window(app);
        ui.win.set_default_size(WIN_W, WIN_H);
        ui.win.present();
        // No slide animation: captures must not catch a page mid-transition.
        crate::app::disable_transitions(&ui);

        // Populate every page (read-only) and kick off the sprite fetch.
        if let Err(e) = crate::app::refresh(&ui, false) {
            eprintln!("[poketoken-screenshot] initial refresh failed: {e:#}");
        }

        let ui = ui.clone();
        let app2 = app.clone();
        let out2 = out_dir.clone();
        let started = Instant::now();
        let mut settled = false;
        let mut armed = false;
        let mut page = 0usize;
        let mut retries = 0usize;

        glib::timeout_add_local(Duration::from_millis(80), move || {
            // Apply any finished sprite load so the picture settles before we capture.
            if let Some(res) = ui.sprite_queue.lock().unwrap().take() {
                crate::app::drain_sprite(&ui, res);
            }

            let elapsed = started.elapsed();
            if !settled {
                let mapped = ui.win.allocated_width() > 0 && ui.win.allocated_height() > 0;
                let sprite_ok = ui.sprite.is_visible() || ui.emoji.is_visible();
                if (mapped && sprite_ok && elapsed > SETTLE_GRACE) || elapsed > SPRITE_DEADLINE {
                    settled = true;
                    crate::app::show_page(&ui, crate::app::PAGE_HOME);
                    armed = false;
                    return glib::ControlFlow::Continue;
                }
                return glib::ControlFlow::Continue;
            }

            // First tick after switching to a page: let the layout settle, capture next tick.
            if !armed {
                armed = true;
                return glib::ControlFlow::Continue;
            }

            let name = PAGES[page];
            let path = out2.join(format!("{name}.png"));
            match capture_content(&ui.root, &path) {
                Ok(()) => eprintln!("[poketoken-screenshot] wrote {}", path.display()),
                // A layout pass pending on another timer tick (e.g. the sprite frame just
                // applied, or a stack page switch) can leave the tree without a *current*
                // allocation for one frame — GTK then refuses the snapshot. Retry a few
                // ticks before giving up.
                Err(e) if retries < 6 => {
                    retries += 1;
                    eprintln!("[poketoken-screenshot] {name}: retry {retries} ({e:#})");
                    return glib::ControlFlow::Continue;
                }
                Err(e) => eprintln!("[poketoken-screenshot] failed {name}: {e:#}"),
            }
            retries = 0;
            page += 1;
            if page >= PAGES.len() {
                app2.quit();
                return glib::ControlFlow::Break;
            }
            crate::app::show_page(&ui, PAGES[page]);
            armed = false;
            glib::ControlFlow::Continue
        });
    });

    // GApplication would otherwise parse the raw process args and reject `--screenshot` (it is
    // our own hidden flag, not a GApplication option). Feed it only the program name.
    let _ = app.run_with_args(&["poketoken-app"]);
    Ok(())
}

/// Render the content (`root`) to a `gsk` render node, replay it onto a cairo surface, and write
/// a PNG.
///
/// gtk4-rs 0.9 does not expose `gtk_widget_snapshot`, so we snapshot via `snapshot_child`, which
/// asserts `child.parent() == parent`. `adw::ApplicationWindow` wraps its content in an internal
/// container, so `root`'s real parent is not the window — we snapshot through `root.parent()`
/// instead. `root` fills that container, so it sits at the origin and needs no re-translation.
fn capture_content(root: &gtk::Box, path: &PathBuf) -> anyhow::Result<()> {
    let w = root.allocated_width();
    let h = root.allocated_height();
    if w <= 0 || h <= 0 {
        anyhow::bail!("content not allocated yet ({}x{})", w, h);
    }
    let parent = root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("content has no parent"))?;

    let snap = gtk::Snapshot::new();
    parent.snapshot_child(root, &snap);
    let node = snap
        .to_node()
        .ok_or_else(|| anyhow::anyhow!("snapshot produced no render node"))?;

    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h)?;
    let cr = cairo::Context::new(&surface)?;
    node.draw(&cr);
    surface.flush();
    let mut file = std::fs::File::create(path)?;
    surface.write_to_png(&mut file)?;
    Ok(())
}
