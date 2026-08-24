//! Phase 2 GUI — the macOS menu-bar popover ported to a libadwaita (GTK4) window.
//!
//! Tabs: Home / Shop / Bag / Collection plus Settings as an in-window page (gear button in the
//! header bar — the macOS popover swaps to a settings *page* inside the same surface, not a
//! separate dialog). Native GNOME pattern: `adw::ApplicationWindow` + `adw::HeaderBar` + a
//! linked pill `ToggleButton` row driving a `gtk::Stack`. `adw::ViewStackSwitcher` is not used
//! because Settings must be reachable from the header without appearing as a fifth tab.
//!
//! Every user-facing string comes from the core i18n `L` table, resolved through
//! `i18n::resolve_language` on every refresh, so a language change (persisted to
//! `state.language`) re-renders the whole window on the next tick.
//!
//! Image slot: `Ui.emoji` (the offline/loading fallback) and `Ui.sprite` (a `gtk::Image`
//! showing the PokéAPI **animated GIF** — the gen-V Black-White sprite, shiny variant when
//! the companion is shiny). [`maybe_load_sprite`] fetches + decodes on a worker thread
//! (disk-cached, slug-keyed) and hands the RGBA frames through an `Arc<Mutex<..>>` that a
//! main-thread timer drains; a second timer advances the frames (as `gdk_pixbuf::Pixbuf`s)
//! at their GIF delays. GTK4 has no animated-image widget, so frames are swapped manually.
//!
//! Limits (Claude 5h/weekly + Codex windows) are fetched by a dedicated worker thread
//! ([`spawn_limits`], 60s cadence, no blocking on the GTK loop) and published the same way;
//! missing credentials/binaries degrade to a "Not available" line.
//!
//! Tray: `run()` also starts a same-process SNI (StatusNotifierItem) worker — pure Rust `zbus`
//! on a background thread — whose clicks come back as [`sni::TrayCommand`]s drained by a
//! main-thread timer. See `sni.rs`.

use poketoken_core::companion::{
    self, CandyUseResult, CompanionEvent, DisplayInput, FreshEgg, ItemKind, RareCandy, Rarity,
    ShopEntry, StateKind,
};
use poketoken_core::config::Config;
use poketoken_core::i18n::{compact_tokens, resolve_language, Language, L};
use poketoken_core::limits::{CodexRateLimitStatus, LimitStatus};
use poketoken_core::windows::local_day;
use poketoken_core::{build_snapshot, pool, ProviderCtx, UsageSnapshot};

use crate::sni;
use crate::style;
use adw::prelude::*;
use gtk4 as gtk;
use gtk::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_SECS: u64 = 15;
/// Official-limits poll cadence (Claude OAuth + Codex app-server) on the worker thread.
const LIMITS_REFRESH_SECS: u64 = 60;
/// The hatch/evolve/graduate "levelUp" display window (macOS uses 4–6s).
const CELEBRATION_WINDOW: Duration = Duration::from_secs(6);
/// Limit-utilization thresholds for coloring + the display-state "tired" rule (macOS defaults).
const LIMIT_WARN_PCT: f64 = 80.0;
const LIMIT_CRIT_PCT: f64 = 95.0;

pub(crate) const PAGE_HOME: &str = "home";
pub(crate) const PAGE_SHOP: &str = "shop";
pub(crate) const PAGE_BAG: &str = "bag";
pub(crate) const PAGE_COLLECTION: &str = "collection";
pub(crate) const PAGE_SETTINGS: &str = "settings";

/// The four tabbed pages (Settings is a separate, header-reached page).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    Shop,
    Bag,
    Collection,
}

impl Tab {
    const fn name(self) -> &'static str {
        match self {
            Tab::Home => PAGE_HOME,
            Tab::Shop => PAGE_SHOP,
            Tab::Bag => PAGE_BAG,
            Tab::Collection => PAGE_COLLECTION,
        }
    }
}

/// A finished sprite-load, handed from the fetch worker thread to the main thread. Only
/// `Send` data (no GTK widget) so it can cross the thread boundary via an `Arc<Mutex<..>>`.
#[derive(Clone)]
pub(crate) struct SpriteResult {
    pub(crate) name: String,
    pub(crate) shiny: bool,
    pub(crate) frames: Option<Vec<poketoken_core::sprite::SpriteFrame>>,
}

/// The sprite animation on the main thread: decoded frames as `Pixbuf`s plus the display
/// position. Not `Send` — GTK-affine, touched only by main-thread timers. Shared between
/// the main window and the floating pet (each keeps its own instance).
#[derive(Clone)]
pub(crate) struct SpriteAnim {
    pub(crate) frames: Vec<gdk_pixbuf::Pixbuf>,
    pub(crate) delays_ms: Vec<u32>,
    pub(crate) index: usize,
    pub(crate) due: Instant,
}

/// A finished limits poll, handed from the limits worker thread to the main thread.
/// `claude` is `Err` when no usable credentials / the HTTP call failed; `codex` is
/// `None` on fetch failure, `Some(None)` when no Codex binary exists (limits hidden, not an
/// error — the core semantics).
#[derive(Clone)]
struct LimitsData {
    claude: Result<LimitStatus, String>,
    codex: Option<Option<CodexRateLimitStatus>>,
}

/// The live celebration (hatch/evolve/graduate/reveal) with its display-window deadline.
#[derive(Clone)]
struct Celebration {
    event: CompanionEvent,
    until: Instant,
}

#[derive(Clone)]
pub(crate) struct Ui {
    pub(crate) win: adw::ApplicationWindow,
    app: adw::Application,
    pub(crate) root: gtk::Box,
    stack: gtk::Stack,
    tab_home: gtk::ToggleButton,
    tab_shop: gtk::ToggleButton,
    tab_bag: gtk::ToggleButton,
    tab_collection: gtk::ToggleButton,
    gear_btn: gtk::Button,
    quit_btn: gtk::Button,
    // home
    pub(crate) emoji: gtk::Label,
    pub(crate) sprite: gtk::Image,
    name: gtk::Label,
    shiny: gtk::Label,
    rarity_badge: gtk::Label,
    egg_guarantee: gtk::Label,
    stage: gtk::Label,
    bar: gtk::ProgressBar,
    sub: gtk::Label,
    status: gtk::Label,
    graduated: gtk::Label,
    row_today: adw::ActionRow,
    row_week: adw::ActionRow,
    row_month: adw::ActionRow,
    row_burn: adw::ActionRow,
    val_today: gtk::Label,
    val_week: gtk::Label,
    val_month: gtk::Label,
    val_burn: gtk::Label,
    limits_box: gtk::Box,
    providers_box: gtk::Box,
    // shop
    wallet: gtk::Label,
    wallet_caption: gtk::Label,
    shop_hint: gtk::Label,
    shop_cards: gtk::Box,
    // bag
    bag_cards: gtk::Box,
    // collection
    seg_dex: gtk::ToggleButton,
    seg_log: gtk::ToggleButton,
    collection_box: gtk::Box,
    // settings
    back_btn: gtk::Button,
    settings_title: gtk::Label,
    lang_label: gtk::Label,
    weekday_label: gtk::Label,
    // `ComboBoxText` is deprecated in GTK 4.10 (hence the allows below: enabling the
    // `v4_10` feature for the FileDialog switches on the deprecation attrs) — but its
    // 4.10 replacement, `StringCombo`, is not in this environment's gtk4 bindings.
    #[allow(deprecated)]
    lang_combo: gtk::ComboBoxText,
    #[allow(deprecated)]
    weekday_combo: gtk::ComboBoxText,
    // worker→main hand-offs
    sprite_for: Arc<Mutex<String>>,
    pub(crate) sprite_queue: Arc<Mutex<Option<SpriteResult>>>,
    /// Current animated-sprite state (main-thread only; advanced by the frame timer).
    sprite_anim: Rc<Mutex<Option<SpriteAnim>>>,
    limits_queue: Arc<Mutex<Option<LimitsData>>>,
    limits_dirty: Arc<AtomicBool>,
    celebration: Arc<Mutex<Option<Celebration>>>,
    /// The floating desktop pet (always built; `floating::sync` shows/hides it per config).
    pub(crate) floating: crate::floating::FloatingPet,
    // settings — launch at login + floating pet rows
    autostart_switch: adw::SwitchRow,
    pet_switch: adw::SwitchRow,
    pet_size_row: adw::ActionRow,
    // settings — save transfer
    export_btn: gtk::Button,
    import_btn: gtk::Button,
    save_hint: gtk::Label,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--screenshot") {
        let dir = args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("--screenshot <DIR> requires a directory argument"))?;
        return crate::screenshot::run(std::path::Path::new(&dir));
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    let app = adw::Application::builder()
        .application_id(
            std::env::var("PTB_APP_ID").unwrap_or_else(|_| "io.github.poketoken.app".into()),
        )
        .flags(gio::ApplicationFlags::default())
        .build();

    // SNI tray: the D-Bus worker thread (pure Rust `zbus`) publishes commands through this
    // channel; the main-thread drain applies them. A missing session bus or tray host just
    // means the thread exits quietly — the window still runs.
    let tray_queue = Arc::new(Mutex::new(None));
    sni::spawn(tray_queue.clone());

    // Single-instance guard (port of the macOS `SingleInstance`): the GApplication id is a
    // D-Bus unique name, so a second launch forwards `activate` to this instance over the
    // session bus and exits. The first instance must not build a second window on that
    // re-activation — it just presents the existing one (the macOS "later instance yields"
    // semantics, realized by the activation hand-off). The window lives in application
    // data: GTK widgets are !Send, so a process-wide static would not type-check.
    // `activate` always runs on the main thread (GApplication dispatch), so a thread-local
    // slot is the safe home for the !Send window handle — no static, no raw pointers.
    app.connect_activate(move |app| {
        MAIN_WINDOW.with(|slot| {
            if let Some(existing) = slot.borrow().as_ref() {
                existing.present();
                return;
            }
            let ui = build_window(app);
            *slot.borrow_mut() = Some(ui.win.clone());
            // One limits poller per instance (Claude OAuth + Codex app-server, off the GTK loop).
            spawn_limits(ui.limits_queue.clone(), ui.limits_dirty.clone());
            start_timers(ui, tray_queue.clone());
        });
    });
    let _ = app.run();
    Ok(())
}

thread_local! {
    static MAIN_WINDOW: RefCell<Option<adw::ApplicationWindow>> = const { RefCell::new(None) };
}

fn print_help() {
    println!(
        "poketoken-app — the GNOME window for your token-companion Pokémon.\n\
         \n\
         Usage: poketoken-app [OPTIONS]\n\
         \n\
         Options:\n\
         \x20 --screenshot <DIR>  (hidden/debug) render one PNG per tab into DIR and exit.\n\
         \x20                     Read-only: loads the real state without saving, no tray,\n\
         \x20                     no timers, no mutations. Safe to run while the normal\n\
         \x20                     instance is active (uses a dedicated non-unique app id).\n\
         \x20 -h, --help          show this help\n\
         \n\
         Environment:\n\
         \x20 PTB_APP_ID     bus name for single-instance forwarding\n\
         \x20 PTB_STATE_DIR  companion state directory override (tests/diagnostics)\n\
         \x20 PTB_LANG       UI language override (en|ko|ja|es)"
    );
}

// ---------------------------------------------------------------------------
// Window + page construction
// ---------------------------------------------------------------------------

