//! Same-process SNI (StatusNotifierItem) tray over D-Bus, in pure Rust: `zbus` (tokio over the
//! `$DBUS_SESSION_BUS_ADDRESS` unix socket — no `libdbus-1`, no GTK3; the GTK3-embedding
//! `tray-icon` route crashes alongside the GTK4 window).
//!
//! A dedicated std thread runs a tokio runtime that:
//! - connects to the session bus,
//! - serves `org.kde.StatusNotifierItem` at `/StatusNotifierItem` and
//!   `com.canonical.dbusmenu` at `/MenuBar`,
//! - registers with the `org.kde.StatusNotifierWatcher` only if that name has an owner.
//!
//! GTK4 widgets are main-thread-affine, so the D-Bus handlers never touch the UI: they publish
//! a [`TrayCommand`] into a shared `Arc<Mutex<Option<..>>>` and the main-thread drain timer in
//! `crate::app` takes it and applies it — the same hand-off pattern used for sprite loads.
//!
//! The icon/menu wire formats match this host's GNOME consumer (gnome-shell +
//! ubuntu-appindicators): `IconPixmap` as `a(iiay)` ARGB buffers, `GetLayout` replying
//! `(u, (i a{sv} av))` with variant-wrapped child nodes, and
//! `RegisterStatusNotifierItem` taking the item path as a string.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, Value};

const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";

const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const APP_ID: &str = "io.github.poketoken.app";
// Empty on purpose: a themed icon name (when found in the user's theme) takes precedence over
// `IconPixmap` in GNOME/appindicator, so any real name would mask the synthesized pokeball.
const THEME_ICON: &str = "";

/// A command the D-Bus side wants the GTK main thread to apply.
#[derive(Clone, Copy, Debug)]
pub enum TrayCommand {
    /// SNI `Activate` (tray left click): show or hide the floating pet.
    TogglePet,
    /// The "Open" dbusmenu item was clicked: show the main window.
    OpenWindow,
    /// The "Quit" dbusmenu item was clicked.
    Quit,
}

/// The shared command channel: at most one pending command (clicks within the drain cadence
/// coalesce — a repeated toggle would just be a no-op).
pub type TrayCommandQueue = Arc<Mutex<Option<TrayCommand>>>;

fn push_command(queue: &TrayCommandQueue, command: TrayCommand) {
    *queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(command);
}

/// Spawn the tray worker thread. Failing to serve the tray never fails the app: without a
/// session bus, without a tray host, or if the registration is refused, the thread logs a
/// note and exits while the window keeps running.
pub fn spawn(command_queue: TrayCommandQueue) {
    let _ = std::thread::Builder::new()
        .name("poketoken-sni".to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| anyhow::anyhow!("creating the tokio runtime: {err}"))?;
                runtime.block_on(serve(command_queue))
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => eprintln!("poketoken-tray: no tray on this session: {err:#}"),
                Err(payload) => eprintln!("poketoken-tray: worker panicked: {payload:?}"),
            }
        });
}

/// Connect, check the watcher, serve the objects, register, then keep the connection alive
/// (the object-server task dispatches incoming calls) until the process exits.
async fn serve(queue: TrayCommandQueue) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;

    let fdo = zbus::fdo::DBusProxy::new(&conn).await?;
    let watcher = BusName::try_from(WATCHER_BUS_NAME).expect("a valid bus name");
    if !fdo.name_has_owner(watcher).await.unwrap_or(false) {
        eprintln!(
            "poketoken-tray: no org.kde.StatusNotifierWatcher in this session; \
             the window runs without a tray icon."
        );
        return Ok(());
    }

    // Serve both objects first so property fetches succeed the moment registration is
    // accepted. Calling `object_server()` also starts the dispatch task for incoming
    // method calls.
    conn.object_server()
        .at(
            SNI_PATH,
            SniObject {
                queue: queue.clone(),
            },
        )
        .await?;
    conn.object_server()
        .at(MENU_PATH, SniMenuObject { queue })
        .await?;

    let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    conn.request_name(bus_name.as_str()).await?;

    let watcher = StatusNotifierWatcherProxy::new(&conn).await?;
    // This host's gnome-shell watcher takes a single string: an item path or a bus name.
    watcher.register_status_notifier_item(SNI_PATH).await?;
    eprintln!(
        "poketoken-tray: registered {bus_name}; left-click toggles the pet, right-click gives Open/Quit."
    );

    std::future::pending::<()>().await;
    Ok(())
}

/// Proxy for the session bus's StatusNotifierWatcher.
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    async fn register_status_notifier_item(&self, service: &str) -> zbus::fdo::Result<()>;
}

/// The StatusNotifierItem object: static identity + icon, click routing to the main thread.
struct SniObject {
    queue: TrayCommandQueue,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl SniObject {
    #[zbus(property)]
    fn category(&self) -> &'static str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &'static str {
        APP_ID
    }

    #[zbus(property)]
    fn title(&self) -> &'static str {
        "PokeTokenBar"
    }

    #[zbus(property)]
    fn status(&self) -> &'static str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &'static str {
        ""
    }

