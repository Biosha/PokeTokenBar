//! Floating pet — the small borderless desktop overlay of the companion sprite
//! (port of the macOS `FloatingPetPanel`).
//!
//! Platform reality (this environment — GNOME Wayland, stripped GTK 4.14): the
//! compositor owns toplevel placement, and this build has no API to move a window
//! itself (no `gdk_seat_start_drag` / toplevel-icon drag, no X11-era hints, no
//! layer-shell). So the pet is a small undecorated, alpha-transparent window that
//! opens wherever the compositor places it:
//!
//!   * left-click opens the main window,
//!   * right-click pops the Open/Hide menu.
//!
//! Its visibility is driven by `Config` (`floating_pet_enabled`) and re-applied on
//! every refresh, so the pet follows the companion (species/shiny) and settings
//! without its own poll loop. The tray icon toggles it (see `sni` + `app`).
//!
//! Remaining limitation vs the macOS `NSPanel`: the pet is a regular toplevel, so
//! the compositor still lists it in the overview/switcher (the skip-taskbar/pager
//! hints were removed from this GTK API — no `gdk_surface_set_skip_*` symbols) and
//! it can be covered by a focused window (GNOME has no always-on-top for toplevels
//! without layer-shell).

use crate::app::{self, Ui};
use poketoken_core::companion::CompanionState;
use poketoken_core::config::Config;
use poketoken_core::i18n::{compact_tokens, L};
use gtk4 as gtk;
use gtk::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The floating pet window with its own sprite pipeline (a separate queue from the
/// main window so the two drains never race).
#[derive(Clone)]
pub struct FloatingPet {
    win: gtk::Window,
    content: gtk::Box,
    sprite: gtk::Image,
    emoji: gtk::Label,
    /// Invisible 1×1 anchor for the right-click menu. This GTK build cannot attach a
    /// `PopoverMenu` to a foreign parent (`GtkPopover:parent` is not writable and there is
    /// no `popup_at_*`), so the menu is the `MenuButton`'s own popover, anchored to the pet.
    menu_btn: gtk::MenuButton,
    sprite_for: Arc<Mutex<String>>,
    queue: Arc<Mutex<Option<app::SpriteResult>>>,
    anim: Rc<Mutex<Option<app::SpriteAnim>>>,
    /// Main-thread handle back to the `Ui`, filled in right after the `Ui` is built —
    /// the "Hide" menu action needs it to persist the config change and re-render the
    /// settings switch.
    ui_ref: Rc<RefCell<Option<Ui>>>,
    /// Diagnostics switch (`PTB_NO_PET=1`): the pet is built inert — no gestures, no
    /// timers, never mapped.
    inert: bool,
}

impl FloatingPet {
    pub fn build(app: &adw::Application, ui_ref: Rc<RefCell<Option<Ui>>>) -> Self {
        let inert = std::env::var("PTB_NO_PET").is_ok_and(|v| v == "1");
        if inert {
            let win = gtk::Window::builder().application(app).build();
            return Self {
                win,
                content: gtk::Box::new(gtk::Orientation::Vertical, 0),
                sprite: gtk::Image::new(),
                emoji: gtk::Label::new(None),
                menu_btn: gtk::MenuButton::new(),
                sprite_for: Arc::new(Mutex::new(String::new())),
                queue: Arc::new(Mutex::new(None)),
                anim: Rc::new(Mutex::new(None)),
                ui_ref,
                inert: true,
            };
        }
        let win = gtk::Window::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .build();
        // Documented limitation (this session: GNOME Wayland, GTK 4.14): the X11-era
        // window-manager hints — skip-taskbar/pager, type-hint, focus-on-map — were
        // removed from the GTK4 public API entirely (verified: no `gdk_surface_set_skip_*`
        // symbols in libgtk-4.so.1), and GNOME does not ship a layer-shell alternative.
        // So the pet cannot hide itself from the overview/switcher the way the macOS
        // `NSPanel` (nonactivating + floating level) does; it stays a regular
        // undecorated toplevel that the compositor places and lists.
        // The CSS `window.ptb-pet { background: transparent }` (Display-level provider,
        // already installed by `style::install`) makes the surface alpha-transparent.
        win.add_css_class("ptb-pet");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);
        let emoji = gtk::Label::new(Some("🥚"));
        emoji.set_halign(gtk::Align::Center);
        emoji.set_valign(gtk::Align::Center);
        emoji.add_css_class("title-1");
        let sprite = gtk::Image::new();
        sprite.set_halign(gtk::Align::Center);
        sprite.set_valign(gtk::Align::Center);
        sprite.set_visible(false);
        content.append(&sprite);
        content.append(&emoji);
        let menu_btn = gtk::MenuButton::new();
        menu_btn.set_size_request(1, 1);
        menu_btn.set_opacity(0.0);
        content.append(&menu_btn);
        win.set_child(Some(&content));

        let pet = Self {
            win,
            content,
            sprite,
            emoji,
            menu_btn,
            sprite_for: Arc::new(Mutex::new(String::new())),
            queue: Arc::new(Mutex::new(None)),
            anim: Rc::new(Mutex::new(None)),
            ui_ref,
            inert: false,
        };

        // Left click → open the main window.
        let left = gtk::GestureClick::builder().button(1).build();
        let open_win = pet.win.clone();
        let open_main = pet.ui_ref.clone();
        left.connect_released(move |_, _, _, _| {
            // Hold the guard for the whole handler (a let-else would drop the borrow).
            let guard = open_main.borrow();
            if let Some(ui) = guard.as_ref() {
                ui.win.present();
            } else {
                open_win.present();
            }
        });
        pet.win.add_controller(left);

        // Right click → the Open/Hide menu (rebuilt per popup so its labels always match
        // the current UI language).
        let right = gtk::GestureClick::builder().button(3).build();
        let pet_for_menu = pet.clone();
        let ui_for_menu = pet.ui_ref.clone();
        right.connect_released(move |_, _, _, _| {
            // Hold the guard for the whole handler (a let-else would drop the borrow).
            let guard = ui_for_menu.borrow();
            if let Some(ui) = guard.as_ref() {
                let l = crate::app::current_language();
                pop_menu(&pet_for_menu, ui, l);
            }
        });
        pet.win.add_controller(right);

        start_timers(pet.clone());
        pet
    }

    /// Apply the config + companion state to the pet (called from `refresh`): show/hide,
    /// resize, re-localize the tooltip, and (re)load the sprite when the species changed.
    pub fn sync(&self, state: &CompanionState, cfg: &Config, l: &L, today_total: i64) {
        if self.inert {
            return;
        }
        if !cfg.floating_pet_enabled {
            if self.win.is_visible() {
                self.win.set_visible(false);
            }
            // Reset the pipeline so a re-enable reloads the sprite for the current species.
            *self.sprite_for.lock().unwrap() = String::new();
            *self.anim.lock().unwrap() = None;
            return;
        }
        let size = (cfg.floating_pet_size as i32).clamp(48, 160);
        self.content.set_size_request(size, size);
        self.sprite.set_pixel_size(size);
        self.sprite.set_size_request(size, size);
        self.win.set_tooltip_text(Some(&format!(
            "{}: {}",
            l.today(),
            compact_tokens(today_total)
        )));
        if !self.win.is_visible() {
            self.win.present();
        }
        let name = state.species_en();
        let shiny = state.current_is_shiny();
        let key = format!("{name}|shiny={shiny}");
        let load = {
            let mut guard = self.sprite_for.lock().unwrap();
            if guard.as_str() == key {
                false
            } else {
                *guard = key;
                true
            }
        };
        if load {
            self.emoji.set_visible(true);
            self.sprite.set_visible(false);
            app::spawn_sprite_load(name.to_string(), shiny, self.queue.clone());
        }
    }
}