pub(crate) fn build_window(app: &adw::Application) -> Ui {
    style::install();

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("PokeTokenBar")
        .default_width(380)
        .default_height(700)
        .build();

    // The floating pet exists for the whole app lifetime (hidden until enabled). Its
    // "Hide" action needs a `Ui` handle, which does not exist yet — a slot filled in
    // right after the `Ui` literal below.
    let ui_ref: Rc<RefCell<Option<Ui>>> = Rc::new(RefCell::new(None));
    let floating_pet = crate::floating::FloatingPet::build(app, ui_ref.clone());

    let root = vbox(0);

    // Header bar: gear → the in-window settings page; Quit mirrors the macOS footer power button.
    let header = adw::HeaderBar::new();
    let gear_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
    gear_btn.set_tooltip_text(Some("Settings"));
    header.pack_end(&gear_btn);
    let quit_btn = gtk::Button::with_label("Quit");
    header.pack_end(&quit_btn);
    root.append(&header);

    // Tab bar — the GNOME segment pattern: a linked row of pill toggle buttons.
    let tabs_bar = hbox(0);
    tabs_bar.add_css_class("linked");
    tabs_bar.set_margin_top(10);
    tabs_bar.set_margin_bottom(10);
    tabs_bar.set_margin_start(14);
    tabs_bar.set_margin_end(14);
    let tab_home = tab_button("");
    let tab_shop = tab_button("");
    let tab_bag = tab_button("");
    let tab_collection = tab_button("");
    for b in [&tab_home, &tab_shop, &tab_bag, &tab_collection] {
        tabs_bar.append(b);
    }
    root.append(&tabs_bar);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);

    // ---- Home ----
    let home_scroll = scrolled();
    let home = vbox(10);
    home.set_margin_top(14);
    home.set_margin_bottom(14);
    home.set_margin_start(14);
    home.set_margin_end(14);

    let header_row = hbox(12);
    let image_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    image_slot.set_halign(gtk::Align::Center);
    image_slot.add_css_class("ptb-sprite-tile");
    let emoji = gtk::Label::new(Some("🥚"));
    emoji.set_halign(gtk::Align::Center);
    emoji.add_css_class("title-2");
    let sprite = gtk::Image::new();
    sprite.set_halign(gtk::Align::Center);
    sprite.set_pixel_size(112);
    sprite.set_size_request(112, 112);
    sprite.set_visible(false);
    image_slot.append(&sprite);
    image_slot.append(&emoji);
    header_row.append(&image_slot);

    let identity_col = vbox(4);
    identity_col.set_hexpand(true);
    let identity_row = hbox(6);
    let name = gtk::Label::new(Some("…"));
    name.set_halign(gtk::Align::Start);
    semibold(&name);
    name.add_css_class("title-4");
    identity_row.append(&name);
    let shiny = gtk::Label::new(Some("✨"));
    shiny.set_visible(false);
    identity_row.append(&shiny);
    let rarity_badge = gtk::Label::new(Some(""));
    rarity_badge.add_css_class("ptb-badge");
    rarity_badge.set_visible(false);
    identity_row.append(&rarity_badge);
    identity_col.append(&identity_row);
    let stage = gtk::Label::new(Some(""));
    stage.set_halign(gtk::Align::Start);
    stage.add_css_class("caption");
    stage.add_css_class("dim-label");
    identity_col.append(&stage);
    let egg_guarantee = gtk::Label::new(Some(""));
    egg_guarantee.add_css_class("ptb-badge");
    egg_guarantee.set_visible(false);
    identity_col.append(&egg_guarantee);
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ptb-xp");
    identity_col.append(&bar);
    let sub = gtk::Label::new(Some(""));
    sub.set_halign(gtk::Align::Start);
    sub.add_css_class("caption");
    sub.add_css_class("dim-label");
    identity_col.append(&sub);
    let status = gtk::Label::new(Some(""));
    status.set_halign(gtk::Align::Start);
    status.add_css_class("caption");
    status.add_css_class("dim-label");
    status.set_wrap(true);
    identity_col.append(&status);
    let graduated = gtk::Label::new(Some(""));
    graduated.set_halign(gtk::Align::Start);
    graduated.add_css_class("caption");
    graduated.add_css_class("warning");
    graduated.set_visible(false);
    graduated.set_wrap(true);
    identity_col.append(&graduated);
    header_row.append(&identity_col);
    home.append(&header_row);

    home.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let val_today = stat_value_label();
    let val_week = stat_value_label();
    let val_month = stat_value_label();
    let val_burn = stat_value_label();
    let row_today = stat_row("Today", &val_today);
    let row_week = stat_row("Week", &val_week);
    let row_month = stat_row("Month", &val_month);
    let row_burn = stat_row("Burn", &val_burn);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rows.add_css_class("boxed-list");
    rows.append(&row_today);
    rows.append(&row_week);
    rows.append(&row_month);
    rows.append(&row_burn);
    home.append(&rows);

    home.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let limits_box = vbox(6);
    home.append(&limits_box);
    let providers_box = vbox(6);
    home.append(&providers_box);

    home_scroll.set_child(Some(&home));
    stack.add_titled(&home_scroll, Some(PAGE_HOME), "Home");

    // ---- Shop ----
    let shop_scroll = scrolled();
    let shop = vbox(10);
    shop.set_margin_top(14);
    shop.set_margin_bottom(14);
    shop.set_margin_start(14);
    shop.set_margin_end(14);
    let wallet_card = card();
    let wallet_col = vbox(2);
    wallet_col.set_margin_top(10);
    wallet_col.set_margin_bottom(10);
    wallet_col.set_margin_start(10);
    wallet_col.set_margin_end(10);
    let wallet_caption = caption_dim("Spendable tokens");
    wallet_col.append(&wallet_caption);
    let wallet = gtk::Label::new(Some("0"));
    wallet.set_halign(gtk::Align::Start);
    wallet.add_css_class("ptb-wallet");
    wallet.add_css_class("monospace");
    wallet_col.append(&wallet);
    let shop_hint = caption_dim2("Spend the tokens you've used on items.");
    wallet_col.append(&shop_hint);
    wallet_card.append(&wallet_col);
    shop.append(&wallet_card);
    let shop_cards = vbox(10);
    shop.append(&shop_cards);
    shop_scroll.set_child(Some(&shop));
    stack.add_titled(&shop_scroll, Some(PAGE_SHOP), "Shop");

    // ---- Bag ----
    let bag_scroll = scrolled();
    let bag = vbox(10);
    bag.set_margin_top(14);
    bag.set_margin_bottom(14);
    bag.set_margin_start(14);
    bag.set_margin_end(14);
    let bag_cards = vbox(10);
    bag.append(&bag_cards);
    bag_scroll.set_child(Some(&bag));
    stack.add_titled(&bag_scroll, Some(PAGE_BAG), "Bag");

    // ---- Collection ----
    let collection_scroll = scrolled();
    let collection = vbox(10);
    collection.set_margin_top(14);
    collection.set_margin_bottom(14);
    collection.set_margin_start(14);
    collection.set_margin_end(14);
    let seg_bar = hbox(0);
    seg_bar.add_css_class("linked");
    let seg_dex = tab_button("");
    let seg_log = tab_button("");
    seg_bar.append(&seg_dex);
    seg_bar.append(&seg_log);
    collection.append(&seg_bar);
    let collection_box = vbox(8);
    collection_box.set_vexpand(true);
    collection.append(&collection_box);
    collection_scroll.set_child(Some(&collection));
    stack.add_titled(&collection_scroll, Some(PAGE_COLLECTION), "Collection");

    // ---- Settings (in-window page, reached via the gear) ----
    let settings_scroll = scrolled();
    let settings = vbox(0);
    let s_header = hbox(6);
    s_header.set_margin_top(10);
    s_header.set_margin_bottom(10);
    s_header.set_margin_start(14);
    s_header.set_margin_end(14);
    let back_btn = gtk::Button::with_label("‹ Back");
    back_btn.add_css_class("flat");
    back_btn.add_css_class("suggested-action");
    s_header.append(&back_btn);
    let s_spacer1 = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    s_spacer1.set_hexpand(true);
    s_header.append(&s_spacer1);
    let settings_title = gtk::Label::new(Some("Settings"));
    semibold(&settings_title);
    settings_title.add_css_class("title-4");
    s_header.append(&settings_title);
    let s_spacer2 = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    s_spacer2.set_hexpand(true);
    s_header.append(&s_spacer2);
    settings.append(&s_header);
    let s_body = vbox(14);
    s_body.set_margin_top(14);
    s_body.set_margin_bottom(14);
    s_body.set_margin_start(14);
    s_body.set_margin_end(14);
    let general_card = card();
    let general_col = vbox(0);
    let lang_row = hbox(10);
    let lang_label = gtk::Label::new(Some("Language"));
    lang_label.set_halign(gtk::Align::Start);
    lang_row.append(&lang_label);
    let lang_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    lang_spacer.set_hexpand(true);
    lang_row.append(&lang_spacer);
    #[allow(deprecated)] // see the Ui field note: StringCombo is unavailable here
    let lang_combo = gtk::ComboBoxText::new();
    #[allow(deprecated)]
    for lang in Language::ALL {
        lang_combo.append_text(lang.label());
    }
    lang_row.append(&lang_combo);
    general_col.append(&lang_row);
    general_col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let weekday_row = hbox(10);
    let weekday_label = gtk::Label::new(Some("Week starts on"));
    weekday_label.set_halign(gtk::Align::Start);
    weekday_row.append(&weekday_label);
    let weekday_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    weekday_spacer.set_hexpand(true);
    weekday_row.append(&weekday_spacer);
    #[allow(deprecated)] // see the Ui field note: StringCombo is unavailable here
    let weekday_combo = gtk::ComboBoxText::new();
    #[allow(deprecated)]
    {
        weekday_combo.append_text("Monday");
        weekday_combo.append_text("Sunday");
    }
    weekday_row.append(&weekday_combo);
    general_col.append(&weekday_row);
    general_card.append(&general_col);
    s_body.append(&general_card);

    // System card: launch at login + the floating desktop pet.
    let system_card = card();
    let system_col = vbox(0);
    system_col.add_css_class("boxed-list");
    let autostart_switch = adw::SwitchRow::builder().build();
    let pet_switch = adw::SwitchRow::builder().build();
    system_col.append(&autostart_switch);
    system_col.append(&pet_switch);
    let pet_size_row = adw::ActionRow::builder().build();
    let pet_size_spin = gtk::SpinButton::with_range(48.0, 160.0, 8.0);
    pet_size_spin.set_hexpand(true);
    // adw 1.x ActionRow is a ListBoxRow, not a Box — the control goes through set_child.
    pet_size_row.set_child(Some(&pet_size_spin));
    system_col.append(&pet_size_row);
    system_card.append(&system_col);
    s_body.append(&system_card);

    // Save-data card: export / import the state envelope (device migration).
    let save_card = card();
    let save_col = vbox(8);
    save_col.set_margin_top(10);
    save_col.set_margin_bottom(10);
    save_col.set_margin_start(10);
    save_col.set_margin_end(10);
    let save_hint = caption_dim2("");
    save_col.append(&save_hint);
    let save_btns = hbox(8);
    let export_btn = gtk::Button::with_label("Export…");
    export_btn.add_css_class("flat");
    let import_btn = gtk::Button::with_label("Import…");
    import_btn.add_css_class("flat");
    save_btns.append(&export_btn);
    save_btns.append(&import_btn);
    save_col.append(&save_btns);
    save_card.append(&save_col);
    s_body.append(&save_card);

    let version = gtk::Label::new(Some(&format!("PokeTokenBar v{}", env!("CARGO_PKG_VERSION"))));
    version.set_halign(gtk::Align::Center);
    version.add_css_class("caption");
    version.add_css_class("dim-label");
    s_body.append(&version);
    settings.append(&s_body);
    settings_scroll.set_child(Some(&settings));
    stack.add_titled(&settings_scroll, Some(PAGE_SETTINGS), "Settings");

    root.append(&stack);
    win.set_content(Some(&root));
    win.present();

    let ui = Ui {
        win,
        app: (*app).clone(),
        root,
        stack,
        tab_home,
        tab_shop,
        tab_bag,
        tab_collection,
        gear_btn,
        quit_btn,
        emoji,
        sprite,
        name,
        shiny,
        rarity_badge,
        egg_guarantee,
        stage,
        bar,
        sub,
        status,
        graduated,
        row_today,
        row_week,
        row_month,
        row_burn,
        val_today,
        val_week,
        val_month,
        val_burn,
        limits_box,
        providers_box,
        wallet,
        wallet_caption,
        shop_hint,
        shop_cards,
        bag_cards,
        seg_dex,
        seg_log,
        collection_box,
        back_btn,
        settings_title,
        lang_label,
        weekday_label,
        lang_combo,
        weekday_combo,
        sprite_for: Arc::new(Mutex::new(String::new())),
        sprite_queue: Arc::new(Mutex::new(None)),
        sprite_anim: Rc::new(Mutex::new(None)),
        limits_queue: Arc::new(Mutex::new(None)),
        limits_dirty: Arc::new(AtomicBool::new(false)),
        celebration: Arc::new(Mutex::new(None)),
        floating: floating_pet,
        autostart_switch,
        pet_switch,
        pet_size_row,
        export_btn,
        import_btn,
        save_hint,
    };
    // Hand the pet its `Ui` handle now that one exists (its Hide/Open actions borrow it).
    *ui_ref.borrow_mut() = Some(ui.clone());
    connect_signals(&ui);
    ui
}