    /// No themed icon: forces consumers onto the `IconPixmap` pokeball bitmaps.
    #[zbus(property)]
    fn icon_name(&self) -> &'static str {
        THEME_ICON
    }

    /// ARGB (0xAARRGGBB byte-packed) pokeball bitmaps — the fallback when the theme lacks
    /// `icon_name`. `a(iiay)`: one (width, height, pixels) tuple per size.
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        vec![pokeball(22), pokeball(64)]
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &'static str {
        ""
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }

    /// Tray left click: toggle the floating pet.
    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        push_command(&self.queue, TrayCommand::TogglePet);
        Ok(())
    }

    /// Right-click: GNOME builds the popup from the dbusmenu at `menu` instead.
    async fn context_menu(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn secondary_activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn scroll(&self, _delta: i32, _orientation: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

/// A `size`-by-`size` ARGB32 pokeball, synthesized per pixel (no image decoder involved).
fn pokeball(size: u32) -> (i32, i32, Vec<u8>) {
    let radius = size as f32 / 2.0;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let cx = (x as f32 + 0.5) - radius;
            let cy = (y as f32 + 0.5) - radius;
            let dist = (cx * cx + cy * cy).sqrt();
            let (a, r, g, b): (u8, u8, u8, u8) = if dist >= radius - 1.0 {
                (0x00, 0x00, 0x00, 0x00)
            } else if dist >= radius - 2.5 || dist <= radius * 0.30 {
                (0xFF, 0x00, 0x00, 0x00)
            } else if dist <= radius * 0.42 {
                (0xFF, 0xFF, 0xFF, 0xFF)
            } else if cy.abs() <= radius * 0.12 {
                (0xFF, 0x00, 0x00, 0x00)
            } else if cy < 0.0 {
                (0xFF, 0xDC, 0x35, 0x35)
            } else {
                (0xFF, 0xFF, 0xFF, 0xFF)
            };
            pixels.extend_from_slice(&[a, r, g, b]);
        }
    }
    (size as i32, size as i32, pixels)
}

#[allow(clippy::type_complexity)]
type MenuNode = (i32, HashMap<String, Value<'static>>, Vec<Value<'static>>);

const QUIT_ID: i32 = 1;
const OPEN_ID: i32 = 2;

fn item_props(label: &str) -> HashMap<String, Value<'static>> {
    [
        (String::from("label"), Value::from(label.to_string())),
        (String::from("enabled"), Value::from(true)),
        (String::from("visible"), Value::from(true)),
        (String::from("type"), Value::from("standard".to_string())),
    ]
    .into_iter()
    .collect()
}

fn quit_props() -> HashMap<String, Value<'static>> {
    item_props("Quit")
}

/// The menu properties for a known item id, if any.
fn props_for(id: i32) -> Option<HashMap<String, Value<'static>>> {
    match id {
        QUIT_ID => Some(quit_props()),
        OPEN_ID => Some(item_props("Open")),
        _ => None,
    }
}

/// The com.canonical.dbusmenu object: a single "Quit" item under the menu root (id 0).
struct SniMenuObject {
    queue: TrayCommandQueue,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl SniMenuObject {
    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn status(&self) -> &'static str {
        "normal"
    }

    #[zbus(property)]
    fn text_direction(&self) -> &'static str {
        "ltr"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    /// `AboutToShow(i) -> b`: false = the current layout is up to date.
    async fn about_to_show(&self, _id: i32) -> zbus::fdo::Result<bool> {
        Ok(false)
    }

    /// `GetLayout(i i as) -> (u (i a{sv} av))`: the revision plus the root node (id 0) whose
    /// children are variant-wrapped nodes (this consumer's recursive layout encoding).
    async fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, MenuNode)> {
        let open: MenuNode = (OPEN_ID, item_props("Open"), Vec::new());
        let quit: MenuNode = (QUIT_ID, quit_props(), Vec::new());
        let root: MenuNode = (
            0,
            HashMap::new(),
            vec![Value::from(open), Value::from(quit)],
        );
        Ok((2, root))
    }

    /// `GetGroupProperties(ai as) -> a(ia{sv})`.
    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, HashMap<String, Value<'static>>)>> {
        Ok(ids
            .into_iter()
            .filter_map(|id| props_for(id).map(|props| (id, props)))
            .collect())
    }

    /// `GetProperty(i s) -> v`.
    async fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<Value<'_>> {
        let props =
            props_for(id).ok_or_else(|| zbus::fdo::Error::UnknownProperty(name.to_string()))?;
        for (key, value) in props {
            if key == name {
                return Ok(value);
            }
        }
        Err(zbus::fdo::Error::UnknownProperty(name.to_string()))
    }

    /// `Event(i s v u)`: "clicked" on Open shows the main window; on Quit, quits the app.
    async fn event(
        &self,
        id: i32,
        event_id: &str,
        _data: Value<'_>,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        if event_id == "clicked" {
            match id {
                OPEN_ID => push_command(&self.queue, TrayCommand::OpenWindow),
                QUIT_ID => push_command(&self.queue, TrayCommand::Quit),
                _ => {}
            }
        }
        Ok(())
    }

    /// `EventGroup(a(isvu)) -> ai`: no failures.
    async fn event_group(
        &self,
        _events: Vec<(i32, String, Value<'_>, u32)>,
    ) -> zbus::fdo::Result<Vec<i32>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TrayCommand>();
    }

    #[test]
    fn pokeball_pixels_are_argb32() {
        let (width, height, pixels) = pokeball(4);
        assert_eq!(width, 4);
        assert_eq!(height, 4);
        assert_eq!(pixels.len(), 4 * 4 * 4);
        // Corners are outside the circle: fully transparent.
        assert_eq!(pixels[0..4], [0x00, 0x00, 0x00, 0x00]);
        // Center pixel is opaque (alpha 0xFF as the first byte of the ARGB group).
        let center = (2 * 4 + 2) * 4;
        assert_eq!(pixels[center], 0xFF);
    }
}
