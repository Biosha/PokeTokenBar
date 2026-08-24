//! Window CSS (see `style.css`). Loaded once at the `Display` level so both the normal
//! app and the `--screenshot` mode pick it up — both paths go through `build_window`.
//!
//! Priority 800 = `GTK_STYLE_PROVIDER_PRIORITY_APPLICATION` (overrides the theme, stays
//! below the user's CSS). gtk4-rs 0.9 dropped the `StyleProviderPriority` enum in favor of
//! the raw `u32`.

pub(crate) fn install() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(&display, &provider, 800);
    }
}