fn connect_signals(ui: &Ui) {
    // The signal receiver is always accessed through the `&Ui` param (a shared, Copy reference),
    // while the `move` closure captures its own clones (`cap`, `btn`, …). Keeping the receiver
    // and the captured values distinct avoids "cannot move out of `ui` while borrowed".
    // Tab pills: click activates → switch the stack; the switcher re-arms the buttons.
    for (btn_ref, tab) in [
        (&ui.tab_home, Tab::Home),
        (&ui.tab_shop, Tab::Shop),
        (&ui.tab_bag, Tab::Bag),
        (&ui.tab_collection, Tab::Collection),
    ] {
        let cap = ui.clone();
        let btn = btn_ref.clone();
        btn_ref.connect_toggled(move |_| {
            if btn.is_active() {
                go_tab(&cap, tab);
            }
        });
    }
    // Gear → the in-window settings page (deactivates the tab pills).
    {
        let cap = ui.clone();
        ui.gear_btn.connect_clicked(move |_| show_page(&cap, PAGE_SETTINGS));
    }
    // Quit (header bar; the SNI menu offers the same).
    {
        let app = ui.app.clone();
        ui.quit_btn.connect_clicked(move |_| app.quit());
    }
    // Back (settings page) → Home.
    {
        let cap = ui.clone();
        ui.back_btn.connect_clicked(move |_| go_tab(&cap, Tab::Home));
    }
    // Language / first-weekday pickers (persist, then re-render everything).
    #[allow(deprecated)] // ComboBoxText (see the Ui field note)
    {
        let cap = ui.clone();
        let combo = ui.lang_combo.clone();
        ui.lang_combo.connect_changed(move |_| {
            if let Some(idx) = combo.active() {
                let idx = idx as usize;
                if idx < Language::ALL.len() {
                    act_set_language(&cap, Language::ALL[idx].code());
                }
            }
        });
    }
    #[allow(deprecated)]
    {
        let cap = ui.clone();
        let combo = ui.weekday_combo.clone();
        ui.weekday_combo.connect_changed(move |_| {
            if let Some(idx) = combo.active() {
                act_set_weekday(&cap, idx == 0);
            }
        });
    }
    // Launch at login (XDG autostart entry — the Linux port of the macOS LoginItem toggle).
    {
        let cap = ui.clone();
        let row = ui.autostart_switch.clone();
        ui.autostart_switch.connect_active_notify(move |_| {
            act_set_autostart(&cap, row.is_active());
        });
    }
    // Floating pet toggle + size.
    {
        let cap = ui.clone();
        let row = ui.pet_switch.clone();
        ui.pet_switch.connect_active_notify(move |_| {
            act_set_floating_pet(&cap, row.is_active());
        });
    }
    {
        let cap = ui.clone();
        let spin = ui.pet_size_row
            .child()
            .and_then(|c| c.downcast::<gtk::SpinButton>().ok())
            .expect("the pet-size row holds the spin button");
        spin.connect_value_changed(move |spin| {
            act_set_pet_size(&cap, spin.value().round() as u32);
        });
    }
    // Save-data export / import.
    {
        let cap = ui.clone();
        ui.export_btn.connect_clicked(move |_| act_export_save(&cap));
    }
    {
        let cap = ui.clone();
        ui.import_btn.connect_clicked(move |_| act_import_save(&cap));
    }
    // Collection segments.
    {
        let cap = ui.clone();
        let seg = ui.seg_dex.clone();
        ui.seg_dex.connect_toggled(move |_| {
            if seg.is_active() {
                render_collection(&cap);
            }
        });
        let cap = ui.clone();
        let seg = ui.seg_log.clone();
        ui.seg_log.connect_toggled(move |_| {
            if seg.is_active() {
                render_collection(&cap);
            }
        });
    }
    // Re-opening the window (tray toggle / WM) always lands on Home — the macOS popover resets
    // its navigation on every open.
    {
        let cap = ui.clone();
        ui.win.connect_map(move |_| go_tab(&cap, Tab::Home));
    }
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

fn tab_button(label_text: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::with_label(label_text);
    b.add_css_class("pill");
    b.set_hexpand(true);
    b
}

fn scrolled() -> gtk::ScrolledWindow {
    let sw = gtk::ScrolledWindow::new();
    sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sw.set_vexpand(true);
    sw
}

fn go_tab(ui: &Ui, tab: Tab) {
    ui.stack.set_visible_child_name(tab.name());
    ui.tab_home.set_active(tab == Tab::Home);
    ui.tab_shop.set_active(tab == Tab::Shop);
    ui.tab_bag.set_active(tab == Tab::Bag);
    ui.tab_collection.set_active(tab == Tab::Collection);
}

/// Screenshot mode only: drop the slide transition so a capture can never catch a page
/// mid-slide (the 80 ms settle tick is shorter than the ~250 ms animation).
pub(crate) fn disable_transitions(ui: &Ui) {
    ui.stack.set_transition_type(gtk::StackTransitionType::None);
}

/// Show a named page, keeping the tab pills consistent (Settings clears all of them).
pub(crate) fn show_page(ui: &Ui, page: &str) {
    ui.stack.set_visible_child_name(page);
    let tab = if page == PAGE_HOME {
        Some(Tab::Home)
    } else if page == PAGE_SHOP {
        Some(Tab::Shop)
    } else if page == PAGE_BAG {
        Some(Tab::Bag)
    } else if page == PAGE_COLLECTION {
        Some(Tab::Collection)
    } else {
        None
    };
    ui.tab_home.set_active(tab == Some(Tab::Home));
    ui.tab_shop.set_active(tab == Some(Tab::Shop));
    ui.tab_bag.set_active(tab == Some(Tab::Bag));
    ui.tab_collection.set_active(tab == Some(Tab::Collection));
}

// ---------------------------------------------------------------------------
// Timers (normal run only — never in screenshot mode)
// ---------------------------------------------------------------------------

fn start_timers(ui: Ui, tray_queue: sni::TrayCommandQueue) {
    // Full model refresh (load → day_delta → add_tokens → save → render all tabs).
    let ui_tick = ui.clone();
    glib::timeout_add_local(Duration::from_secs(REFRESH_SECS), move || {
        if let Err(e) = refresh(&ui_tick, true) {
            eprintln!("[poketoken] refresh failed: {e:#}");
        }
        glib::ControlFlow::Continue
    });

    // Drain finished sprite loads (worker → main).
    let ui_drain = ui.clone();
    let queue_drain = ui.sprite_queue.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        if let Some(res) = queue_drain.lock().unwrap().take() {
            drain_sprite(&ui_drain, res);
        }
        glib::ControlFlow::Continue
    });

    // Advance the sprite GIF one frame when its display deadline passes (main thread only;
    // GTK4 has no animated-image widget, so the current frame is swapped by hand). The tick
    // must be well below the shortest frame delay (the PokéAPI Gen-V sprites run at 100 ms):
    // at 80 ms a 100 ms frame was shown 80 or 160 ms depending on phase, which stuttered.
    let ui_frames = ui.clone();
    glib::timeout_add_local(Duration::from_millis(20), move || {
        let mut anim = ui_frames.sprite_anim.lock().unwrap();
        if let Some(a) = anim.as_mut() {
            if Instant::now() >= a.due {
                ui_frames.sprite.set_from_pixbuf(Some(&a.frames[a.index]));
                a.index = (a.index + 1) % a.frames.len();
                a.due = Instant::now() + Duration::from_millis(a.delays_ms[a.index].max(1) as u64);
            }
        }
        glib::ControlFlow::Continue
    });

    // Limits worker finished a poll → full refresh (the limit window also feeds the
    // display-state "tired" rule, so a partial update would leave the status stale).
    let ui_limits = ui.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        if ui_limits.limits_dirty.swap(false, Ordering::SeqCst) {
            if let Err(e) = refresh(&ui_limits, true) {
                eprintln!("[poketoken] limits refresh failed: {e:#}");
            }
        }
        glib::ControlFlow::Continue
    });

    // Drain SNI tray commands (D-Bus thread → main thread).
    let ui_tray = ui.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        if let Some(command) = tray_queue.lock().unwrap().take() {
            match command {
                // Tray left click: flip the pet's config flag and re-render so
                // `floating.sync` shows/hides it (the settings switch follows along).
                sni::TrayCommand::TogglePet => {
                    let mut cfg = Config::load();
                    cfg.floating_pet_enabled = !cfg.floating_pet_enabled;
                    if let Err(e) = cfg.save() {
                        eprintln!("[poketoken] failed to save config (pet toggle): {e:#}");
                    }
                    if let Err(e) = refresh(&ui_tray, true) {
                        eprintln!("[poketoken] pet-toggle refresh failed: {e:#}");
                    }
                }
                // Tray menu "Open": show the main window.
                sni::TrayCommand::OpenWindow => {
                    ui_tray.win.set_visible(true);
                    ui_tray.win.present();
                }
                sni::TrayCommand::Quit => ui_tray.app.quit(),
            }
        }
        glib::ControlFlow::Continue
    });
}

// ---------------------------------------------------------------------------
// Limits worker (background thread — never on the GTK loop)
// ---------------------------------------------------------------------------

/// Spawn the limits poller: every [`LIMITS_REFRESH_SECS`] it fetches the Claude OAuth usage
/// and the Codex app-server rate limits (both sync in core, both slow when reachable) and
/// publishes a [`LimitsData`] for the main-thread drain.
fn spawn_limits(queue: Arc<Mutex<Option<LimitsData>>>, dirty: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("poketoken-limits".to_string())
        .spawn(move || {
            loop {
                let claude = poketoken_core::limits::ClaudeLimitsProvider::new()
                    .fetch()
                    .map_err(|e| e.to_string());
                let codex = match poketoken_core::limits::CodexLimitsProvider::new().fetch() {
                    Ok(status) => Some(status),
                    Err(e) => {
                        eprintln!("[poketoken-limits] codex: {e:#}");
                        None
                    }
                };
                *queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LimitsData {
                        claude,
                        codex,
                    });
                dirty.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_secs(LIMITS_REFRESH_SECS));
            }
        })
        .ok();
}

/// The highest window utilization across everything polled — feeds `DisplayInput.limit_warning`.
fn max_limit_utilization(limits: Option<&LimitsData>) -> f64 {
    let Some(data) = limits else { return 0.0 };
    let mut max = 0.0f64;
    if let Ok(status) = &data.claude {
        if let Some(w) = &status.five_hour {
            max = max.max(w.utilization.unwrap_or(0.0));
        }
        if let Some(w) = &status.seven_day {
            max = max.max(w.utilization.unwrap_or(0.0));
        }
        for e in status.scoped_limit_entries() {
            max = max.max(e.percent.unwrap_or(0.0));
        }
    }
    if let Some(Some(codex)) = &data.codex {
        if let Some(p) = codex.max_primary_used_percent() {
            max = max.max(p as f64);
        }
    }
    max
}