fn pop_menu(pet: &FloatingPet, ui: &Ui, l: L) {
    let group = gio::SimpleActionGroup::new();
    let open_action = gio::SimpleAction::new("open", None);
    let open_win = ui.win.clone();
    open_action.connect_activate(move |_, _| open_win.present());
    group.add_action(&open_action);
    let hide_action = gio::SimpleAction::new("hide", None);
    let ui_clone = ui.clone();
    hide_action.connect_activate(move |_, _| {
        let mut cfg = Config::load();
        cfg.floating_pet_enabled = false;
        if let Err(e) = cfg.save() {
            eprintln!("[poketoken] failed to save config: {e:#}");
        }
        if let Err(e) = crate::app::refresh(&ui_clone, true) {
            eprintln!("[poketoken] post-hide refresh failed: {e:#}");
        }
    });
    group.add_action(&hide_action);
    pet.win.insert_action_group("pet", Some(&group));

    // A fresh model per popup keeps the labels in the current UI language.
    let model = gio::Menu::new();
    model.append(Some(l.floating_pet_menu_open()), Some("pet.open"));
    model.append(Some(l.floating_pet_menu_hide()), Some("pet.hide"));
    pet.menu_btn.set_menu_model(Some(&model));
    pet.menu_btn.popup();
}

/// The pet's own drain (150 ms) and frame-advance (20 ms) timers — the same cadence as
/// the main window's, kept separate because the pet holds its own `SpriteAnim`.
fn start_timers(pet: FloatingPet) {
    let pet_drain = pet.clone();
    let queue = pet.queue.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        if let Some(res) = queue.lock().unwrap().take() {
            drain_pet(&pet_drain, res);
        }
        glib::ControlFlow::Continue
    });

    let pet_frames = pet.clone();
    glib::timeout_add_local(Duration::from_millis(20), move || {
        let mut anim = pet_frames.anim.lock().unwrap();
        if let Some(a) = anim.as_mut() {
            if Instant::now() >= a.due {
                pet_frames.sprite.set_from_pixbuf(Some(&a.frames[a.index]));
                a.index = (a.index + 1) % a.frames.len();
                a.due = Instant::now() + Duration::from_millis(a.delays_ms[a.index].max(1) as u64);
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Apply a drained sprite result to the pet's widgets. Ignores a stale result whose
/// species/shiny no longer matches `sprite_for` (a newer load superseded it).
fn drain_pet(pet: &FloatingPet, res: app::SpriteResult) {
    let key = format!("{}|shiny={}", res.name, res.shiny);
    let current = pet.sprite_for.lock().unwrap().clone();
    if key != current {
        return;
    }
    match res.frames {
        Some(frames) if !frames.is_empty() => {
            let anim = app::SpriteAnim {
                delays_ms: frames.iter().map(|f| f.delay_ms).collect(),
                index: 0,
                due: Instant::now() + Duration::from_millis(frames[0].delay_ms.max(1) as u64),
                frames: app::frames_to_pixbufs(&frames),
            };
            let first = anim.frames[0].clone();
            *pet.anim.lock().unwrap() = Some(anim);
            pet.sprite.set_from_pixbuf(Some(&first));
            pet.sprite.set_visible(true);
            pet.emoji.set_visible(false);
        }
        _ => {
            *pet.anim.lock().unwrap() = None;
            pet.sprite.set_visible(false);
            pet.emoji.set_visible(true);
        }
    }
}