// ---------------------------------------------------------------------------
// Model refresh + render
// ---------------------------------------------------------------------------

/// Rebuild the whole model and every visible tab. `write` runs the day-counter mutation
/// (`day_delta` → `add_tokens` → `save`); the screenshot path passes `false` (read-only).
pub(crate) fn refresh(ui: &Ui, write: bool) -> anyhow::Result<()> {
    let ctx = ProviderCtx::system();
    let now = chrono::Utc::now();
    let cfg = Config::load();
    let snap = build_snapshot(&ctx, now, cfg.first_weekday());

    let day_key = local_day(now, &ctx.tz);
    let day_total = snap
        .combined_today
        .as_ref()
        .map(|t| t.total_tokens)
        .unwrap_or(0);

    let mut state = companion::load();
    let mut events = Vec::new();
    if write {
        let applied = companion::day_delta(&mut state, &day_key, day_total);
        events = state.add_tokens(applied);
        if let Err(e) = companion::save(&state) {
            eprintln!("[poketoken] failed to save companion state: {e:#}");
        }
    }
    for e in events.iter().filter(|e| e.is_celebration()) {
        *ui.celebration.lock().unwrap() = Some(Celebration {
            event: e.clone(),
            until: Instant::now() + CELEBRATION_WINDOW,
        });
    }
    let celebration = ui
        .celebration
        .lock()
        .unwrap()
        .as_ref()
        .filter(|c| c.until > Instant::now())
        .cloned();
    let limits = ui.limits_queue.lock().unwrap().clone();
    let limit_warning = max_limit_utilization(limits.as_ref()) >= LIMIT_WARN_PCT;
    let recent_tpm = {
        let sum: f64 = snap
            .providers
            .iter()
            .filter_map(|p| p.active_block.as_ref().and_then(|b| b.tokens_per_minute))
            .sum();
        if sum > 0.0 {
            Some(sum)
        } else {
            None
        }
    };
    let di = DisplayInput {
        tpm: recent_tpm,
        limit_warning,
        has_usage_data: day_total > 0,
        today_total: day_total,
        celebration: celebration.is_some(),
    };
    let kind = companion::display_state(&state, &di);
    let lang = resolve_language(&state.language, &cfg.language);
    let l = L::new(lang);

    render(ui, &state, &snap, limits.as_ref(), kind, celebration.as_ref(), &l, cfg.first_weekday_is_monday);
    maybe_load_sprite(ui, state.species_en(), state.current_is_shiny());
    ui.floating.sync(&state, &cfg, &l, day_total);
    Ok(())
}

/// The UI language as resolved right now (state + config fallback) — for ad-hoc popups
/// built outside a `refresh` (the floating pet's right-click menu).
pub(crate) fn current_language() -> L {
    let state = companion::load();
    let cfg = Config::load();
    L::new(resolve_language(&state.language, &cfg.language))
}

#[allow(clippy::too_many_arguments)] // one argument per rendered concern (tabs + 5 pages)
fn render(
    ui: &Ui,
    state: &companion::CompanionState,
    snap: &UsageSnapshot,
    limits: Option<&LimitsData>,
    kind: StateKind,
    celebration: Option<&Celebration>,
    l: &L,
    week_starts_monday: bool,
) {
    ui.tab_home.set_label(l.home());
    ui.tab_shop.set_label(l.shop());
    ui.tab_bag.set_label(l.bag());
    ui.tab_collection.set_label(l.collection());
    ui.gear_btn.set_tooltip_text(Some(l.settings()));
    ui.quit_btn.set_label(l.quit());

    render_home(ui, state, snap, limits, kind, celebration, l);
    render_shop(ui, state, l);
    render_bag(ui, state, l);
    render_collection(ui);
    render_settings(ui, state, l, week_starts_monday);
}

fn render_home(
    ui: &Ui,
    state: &companion::CompanionState,
    snap: &UsageSnapshot,
    limits: Option<&LimitsData>,
    kind: StateKind,
    celebration: Option<&Celebration>,
    l: &L,
) {
    ui.name.set_text(state.species());
    ui.shiny.set_visible(state.current_is_shiny());
    match state.rarity() {
        Some(r) => {
            set_rarity_classes(&ui.rarity_badge, r);
            ui.rarity_badge.set_text(&l.rarity_label(r).to_uppercase());
            ui.rarity_badge.set_visible(true);
        }
        None => ui.rarity_badge.set_visible(false),
    }

    if state.is_egg() {
        let imminent = state.progress_fraction() >= 0.9;
        ui.stage
            .set_text(if imminent { l.egg_imminent() } else { l.egg_incubating() });
        ui.stage.remove_css_class("warning");
        ui.stage.set_visible(true);
        if imminent {
            ui.stage.add_css_class("warning");
        }
        match state.egg_tier {
            Some(tier) => {
                set_rarity_classes(&ui.egg_guarantee, tier);
                ui.egg_guarantee.set_text(&l.egg_guarantee_hint(tier));
                ui.egg_guarantee.set_visible(true);
            }
            None => ui.egg_guarantee.set_visible(false),
        }
        ui.bar.set_fraction(state.progress_fraction());
        let remaining =
            (companion::EGG_HATCH_THRESHOLD - state.egg_progress).max(0);
        ui.sub
            .set_text(&l.egg_to_hatch(&compact_tokens(remaining)));
        if state.egg_progress == 0 && state.used_since_install == 0 {
            ui.status.set_text(l.egg_first_run_hint());
        } else {
            ui.status.set_text(&companion::status_text(state, kind, l));
        }
    } else {
        ui.stage.remove_css_class("warning");
        ui.stage.add_css_class("dim-label");
        ui.stage.set_visible(true);
        let stage = if state.graduated {
            l.final_form().to_string()
        } else {
            l.stage(state.form_index + 1, state.total_forms())
        };
        let nature = state
            .current_nature()
            .map(|n| format!(" · {}", n.name(l.lang())))
            .unwrap_or_default();
        ui.stage.set_text(&format!("{stage}{nature}"));
        ui.egg_guarantee.set_visible(false);
        ui.bar.set_fraction(state.progress_fraction());
        if state.graduated {
            ui.sub.set_text("");
        } else if let Some(cost) = state.next_cost() {
            let remaining = (cost - state.phase_progress).max(0);
            let is_final = state.form_index + 1 >= state.total_forms();
            let sub_text = if is_final {
                l.to_graduation(&compact_tokens(remaining))
            } else {
                l.to_next_evolution(&compact_tokens(remaining))
            };
            ui.sub.set_text(&sub_text);
        } else {
            ui.sub.set_text("");
        }
        let status = match (kind, celebration) {
            (
                StateKind::LevelUp,
                Some(Celebration {
                    event: CompanionEvent::Evolved { to },
                    ..
                }),
            ) => l.status_evolved(to),
            _ => companion::status_text(state, kind, l),
        };
        ui.status.set_text(&status);
    }
    ui.graduated.set_visible(state.graduated && !state.is_egg());
    if state.graduated && !state.is_egg() {
        ui.graduated.set_text(&l.graduated(state.species()));
    }

    // Stat rows (titles re-localized on every render).
    ui.row_today.set_title(l.today());
    ui.row_week.set_title(l.week());
    ui.row_month.set_title(l.month());
    ui.row_burn.set_title(l.burn());
    ui.val_today
        .set_text(&stat_value(snap.combined_today.as_ref()));
    let (week_tokens, week_cost) = period_total(snap, false);
    let (month_tokens, month_cost) = period_total(snap, true);
    ui.val_week
        .set_text(&format!("{}  ${:.2}", compact_tokens(week_tokens), week_cost));
    ui.val_month
        .set_text(&format!("{}  ${:.2}", compact_tokens(month_tokens), month_cost));
    let burn = snap
        .providers
        .iter()
        .filter_map(|p| p.active_block.as_ref().and_then(|b| b.tokens_per_minute))
        .sum::<f64>();
    let burn_text = if burn > 0.0 {
        format!("{burn:.0} tok/min")
    } else {
        "—".to_string()
    };
    ui.val_burn.set_text(&burn_text);

    render_limits_box(ui, limits, l);
    render_providers_box(ui, snap, l);
}

fn stat_value(d: Option<&poketoken_core::DailyUsage>) -> String {
    match d {
        Some(d) => format!("{}  ${:.2}", compact_tokens(d.total_tokens), d.total_cost),
        None => "—".to_string(),
    }
}

fn period_total(snap: &UsageSnapshot, monthly: bool) -> (i64, f64) {
    let mut tokens = 0i64;
    let mut cost = 0.0f64;
    for p in &snap.providers {
        if let Some(pd) = if monthly {
            p.month_total.as_ref()
        } else {
            p.week_total.as_ref()
        } {
            tokens += pd.total_tokens;
            cost += pd.total_cost;
        }
    }
    (tokens, cost)
}

fn limit_color_class(util: f64) -> &'static str {
    if util >= LIMIT_CRIT_PCT {
        "error"
    } else if util >= LIMIT_WARN_PCT {
        "warning"
    } else {
        ""
    }
}

fn render_limits_box(ui: &Ui, limits: Option<&LimitsData>, l: &L) {
    clear_box(&ui.limits_box);
    let heading = gtk::Label::new(Some(l.limits_official()));
    heading.set_halign(gtk::Align::Start);
    heading.add_css_class("ptb-section-title");
    ui.limits_box.append(&heading);

    match limits {
        None => {
            ui.limits_box.append(&unavailable_line(l));
        }
        Some(data) => {
            let mut any_rows = false;
            match &data.claude {
                Ok(status) => {
                    if let Some(plan) = status.plan_display() {
                        let plan_label = gtk::Label::new(Some(&l.plan(&plan)));
                        plan_label.set_halign(gtk::Align::Start);
                        plan_label.add_css_class("caption");
                        plan_label.add_css_class("dim-label");
                        ui.limits_box.append(&plan_label);
                    }
                    for (name, window) in [
                        (
                            l.five_hour_session(),
                            status.five_hour.clone(),
                        ),
                        (l.weekly(), status.seven_day.clone()),
                    ] {
                        if let Some(w) = &window {
                            if w.utilization.is_some() {
                                any_rows = true;
                                append_limit_row(
                                    &ui.limits_box,
                                    name,
                                    w.utilization,
                                    w.reset_date(),
                                );
                            }
                        }
                    }
                    for e in status.scoped_limit_entries() {
                        if e.percent.is_none() {
                            continue;
                        }
                        any_rows = true;
                        let entry_name = e
                            .scope
                            .as_ref()
                            .and_then(|s| s.model.as_ref())
                            .and_then(|m| m.display_name.as_deref())
                            .map(|d| format!("{} · {}", l.weekly(), d))
                            .unwrap_or_else(|| l.weekly().to_string());
                        append_limit_row(
                            &ui.limits_box,
                            &entry_name,
                            e.percent,
                            e.reset_date(),
                        );
                    }
                }
                Err(_) => {
                    ui.limits_box.append(&unavailable_line(l));
                }
            }
            if let Some(Some(codex)) = &data.codex {
                if codex.has_visible_limit() {
                    for s in codex.visible_snapshots() {
                        if let Some(p) = &s.primary {
                            any_rows = true;
                            append_limit_row(
                                &ui.limits_box,
                                &p.display_name(),
                                Some(p.used_percent as f64),
                                p.reset_date(),
                            );
                        }
                        if let Some(sec) = &s.secondary {
                            any_rows = true;
                            append_limit_row(
                                &ui.limits_box,
                                &sec.display_name(),
                                Some(sec.used_percent as f64),
                                sec.reset_date(),
                            );
                        }
                    }
                }
            }
            let _ = any_rows;
        }
    }
}

fn unavailable_line(l: &L) -> gtk::Label {
    let label = gtk::Label::new(Some(l.limits_unavailable()));
    label.set_halign(gtk::Align::Start);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label
}

fn append_limit_row(parent: &gtk::Box, name: &str, utilization: Option<f64>, reset: Option<chrono::DateTime<chrono::Utc>>) {
    let Some(util) = utilization else { return };
    let col = vbox(2);
    let row = hbox(6);
    let name_label = gtk::Label::new(Some(name));
    name_label.set_halign(gtk::Align::Start);
    row.append(&name_label);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row.append(&spacer);
    let pct = gtk::Label::new(Some(&format!("{util:.0}%")));
    pct.add_css_class("monospace");
    let cls = limit_color_class(util);
    if !cls.is_empty() {
        pct.add_css_class(cls);
    }
    row.append(&pct);
    if let Some(reset) = reset {
        let reset_text = reset
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        let reset_label = gtk::Label::new(Some(&format!("· {reset_text}")));
        reset_label.add_css_class("caption");
        reset_label.add_css_class("dim-label");
        row.append(&reset_label);
    }
    col.append(&row);
    let bar = gtk::ProgressBar::new();
    bar.set_fraction((util / 100.0).clamp(0.0, 1.0));
    bar.add_css_class("limit-bar");
    let cls = limit_color_class(util);
    if !cls.is_empty() {
        bar.add_css_class(cls);
    }
    col.append(&bar);
    parent.append(&col);
}

fn render_providers_box(ui: &Ui, snap: &UsageSnapshot, l: &L) {
    clear_box(&ui.providers_box);
    for p in &snap.providers {
        let card_box = card();
        let inner = vbox(3);
        inner.set_margin_top(8);
        inner.set_margin_bottom(8);
        inner.set_margin_start(10);
        inner.set_margin_end(10);
        let top = hbox(8);
        let active = p.today.is_some();
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class(if active { "success" } else { "dim-label" });
        top.append(&dot);
        let name_label = gtk::Label::new(Some(&p.display_name));
        semibold(&name_label);
        name_label.set_halign(gtk::Align::Start);
        top.append(&name_label);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        top.append(&spacer);
        match &p.today {
            Some(t) => {
                let total = gtk::Label::new(Some(&compact_tokens(t.total_tokens)));
                total.add_css_class("monospace");
                top.append(&total);
                if p.reports_cost {
                    let cost = gtk::Label::new(Some(&format!("${:.2}", t.total_cost)));
                    cost.add_css_class("caption");
                    cost.add_css_class("dim-label");
                    top.append(&cost);
                }
                let state_label = gtk::Label::new(Some(if active {
                    l.provider_active()
                } else {
                    l.provider_idle()
                }));
                state_label.add_css_class("caption");
                state_label.add_css_class("dim-label");
                top.append(&state_label);
                let breakdown = hbox(10);
                breakdown.append(&token_pair(l.tok_in(), t.input_tokens));
                breakdown.append(&token_pair(l.tok_out(), t.output_tokens));
                breakdown.append(&token_pair(
                    l.tok_cache_write(),
                    t.cache_creation_tokens,
                ));
                breakdown.append(&token_pair(l.tok_cache_read(), t.cache_read_tokens));
                inner.append(&top);
                inner.append(&breakdown);
            }
            None => {
                let state_label = gtk::Label::new(Some(l.provider_idle()));
                state_label.add_css_class("caption");
                state_label.add_css_class("dim-label");
                top.append(&state_label);
                inner.append(&top);
            }
        }
        card_box.append(&inner);
        ui.providers_box.append(&card_box);
    }
}

fn token_pair(name: &str, value: i64) -> gtk::Box {
    let pair = hbox(3);
    let n = gtk::Label::new(Some(name));
    n.add_css_class("caption");
    n.add_css_class("dim-label");
    pair.append(&n);
    let v = gtk::Label::new(Some(&compact_tokens(value)));
    v.add_css_class("caption");
    v.add_css_class("dim-label");
    v.add_css_class("monospace");
    pair.append(&v);
    pair
}

// ---------------------------------------------------------------------------
// Shop
// ---------------------------------------------------------------------------

fn render_shop(ui: &Ui, state: &companion::CompanionState, l: &L) {
    ui.wallet.set_text(&compact_tokens(state.available_tokens()));
    ui.wallet_caption.set_text(l.spendable_tokens());
    ui.shop_hint.set_text(l.shop_hint());
    clear_box(&ui.shop_cards);
    for entry in state.shop_entries() {
        let card_widget = match entry {
            ShopEntry::Item(kind) => shop_item_card(ui, state, kind, *l),
            ShopEntry::Egg(tier) => egg_card(ui, tier, *l),
        };
        ui.shop_cards.append(&card_widget);
    }
}

fn shop_item_card(
    ui: &Ui,
    state: &companion::CompanionState,
    kind: ItemKind,
    l: L,
) -> gtk::Box {
    let card_box = card();
    let inner = vbox(8);
    inner.set_margin_top(10);
    inner.set_margin_bottom(10);
    inner.set_margin_start(10);
    inner.set_margin_end(10);
    let top = hbox(10);
    let icon = gtk::Label::new(Some(kind.fallback_emoji()));
    icon.add_css_class("title-3");
    top.append(&icon);
    let info = vbox(2);
    info.set_hexpand(true);
    let name_row = hbox(6);
    let name = gtk::Label::new(Some(l.item_name(kind)));
    semibold(&name);
    name.set_halign(gtk::Align::Start);
    name_row.append(&name);
    let owned = state.item_count(kind);
    if owned > 0 && !kind.is_passive() {
        let owned_label = gtk::Label::new(Some(&l.owned_count(owned)));
        owned_label.add_css_class("caption");
        owned_label.add_css_class("dim-label");
        name_row.append(&owned_label);
    }
    info.append(&name_row);
    let desc = gtk::Label::new(Some(&l.item_description(kind)));
    desc.set_halign(gtk::Align::Start);
    desc.set_wrap(true);
    desc.add_css_class("caption");
    desc.add_css_class("dim-label");
    info.append(&desc);
    top.append(&info);
    inner.append(&top);
    let controls = hbox(8);
    shop_item_controls(&controls, ui, kind, l, false);
    inner.append(&controls);
    card_box.append(&inner);
    card_box
}

fn shop_item_controls(controls: &gtk::Box, ui: &Ui, kind: ItemKind, l: L, confirming: bool) {
    clear_box(controls);
    let state = companion::load();
    if kind.is_passive() && state.item_count(kind) > 0 {
        // Passive one-time purchase (Shiny Charm) — show "owned" instead of a buy control.
        let ok = gtk::Label::new(Some("✓"));
        ok.add_css_class("success");
        controls.append(&ok);
        let owned_label = gtk::Label::new(Some(l.owned_already()));
        owned_label.add_css_class("caption");
        owned_label.add_css_class("success");
        semibold(&owned_label);
        controls.append(&owned_label);
        return;
    }
    if confirming {
        let question = gtk::Label::new(Some(&l.buy_confirm(l.item_name(kind))));
        question.set_hexpand(true);
        question.set_halign(gtk::Align::Start);
        question.add_css_class("caption");
        question.add_css_class("dim-label");
        controls.append(&question);
        let buy = gtk::Button::with_label(l.buy());
        buy.add_css_class("suggested-action");
        let ui_buy = ui.clone();
        buy.connect_clicked(move |_| act_buy_item(&ui_buy, kind));
        controls.append(&buy);
        let cancel = gtk::Button::with_label(l.cancel());
        let controls_cancel = controls.clone();
        let ui_cancel = ui.clone();
        cancel.connect_clicked(move |_| {
            shop_item_controls(&controls_cancel, &ui_cancel, kind, l, false);
        });
        controls.append(&cancel);
        return;
    }
    let price = kind.shop_price().unwrap_or(0);
    let price_label = gtk::Label::new(Some(&format!(
        "{} {}",
        l.shop_price_label(),
        compact_tokens(price)
    )));
    price_label.add_css_class("caption");
    price_label.add_css_class("dim-label");
    controls.append(&price_label);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    controls.append(&spacer);
    if state.can_buy(kind) {
        let buy = gtk::Button::with_label(l.buy());
        let controls_buy = controls.clone();
        let ui_buy = ui.clone();
        buy.connect_clicked(move |_| {
            shop_item_controls(&controls_buy, &ui_buy, kind, l, true);
        });
        controls.append(&buy);
    } else {
        let not_enough = gtk::Label::new(Some(l.not_enough_tokens()));
        not_enough.add_css_class("caption");
        not_enough.add_css_class("dim-label");
        controls.append(&not_enough);
    }
}

fn act_buy_item(ui: &Ui, kind: ItemKind) {
    let mut state = companion::load();
    if state.buy(kind) {
        if let Err(e) = companion::save(&state) {
            eprintln!("[poketoken] failed to save after buy: {e:#}");
        }
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-buy refresh failed: {e:#}");
    }
}

/// Egg purchase stages: idle → confirm → (shiny only) a second discard warning.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EggStage {
    Idle,
    Confirm,
    ShinyConfirm,
}

fn egg_card(
    ui: &Ui,
    tier: Option<Rarity>,
    l: L,
) -> gtk::Box {
    let card_box = card();
    let inner = vbox(8);
    inner.set_margin_top(10);
    inner.set_margin_bottom(10);
    inner.set_margin_start(10);
    inner.set_margin_end(10);
    let top = hbox(10);
    let icon = gtk::Label::new(Some("🥚"));
    icon.add_css_class("title-3");
    top.append(&icon);
    let info = vbox(2);
    info.set_hexpand(true);
    let name_row = hbox(6);
    let name = gtk::Label::new(Some(l.egg_name(tier)));
    semibold(&name);
    name.set_halign(gtk::Align::Start);
    name_row.append(&name);
    if let Some(tier) = tier {
        let tier_badge = gtk::Label::new(Some(&l.rarity_label(tier).to_uppercase()));
        tier_badge.add_css_class("ptb-badge");
        tier_badge.add_css_class(rarity_css(tier));
        name_row.append(&tier_badge);
    }
    info.append(&name_row);
    let desc = gtk::Label::new(Some(&l.egg_description(tier)));
    desc.set_halign(gtk::Align::Start);
    desc.set_wrap(true);
    desc.add_css_class("caption");
    desc.add_css_class("dim-label");
    info.append(&desc);
    top.append(&info);
    inner.append(&top);
    let controls = hbox(8);
    egg_controls(&controls, ui, tier, l, EggStage::Idle);
    inner.append(&controls);
    card_box.append(&inner);
    card_box
}

fn egg_controls(controls: &gtk::Box, ui: &Ui, tier: Option<Rarity>, l: L, stage: EggStage) {
    clear_box(controls);
    let state = companion::load();
    match stage {
        EggStage::Idle => {
            let price = FreshEgg::price(tier);
            let price_label = gtk::Label::new(Some(&format!(
                "{} {}",
                l.shop_price_label(),
                compact_tokens(price)
            )));
            price_label.add_css_class("caption");
            price_label.add_css_class("dim-label");
            controls.append(&price_label);
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            controls.append(&spacer);
            if state.can_buy_egg(tier) {
                let buy = gtk::Button::with_label(l.buy());
                let controls_buy = controls.clone();
                let ui_buy = ui.clone();
                buy.connect_clicked(move |_| {
                    egg_controls(&controls_buy, &ui_buy, tier, l, EggStage::Confirm);
                });
                controls.append(&buy);
            } else {
                let not_enough = gtk::Label::new(Some(l.not_enough_tokens()));
                not_enough.add_css_class("caption");
                not_enough.add_css_class("dim-label");
                controls.append(&not_enough);
            }
        }
        EggStage::Confirm => {
            let question = gtk::Label::new(Some(&l.egg_confirm(state.species(), l.egg_name(tier))));
            question.set_hexpand(true);
            question.set_halign(gtk::Align::Start);
            question.set_wrap(true);
            question.add_css_class("caption");
            question.add_css_class("dim-label");
            controls.append(&question);
            let buy = gtk::Button::with_label(l.buy());
            buy.add_css_class("suggested-action");
            let controls_buy = controls.clone();
            let ui_buy = ui.clone();
            buy.connect_clicked(move |_| {
                // Shiny individuals get one more warning step (accidental discard guard).
                let fresh = companion::load();
                let next = if fresh.current_is_shiny() {
                    EggStage::ShinyConfirm
                } else {
                    EggStage::Idle
                };
                if next == EggStage::Idle {
                    act_buy_egg(&ui_buy, tier);
                } else {
                    egg_controls(&controls_buy, &ui_buy, tier, l, EggStage::ShinyConfirm);
                }
            });
            controls.append(&buy);
            let cancel = gtk::Button::with_label(l.cancel());
            let controls_cancel = controls.clone();
            let ui_cancel = ui.clone();
            cancel.connect_clicked(move |_| {
                egg_controls(&controls_cancel, &ui_cancel, tier, l, EggStage::Idle);
            });
            controls.append(&cancel);
        }
        EggStage::ShinyConfirm => {
            let warning = gtk::Label::new(Some(l.fresh_egg_shiny_warning()));
            warning.set_hexpand(true);
            warning.set_halign(gtk::Align::Start);
            warning.set_wrap(true);
            warning.add_css_class("caption");
            warning.add_css_class("warning");
            semibold(&warning);
            controls.append(&warning);
            let discard = gtk::Button::with_label(l.fresh_egg_discard_shiny());
            discard.add_css_class("suggested-action");
            let ui_discard = ui.clone();
            discard.connect_clicked(move |_| act_buy_egg(&ui_discard, tier));
            controls.append(&discard);
            let cancel = gtk::Button::with_label(l.cancel());
            let controls_cancel = controls.clone();
            let ui_cancel = ui.clone();
            cancel.connect_clicked(move |_| {
                egg_controls(&controls_cancel, &ui_cancel, tier, l, EggStage::Idle);
            });
            controls.append(&cancel);
        }
    }
}

fn act_buy_egg(ui: &Ui, tier: Option<Rarity>) {
    let mut state = companion::load();
    if state.buy_egg(tier) {
        if let Err(e) = companion::save(&state) {
            eprintln!("[poketoken] failed to save after egg buy: {e:#}");
        }
        // Show the fresh egg immediately (macOS switches Home on success).
        go_tab(ui, Tab::Home);
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-egg refresh failed: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// Bag
// ---------------------------------------------------------------------------

fn render_bag(ui: &Ui, state: &companion::CompanionState, l: &L) {
    clear_box(&ui.bag_cards);
    let owned = state.owned_items();
    if owned.is_empty() {
        let empty = vbox(10);
        empty.set_vexpand(true);
        let mascot = gtk::Label::new(Some("💤"));
        mascot.set_halign(gtk::Align::Center);
        mascot.add_css_class("title-1");
        empty.append(&mascot);
        let title = gtk::Label::new(Some(l.bag_empty_title()));
        title.set_halign(gtk::Align::Center);
        semibold(&title);
        empty.append(&title);
        ui.bag_cards.append(&empty);
        return;
    }
    for (kind, count) in owned {
        ui.bag_cards.append(&bag_card(ui, kind, count, *l));
    }
}

fn bag_card(ui: &Ui, kind: ItemKind, count: i64, l: L) -> gtk::Box {
    let card_box = card();
    let inner = vbox(8);
    inner.set_margin_top(10);
    inner.set_margin_bottom(10);
    inner.set_margin_start(10);
    inner.set_margin_end(10);
    let top = hbox(10);
    let icon = gtk::Label::new(Some(kind.fallback_emoji()));
    icon.add_css_class("title-3");
    top.append(&icon);
    let info = vbox(2);
    info.set_hexpand(true);
    let name_row = hbox(6);
    let name = gtk::Label::new(Some(l.item_name(kind)));
    semibold(&name);
    name.set_halign(gtk::Align::Start);
    name_row.append(&name);
    if !kind.is_passive() {
        let count_label = gtk::Label::new(Some(&format!("×{count}")));
        count_label.add_css_class("caption");
        count_label.add_css_class("dim-label");
        name_row.append(&count_label);
    }
    info.append(&name_row);
    let desc = gtk::Label::new(Some(&l.item_description(kind)));
    desc.set_halign(gtk::Align::Start);
    desc.set_wrap(true);
    desc.add_css_class("caption");
    desc.add_css_class("dim-label");
    info.append(&desc);
    top.append(&info);
    inner.append(&top);
    let controls = hbox(8);
    bag_controls(&controls, ui, kind, l, false);
    inner.append(&controls);
    card_box.append(&inner);
    card_box
}

fn bag_controls(controls: &gtk::Box, ui: &Ui, kind: ItemKind, l: L, confirming: bool) {
    clear_box(controls);
    let state = companion::load();
    if kind.is_passive() {
        // Owned passive (Shiny Charm) — no use action, show the standing effect.
        let ok = gtk::Label::new(Some("✓"));
        ok.add_css_class("success");
        controls.append(&ok);
        let hint = gtk::Label::new(Some(l.shiny_charm_effect_hint()));
        hint.add_css_class("caption");
        hint.add_css_class("success");
        semibold(&hint);
        controls.append(&hint);
        return;
    }
    let can_use = match kind {
        ItemKind::RareCandy => state.can_use_rare_candy(),
        ItemKind::Mint => state.can_use_mint(),
        ItemKind::ShinyCharm => false,
    };
    if can_use {
        if confirming {
            let question = gtk::Label::new(Some(&l.use_on_current(state.species())));
            question.set_hexpand(true);
            question.set_halign(gtk::Align::Start);
            question.add_css_class("caption");
            question.add_css_class("dim-label");
            controls.append(&question);
            let use_btn = gtk::Button::with_label(l.use_());
            use_btn.add_css_class("suggested-action");
            let ui_use = ui.clone();
            use_btn.connect_clicked(move |_| act_use_item(&ui_use, kind));
            controls.append(&use_btn);
            let cancel = gtk::Button::with_label(l.cancel());
            let controls_cancel = controls.clone();
            let ui_cancel = ui.clone();
            cancel.connect_clicked(move |_| {
                bag_controls(&controls_cancel, &ui_cancel, kind, l, false);
            });
            controls.append(&cancel);
        } else {
            let hint_text = match kind {
                ItemKind::RareCandy => format!("+{} XP", compact_tokens(RareCandy::XP)),
                ItemKind::Mint => l.mint_effect_hint().to_string(),
                ItemKind::ShinyCharm => l.shiny_charm_effect_hint().to_string(),
            };
            let hint = gtk::Label::new(Some(&hint_text));
            hint.add_css_class("caption");
            hint.add_css_class("dim-label");
            controls.append(&hint);
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            controls.append(&spacer);
            let use_btn = gtk::Button::with_label(l.use_item());
            let controls_use = controls.clone();
            let ui_use = ui.clone();
            use_btn.connect_clicked(move |_| {
                bag_controls(&controls_use, &ui_use, kind, l, true);
            });
            controls.append(&use_btn);
        }
    } else {
        let reason = if state.is_egg() {
            l.use_after_hatch()
        } else {
            l.use_needs_pokemon()
        };
        let reason_label = gtk::Label::new(Some(reason));
        reason_label.add_css_class("caption");
        reason_label.add_css_class("dim-label");
        controls.append(&reason_label);
    }
}

fn act_use_item(ui: &Ui, kind: ItemKind) {
    let mut state = companion::load();
    let celebration = match kind {
        ItemKind::RareCandy => match state.use_rare_candy() {
            CandyUseResult::Evolved => Some(CompanionEvent::Evolved {
                to: state.species().to_string(),
            }),
            CandyUseResult::Graduated => Some(CompanionEvent::Graduated {
                species: state.species().to_string(),
            }),
            _ => None,
        },
        ItemKind::Mint => {
            let _ = state.use_mint();
            None
        }
        ItemKind::ShinyCharm => None,
    };
    if let Err(e) = companion::save(&state) {
        eprintln!("[poketoken] failed to save after item use: {e:#}");
    }
    if let Some(event) = celebration {
        *ui.celebration.lock().unwrap() = Some(Celebration {
            event,
            until: Instant::now() + CELEBRATION_WINDOW,
        });
    }
    // Home shows the celebration / new nature (macOS switches Home after any use).
    go_tab(ui, Tab::Home);
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-use refresh failed: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// Collection (read-only Pokédex: species grid + catch log)
// ---------------------------------------------------------------------------

struct DexSpeciesRow {
    id: u16,
    name: String,
    shiny: bool,
    raising: bool,
}

/// Species grid data — graduated `dex_entries` chain ids ∪ the current companion's reached
/// path prefix (mirrors the macOS `dexSpecies` accumulator; `planned_path` is never shown —
/// unreached stages are not collected yet).
fn dex_species(state: &companion::CompanionState, lang: Language) -> Vec<DexSpeciesRow> {
    use std::collections::HashMap;
    // (id) → (shiny, graduated). A species is "graduated" once it appears in a dex entry's
    // chain; the current (non-egg) companion contributes its reached path prefix.
    let mut acc: HashMap<u16, (bool, bool)> = HashMap::new();
    for entry in &state.dex_entries {
        for &id in &entry.chain_order {
            let a = acc.entry(id).or_insert((false, false));
            a.0 |= entry.is_shiny;
            a.1 = true;
        }
    }
    if !state.is_egg() {
        let reached = state.path.iter().take((state.form_index + 1).max(0) as usize);
        for &id in reached {
            let a = acc.entry(id).or_insert((false, false));
            a.0 |= state.current_is_shiny();
        }
    }
    let mut rows: Vec<DexSpeciesRow> = acc
        .into_iter()
        .map(|(id, (shiny, graduated))| DexSpeciesRow {
            id,
            name: pool::localized_name(id, lang),
            shiny,
            raising: !graduated,
        })
        .collect();
    rows.sort_by_key(|r| r.id);
    rows
}

struct LogRow {
    ids: Vec<u16>,
    rarity: Rarity,
    shiny: bool,
    nature: Option<poketoken_core::nature::Nature>,
    caught_at: Option<String>,
    active: bool,
}

/// Catch-log rows — the active companion first (never graduated, so it can be discarded via a
/// fresh egg), then graduated entries newest-first (macOS `dexEntriesSorted`).
fn catch_log_entries(state: &companion::CompanionState) -> Vec<LogRow> {
    let mut rows: Vec<LogRow> = Vec::new();
    if !state.is_egg() {
        if let Some(rarity) = state.rarity() {
            let reached: Vec<u16> = state
                .path
                .iter()
                .take((state.form_index + 1).max(0) as usize)
                .copied()
                .collect();
            rows.push(LogRow {
                ids: reached,
                rarity,
                shiny: state.current_is_shiny(),
                nature: state.current_nature(),
                caught_at: None,
                active: true,
            });
        }
    }
    let mut graduated: Vec<&companion::DexEntry> = state.dex_entries.iter().collect();
    graduated.sort_by(|a, b| b.caught_at.cmp(&a.caught_at));
    for e in graduated {
        rows.push(LogRow {
            ids: e.chain_order.clone(),
            rarity: e.rarity,
            shiny: e.is_shiny,
            nature: e.nature,
            caught_at: e.caught_at.clone(),
            active: false,
        });
    }
    rows
}

fn render_collection(ui: &Ui) {
    let state = companion::load();
    let lang = resolve_language(&state.language, &Config::load().language);
    let l = L::new(lang);
    ui.seg_dex.set_label(l.dex_title());
    ui.seg_log.set_label(l.catch_log_title());
    clear_box(&ui.collection_box);

    if state.dex_entries.is_empty() && state.is_egg() {
        let empty = vbox(10);
        empty.set_vexpand(true);
        let mascot = gtk::Label::new(Some("🐣"));
        mascot.set_halign(gtk::Align::Center);
        mascot.add_css_class("title-1");
        empty.append(&mascot);
        let title = gtk::Label::new(Some(l.dex_empty_title()));
        title.set_halign(gtk::Align::Center);
        semibold(&title);
        empty.append(&title);
        let hint = gtk::Label::new(Some(l.dex_empty_hint()));
        hint.set_halign(gtk::Align::Center);
        hint.set_wrap(true);
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        empty.append(&hint);
        ui.collection_box.append(&empty);
        return;
    }

    if ui.seg_log.is_active() {
        for row in catch_log_entries(&state) {
            ui.collection_box.append(&log_card(&row, l));
        }
    } else {
        let species = dex_species(&state, lang);
        let total = gtk::Label::new(Some(&l.dex_species_total(species.len() as i64)));
        total.set_halign(gtk::Align::Start);
        total.add_css_class("caption");
        total.add_css_class("dim-label");
        ui.collection_box.append(&total);
        for chunk in species.chunks(4) {
            let row_box = hbox(6);
            for row in chunk {
                let cell = dex_cell(row, l);
                cell.set_hexpand(true);
                row_box.append(&cell);
            }
            for _ in chunk.len()..4 {
                let filler = gtk::Box::new(gtk::Orientation::Vertical, 0);
                filler.set_hexpand(true);
                row_box.append(&filler);
            }
            ui.collection_box.append(&row_box);
        }
    }
}

fn dex_cell(row: &DexSpeciesRow, l: L) -> gtk::Box {
    let cell = card();
    let inner = vbox(2);
    inner.set_margin_top(6);
    inner.set_margin_bottom(6);
    inner.set_margin_start(4);
    inner.set_margin_end(4);
    let top = hbox(2);
    let number = gtk::Label::new(Some(&format!("#{}", row.id)));
    number.add_css_class("caption");
    number.add_css_class("dim-label");
    top.append(&number);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top.append(&spacer);
    if row.shiny {
        let star = gtk::Label::new(Some("✨"));
        star.set_tooltip_text(Some(l.dex_shiny_label()));
        top.append(&star);
    }
    inner.append(&top);
    // Sprite when the disk cache already has it (no network from the grid; a GIF's first
    // frame — the grid is static); a "?" otherwise.
    match pool::species_by_id(row.id) {
        Some(s) if poketoken_core::sprite::cache_path(s.slug).exists() => {
            let picture = gtk::Image::new();
            if let Ok(first) =
                gdk_pixbuf::Pixbuf::from_file(poketoken_core::sprite::cache_path(s.slug))
            {
                picture.set_from_pixbuf(Some(&first));
            }
            picture.set_pixel_size(44);
            picture.set_size_request(44, 44);
            picture.set_halign(gtk::Align::Center);
            inner.append(&picture);
        }
        _ => {
            let placeholder = gtk::Label::new(Some("❔"));
            placeholder.set_halign(gtk::Align::Center);
            placeholder.add_css_class("title-3");
            placeholder.add_css_class("dim-label");
            let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
            slot.set_size_request(44, 44);
            slot.set_valign(gtk::Align::Center);
            slot.append(&placeholder);
            inner.append(&slot);
        }
    }
    let name = gtk::Label::new(Some(&row.name));
    name.set_halign(gtk::Align::Center);
    name.add_css_class("caption");
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    inner.append(&name);
    if row.raising {
        let raising = gtk::Label::new(Some(&l.dex_raising().to_uppercase()));
        raising.add_css_class("ptb-raising");
        raising.set_halign(gtk::Align::Center);
        inner.append(&raising);
    }
    cell.append(&inner);
    cell
}

fn log_card(row: &LogRow, l: L) -> gtk::Box {
    let card_box = card();
    let inner = vbox(4);
    inner.set_margin_top(8);
    inner.set_margin_bottom(8);
    inner.set_margin_start(10);
    inner.set_margin_end(10);
    let header = hbox(6);
    let rarity = gtk::Label::new(Some(&l.rarity_label(row.rarity).to_uppercase()));
    rarity.add_css_class("ptb-badge");
    rarity.add_css_class(rarity_css(row.rarity));
    header.append(&rarity);
    if row.active {
        let raising = gtk::Label::new(Some(&l.dex_raising().to_uppercase()));
        raising.add_css_class("ptb-raising");
        header.append(&raising);
    }
    if row.shiny {
        let star = gtk::Label::new(Some("✨"));
        star.set_tooltip_text(Some(l.dex_shiny_label()));
        header.append(&star);
    }
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);
    if let Some(nature) = row.nature {
        let nature_label = gtk::Label::new(Some(nature.name(l.lang())));
        nature_label.add_css_class("caption");
        nature_label.add_css_class("dim-label");
        header.append(&nature_label);
    }
    inner.append(&header);
    let chain = hbox(4);
    let mut first = true;
    for &id in &row.ids {
        if !first {
            let arrow = gtk::Label::new(Some("→"));
            arrow.add_css_class("dim-label");
            chain.append(&arrow);
        }
        first = false;
        let name = gtk::Label::new(Some(&pool::localized_name(id, l.lang())));
        name.add_css_class("caption");
        chain.append(&name);
    }
    inner.append(&chain);
    if let Some(caught_at) = &row.caught_at {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(caught_at) {
            let text = dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            let when = gtk::Label::new(Some(&text));
            when.set_halign(gtk::Align::Start);
            when.add_css_class("caption");
            when.add_css_class("dim-label");
            inner.append(&when);
        }
    }
    card_box.append(&inner);
    card_box
}

// ---------------------------------------------------------------------------
// Settings (in-window page)
// ---------------------------------------------------------------------------

fn render_settings(
    ui: &Ui,
    state: &companion::CompanionState,
    l: &L,
    week_starts_monday: bool,
) {
    ui.back_btn.set_label(&format!("‹ {}", l.back()));
    ui.settings_title.set_text(l.settings());
    ui.lang_label.set_text(l.language());
    ui.weekday_label.set_text(l.week_starts());
    #[allow(deprecated)] // ComboBoxText (see the Ui field note)
    {
        ui.weekday_combo.set_active(None);
        ui.weekday_combo.remove_all();
        ui.weekday_combo.append_text(l.monday());
        ui.weekday_combo.append_text(l.sunday());
        ui.weekday_combo.set_active(Some(if week_starts_monday { 0 } else { 1 }));
    }
    // Re-localize the language combo (native labels are language-independent, but keep the
    // active selection in sync with the persisted state).
    let current = Language::from_code(&state.language).unwrap_or(Language::En);
    let idx = Language::ALL.iter().position(|x| *x == current).unwrap_or(0);
    #[allow(deprecated)]
    {
        ui.lang_combo.set_active(Some(idx as u32));
    }
    // System rows (the file/config is the source of truth; setting an equal value is a
    // no-op, so this never feeds back into the notify handlers).
    let cfg = Config::load();
    ui.autostart_switch.set_title(l.launch_at_login());
    ui.autostart_switch.set_subtitle(l.launch_at_login_hint());
    ui.autostart_switch.set_active(poketoken_core::autostart::is_enabled());
    ui.pet_switch.set_title(l.floating_pet());
    ui.pet_switch.set_subtitle(l.floating_pet_hint());
    ui.pet_switch.set_active(cfg.floating_pet_enabled);
    ui.pet_size_row.set_title(l.pet_size());
    ui.pet_size_row.set_sensitive(cfg.floating_pet_enabled);
    ui.pet_size_row
        .child()
        .and_then(|c| c.downcast::<gtk::SpinButton>().ok())
        .expect("the pet-size row holds the spin button")
        .set_value(cfg.floating_pet_size as f64);
    // Save-data card.
    ui.save_hint.set_text(l.save_hint());
    ui.export_btn.set_label(l.export_save());
    ui.import_btn.set_label(l.import_save());
}

fn act_set_language(ui: &Ui, code: &str) {
    let mut state = companion::load();
    if state.language == code {
        return;
    }
    state.language = code.to_string();
    if let Err(e) = companion::save(&state) {
        eprintln!("[poketoken] failed to save language: {e:#}");
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-language refresh failed: {e:#}");
    }
}

fn act_set_weekday(ui: &Ui, monday: bool) {
    let mut cfg = Config::load();
    if cfg.first_weekday_is_monday == monday {
        return;
    }
    cfg.first_weekday_is_monday = monday;
    if let Err(e) = cfg.save() {
        eprintln!("[poketoken] failed to save config: {e:#}");
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-weekday refresh failed: {e:#}");
    }
}

fn act_set_autostart(ui: &Ui, on: bool) {
    // The file state is the source of truth (a hand-removed entry must not read as on).
    if poketoken_core::autostart::is_enabled() == on {
        return;
    }
    if let Err(e) = poketoken_core::autostart::set_enabled(on) {
        eprintln!("[poketoken] failed to set launch-at-login: {e:#}");
    }
    let _ = ui;
}

fn act_set_floating_pet(ui: &Ui, on: bool) {
    let mut cfg = Config::load();
    if cfg.floating_pet_enabled == on {
        return;
    }
    cfg.floating_pet_enabled = on;
    if let Err(e) = cfg.save() {
        eprintln!("[poketoken] failed to save config: {e:#}");
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-pet-toggle refresh failed: {e:#}");
    }
}

fn act_set_pet_size(ui: &Ui, size: u32) {
    let size = size.clamp(48, 160);
    let mut cfg = Config::load();
    if cfg.floating_pet_size == size {
        return;
    }
    cfg.floating_pet_size = size;
    if let Err(e) = cfg.save() {
        eprintln!("[poketoken] failed to save config: {e:#}");
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-pet-size refresh failed: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// Save transfer (export / import, device migration)
// ---------------------------------------------------------------------------

/// Best-effort device name for the export envelope metadata.
fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "linux".into())
}

fn act_export_save(ui: &Ui) {
    let l = current_language();
    let state = companion::load();
    let now = chrono::Utc::now();
    let device = device_name();
    let Ok(bytes) = poketoken_core::save_transfer::encode(
        &state,
        env!("CARGO_PKG_VERSION"),
        &device,
        &now,
    ) else {
        show_message(ui, l.error_heading(), l.import_failed());
        return;
    };
    let dialog = gtk::FileDialog::new();
    dialog.set_title(l.export_save());
    dialog.set_initial_name(Some(
        &poketoken_core::save_transfer::suggested_filename(&now),
    ));
    let ui_clone = ui.clone();
    let l_clone = l;
    dialog.save(Some(&ui.win), None::<&gio::Cancellable>, move |result| match result {
        Ok(file) => match file.path() {
            // A local path: plain `std::fs::write` (a save is a few KB — no async ceremony).
            Some(path) => match std::fs::write(&path, &bytes) {
                Ok(()) => show_message(&ui_clone, l_clone.save_data(), l_clone.save_exported()),
                Err(e) => {
                    eprintln!("[poketoken] save export write failed: {e:#}");
                    show_message(&ui_clone, l_clone.error_heading(), l_clone.import_failed());
                }
            },
            None => {
                eprintln!("[poketoken] save export: the picked location has no local path");
                show_message(&ui_clone, l_clone.error_heading(), l_clone.import_failed());
            }
        },
        Err(e) if is_cancelled(&e) => {}
        Err(e) => eprintln!("[poketoken] save export dialog failed: {e:#}"),
    });
}

fn act_import_save(ui: &Ui) {
    let l = current_language();
    let dialog = gtk::FileDialog::new();
    dialog.set_title(l.import_save());
    let ui_clone = ui.clone();
    let l_clone = l;
    dialog.open(Some(&ui.win), None::<&gio::Cancellable>, move |result| match result {
        Ok(file) => match file.path() {
            Some(path) => match std::fs::read(&path) {
                Ok(buf) => match poketoken_core::save_transfer::decode(&buf) {
                    Ok(state) => confirm_import(&ui_clone, state, l_clone),
                    Err(e) => show_save_error(&ui_clone, l_clone, &e),
                },
                Err(e) => {
                    eprintln!("[poketoken] import read failed: {e:#}");
                    show_message(&ui_clone, l_clone.error_heading(), l_clone.import_failed());
                }
            },
            None => {
                eprintln!("[poketoken] import: the picked file has no local path");
                show_message(&ui_clone, l_clone.error_heading(), l_clone.import_failed());
            }
        },
        Err(e) if is_cancelled(&e) => {}
        Err(e) => eprintln!("[poketoken] import dialog failed: {e:#}"),
    });
}

/// Minimal `g-io-error-quark` domain: this environment's stripped glib/gio does not export
/// `glib::io_error`, so declare the one variant the file dialogs emit on cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoCancelled {
    Cancelled,
    Other,
}

impl glib::error::ErrorDomain for IoCancelled {
    fn domain() -> glib::Quark {
        glib::Quark::from("g-io-error-quark")
    }

    fn code(self) -> i32 {
        match self {
            Self::Cancelled => gio::ffi::G_IO_ERROR_CANCELLED,
            Self::Other => -1,
        }
    }

    fn from(code: i32) -> Option<Self> {
        Some(if code == gio::ffi::G_IO_ERROR_CANCELLED {
            Self::Cancelled
        } else {
            Self::Other
        })
    }
}

fn is_cancelled(e: &glib::Error) -> bool {
    matches!(e.kind::<IoCancelled>(), Some(IoCancelled::Cancelled))
}

/// The destructive-step confirmation: shows what the import replaces, and **Cancel is the
/// default** — a destructive action is never the default button (macOS `ImportConfirmPolicy`).
fn confirm_import(ui: &Ui, imported: poketoken_core::companion::CompanionState, l: L) {
    let summary = poketoken_core::save_transfer::SaveSummary::of(&imported);
    // Stable ids (localized labels would collide across languages); Cancel is the
    // default response — a destructive action is never the default button
    // (macOS `ImportConfirmPolicy.replaceButtonIndex`/`cancelButtonIndex`).
    const CANCEL_ID: &str = "cancel";
    const REPLACE_ID: &str = "replace";
    let dlg = adw::MessageDialog::builder()
        .heading(l.import_confirm(summary.dex_count as i64, summary.lifetime_tokens))
        .default_response(CANCEL_ID)
        .build();
    dlg.add_response(CANCEL_ID, l.cancel());
    dlg.add_response(REPLACE_ID, l.replace());
    dlg.set_response_appearance(REPLACE_ID, adw::ResponseAppearance::Destructive);
    let ui_clone = ui.clone();
    let imported_clone = imported.clone();
    dlg.connect_response(None, move |dlg, response| {
        let handled = response.to_string();
        dlg.response(&handled);
        if handled == REPLACE_ID {
            // Clone inside: the response handler is `Fn` (re-entrant-safe), never FnOnce.
            do_import(&ui_clone, imported_clone.clone(), l);
        }
    });
    dlg.present();
}

/// Backup → rebase against this device → persist → refresh. Aborts (keeping the current
/// state) when the backup cannot be written: the confirmation dialog promised a
/// recovery path, so without it the import must not proceed.
fn do_import(
    ui: &Ui,
    imported: poketoken_core::companion::CompanionState,
    l: L,
) {
    let now = chrono::Utc::now();
    let current = companion::load();
    if let Err(e) = poketoken_core::save_transfer::backup_current(&now) {
        eprintln!("[poketoken] pre-import backup failed: {e:#}");
        show_message(ui, l.error_heading(), l.import_backup_failed());
        return;
    }
    let rebased = poketoken_core::save_transfer::rebase(imported, &current);
    if let Err(e) = companion::save(&rebased) {
        eprintln!("[poketoken] failed to persist imported state: {e:#}");
        show_message(ui, l.error_heading(), l.import_failed());
        return;
    }
    if let Err(e) = refresh(ui, true) {
        eprintln!("[poketoken] post-import refresh failed: {e:#}");
    }
    show_message(ui, l.save_data(), l.save_imported());
}

fn show_save_error(
    ui: &Ui,
    l: L,
    err: &poketoken_core::save_transfer::SaveTransferError,
) {
    let msg = match err {
        poketoken_core::save_transfer::SaveTransferError::NotASaveFile => {
            l.import_not_save().to_string()
        }
        poketoken_core::save_transfer::SaveTransferError::NewerSchema { found, .. } => {
            l.import_newer(*found)
        }
        poketoken_core::save_transfer::SaveTransferError::FileTooLarge { .. } => {
            l.import_too_large().to_string()
        }
    };
    show_message(ui, l.error_heading(), &msg);
}

fn show_message(_ui: &Ui, heading: &str, body: &str) {
    let l = current_language();
    let dlg = adw::MessageDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dlg.add_response("ok", l.ok());
    dlg.connect_response(None, |dlg, response| {
        let handled = response.to_string();
        dlg.response(&handled);
    });
    dlg.present();
}

// ---------------------------------------------------------------------------
// Small widget builders
// ---------------------------------------------------------------------------

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

fn card() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("card");
    b
}

/// Apply a semibold weight to a label via a Pango attribute (GTK4 `Label` exposes no
/// `set_weight`; the macOS port uses semibold for name/heading rows).
fn semibold(label: &gtk::Label) {
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Semibold));
    label.set_attributes(Some(&attrs));
}

fn caption_dim(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_halign(gtk::Align::Start);
    l.add_css_class("caption");
    l.add_css_class("dim-label");
    l
}

fn caption_dim2(text: &str) -> gtk::Label {
    let l = caption_dim(text);
    l.set_wrap(true);
    l
}

const RARITY_CSS_CLASSES: [&str; 4] = [
    "rarity-common",
    "rarity-uncommon",
    "rarity-rare",
    "rarity-legendary",
];

fn rarity_css(r: Rarity) -> &'static str {
    match r {
        Rarity::Common => "rarity-common",
        Rarity::Uncommon => "rarity-uncommon",
        Rarity::Rare => "rarity-rare",
        Rarity::Legendary => "rarity-legendary",
    }
}

/// (Re)color a persistent badge for `r` — the badge widgets live for the whole window, so the
/// previous rarity's class must be dropped before the new one is added.
fn set_rarity_classes(badge: &gtk::Label, r: Rarity) {
    for c in RARITY_CSS_CLASSES {
        badge.remove_css_class(c);
    }
    badge.add_css_class(rarity_css(r));
}

fn stat_value_label() -> gtk::Label {
    let label = gtk::Label::new(Some(""));
    label.set_halign(gtk::Align::End);
    label.add_css_class("monospace");
    label
}

fn stat_row(title: &str, value: &gtk::Label) -> adw::ActionRow {
    adw::ActionRow::builder().title(title).child(value).build()
}

fn clear_box(box_: &gtk::Box) {
    let mut child = box_.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        box_.remove(&c);
        child = next;
    }
}

// ---------------------------------------------------------------------------
// Sprite pipeline (worker thread → queue → main-thread drain)
// ---------------------------------------------------------------------------

/// Kick off (and cache) the PokéAPI animated sprite for `name` on a worker thread. Dedups by
/// species + shiny so the refresh loop never refetches one we're already loading or have on
/// disk. The worker fetches + decodes and publishes a [`SpriteResult`] to `Ui.sprite_queue`;
/// `drain_sprite` applies it on the main thread.
fn maybe_load_sprite(ui: &Ui, name: &str, shiny: bool) {
    let key = format!("{name}|shiny={shiny}");
    let should = {
        let mut guard = ui.sprite_for.lock().unwrap();
        if guard.as_str() == key {
            false
        } else {
            *guard = key;
            true
        }
    };
    if !should {
        return;
    }

    // Fresh species: show the emoji fallback now; the drained result replaces it.
    ui.sprite.set_visible(false);
    ui.emoji.set_visible(true);
    spawn_sprite_load(name.to_string(), shiny, ui.sprite_queue.clone());
}

/// The fetch+decode worker, shared by the main window and the floating pet (the pet keeps
/// its own queue so its drain never races the main window's).
pub(crate) fn spawn_sprite_load(
    name: String,
    shiny: bool,
    queue: Arc<Mutex<Option<SpriteResult>>>,
) {
    std::thread::spawn(move || {
        // The on-disk cache is keyed by the slug (lowercase), not the display name — a
        // case-sensitive filesystem (Linux) will not match `Charmander` against `charmander`.
        let frames = match poketoken_core::sprite::fetch_gif(&name, shiny) {
            Ok(Some(bytes)) => match poketoken_core::sprite::decode_gif_frames(&bytes) {
                Ok(frames) => Some(frames),
                Err(e) => {
                    eprintln!("[poketoken] sprite decode failed for {name}: {e:#}");
                    None
                }
            },
            Ok(None) => {
                eprintln!("[poketoken] sprite fetch: no animated sprite for {name}");
                None
            }
            Err(e) => {
                eprintln!("[poketoken] sprite fetch failed for {name}: {e:#}");
                None
            }
        };
        *queue.lock().unwrap() = Some(SpriteResult { name, shiny, frames });
    });
}

/// Raw frames → `Pixbuf`s (main thread only: GDK objects are main-thread-affine).
pub(crate) fn frames_to_pixbufs(
    frames: &[poketoken_core::sprite::SpriteFrame],
) -> Vec<gdk_pixbuf::Pixbuf> {
    frames
        .iter()
        .map(|f| {
            let rgba = f.rgba.clone();
            gdk_pixbuf::Pixbuf::from_mut_slice(
                rgba,
                gdk_pixbuf::Colorspace::Rgb,
                true,
                8,
                f.width,
                f.height,
                f.width * 4,
            )
        })
        .collect()
}

/// Apply a drained sprite result to the widgets (main thread). Ignores a stale result whose
/// species/shiny no longer matches `sprite_for` because a newer load superseded it.
pub(crate) fn drain_sprite(ui: &Ui, res: SpriteResult) {
    let key = format!("{}|shiny={}", res.name, res.shiny);
    let current = ui.sprite_for.lock().unwrap().clone();
    if key != current {
        return;
    }
    match res.frames {
        Some(frames) if !frames.is_empty() => {
            let anim = SpriteAnim {
                delays_ms: frames.iter().map(|f| f.delay_ms).collect(),
                index: 0,
                due: Instant::now() + Duration::from_millis(frames[0].delay_ms.max(1) as u64),
                frames: frames_to_pixbufs(&frames),
            };
            let first = anim.frames[0].clone();
            *ui.sprite_anim.lock().unwrap() = Some(anim);
            ui.sprite.set_from_pixbuf(Some(&first));
            ui.sprite.set_visible(true);
            ui.emoji.set_visible(false);
        }
        _ => {
            *ui.sprite_anim.lock().unwrap() = None;
            ui.sprite.set_visible(false);
            ui.emoji.set_visible(true);
        }
    }
}

#[cfg(test)]
mod __send_probe {
    fn is_send<T: Send>() {}
    #[test]
    fn sprite_result_send() {
        // The worker→main hand-off payload must be Send (GTK widgets themselves are not —
        // they are main-thread-affine and must never be moved off the main loop).
        is_send::<super::SpriteResult>();
    }

    #[test]
    fn limits_data_send() {
        is_send::<super::LimitsData>();
    }
}
