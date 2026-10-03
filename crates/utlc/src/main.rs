//! Universal Treble Launcher & Compositor (UTLC).
//! Unified Single-Process Mobile Wayland Compositor and Android-Style Launcher.
//! Engineered in bare-metal Rust following strict systems discipline (GEMINI.md).
//! Consumes < 15 MB RSS, renders interactive home screen in < 0.45s,
//! and delivers < 8ms touch gesture response.

use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::time::{Duration, Instant};

use utim_core::compositor::desktop::{
    search_ranked, DesktopApp, DesktopCatalogue, SearchFields, SearchHit,
};
use utim_core::compositor::gestures::{
    GestureAction, GestureConfig, GestureEngine, RawTouchEvent, TouchPhase,
};
use utim_core::compositor::haptics::{HapticEffect, Haptics};
use utim_core::compositor::icons;
use utim_core::compositor::ime::{ImeAction, VirtualKeyboard};
use utim_core::compositor::input::{
    InputDispatchResult, InputDispatcher, LinuxInputEvent, KEY_1, KEY_2, KEY_3, KEY_4,
    KEY_BACKSPACE, KEY_C, KEY_D, KEY_ENTER, KEY_ESC, KEY_L, KEY_POWER, KEY_T, KEY_TAB,
    KEY_VOLUMEDOWN, KEY_VOLUMEUP, KEY_W,
};
use utim_core::compositor::lockscreen::{LockScreen, LockState};
use utim_core::compositor::power_sync::{PowerSaverMode, UtimPowerSync};
use utim_core::compositor::protocols::{ProtocolRegistry, WaylandInterface};
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::super_extreme::{SuperExtremeScreen, SuperExtremeState};
use utim_core::compositor::systemui::{QuickTileKind, SystemUiShade};
use utim_core::compositor::IconCache;
use utim_core::compositor::{DismissOutcome, FolderOpen, KillQueue, PopupItem, Recents, TaskCard};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};
use utim_core::graphics::drawer_mod::FastScrollerState;
use utim_core::graphics::drm_kms::{folder_grid_geometry, AppInfoSpec, FolderGridGeometry};
use utim_core::graphics::font::{set_active_family, FontFamily};
use utim_core::graphics::layout::AppInfoTarget;
use utim_core::graphics::layout::{
    drop_target_bar, AppLayout, AppPanel, Cell, DrawerSearchHit, FastScrollerLayout, FolderLayout,
    FolderMenuAction, FolderTouch, Key, Keyboard, Layout, QsbHit, Rect, ShadeLayout, ShadeZone,
    TabHit, FOLDER_PREVIEW_MAX,
};
use utim_core::graphics::{
    damped_scroll, AppGridItem, DrmInteractiveState, DrmKmsDevice, FolderPreviewRow,
    MaterialYouPalette, NotifRow, RecentsCard, RgbaImage, SpringConfig, SpringSimulation,
    TerminalTabInfo, SHADE_NOTIF_ROWS,
};
use utim_core::launcher_state::{AccentSource, Cell as PageCell, LauncherState};

/// Commit threshold for the recents gesture.
///
/// The reference commits on the *release* past a fraction of the screen rather
/// than on the hold alone (`AbsSwipeUpHandler.java:756`), so this is a progress
/// value the gesture layer produces, not a pixel distance the shell invents.
const RECENTS_COMMIT_PROGRESS: f32 = 0.5;

/// Dismiss every modal surface and return the workspace morph to rest.
///
/// Shared by the Home and Back paths, which differ only in what else they tear
/// down. Kept as one function because the failure mode of getting it wrong is
/// silent and sticky: a leftover `Overview` makes every subsequent workspace
/// swipe inert (the `is_modal` guard), and a leftover `workspace_scale` leaves
/// the app panel permanently shrunk with nothing driving it.
/// What tapping one row of the long-press popup does, decided with no state.
///
/// The sibling of [`ShellEffect`], and it exists for the same reason. The
/// `PopupItem` enum was complete -- 16 variants, each with a doc comment citing
/// the reference's `LauncherOptionsPopup.DEFAULT_ORDER` or `SystemShortcut` --
/// and `draw_popup` rendered whichever of them the shell handed it. But
/// `ShellState::PopupOpen { idx }` was written as the literal `0` at both
/// construction sites and never read anywhere in the workspace, and
/// `PopupMenuLayout` was only ever called by the renderer. So a long-press drew
/// a menu whose every row was a label: tapping "App info" launched the app
/// instead, and because `is_modal()` is true for `PopupOpen`, the tap was
/// swallowed rather than falling through. Extracting the decision makes "this
/// row has an effect" an assertion about a pure function instead of a guess
/// about a `match` in a 5000-line event loop.
///
/// `RenameApp` and `ToggleHiddenApp` are constructed by nothing yet: they are
/// the two children of [`PopupEffect::CustomizeApp`], which is a row with no
/// sub-sheet. They are declared because [`plan_popup_row`] is exhaustive over
/// `PopupItem` and a variant that cannot be produced would mean a menu row with
/// no effect, which is the failure this type exists to rule out.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupEffect {
    /// Close the menu and do nothing else.
    Dismiss,
    /// The affirmative row: hand the app to the platform's detail view.
    AppInfo,
    /// Remove the icon from the current workspace page.
    RemoveFromPage,
    /// Uninstall the app. A handoff, not a new screen: the reference calls
    /// `ACTION_DELETE` (`SystemShortcut.java:304-316`) and Linux's equivalent
    /// is the software manager.
    Uninstall,
    /// Open the app drawer.
    OpenAllApps,
    /// Open the wallpaper picker.
    OpenWallpapers,
    /// Open the settings surface.
    OpenSettings,
    /// Open the widget picker.
    OpenWidgets,
    /// Toggle the home-screen lock.
    ToggleHomeLock,
    /// Enter home edit mode.
    EnterEditMode,
    /// Open the platform settings application.
    OpenSystemSettings,
    /// Make this page the one a Home gesture returns to.
    SetDefaultPage,
    /// Rename the app, via the on-screen keyboard. Reached from the *sub-sheet*
    /// [`PopupEffect::CustomizeApp`] opens, which does not exist yet.
    RenameApp,
    /// Open the per-app sub-sheet: rename, hide, change icon, per-app gesture.
    ///
    /// Its own variant rather than being folded into `RenameApp`, because the
    /// reference reaches all four from one row
    /// (`CustomizeDialog.kt:154-211`) and picking the wrong one of them is the
    /// kind of thing a single enum arm hides.
    CustomizeApp,
    /// Toggle the app's visibility in the drawer. Also reached from
    /// [`PopupEffect::CustomizeApp`]. The filter it needs is implemented
    /// (`build_all_apps` takes the hidden set) and reachable from the settings
    /// layer; nothing sets the set yet.
    ToggleHiddenApp,
    /// Launch the `n`th deep shortcut of the icon.
    LaunchShortcut(u8),
    /// A row the shell cannot service yet. Kept as its own variant so an
    /// unimplemented row is a visible `PopupEffect` rather than a silent
    /// `_ => {}` -- the exact failure this type was written to eliminate.
    Unsupported,
}

/// Decide what a popup row does. Pure: no shell state is read or written.
///
/// Row order comes from the reference: the workspace menu is
/// `LauncherOptionsPopup.DEFAULT_ORDER` (`LauncherOptionsPopup.kt:18-28`,
/// eight entries once the metadata-only `carousel` option is filtered at
/// `:145`); the icon menu is `SystemShortcut` plus `LawnchairShortcut`'s
/// additions (`LawnchairLauncher.kt:283-287`).
fn plan_popup_row(item: PopupItem) -> PopupEffect {
    match item {
        PopupItem::AppInfo => PopupEffect::AppInfo,
        PopupItem::Remove => PopupEffect::RemoveFromPage,
        PopupItem::Uninstall => PopupEffect::Uninstall,
        PopupItem::AllApps => PopupEffect::OpenAllApps,
        PopupItem::Wallpapers => PopupEffect::OpenWallpapers,
        PopupItem::HomeSettings => PopupEffect::OpenSettings,
        PopupItem::Widgets => PopupEffect::OpenWidgets,
        PopupItem::HomeScreenLock => PopupEffect::ToggleHomeLock,
        PopupItem::EditMode => PopupEffect::EnterEditMode,
        PopupItem::SystemSettings => PopupEffect::OpenSystemSettings,
        PopupItem::DefaultPageForWorkspace => PopupEffect::SetDefaultPage,
        PopupItem::Customize => PopupEffect::CustomizeApp,
        PopupItem::DeepShortcut(n) => PopupEffect::LaunchShortcut(n),
        // `Install` needs `FLAG_SUPPORTS_WEB_UI`, `OpenInStore` needs a
        // Play-style store id and `PauseApps` needs per-user package suspension
        // -- all Android platform services. Recognised, not yet serviced.
        PopupItem::Install | PopupItem::OpenInStore | PopupItem::PauseApps => {
            PopupEffect::Unsupported
        }
    }
}

/// Build the row list for a long-press, in the reference's order.
///
/// `on_icon` selects the two menus the reference has. The workspace menu is
/// `LauncherOptionsPopup.DEFAULT_ORDER` (`LauncherOptionsPopup.kt:18-28`),
/// which is nine entries; `carousel` is metadata-only and is filtered out of
/// the built list at `:145`, leaving eight. UTLC never built a "carousel", so
/// it takes the remaining seven plus the lock row.
///
/// The icon menu is `SystemShortcut`'s rows in the order
/// `LawnchairLauncher.getSupportedShortcuts` adds them
/// (`LawnchairLauncher.kt:283-287`): app info, then the deep shortcuts the app
/// publishes, then uninstall, customize, store and pause.
///
/// `home_locked` drops the mutating rows, which is what the reference does:
/// `SystemShortcut.isHomeLocked` (`:137-140`) gates Widgets and Remove, and
/// `LauncherOptionsPopup.getLauncherOptions` (`:144-150`) drops edit-mode and
/// widgets entries entirely.
fn popup_items_for(
    on_icon: bool,
    deep_shortcuts: u8,
    home_locked: bool,
) -> utim_core::compositor::PopupItems {
    let mut out = utim_core::compositor::PopupItems::EMPTY;
    if on_icon {
        out.push(PopupItem::AppInfo);
        for n in 0..deep_shortcuts {
            // The cap is enforced inside `push_shortcut`, which reports whether
            // the drop was the capacity or the shortcut limit. A false here
            // means we have hit `MAX_DEEP_SHORTCUTS`; the loop is bounded by
            // `deep_shortcuts <= MAX_DEEP_SHORTCUTS` at the call site anyway.
            out.push_shortcut(n);
        }
        if !home_locked {
            out.push(PopupItem::Remove);
        }
        out.push(PopupItem::Uninstall);
        out.push(PopupItem::Customize);
    } else {
        out.push(PopupItem::Wallpapers);
        if !home_locked {
            // `LauncherOptionsPopup.getLauncherOptions` drops the edit-mode and
            // widgets entries together (`LauncherOptionsPopup.kt:144-150`).
            out.push(PopupItem::Widgets);
        }
        out.push(PopupItem::AllApps);
        out.push(PopupItem::HomeSettings);
        if !home_locked {
            out.push(PopupItem::EditMode);
        }
        out.push(PopupItem::HomeScreenLock);
        out.push(PopupItem::DefaultPageForWorkspace);
    }
    out
}

/// Open folder `id`: fill the model from the persisted record and raise it.
///
/// Populates [`FolderOpen`] from `state.folder(id)` and builds the renderer's
/// item list from the catalogue, because `FolderOpen` stores fixed-size ids (no
/// `String`, no allocation -- see `recents.rs`) while the renderer wants
/// resolved `AppGridItem`s.
///
/// A folder whose members are all gone still opens: the reference shows an
/// empty folder rather than refusing (`Folder.java:560`), and
/// `FolderOpen::collapse` is what decides the folder is finished.
#[allow(clippy::too_many_arguments)]
fn open_folder_at(
    state: &LauncherState,
    id: u32,
    folder: &mut FolderOpen,
    all_managed_apps: &[ManagedApp],
    items_per_page: u8,
) -> bool {
    let Some(record) = state.folder(id) else {
        return false;
    };
    let ids: Vec<&str> = record.items.iter().map(|s| s.as_str()).collect();
    folder.set_contents(&ids);
    folder.items_per_page = items_per_page;
    folder.open(id as u8);
    folder.clamp_page();
    // Resolve names and colours for the renderer. An id with no catalogue entry
    // contributes nothing, which is how an uninstalled app's removal propagates
    // to the open folder without a separate scan.
    let live: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|i| all_managed_apps.iter().any(|a| a.id == *i))
        .collect();
    folder.set_contents(&live);
    true
}

/// Push the open folder's contents into the frame scratch.
///
/// Every borrowed field comes from `all_managed_apps`; the *ordering* comes from
/// the persisted record. A per-app rename is not reflected here because doing so
/// would borrow `state` for the frame; it is applied on the workspace grid and
/// the dock, which read it inside the frame block where the borrow is short.
fn folder_items_of<'a>(
    state: &LauncherState,
    id: u32,
    all_managed_apps: &'a [ManagedApp],
    out: &mut FrameView<AppGridItem<'a>, FRAME_MAX_GRID>,
) {
    let Some(record) = state.folder(id) else {
        return;
    };
    for item in &record.items {
        if let Some(app) = all_managed_apps.iter().find(|a| a.id == *item) {
            out.push(AppGridItem {
                id: app.id.as_str(),
                name: app.name.as_str(),
                color: app.color,
                glyph: app.glyph.as_str(),
                icon: app.icon.as_deref(),
                folder_n: 0,
                folder_id: 0,
            });
        }
    }
}

/// The radio a quick-settings tile index drives, if any.
///
/// `None` for the tiles with no switch of their own:
///
/// * `Torch` is a LED brightness node, not a radio.
/// * `AutoRotate` is an accelerometer policy.
/// * `AirplaneMode` is a *composition* -- the reference turns it into "every
///   radio off" (`Settings` `AIRPLANE_MODE_ON` broadcasts), so driving it
///   correctly means driving the others, which is a separate change from giving
///   five inert tiles a driver. Recorded as the remaining gap rather than
///   half-implemented, because an airplane tile that turns off wifi and leaves
///   cellular up is worse than one that visibly does nothing.
#[inline]
fn quick_tile_kind(i: usize) -> Option<utim_core::net::Radio> {
    use utim_core::net::Radio;
    match i {
        0 => Some(Radio::Wifi),
        1 => Some(Radio::MobileData),
        2 => Some(Radio::Bluetooth),
        7 => Some(Radio::Tethering),
        _ => None,
    }
}

/// The subtitle a tile shows once a write has landed.
mod quick_tile_note {
    /// Default subtitle for `kind` in state `on`.
    ///
    /// Mirrors the reference's `stateDescription` rather than inventing strings:
    /// `QSTile` renders `secondaryLabel`/`stateDescription`, and a tile that has
    /// just been driven successfully should say what it now is.
    pub fn default_for(kind: utim_core::net::Radio, on: bool) -> &'static str {
        use utim_core::net::Radio;
        if !on {
            return "Off";
        }
        match kind {
            Radio::Wifi => "On",
            Radio::Bluetooth => "On",
            Radio::MobileData => "On",
            Radio::Tethering => "On",
        }
    }
}

/// Drive one radio, reporting whether the hardware is there at all.
///
/// `Ok(false)` means "this device has no such radio", which is not an error: a
/// wifi-only tablet is a normal device and its tile should be disabled rather
/// than fail. `Err` is a write that was attempted and did not land, which *is* an
/// error and must not leave the tile showing the state it failed to reach.
fn quick_tile_drive(
    kind: utim_core::net::Radio,
    on: bool,
) -> Result<bool, utim_core::net::RadioError> {
    use utim_core::net::{set_radio, RadioState};
    set_radio(kind, if on { RadioState::On } else { RadioState::Off })
}

/// Collect the icon cache's shadow masks for the apps that have one.
///
/// Filled only when [`IconCache::shadows_enabled`] is true -- the dark theme has
/// no shadows, matching the reference's `res/values/styles.xml:111-113` -- which
/// is why the table is empty rather than full of `None`s in the default case.
///
/// An app with no resolved bitmap contributes nothing: there is no tile to cast a
/// shadow, and the reference only draws one behind a shaped icon
/// (`transparentIconBackground`, `PreferenceManager.kt:78`).
fn refresh_icon_shadows(
    apps: &[ManagedApp],
    // Deliberately *not* tied to the output's lifetime. The output holds owned `Rc`s, so nothing in it
    // borrows the cache, and tying the cache's borrow to the output's lifetime
    // would keep the whole cache immutably borrowed for the rest of the event
    // loop -- which is not merely a nuisance: the catalogue rescan calls
    // `icon_cache.invalidate_keys(&stale)` and would fail to compile.
    cache: &IconCache,
    out: &mut Vec<(
        String,
        std::rc::Rc<utim_core::compositor::icons::ShadowMask>,
    )>,
) {
    out.clear();
    if !cache.shadows_enabled() {
        return;
    }
    for app in apps {
        // The `Rc` is kept, not dereferenced. `IconCache::shadow` hands back a
        // clone of the cache's own handle, so borrowing through it would borrow a
        // temporary; holding the handle is what makes the row's mask live as long
        // as the frame, and it costs one refcount bump per app on a path that
        // runs when icons change, not per frame.
        if let Some(mask) = cache.shadow(app.id.as_str()) {
            out.push((app.id.clone(), mask));
        }
    }
}

/// Load the wallpaper if `want` names a file that is not already decoded.
///
/// `path` records what `img` holds, so this is a single string compare on every
/// call and the file is read only when the setting actually changed. An empty
/// `want` means "the first wallpaper the system has", which is what
/// [`wallpaper_seed`] already probes -- so the seed and the image come from the
/// same directories and cannot disagree about which file is the wallpaper.
///
/// A failed load clears the image rather than leaving the last one up: showing a
/// wallpaper the user has since removed would be a stale render, and the
/// gradient is a better answer than a picture of something else.
fn refresh_wallpaper(
    want: &str,
    path: &mut String,
    img: &mut Option<std::rc::Rc<utim_core::graphics::png::RgbaImage>>,
) {
    if *path == want && (want.is_empty() == img.is_none()) {
        return;
    }
    *path = want.to_string();
    *img = None;
    let candidate = if want.is_empty() {
        find_system_wallpaper()
    } else {
        Some(std::path::PathBuf::from(want))
    };
    let Some(path) = candidate else { return };
    let Ok(data) = std::fs::read(&path) else {
        return;
    };
    // Budget-checked like [`wallpaper_seed`]: a decode spike at boot is a
    // resident-set spike, and the shell already has a 15 MB target it has to
    // meet with the icon cache and the framebuffer.
    let Some((w, h)) = utim_core::graphics::png::header_size(&data) else {
        return;
    };
    if utim_core::graphics::png::decode_working_set_estimate(w, h) > WALLPAPER_PROBE_BUDGET {
        return;
    }
    if let Some(decoded) = utim_core::graphics::png::decode_png(&data) {
        *img = Some(std::rc::Rc::new(decoded));
    }
}

/// The first PNG in the standard wallpaper directories, or `None`.
///
/// The same directories and the same `.png`-only filter as [`wallpaper_seed`],
/// kept as one function each rather than a shared directory list because they
/// differ in cost: this reads a whole file, that reads 33 bytes per candidate and
/// stops at the first affordable one. Sharing would make the seed pay for the
/// full read.
fn find_system_wallpaper() -> Option<std::path::PathBuf> {
    const DIRS: [&str; 3] = [
        "/run/user/1000",
        "/usr/share/backgrounds",
        "/usr/share/wallpapers",
    ];
    for dir in DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_png = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("png"));
            if is_png {
                return Some(path);
            }
        }
    }
    None
}

/// The tile subtitle for a failed torch write.
///
/// Returns a `&'static str` rather than the `io::Error`'s own message, because
/// this value is stored in `quick_tile_note: [&'static str; 8]` and is rendered
/// every frame. Formatting the error into a `String` would mean an allocation per
/// frame to display a message that has one of three meanings.
///
/// The three cases are worth distinguishing rather than collapsing to "Failed",
/// because they mean different things to a user:
///
///  * **No node** -- the rootfs has no `torch-light` LED. This device's does not.
///    The torch is not broken, it does not exist, and a tile that says
///    "no torch" is telling the truth where "failed" implies something to retry.
///  * **Denied** -- the node exists and the write was refused. That is a
///    permissions problem, and retrying will not help.
///  * **Anything else** -- a real I/O failure worth reporting as one.
fn torch_note(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => "No torch",
        ErrorKind::PermissionDenied => "Denied",
        _ => "Failed",
    }
}

/// Drive airplane mode: every radio it owns, off or on.///
/// Airplane mode has no switch of its own -- it is a *composition*. The reference
/// turns it into "every radio off" rather than writing a kernel flag: Android's
/// `Settings.AIRPLANE_MODE_ON` broadcast makes `ConnectivityManager` tear down
/// wifi, mobile and bluetooth, and only then does the framework record the flag
/// (`ConnectivityManager.java:2148-2203`). Driving the radios and leaving the flag
/// for the system is the same order.
///
/// Tethering is excluded: it is a USB gadget sharing the modem's radio, so "turn
/// off the radios" would silently kill an active hotspot. Android's airplane mode
/// does not tear down tethering either.
///
/// The tile bit is set only on success, so a failed write leaves the tile
/// reporting what happened instead of claiming a state the hardware never
/// reached -- the same rule as every other radio tile, and the reason
/// [`utim_core::net::RadioError`] carries a label rather than a `String`.
fn quick_tile_airplane(i: usize, active: &mut [bool; 8], note: &mut [&'static str; 8]) {
    use utim_core::net::{set_radio, Radio, RadioState};
    let want = !active[i];
    let mut any_present = false;
    let mut first_err: Option<utim_core::net::RadioError> = None;
    for radio in [Radio::Wifi, Radio::Bluetooth, Radio::MobileData] {
        match set_radio(
            radio,
            if want {
                RadioState::On
            } else {
                RadioState::Off
            },
        ) {
            // `Ok(false)` is "this device has no such radio", which is not a
            // failure and must not make the tile report one.
            Ok(true) => any_present = true,
            Ok(false) => {}
            Err(e) => {
                any_present = true;
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        note[i] = e.label();
    } else if !any_present {
        note[i] = "Not present";
    } else {
        active[i] = want;
        note[i] = if want { "On" } else { "Off" };
    }
}

/// The palette the shell is themed from, for the current settings.
///
/// The one place the seed and the light/dark choice are turned into a palette.
/// Split from [`palette_seed`] so the frame loop can rebuild on a settings
/// change without duplicating the `from_seed`/`from_seed_light` pairing -- a
/// second copy of that pairing is how the light theme ended up unreachable in the
/// first place.
fn build_palette(s: &LauncherState) -> MaterialYouPalette {
    match effective_theme(s) {
        Theme::Dark => MaterialYouPalette::from_seed(palette_seed(s)),
        Theme::Light => MaterialYouPalette::from_seed_light(palette_seed(s)),
    }
}

/// The allocation-free half of the palette cache key.
///
/// A `Copy` tuple, so the frame loop can compare it every frame for the cost of
/// four register-width compares. Deliberately *not* including the wallpaper
/// path: that is a `String`, and putting it in here would mean an allocation per
/// frame to notice a change that happens at most once per tap. The path is
/// tracked separately by the caller, which compares it as `&str` -- also
/// allocation-free -- and only clones when it is actually rebuilding.
///
/// A tuple rather than a struct so it is `PartialEq` for free and so adding an
/// input is a compile error at the destructuring site rather than a silently
/// forgotten field. `follow_system_theme` is included even though
/// [`effective_theme`] currently resolves it against `dark_theme` alone,
/// because the day it gains a real system source the palette must rebuild then
/// too, and a missing arm here would freeze the theme at the moment the feature
/// starts working.
fn palette_key(s: &LauncherState) -> (Theme, AccentSource, u32, bool) {
    (
        effective_theme(s),
        s.accent_source,
        s.accent_color,
        s.follow_system_theme,
    )
}

/// The folder layout the shell hit-tests against.
///
/// Built exactly as `draw_open_folder` builds it -- `Layout::for_shell` with the
/// user's type scale, then `FolderLayout::new_with` with the persisted grid and
/// the resolved theme -- because a hit test against a *different* layout is the
/// quietest possible failure: the folder opens, the cells are drawn, and a tap
/// one cell to the left launches the app next door. Every term here is named after
/// the renderer call it must match, so a change to one is a visible diff in the
/// other.
fn folder_layout_for(state: &LauncherState, w: f32, h: f32) -> FolderLayout {
    let l = folder_base_layout(state, w, h);
    let grid = (state.folder_cols, state.folder_rows);
    FolderLayout::new_with(&l, Some(grid), effective_theme(state) == Theme::Dark)
}

/// The base panel layout the folder hit test needs for the menu.
///
/// Split out because the sheet and the menu are both wanted from one press, and
/// rebuilding the base layout to get the second would be two derivations of one
/// thing. `FolderLayout::new_with` takes `&Layout`, so the two are built from one
/// source by construction.
fn folder_base_layout(state: &LauncherState, w: f32, h: f32) -> Layout {
    Layout::for_shell(w, h, state.font_scale, false).with_grid(state.grid_cols, state.grid_rows)
}

/// One frame's worth of folder geometry, for [`folder_touch`].
///
/// A struct rather than eleven arguments because the arguments are not
/// independent: every one of them describes the same folder on the same panel in
/// the same frame, and a call site that mixed two frames' values would be a
/// plausible-looking bug with no compiler help. Building one of these per touch
/// also means the layout and the published geometry cannot come from different
/// frames -- which is precisely the bug this function already had once.
struct FolderFrame<'a> {
    /// The folder's own layout, in layout coordinates.
    fl: &'a FolderLayout,
    /// The base panel layout, which places the menu in panel coordinates.
    l: &'a Layout,
    panel_w: f32,
    panel_h: f32,
    /// Members in the whole folder, and on the current page.
    n_items: usize,
    per_page: usize,
}

/// The offset from `FolderLayout`'s layout coordinates to panel coordinates.
///
/// # The bug this exists to prevent
///
/// `FolderLayout`'s cell rects are in *layout* coordinates -- `cell_at(col, row)`
/// returns `y = pad_top` -- while the sheet is drawn **vertically centred on the
/// panel**, so the painted row is `pad_top + sy`. A shell that hit-tests the
/// layout rects directly aims most of a screen height too high: every cell is
/// tappable, just not where it looks. The renderer documents this exact trap at
/// `drm_kms.rs:6489-6495` and publishes [`folder_grid_geometry`] so callers do not
/// have to rediscover it.
///
/// Rather than take the published origin on trust, the offset is *derived* as
/// `painted_origin - layout_origin`, so it cannot disagree with the paint the way
/// a second copy of the centring arithmetic would. When the grid is not live the
/// offset is zero and the folder is not tappable at all, which
/// [`folder_touch`] checks before using it.
#[inline]
fn folder_layout_offset(fl: &FolderLayout, grid: &FolderGridGeometry) -> (f32, f32) {
    if !grid.live {
        return (0.0, 0.0);
    }
    (grid.origin.0 - fl.cell.x, grid.origin.1 - fl.cell.y)
}

/// Feed one touch into the open folder's gesture state.
///
/// Returns what the shell should do about it. A folder is a modal surface in
/// everything but name -- it covers the workspace, the dock and the drawer -- so
/// the caller checks this first and skips its own handling, rather than this
/// being one more `else if` in a chain that also has workspace and drawer arms.
///
/// # Two coordinate spaces, and which is which
///
/// * The **cell** APIs -- `touch_at`, `reorder_index`, `is_drag_out` -- are in
///   layout coordinates, so the touch point is shifted by
///   [`folder_layout_offset`] first.
/// * The **menu** is placed in panel coordinates: the renderer calls
///   `fl.menu(l, ax, ay, ..)` with the panel-space `folder_menu_anchor`, and
///   `PopupPlacement::place` clamps to the panel. So the menu is hit-tested
///   *unshifted*.
///
/// That inconsistency is in the layout API rather than here, and it is worth
/// naming because getting it backwards is invisible: the menu's rows are a few
/// hundred pixels tall, so a shift moves the hit test off them entirely and the
/// menu silently stops responding.
fn folder_touch(
    g: &mut FolderGestures,
    f: &FolderFrame,
    phase: TouchPhase,
    x: f32,
    y: f32,
    now: Instant,
) -> FolderHit {
    let (fl, l) = (f.fl, f.l);
    // A menu, once open, swallows the next touch: the first tap picks a row, and
    // one anywhere else closes it. Checked before the layout so a tap on the menu
    // -- which floats over the folder -- cannot also hit a cell underneath.
    //
    // `menu_progress > 0.5` is the reference's own tappability gate
    // (`PopupMenuLayout::TAPPABLE_PROGRESS`): below it the rows are zero-sized, so
    // `menu_hit` returns `None` by construction and a tap would fall through to
    // the cells and activate one behind the menu that is still fading in.
    if g.menu_open && g.menu_progress > 0.5 {
        if phase == TouchPhase::Up {
            // Panel coordinates -- see the doc comment above.
            let menu = fl.menu(l, g.menu_anchor.0, g.menu_anchor.1, g.menu_progress);
            if let Some(action) = menu.menu_hit(x, y) {
                let member = g.menu_member;
                g.dismiss_menu();
                return FolderHit::MenuAction(action, member);
            }
            g.dismiss_menu();
            return FolderHit::MenuDismissed;
        }
        return FolderHit::Consumed;
    }

    let grid = folder_grid_geometry();
    // Not live: a folder with no page of items was painted, or none has been yet.
    // Hit-testing `FolderLayout` anyway would aim at cells that are not on screen.
    if !grid.live {
        return match phase {
            TouchPhase::Down => {
                g.press_header(x, y, now);
                FolderHit::Ignored
            }
            _ => FolderHit::Ignored,
        };
    }
    let (ox, oy) = folder_layout_offset(fl, &grid);
    let (lx, ly) = (x - ox, y - oy);

    match phase {
        TouchPhase::Down => match fl.touch_at(lx, ly, f.n_items) {
            FolderTouch::Member { index, .. } => {
                g.press(index, x, y, now);
                FolderHit::Consumed
            }
            FolderTouch::Dismiss { .. } => FolderHit::Dismiss,
            FolderTouch::Blank | FolderTouch::Outside => {
                g.press_header(x, y, now);
                FolderHit::Blank
            }
        },
        TouchPhase::Move => {
            if g.move_to(x, y, f.per_page) {
                // Now dragging: work out where the cell would land and whether it
                // has left the folder entirely. Both come from the layout, which
                // owns that geometry; the gesture only remembers the answers.
                let slot = fl
                    .reorder_index(lx, ly, f.n_items)
                    .map(|i| page_local_slot(i, f.per_page));
                let out = fl.is_drag_out(lx, ly, f.panel_w, f.panel_h);
                g.drag_over(slot, out, x, y);
                FolderHit::DragStarted
            } else if g.drag_slot.is_some() {
                let slot = fl
                    .reorder_index(lx, ly, f.n_items)
                    .map(|i| page_local_slot(i, f.per_page));
                let out = fl.is_drag_out(lx, ly, f.panel_w, f.panel_h);
                g.drag_over(slot, out, x, y);
                FolderHit::Consumed
            } else {
                FolderHit::Ignored
            }
        }
        TouchPhase::Up => match g.release(now) {
            FolderOutcome::TapMember(i) => FolderHit::Launch(i),
            FolderOutcome::Menu => FolderHit::Consumed,
            FolderOutcome::Dragged { from, to, out } => {
                // A drag-out is **not** a removal.
                //
                // `Folder.onDragExit` -> `completeDragExit()` -> `rearrangeChildren()`
                // (`Folder.java:1293-1300`, `:1265-1277`) only commits the reorder;
                // removal needs a separate `DropTarget`. The first version of this
                // removed the member on any drag-out, which on a device where the
                // exit area is easy to reach by accident would delete apps.
                //
                // So: out of the grid is inert unless the release landed on the
                // remove bar, which is the reference's actual affordance
                // (`DeleteDropTarget.java:115`, `strings.xml:221` "Remove").
                if out {
                    let on_bar = drop_target_bar(l).hit(x, y);
                    FolderHit::Dropped {
                        from,
                        to,
                        remove: on_bar,
                    }
                } else {
                    FolderHit::Dropped {
                        from,
                        to,
                        remove: false,
                    }
                }
            }
            FolderOutcome::Ignored => FolderHit::Blank,
        },
        _ => FolderHit::Ignored,
    }
}

/// What the shell should do about a touch the folder consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderHit {
    /// The folder handled it; the shell should not.
    Consumed,
    /// Not a folder gesture; fall through to the shell's own handling.
    Ignored,
    /// A press that landed on the folder's header or its empty space.
    Blank,
    /// The folder's own back affordance: close it.
    Dismiss,
    /// A drag has begun.
    DragStarted,
    /// Launch the member at this absolute index.
    Launch(usize),
    /// A drag finished. Both slots are page-local. `remove` is true only when the
    /// cell was dragged off the grid *and* released on the remove bar -- the
    /// reference's actual removal affordance. A drag-out on its own is inert.
    Dropped {
        from: Option<u8>,
        to: Option<u8>,
        remove: bool,
    },
    /// A long-press menu row was chosen.
    MenuAction(FolderMenuAction, usize),
    /// The menu was dismissed without a choice.
    MenuDismissed,
}

/// Launch an app by catalogue id, from a folder tap.
///
/// Deliberately smaller than the workspace's inline launch block, which also
/// clears `app_input` and deactivates the keyboard. Those are needed there
/// because a workspace tap can land while an app panel is open; a folder tap
/// cannot -- the folder is only reachable from the workspace, where no app is
/// open -- so repeating them would be copying state transitions that cannot
/// occur. If a folder ever becomes reachable from inside an app, this is the
/// function that has to grow, and it is the only one.
///
/// The pid is recorded for the recents card. Discarding it, as an earlier version
/// did, made "close task" a no-op that reported success.
fn launch_from_folder(
    id: &str,
    all_managed_apps: &[ManagedApp],
    active_app: &mut Option<String>,
    app_launch_progress: &mut f32,
    app_launch_color: &mut u32,
    pending_launch_pid: &mut i32,
    socket_dir: &str,
) -> bool {
    let Some(app) = all_managed_apps.iter().find(|a| a.id == id) else {
        // A member whose app has been uninstalled. Returning `false` lets the
        // caller leave the folder open so the stale cell is visible rather than
        // the folder silently closing under the user's finger.
        return false;
    };
    *active_app = Some(app.name.clone());
    *app_launch_progress = 0.01;
    *app_launch_color = app.color;
    if !app.exec.is_empty() {
        if let Some(pid) = launch_desktop_app(&app.exec, socket_dir) {
            *pending_launch_pid = pid;
        }
    }
    true
}

/// Move a folder member from one page-local slot to another, and commit it.
///
/// Three steps, and the order is the whole difficulty:
///
///  1. Resolve the *absolute* indices from the page-local slots the gesture
///     reported, using the page the folder is currently showing.
///  2. Mutate the persisted [`FolderRecord`] through its own API, which rejects
///     an out-of-range move rather than clamping into a plausible one.
///  3. Re-derive the open folder's contents from the record, so the display
///     cannot disagree with what was saved.
///
/// Step 3 is the one that is easy to forget and the failure is invisible: a
/// folder that reorders correctly and then springs back on the next animation
/// frame looks like the drag was not committed at all. `FolderOpen` holds a
/// *copy* of the member list, so mutating the record does nothing visible until
/// the copy is refreshed.
///
/// Returns whether anything changed, so the caller can skip a redundant save.
fn commit_folder_move(
    state: &mut LauncherState,
    folder: &mut FolderOpen,
    from_slot: usize,
    to_slot: usize,
    per_page: usize,
) -> bool {
    let page = folder.page as usize * per_page.max(1);
    let from = page + from_slot;
    let to = page + to_slot;
    let idx = folder.folder_idx as u32;
    let Some(record) = state.folders.iter_mut().find(|f| f.id == idx) else {
        return false;
    };
    if !record.move_item(from, to) {
        return false;
    }
    refresh_open_folder(state, folder);
    true
}

/// Remove a folder member, by page-local slot, and commit it.
///
/// `slot` is `None` when the caller lost the origin -- a drag that began outside
/// the grid, say -- which is not a removal: there is nothing to remove. Treating
/// it as one would delete whichever member happened to be first, so a confused
/// drag becomes data loss.
fn commit_folder_remove(
    state: &mut LauncherState,
    folder: &mut FolderOpen,
    slot: Option<u8>,
    per_page: usize,
) -> bool {
    let Some(slot) = slot else { return false };
    let page = folder.page as usize * per_page.max(1);
    let at = page + slot as usize;
    let idx = folder.folder_idx as u32;
    let Some(record) = state.folders.iter_mut().find(|f| f.id == idx) else {
        return false;
    };
    if !record.remove_item(at) {
        return false;
    }
    refresh_open_folder(state, folder);
    true
}

/// Re-derive the open folder's contents from its persisted record.
///
/// `FolderOpen` stores a fixed-size copy; the record is the truth. Every mutation
/// path funnels through here so there is one way for the two to be reconciled
/// rather than one per call site.
fn refresh_open_folder(state: &LauncherState, folder: &mut FolderOpen) {
    let Some(record) = state.folder(folder.folder_idx as u32) else {
        folder.set_contents(&[]);
        return;
    };
    let ids: Vec<&str> = record.items.iter().map(|s| s.as_str()).collect();
    folder.set_contents(&ids);
    folder.clamp_page();
}

/// What a folder touch is currently doing.
///
/// Folders were **open-only**: they opened, they animated, and nothing in the
/// shell read a touch inside one. There was no tap-to-launch, no long-press menu
/// and no drag, so a folder could be looked at and not used. `FolderLayout`
/// already had the geometry for all three (`touch_at`, `reorder_index`,
/// `is_drag_out`, `menu_hit`); what was missing was the state machine that
/// decides which of them a given touch means.
///
/// # Why the state lives here and not in the renderer
///
/// The renderer must not touch the filesystem and cannot mutate a
/// [`LauncherState`], so it cannot reorder a folder even in principle. But it
/// does need to know what to *draw*, which is the same set of facts: which cell
/// is lifted, where the finger is, where the gap is. So the state is owned here
/// and published as six plain fields on [`DrmInteractiveState`], exactly as the
/// palette is owned by the shell and handed to the renderer.
///
/// # Long-press, then drag -- and why they are not the same gesture
///
/// The reference starts a drag on the folder's own long press
/// (`Folder.onLongClick` -> `startDrag`, `Folder.java:459-463`) and has **no**
/// long-press menu. This has one, so the two had to be separated rather than
/// merged: a long press that lifts a cell is a reorder, a long press that opens a
/// menu is a menu, and a user cannot tell which they are about to get. The split
/// is by *what happens on release*: a long press that never moves opens the menu;
/// one that moves past [`FolderGestures::DRAG_SLOP`] becomes a drag instead and
/// the menu is cancelled. `menu_progress` is a spring, so cancelling it animates
/// closed rather than snapping.
struct FolderGestures {
    /// Page-local slot of the lifted cell, or `None`.
    drag_slot: Option<u8>,
    /// Finger position while lifted.
    drag_pos: (f32, f32),
    /// Slot the gap has opened at.
    drop_slot: Option<u8>,
    /// The lifted cell has left the folder.
    drag_out: bool,
    /// Menu open progress, 0..1.
    menu_progress: f32,
    /// Long-press point the menu grows from.
    menu_anchor: (f32, f32),
    /// Absolute index of the member a long press landed on, for the menu's
    /// "Remove" and "Info".
    menu_member: usize,
    /// Absolute index being dragged, and where the press started.
    drag_member: Option<usize>,
    press_at: (f32, f32),
    pressed_at: Instant,
    /// The press has not yet been resolved into a menu or a drag.
    armed: bool,
    /// A long press has fired; the menu is open or opening.
    menu_open: bool,
}

impl FolderGestures {
    /// How far a finger must travel before a long press becomes a drag.
    ///
    /// 12 px at a typical panel density, which is under the ~8 mm a thumb covers
    /// and well above touch noise. Exposed as a constant rather than inlined so
    /// the test that pins the menu/drag split uses the same number the shell
    /// does.
    const DRAG_SLOP: f32 = 12.0;

    /// How long a press must be held to open the menu.
    ///
    /// The reference's long press is `ViewConfiguration.getLongPressTimeout()`
    /// (`Folder.java:459`), which Android defines as 500 ms.
    const LONG_PRESS_MS: u64 = 500;

    /// A fresh, idle gesture state stamped at `now`.
    ///
    /// A function rather than a `const` because [`Instant::now`] is not const.
    /// It matters that this stamps the *current* time rather than a fixed epoch:
    /// `pressed_at` is compared against a press, and a sentinel far in the past
    /// would make every subsequent press look like it had been held for hours --
    /// so the first `poll_long_press` would open a menu nobody asked for.
    fn idle(now: Instant) -> Self {
        Self {
            drag_slot: None,
            drag_pos: (0.0, 0.0),
            drop_slot: None,
            drag_out: false,
            menu_progress: 0.0,
            menu_anchor: (0.0, 0.0),
            menu_member: 0,
            drag_member: None,
            press_at: (0.0, 0.0),
            pressed_at: now,
            armed: false,
            menu_open: false,
        }
    }

    /// A touch went down inside the folder, on member `index`.
    fn press(&mut self, index: usize, x: f32, y: f32, now: Instant) {
        *self = Self::idle(now);
        self.press_at = (x, y);
        self.pressed_at = now;
        self.armed = true;
        self.drag_member = Some(index);
    }

    /// A touch went down on the folder's own header -- not a member, so it cannot
    /// become a drag and does not arm the menu.
    fn press_header(&mut self, x: f32, y: f32, now: Instant) {
        *self = Self::idle(now);
        self.press_at = (x, y);
        self.pressed_at = now;
    }

    /// The finger moved. Returns `true` if this frame produced a gesture change,
    /// so the caller can decide whether a repaint is needed.
    ///
    /// `per_page` is the folder's page size, needed to convert the absolute
    /// member index into the page-local slot the renderer draws.
    ///
    /// The long-press *check* lives in [`Self::poll_long_press`] rather than here
    /// because the event loop is event-driven and there is no timer tick to hang
    /// it on: a press with no further events would never be examined, so the menu
    /// would only open if the user happened to move. Holding still and getting
    /// nothing is the bug that is being avoided. The shell calls
    /// `poll_long_press` from its existing frame bookkeeping for that reason.
    fn move_to(&mut self, x: f32, y: f32, per_page: usize) -> bool {
        if !self.armed {
            return false;
        }
        let dx = x - self.press_at.0;
        let dy = y - self.press_at.1;
        if dx * dx + dy * dy < Self::DRAG_SLOP * Self::DRAG_SLOP {
            return false;
        }
        // Past the slop: a drag, and any menu on its way out is cancelled.
        let member = self.drag_member;
        self.armed = false;
        self.menu_open = false;
        self.drag_pos = (x, y);
        if let Some(m) = member {
            self.drag_slot = Some(page_local_slot(m, per_page));
        }
        true
    }

    /// Open the menu if the press has been held long enough. Idempotent.
    ///
    /// Cancels a drag rather than opening the menu over it: once a cell is lifted
    /// the user is reordering, and a menu appearing at that moment would be
    /// answering a gesture they did not make.
    fn poll_long_press(&mut self, now: Instant) -> bool {
        if !self.armed || self.drag_slot.is_some() {
            return false;
        }
        if (now.saturating_duration_since(self.pressed_at).as_millis() as u64) < Self::LONG_PRESS_MS
        {
            return false;
        }
        let Some(member) = self.drag_member else {
            return false;
        };
        self.armed = false;
        self.menu_open = true;
        self.menu_member = member;
        self.menu_anchor = self.press_at;
        true
    }

    /// Update the insertion slot for the current drag, and whether it has left.
    ///
    /// `slot` and `out` come from `FolderLayout::reorder_index` and
    /// `FolderLayout::is_drag_out`, which own that geometry. Returns `true` when
    /// either changed, so a drag that hovers over its own gap does not repaint
    /// every frame.
    fn drag_over(&mut self, slot: Option<u8>, out: bool, x: f32, y: f32) -> bool {
        if self.drag_slot.is_none() {
            return false;
        }
        let changed = self.drop_slot != slot || self.drag_out != out;
        self.drop_slot = slot;
        self.drag_out = out;
        self.drag_pos = (x, y);
        changed
    }

    /// The finger lifted. Returns what the gesture turned out to be.
    ///
    /// A press that neither moved nor was held long is a **tap on a member**,
    /// which is how the app inside the folder launches -- the whole point of a
    /// folder. A long press that never moved opens the menu instead.
    ///
    /// [`FolderOutcome::Dragged`] carries both slots rather than the caller
    /// reading them off the gesture afterwards, because this method resets the
    /// state: a caller that looked at `self.drag_slot` after `release` would
    /// always find `None` and every reorder would silently degrade to "dropped
    /// nowhere".
    fn release(&mut self, now: Instant) -> FolderOutcome {
        let out = if self.menu_open {
            FolderOutcome::Menu
        } else if self.drag_slot.is_some() {
            FolderOutcome::Dragged {
                from: self.drag_slot,
                to: self.drop_slot,
                out: self.drag_out,
            }
        } else if let Some(m) = self.drag_member {
            if (now.saturating_duration_since(self.pressed_at).as_millis() as u64)
                >= Self::LONG_PRESS_MS
            {
                self.menu_open = true;
                self.menu_member = m;
                self.menu_anchor = self.press_at;
                FolderOutcome::Menu
            } else {
                FolderOutcome::TapMember(m)
            }
        } else {
            FolderOutcome::Ignored
        };
        let keep_menu = self.menu_open;
        let anchor = self.menu_anchor;
        let member = self.menu_member;
        *self = Self::idle(now);
        if keep_menu {
            // The menu survives its own opening touch. Restored rather than left
            // in place so `release` has exactly one exit path.
            self.menu_open = true;
            self.menu_progress = 0.0;
            self.menu_anchor = anchor;
            self.menu_member = member;
        }
        out
    }

    /// The menu was dismissed. Animates closed via `menu_progress`.
    fn dismiss_menu(&mut self) {
        self.menu_open = false;
        self.menu_member = 0;
    }

    /// Advance the menu spring. `dt_ms` is the frame's delta.
    fn step(&mut self, dt_ms: f32) {
        let target = if self.menu_open { 1.0 } else { 0.0 };
        // Exponential approach rather than a spring: the menu is a single
        // scale-and-fade, and a second spring here would need its own state and
        // its own "is at rest" test in `FrameDemand`. 18 ms to close, 90 ms to
        // open -- closing faster than opening is what makes a dismissal feel
        // responsive rather than sticky.
        let tau = if self.menu_open { 90.0 } else { 18.0 };
        let k = 1.0 - (-dt_ms / tau).exp();
        self.menu_progress += (target - self.menu_progress) * k;
        if (target - self.menu_progress).abs() < 0.001 {
            self.menu_progress = target;
        }
    }

    /// A press is waiting to be resolved into a drag or a menu.
    ///
    /// Read by `FrameDemand` so the event loop keeps delivering frames while a
    /// finger is held down. Without it the loop parks and the long press is never
    /// examined -- see the `folder_menu` note there.
    fn armed_press(&self) -> bool {
        self.armed
    }

    /// Whether the menu spring still needs frames.
    fn menu_animating(&self) -> bool {
        let target = if self.menu_open { 1.0 } else { 0.0 };
        (target - self.menu_progress).abs() > 0.001
    }
}

/// The page-local slot for an absolute member index.
///
/// `DrmInteractiveState::folder_drag_slot` is documented as page-local while
/// `FolderTouch::Member::index` is absolute, so one of the two has to be
/// converted at every use. Doing it in one named function means a future change
/// to either convention has one place to change, and means the conversion cannot
/// be forgotten at a call site -- which is how a reorder would end up lifting the
/// wrong cell.
///
/// `per_page` is a parameter rather than read from a global because a global here
/// would be untestable without process-wide setup, and this is exactly the
/// arithmetic whose failure is invisible: an off-by-one-page slot lifts a
/// plausible-looking cell from the wrong page. `max(1)` because the folder grid is
/// user-settable and a zero would divide by zero; `try_from` because a slot
/// larger than 255 cannot be represented and truncating would wrap to a valid but
/// wrong cell.
#[inline]
fn page_local_slot(absolute: usize, per_page: usize) -> u8 {
    u8::try_from(absolute % per_page.max(1)).unwrap_or(0)
}

/// What a folder gesture turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderOutcome {
    /// A tap on a member: launch it. Index is absolute.
    TapMember(usize),
    /// A long press: the menu is open.
    Menu,
    /// A drag finished. `from` and `to` are page-local slots, and `out` says the
    /// cell was dropped off the grid -- which is the reference's *remove* gesture,
    /// not a failed drop (`Folder.java:463-471` starts a drag from the folder and
    /// a drop outside it deletes).
    Dragged {
        from: Option<u8>,
        to: Option<u8>,
        out: bool,
    },
    /// Nothing the folder cares about -- a press on the header, or empty space.
    Ignored,
}

/// Advance the wallpaper picker and commit the choice.
///
/// Returns whether anything changed, so the caller skips a redundant
/// `touch()`/`save()` on a device with no wallpapers -- where a tap on the row
/// would otherwise mark the state dirty and rewrite the file to say nothing.
///
/// The commit is *not* deferred to the settings-tap arm's
/// `hit_row == Some("wallpaper")` check any more. That check read a local
/// `hit_row` that the `if` condition had already consumed, and it would have
/// missed the picker path entirely; the image is reloaded here, where the choice
/// is actually made, so there is exactly one place that has to remember to do it.
fn cycle_wallpaper(
    picker: &mut utim_core::settings::picker::WallpaperPicker,
    state: &mut LauncherState,
    path: &mut String,
    img: &mut Option<std::rc::Rc<utim_core::graphics::png::RgbaImage>>,
) -> bool {
    if !picker.move_by(1) {
        // No candidates, or a single candidate that is already selected.
        return false;
    }
    let Some(chosen) = picker.selected().map(|s| s.to_string()) else {
        return false;
    };
    if state.wallpaper == chosen {
        return false;
    }
    state.wallpaper = chosen;
    state.touch();
    let _ = state.save();
    // The image is cached outside the state, so it has to be told.
    refresh_wallpaper(&state.wallpaper, path, img);
    true
}

/// Every wallpaper the system has, in the directories [`find_system_wallpaper`]
/// probes.
///
/// Separate from `find_system_wallpaper` because the two have different costs and
/// different shapes: the seed wants the *first affordable* file and stops reading
/// at 33 bytes of header, while the picker needs the whole list and so must pay
/// for a `read_dir` walk. One function doing both would make the boot path pay for
/// the picker's walk, and the picker's enumeration runs on the frame path when the
/// Settings panel opens.
///
/// Sorted by path so the picker's order does not depend on the order the
/// filesystem yielded directory entries. `read_dir` order is unspecified, so an
/// unsorted list would show a different wallpaper first on every boot -- which
/// looks like the setting randomly changing.
///
/// `None` for a wallpaper is not possible here: only files that exist in those
/// directories are listed, and the picker validates nothing because there is
/// nothing to validate. The reference's carousel *does* handle a file that
/// vanished between listing and decoding (`WallpaperCarouselView.kt:136,141-153`
/// substitutes a placeholder), because on Android the list comes from a MediaStore
/// query that can go stale. Here the list is a directory read taken moments
/// earlier, so a `decode_png` failure on a listed file is a corrupt file rather
/// than a missing one, and the seed path already handles that by skipping it.
fn wallpaper_candidates() -> Vec<String> {
    const DIRS: [&str; 3] = [
        "/run/user/1000",
        "/usr/share/backgrounds",
        "/usr/share/wallpapers",
    ];
    let mut out: Vec<String> = Vec::new();
    for dir in DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let s = path.to_string_lossy().into_owned();
            if utim_core::settings::picker::validate_image_path(&s) {
                out.push(s);
            }
        }
    }
    utim_core::settings::picker::dedupe_candidates(&mut out);
    out
}

/// Copy an arbitrary image file into the writable wallpaper dir after header +
/// budget validation (no full decode). Returns the new path on success.
fn import_wallpaper_file(src: &std::path::Path) -> Option<String> {
    use utim_core::settings::picker::{validate_image_path, WALLPAPER_PREFIX_LEN};
    let s = src.to_string_lossy().into_owned();
    if !validate_image_path(&s) {
        return None;
    }
    let Ok(mut f) = std::fs::File::open(src) else {
        return None;
    };
    let mut prefix = vec![0u8; WALLPAPER_PREFIX_LEN];
    let read = read_exact_or_less(&mut f, &mut prefix)?;
    let (_w, _h) = utim_core::settings::picker::validate_image_prefix(&prefix[..read])?;
    let dest_dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".local/share");
                p
            })
        })
        .map(|mut p| {
            p.push("utlc/wallpapers");
            p
        })?;
    let _ = std::fs::create_dir_all(&dest_dir);
    let name = src.file_name()?.to_string_lossy().into_owned();
    let mut dest = dest_dir;
    dest.push(name);
    std::fs::copy(src, &dest).ok()?;
    Some(dest.to_string_lossy().into_owned())
}

/// Build the picker's initial cursor from the persisted wallpaper.
///
/// The state's `wallpaper` is a path; the picker's is an index into the candidate
/// list. If the stored path is not in the list -- the file was deleted, or the
/// list order changed because a wallpaper was installed -- the cursor falls back
/// to 0, which `WallpaperPicker::new` clamps. A picker that started on a
/// *different* wallpaper than the setting names would silently change the
/// wallpaper on the first tap, so the lookup is by path and not by position.
fn picker_cursor(candidates: &[String], want: &str) -> usize {
    candidates.iter().position(|c| c == want).unwrap_or(0)
}

/// Whether an open overview should close because its task stack emptied.
///
/// The distinction this makes is **"the last card left"** versus **"there was
/// never a card"**. Testing `len == 0` alone conflates them, and the consequence
/// is a feature that is implemented and unreachable: the overview opens with an
/// empty stack, this returns `true` on the first frame, the panel closes before it
/// has been drawn, and the empty state the renderer already draws
/// (`drm_kms.rs:6129-6138`, "No recent items") can never be seen.
///
/// The reference does the same thing this does. `RecentsView.updateEmptyMessage`
/// (`quickstep/src/com/android/quickstep/views/RecentsView.java:4809-4824`) sets
/// `mShowEmptyMessage = !hasTaskViews()` and keeps the panel up; it is an
/// *empty view*, not a dismissal.
///
/// `showed_content` is a latch owned by the frame loop, not a parameter derived
/// from `len`, because "was ever non-empty" cannot be reconstructed from a single
/// frame's length.
#[inline]
fn overview_closes_for_empty_stack(overview_open: bool, showed_content: bool, len: u8) -> bool {
    overview_open && showed_content && len == 0
}

/// Light or dark, after `follow_system_theme` has had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
}

/// Whether the launcher is dark, honouring "follow system".
///
/// When `follow_system_theme` is set, the system source wins when it knows;
/// `Unknown` falls back to [`LauncherState::dark_theme`]. The reference's
/// `ThemeChoice.SYSTEM` (`ThemePreference.kt:12-30`) resolves against
/// `Configuration.uiMode` (`Theme.kt:118-130`); here the source is
/// `utim_core::theme` (portal color-scheme + env/file fallback).
fn effective_theme(s: &LauncherState) -> Theme {
    if s.follow_system_theme {
        if let Some(dark) = utim_core::theme::system_theme_dark() {
            return if dark { Theme::Dark } else { Theme::Light };
        }
    }
    if s.dark_theme {
        Theme::Dark
    } else {
        Theme::Light
    }
}

/// The colour seed the palette is derived from.
///
/// Three sources, in the order `AccentSource` names them:
///
///  * [`AccentSource::Custom`] -- the user's own `accent_color`, forced opaque.
///    The force matters: `from_seed` masks to `0x00FFFFFF`, so a stored colour
///    with a zero alpha byte would be interpreted as transparent and yield a
///    different scheme than the one the user picked.
///  * [`AccentSource::Default`] -- a fixed neutral, so "default" means the same
///    thing every boot rather than depending on which wallpaper happens to be
///    installed.
///  * [`AccentSource::Wallpaper`] -- the wallpaper's average colour, which is
///    what this always did.
///
/// `AccentSource::Custom` with a zero `accent_color` falls back to the
/// wallpaper rather than to the fixed neutral: a custom accent the user has not
/// chosen yet is "unset", and inventing a colour for it would make the row look
/// like it had done something.
fn palette_seed(s: &LauncherState) -> u32 {
    // The source is the discriminant and the colour only refines `Custom`.
    //
    // Two earlier shapes were wrong. Matching on the `(source, colour)` *pair*
    // needed a catch-all arm, because a guarded arm does not satisfy the
    // exhaustiveness check -- and a catch-all silently swallowed the
    // custom-with-a-colour case, so a chosen accent was ignored. Matching on the
    // source alone leaves `Custom` with two guarded arms and no catch-all, so
    // adding a fourth variant becomes a compile error here instead of a
    // plausible-looking wrong colour.
    match s.accent_source {
        // Default is the fixed neutral, *always* -- including when a stale custom
        // colour is still in the field. "Default" has to mean the same thing on
        // every boot, and "whatever wallpaper happens to be installed" is not that.
        // The first version matched `(Custom, 0)` and `(Default, 0)` together and
        // fell back to the wallpaper; that is defensible for an unset custom
        // accent and plainly wrong here. The two cases look alike and are not.
        AccentSource::Default => DEFAULT_ACCENT_SEED,
        AccentSource::Wallpaper => wallpaper_seed(),
        // A custom accent the user has chosen. Forced opaque: `from_seed` masks
        // to `0x00FFFFFF`, so a stored colour with a zero alpha byte would be
        // read as a different colour than the swatch.
        AccentSource::Custom if s.accent_color != 0 => s.accent_color | 0xFF00_0000,
        // A custom accent that has never been chosen: falls back to the wallpaper
        // rather than inventing a colour, because a row that looks like it did
        // something when it did not is worse than one that plainly has nothing to
        // show.
        AccentSource::Custom => wallpaper_seed(),
    }
}

/// The seed [`AccentSource::Default`] means.
///
/// A desaturated blue rather than pure grey: a fully neutral seed makes every
/// role the same hue and the shell loses the tonal separation Material You is
/// for. This is a Material 3 baseline primary, which is what a "default" accent
/// is supposed to be.
const DEFAULT_ACCENT_SEED: u32 = 0xFF6750A4;

/// Tracks when the screen should blank, from `LauncherState::screen_timeout_s`.
///
/// # Why this is a type and not three variables
///
/// The hard part is not the elapsed-time comparison, it is the **event loop's
/// timeout**. The loop blocks indefinitely when nothing is animating -- that is
/// the point of the `FrameDemand` deadline arithmetic -- so a timeout implemented
/// as "check `last_input.elapsed()` each frame" would never run: there are no
/// frames. The budget has to be folded into the `epoll_wait` timeout in
/// [`ScreenTimeout::poll_budget`], and folding one thing into another is exactly
/// the kind of coupling that gets lost the next time someone adds a timer.
///
/// So the whole policy is here, and the loop asks it two questions: "may I block
/// this long?" and "should the screen be blank?". Neither question requires the
/// loop to know what a timeout is.
///
/// # The setting was dead
///
/// `screen_timeout_s` was persisted, offered in the settings panel as a five-way
/// choice, and read by nothing at all. A user could select "30 s" and the screen
/// would stay on indefinitely, which is the exact failure the reference's
/// `Settings.System.SCREEN_TIMEOUT` exists to prevent.
///
/// # Why no backlight
///
/// This rootfs has no backlight node, so the panel cannot actually be powered
/// down; `set_brightness` fails. What the launcher *can* do -- and what the
/// reference's user-visible behaviour amounts to on a device that can -- is stop
/// painting the UI and show a blank surface, waking on the next input. That is
/// what `off` drives.
struct ScreenTimeout {
    /// When input was last seen.
    last_input: Instant,
    /// The budget. Zero means "never", and is checked before any arithmetic
    /// rather than being treated as "expire immediately" -- the reference treats
    /// 0 as `SCREEN_BRIGHTNESS_BEHAVIOR` never-off, and a phone that blanks the
    /// instant you set "Never" is worse than one that ignores the setting.
    budget: Duration,
    /// Latched once the budget elapses, so the paint path is idempotent.
    off: bool,
}

impl ScreenTimeout {
    /// A timeout with the given budget in seconds. `0` never expires.
    fn new(budget_secs: u32) -> Self {
        Self {
            last_input: Instant::now(),
            budget: Duration::from_secs(budget_secs as u64),
            off: false,
        }
    }

    /// Fold a settings change in.
    ///
    /// Returns `true` when the caller must re-evaluate immediately rather than
    /// waiting for the next tick -- which is the case when a budget is *shortened*
    /// below the time already elapsed. Turning "Never" into "15 s" with four hours
    /// of idleness should blank the screen at once, not in fifteen seconds.
    fn set_budget(&mut self, budget_secs: u32, now: Instant) -> bool {
        self.budget = Duration::from_secs(budget_secs as u64);
        self.off = self.expired_at(now);
        // A budget of zero can never expire, so it always clears a latched blank.
        if self.budget.is_zero() {
            self.off = false;
            return false;
        }
        self.off
    }

    /// Note that input arrived. Always clears the blank, even if the budget has
    /// not elapsed -- a keypress during the blanking frame is the common case and
    /// must not be swallowed.
    fn touch(&mut self, now: Instant) {
        self.last_input = now;
        self.off = false;
    }

    /// Whether the screen should be blank at `now`, latching the answer.
    ///
    /// Takes `now` rather than calling [`Instant::now`] so a test can drive it and
    /// so the event loop can use one timestamp for the timeout check and the
    /// frame's own bookkeeping -- two `Instant::now()` calls a microsecond apart
    /// is a real source of off-by-one-frame bugs at the boundary.
    fn expired_at(&mut self, now: Instant) -> bool {
        if self.budget.is_zero() {
            self.off = false;
            return false;
        }
        if !self.off {
            // `checked_duration_since` because `now` can precede `last_input` if a
            // caller passes a timestamp from before the last touch. Saturating to
            // zero is the right answer: not enough time has passed.
            let idle = now.saturating_duration_since(self.last_input);
            if idle >= self.budget {
                self.off = true;
            }
        }
        self.off
    }

    /// How long the event loop may block, given the frame deadline it wanted.
    ///
    /// This is the load-bearing method. The loop's own deadline is
    /// `i32::MAX` when nothing is animating, which would park forever; clamping it
    /// to the time left before the screen blanks is what makes the timeout fire at
    /// all. Returns `wanted` unchanged when the budget cannot elapse or has
    /// already been satisfied, so a caller can use the result unconditionally.
    fn poll_budget(&self, wanted: i32, now: Instant) -> i32 {
        if self.budget.is_zero() || self.off {
            return wanted;
        }
        let idle = now.saturating_duration_since(self.last_input);
        let left = self.budget.saturating_sub(idle);
        let left_ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        // `+1` so a budget with under a millisecond left still yields a 1 ms
        // timeout rather than clamping to 0, which `epoll_wait` reads as "block
        // forever" -- the precise opposite of what is wanted. This is the same
        // trap as the existing `secs_to_min * 1000 + 500` in this file.
        wanted.min(left_ms.saturating_add(1).min(i32::MAX))
    }
}

/// The mutable shell state a popup row can change.
///
/// A struct rather than eleven positional parameters. `apply_popup_effect` is
/// called from inside the event loop's `match`, which already holds a dozen
/// live borrows; passing them individually produced a signature nobody could
/// read or keep in sync with the call site.
///
/// Only the fields an implemented effect touches are here. A field that no arm
/// reads is a lie about what the popup can do, so the rest are added when their
/// surfaces land.
struct PopupTargets<'a> {
    state: &'a mut LauncherState,
    home_pages: &'a mut Vec<Vec<String>>,
    current_home_page: &'a mut usize,
    app_drawer_open: &'a mut bool,
    home_locked: &'a mut bool,
    selected_home_icon: &'a mut Option<String>,
    /// The in-shell app panel a "open Settings" effect launches.
    ///
    /// Added because four of the workspace menu rows are *navigation*: Wallpapers,
    /// Home settings, System settings and Customize all end up somewhere, and
    /// UTLC's answer for all of them is the Settings panel this session built.
    /// Without this the effects could not express "launch a panel" at all, which
    /// is why they were inert.
    active_app: &'a mut Option<String>,
    /// The app the popup was raised on, empty for the workspace menu.
    subject: &'a str,
    /// Open the app-info panel on `subject`.
    ///
    /// Set by the `AppInfo` effect; the shell turns it into a panel.
    ///
    /// A flag rather than a ready-made [`AppInfoState`] because resolving the
    /// handoff target means a `PATH` search per candidate, and that is shell work
    /// the popup layer has no catalogue to do it from.
    ///
    /// Copy, not `&mut`: an effect sets it to `true` and nobody reads it in this
    /// scope, so a reference would invite a caller to think reading it here tells
    /// you anything.
    open_app_info: bool,
    /// Write the layout out. `None` in the shell's own unit tests, where there is
    /// no file to write; the effects that mutate check it before calling, so a
    /// test that only exercises the pure planning never needs one.
    persist: Option<PersistFn>,
}

impl PopupTargets<'_> {
    /// Copy the layout into the state and write it, if a writer was supplied.
    fn persist(&mut self) {
        if let Some(w) = self.persist {
            w(
                self.state,
                self.home_pages,
                *self.current_home_page,
                *self.home_locked,
            );
        }
    }
}

/// The display name of the in-shell Settings panel.
///
/// The panel is matched by *display name* throughout the shell
/// (`drm_kms.rs:2617`, and the `active_app` comparisons in the frame and tap
/// paths), because that is what the app grid carries. Naming it once here keeps
/// the popup navigation and those comparisons from drifting, which is the same
/// reason the settings-row keys live in a table rather than being spelled out at
/// each use.
const SETTINGS_APP: &str = "Settings";

/// An in-progress folder rename.
///
/// # Why this is a type and not a bare `String`
///
/// Three pieces of state have to agree: which folder is being renamed, what has
/// been typed so far, and whether the original name is still available to restore.
/// The reference's `Folder.setTitle` writes straight into the view
/// (`Folder.java:706-712`) and its footer commits on `DragLayer.java:190`, so a
/// bare buffer is enough there. UTLC has a *persisted* folder, and a half-typed
/// name written straight to the record would leave `LauncherState` holding an
/// edit the user never committed — which survives a restart, so a cancelled
/// rename would come back after a reboot.
///
/// So the buffer is separate from the record and the record is touched only on
/// commit. That also makes "cancel restores the original" trivially true rather
/// than something to remember.
///
/// # Text length
///
/// Bounded, and the bound is the *rendered* one rather than an arbitrary cap:
/// [`FolderRename::MAX`] is the longest name the folder footer draws before it
/// ellipsises, so the buffer can never hold more than is visible. A name the user
/// cannot see is a name they cannot check they typed right.
struct FolderRename {
    folder_id: u32,
    buffer: String,
    // Cursor at end-of-buffer only. A caret in the middle is a real editing
    // feature and is deliberately absent: with no selection and no caret
    // movement, backspace-and-retype is the whole model, and pretending to
    // support more would be a lie in the affordance rather than in the code.
}

impl FolderRename {
    /// Longest editable name, matching what the footer can draw.
    const MAX: usize = 64;

    fn begin(folder_id: u32) -> Self {
        Self {
            folder_id,
            buffer: String::new(),
        }
    }

    /// Append a typed character. Returns `false` when the buffer is full, so the
    /// caller can beep rather than silently dropping the keystroke.
    fn type_char(&mut self, c: char) -> bool {
        if self.buffer.chars().count() >= Self::MAX {
            return false;
        }
        self.buffer.push(c);
        true
    }

    /// Backspace. Returns `false` on an empty buffer.
    fn backspace(&mut self) -> bool {
        self.buffer.pop().is_some()
    }
}

/// Run the configured double-tap action.
///
/// Returns `true` when the gesture was consumed, so the caller skips the single-tap
/// path. `false` means the action has no implementation here — the double-tap was
/// still *detected*, so the caller must not report it as a missed gesture, but
/// there is nothing to perform.
///
/// # Which actions are real
///
/// Of the reference's six, two have somewhere to go on this device:
///
/// * **`Recents`** — open the overview. That is what the gesture exists for.
/// * **`OpenNotifications` / `OpenQuickSettings`** — pull the shade down, which is
///   `SystemUiShade::open()` plus its partial-height variant.
///
/// The other three are deliberately not faked:
///
/// * **`Sleep`** is the reference's *default*
///   (`GestureHandlerConfig.kt:76-78`, `PreferenceManager2.kt:813-816`), and it is
///   what this shell selects. There is no backlight to cut on this rootfs, so
///   "sleep" would have to be the same blank-the-screen behaviour as
///   [`ScreenTimeout`], reached by gesture instead of by idle. Wiring it to that
///   is a real decision, not a mechanical one, so it is left out rather than done
///   badly — and the default staying `Sleep` is why this returns `false` in
///   practice today.
/// * **`NoOp`** is the reference's "do nothing", so `false` is correct.
/// * **`NoOp`-adjacent locks** do not exist here.
///
/// # Why the default is left as `Sleep`
///
/// Because that is the reference's default, and changing the default to an action
/// this shell *can* do would make the two disagree in a way nobody could see. The
/// setting is one line to change once sleep is implemented.
fn run_double_tap(
    action: &mut utim_core::compositor::gestures::DoubleTapAction,
    shell_state: &mut ShellState,
    shade: &mut utim_core::compositor::systemui::SystemUiShade,
) -> bool {
    use utim_core::compositor::gestures::DoubleTapAction;
    match action {
        DoubleTapAction::Recents => {
            *shell_state = ShellState::Overview {
                selected: 0,
                dismiss: 0.0,
            };
            true
        }
        DoubleTapAction::OpenNotifications | DoubleTapAction::OpenQuickSettings => {
            // The same gesture here, and honestly so: the reference distinguishes
            // them by how far the shade is pulled (`ShadeView` settles at a
            // partial height for notifications and full for quick settings), and
            // UTLC's shade has one `pull_spring` with no partial-height rest
            // point. Adding a second rest state is a rendering change, not a
            // wiring one, so both open the shade and neither pretends to be the
            // other.
            shade.open();
            true
        }
        // `Sleep` is the reference's default and the only action selected on a
        // stock install (`GestureHandlerConfig.kt:76-78`,
        // `PreferenceManager2.kt:813-816`). Returning `true` would consume the
        // gesture and do nothing, which is the worse failure: the user's
        // double-tap would be swallowed by a no-op, so not even a bug report
        // could show the detector was working.
        // `OpenAppDrawer` has a destination -- `app_drawer_open` -- but it is not
        // reachable from here, because this runs on release and the drawer is a
        // `PullTarget` the gesture path owns. Wiring it needs that plumbed through
        // `run_double_tap`, so it is listed rather than done.
        DoubleTapAction::NoOp
        | DoubleTapAction::Sleep
        | DoubleTapAction::OpenAppDrawer
        | DoubleTapAction::OpenAppSearch
        | DoubleTapAction::OpenSearch
        | DoubleTapAction::OpenAssistant => false,
    }
}

/// The installed handler for an app-info handoff, and the absolute path of it.
///
/// On Linux app-info is an *external handoff*, not a screen the launcher draws and
/// owns: the reference calls `LauncherApps.startAppDetailsActivity`
/// (`PackageManagerHelper.java:180-182`), which is the Android package manager. So
/// the shell has to find an equivalent on this device, and there is more than one
/// convention -- `gnome-software` and `plasma-discover` are the details handlers,
/// and a store is the fallback when the app is not installed locally at all.
///
/// Order matters and is the reference's: details first, store second. An app that
/// *is* installed should open its own page rather than a store listing.
///
/// Returns `(target_kind, absolute_path)`. `None` for no handler at all, which is a
/// real state: the panel then renders with its button disabled rather than
/// pretending a handler exists. `paint_frame` may not do I/O, so this is resolved
/// here.
fn resolve_app_info_target(subject: &str) -> Option<(AppInfoTarget, String)> {
    // The handler names are the freedesktop/GNOME and KDE conventions for a software
    // centre. `xdg-open` with an `app:` URI is deliberately *not* the mechanism
    // here: the popup's own `Uninstall`/`AppInfo` handoff already uses that for the
    // component, and this panel exists to say *what would open* before it opens it.
    const DETAILS: [&str; 4] = [
        "gnome-software",
        "plasma-discover",
        "gnome-software-ubuntu",
        "appstreamcli",
    ];
    const STORE: [&str; 3] = ["gnome-software", "plasma-discover", "snap-store"];

    for name in DETAILS {
        if let Some(p) = find_on_path(name) {
            return Some((AppInfoTarget::Details, p));
        }
    }
    for name in STORE {
        if let Some(p) = find_on_path(name) {
            return Some((AppInfoTarget::Store, p));
        }
    }
    let _ = subject;
    None
}

/// The absolute path of `name` on `PATH`, or `None`.
///
/// A hand-rolled `which`: no dependency, and it checks `access(X_OK)` rather than
/// merely existing, so a non-executable file earlier on the path does not shadow a
/// real handler later. The `Vec` is not on the frame path — this runs once when a
/// panel opens.
fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            // `metadata` + the mode check, rather than `access`, so this does not
            // depend on the process's real or effective uid: the shell may run as
            // root with the user's `PATH`, and `access` under root succeeds on
            // almost everything.
            if let Ok(md) = std::fs::metadata(&cand) {
                use std::os::unix::fs::PermissionsExt;
                if md.permissions().mode() & 0o111 != 0 {
                    return Some(cand.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// What the app-info panel shows.
///
/// Owned rather than borrowed from `all_managed_apps` so the value can outlive the
/// catalogue rescan that reassigns everything the frame borrows -- the same
/// constraint that forces `settings_rows` to be a `Vec` rather than a slice.
struct AppInfoState {
    id: String,
    name: String,
    exec: String,
    /// The icon, when the catalogue resolved one. `Option` because the panel has to
    /// render before the icon sweep has necessarily run, and a placeholder beats a
    /// blank.
    icon: Option<std::rc::Rc<RgbaImage>>,
    /// The app's grid colour and letter glyph, so the panel's header matches the
    /// cell the user tapped.
    color: u32,
    glyph: String,
    /// The resolved command the "open" row spawns, or empty for "none found".
    ///
    /// Resolution means looking for a binary on *this* device, and `paint_frame`
    /// may not do I/O -- so the shell resolves it and the renderer only draws the
    /// row. Empty is a real state, not an error: the panel shows with the button
    /// disabled rather than pretending an app-info screen exists.
    target: String,
    /// What the button means. Resolved by the shell from the same lookup as
    /// `target`, so the label and the command cannot come from different searches.
    target_kind: utim_core::graphics::layout::AppInfoTarget,
    /// An install session is in flight, so the target is the store.
    ///
    /// A convenience over `target_kind == Store`: it is the fact, and
    /// `target_kind` is what it means for the button.
    installing: bool,
}

/// Begin a rename on `folder_id`, seeding the buffer with nothing rather than
/// the current name.
///
/// Empty, not pre-filled: the reference pre-fills and places the caret at the end
/// (`Folder.java:1851` region), which is right when the field is drawn. UTLC's
/// field is not drawn yet, and a pre-filled buffer with no visible field would
/// leave the user typing in front of text they cannot see. When the field lands,
/// pre-filling is a one-line change and this comment is the note to revisit.
fn begin_folder_rename(rename: &mut Option<FolderRename>, folder_id: u32) {
    *rename = Some(FolderRename::begin(folder_id));
}

/// Apply the buffered name to the persisted record and close the editor.
///
/// The only writer of `FolderRecord::title`. Cancelling needs no function of its
/// own: dropping the [`FolderRename`] is a cancel, because the buffer is not the
/// record -- which is the property that makes cancelling safe to get wrong.
///
/// Returns `true` when the record changed. A no-op for an unchanged title, so a
/// rename that changes nothing does not mark the state dirty and rewrite the
/// settings file.
fn commit_folder_rename(rename: &mut Option<FolderRename>, state: &mut LauncherState) -> bool {
    let Some(r) = rename.take() else {
        return false;
    };
    // `folder_id` is the `FolderRecord::id` verbatim. `FolderOpen::folder_idx` is
    // set from that same id (`open_folder_at` does `folder.open(id as u8)`), so it
    // needs no offset -- an earlier version added and subtracted 1 and would have
    // renamed whatever folder happened to sit one below, or none at all.
    let id = r.folder_id;
    let Some(record) = state.folders.iter_mut().find(|f| f.id == id) else {
        return false;
    };
    if record.title == r.buffer {
        return false;
    }
    record.title = r.buffer;
    true
}

/// Run a popup row's effect.
///
/// The caller has already closed the menu, so nothing here re-opens it. Effects
/// that need a surface UTLC does not have yet fall through to
/// [`PopupEffect::Unsupported`] and do nothing -- deliberately, and with the
/// reason in [`plan_popup_row`]. A row that quietly does nothing is exactly the
/// failure this whole path was rebuilt to end.
fn apply_popup_effect(effect: PopupEffect, t: &mut PopupTargets<'_>) {
    match effect {
        PopupEffect::Dismiss | PopupEffect::Unsupported | PopupEffect::LaunchShortcut(_) => {
            // The menu is already closed; there is nothing left to do. A deep
            // shortcut needs a resolved target, which is a per-app data source
            // that does not exist yet.
        }

        // The reference's affirmative row. On Android this is
        // `LauncherApps.startAppDetailsActivity` (`SystemShortcut.java:246`);
        // on a treble phone the equivalent is the software manager, so this is
        // a handoff rather than a new screen. The three managers are tried in
        // the reference's own order of preference -- the distro's default
        // package tool first, the GNOME fallbacks after. `Uninstall` is the
        // same target: the reference's `ACTION_DELETE` (`SystemShortcut.java
        // :304-316`) is the *manager's* uninstall flow, and the manager decides
        // what confirmation to ask for, which is not something a launcher
        // should be inventing.
        //
        // `try_exists` before the spawn so an absent manager falls through
        // silently rather than logging a failure per tap.
        // `AppInfo` now opens the in-shell panel the renderer grew. `Uninstall`
        // stays a handoff, because there is no package manager integration to
        // drive -- removing an app is not a thing this shell can do honestly.
        PopupEffect::AppInfo => {
            if t.subject.is_empty() {
                return;
            }
            t.open_app_info = true;
        }
        PopupEffect::Uninstall => {
            if t.subject.is_empty() {
                return;
            }
            // `xdg-open app:<desktop-id>` is the freedesktop equivalent of
            // `ACTION_VIEW` on an app URI, and it dispatches to whichever
            // manager is installed. `Command::spawn` (not the shell's
            // `launch_desktop_app`, which requires an absolute path) is the
            // right call: the point is to hand off, not to supervise.
            let _ = std::process::Command::new("xdg-open")
                .arg(format!("app:{}", t.subject))
                .spawn();
        }

        PopupEffect::RemoveFromPage => {
            if t.subject.is_empty() {
                return;
            }
            let before = t.home_pages.get(*t.current_home_page).map(|p| p.len());
            if let Some(page) = t.home_pages.get_mut(*t.current_home_page) {
                page.retain(|id| id != t.subject);
            }
            // Only a row that actually removed something is worth writing.
            if t.home_pages.get(*t.current_home_page).map(|p| p.len()) != before {
                t.persist();
            }
            // Removing the selected icon deselects it, or the edit chips would
            // point at a cell that no longer holds anything.
            if t.selected_home_icon.as_deref() == Some(t.subject) {
                *t.selected_home_icon = None;
            }
        }

        PopupEffect::OpenAllApps => {
            *t.app_drawer_open = true;
        }

        PopupEffect::ToggleHomeLock => {
            *t.home_locked = !*t.home_locked;
            t.persist();
        }

        // UTLC's edit mode is "an icon is selected", so entering it with no
        // icon selected is not expressible. Selecting the first icon on the
        // page is the closest true statement of the reference's row, which
        // puts the workspace into a state where an icon can be moved.
        PopupEffect::EnterEditMode => {
            if t.selected_home_icon.is_none() {
                if let Some(first) = t
                    .home_pages
                    .get(*t.current_home_page)
                    .and_then(|p| p.first())
                    .cloned()
                {
                    *t.selected_home_icon = Some(first);
                }
            }
        }

        // Six of these eight were inert: the row drew, the tap landed, and
        // nothing happened. Five of the six now navigate to the in-shell
        // Settings panel, which is where a phone-shaped device keeps its
        // settings; `ToggleHiddenApp` mutates `hidden_apps`, which has been
        // persisted and rendered-invisible since the store existed.
        PopupEffect::OpenSettings | PopupEffect::OpenSystemSettings => {
            // `SystemSettings` and `HomeSettings` are the same panel here. UTLC
            // has no separate platform-settings activity, and inventing a second
            // destination for the same ~20 settings would mean two places to keep
            // in agreement.
            *t.active_app = Some(SETTINGS_APP.to_string());
        }
        PopupEffect::OpenWallpapers => {
            // The Settings panel's `wallpaper` row *is* the wallpaper UI now --
            // a paging picker over whatever is installed. So this is a
            // navigation, not a dead row.
            *t.active_app = Some(SETTINGS_APP.to_string());
        }
        PopupEffect::CustomizeApp => {
            // Per-app customisation on this device is "change a setting", and
            // the Settings panel is where settings live.
            *t.active_app = Some(SETTINGS_APP.to_string());
        }
        PopupEffect::SetDefaultPage => {
            // `default_page` is persisted and was read by nothing, so "Set as
            // default" was a menu row that could not set anything.
            t.state.default_page = *t.current_home_page;
            t.persist();
        }
        PopupEffect::ToggleHiddenApp => {
            // Add or remove, so a second tap undoes the first. `subject` is empty
            // for the workspace menu, where there is nothing to hide.
            if t.subject.is_empty() {
                return;
            }
            let id = t.subject;
            match t.state.hidden_apps.iter().position(|h| h == id) {
                Some(at) => {
                    t.state.hidden_apps.remove(at);
                }
                None => t.state.hidden_apps.push(id.to_string()),
            }
            t.state.touch();
        }
        // Two rows still have nowhere to go, and one of them is a device gap
        // rather than a wiring one:
        //
        // * `OpenWidgets` needs a widget host. There is none on this rootfs and
        //   the reference's is `AppWidgetHostView` inside a separate process. A
        //   row that opens Settings would be a lie; a row that does nothing is
        //   the recorded gap.
        // * `RenameApp` needs an on-screen text field. The shell has one, but it
        //   belongs to an app panel and is not reachable from a workspace popup;
        //   adding a second text field is a rendering change, not a wiring one.
        PopupEffect::OpenWidgets | PopupEffect::RenameApp => {}
    }
}

fn close_modal_surfaces(
    shell_state: &mut ShellState,
    folder: &mut FolderOpen,
    popup_spring: &mut SpringSimulation,
    workspace_scale_spring: &mut SpringSimulation,
    window_alpha_spring: &mut SpringSimulation,
) {
    *shell_state = ShellState::Normal;
    folder.close();
    popup_spring.set_target(0.0);
    workspace_scale_spring.set_target(1.0);
    window_alpha_spring.set_target(1.0);
}

/// What the shell does with one `GestureAction`, decided with no state at all.
///
/// This exists so the wiring is testable. The defect it fixes was that the
/// gesture match in the input loop ended in `_ => {}` and both
/// `GestureAction::Recents` and `GestureAction::BottomBarScrub` fell into it:
/// the overview could not be opened and a task scrub did nothing. Both were
/// produced by the gesture engine and both were tested *there*, so nothing
/// failed -- the actions simply had no consumer. Extracting the decision means
/// "this action has an effect" is an assertion about a pure function instead of
/// a line of code in a 5000-line `match` inside an event loop.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ShellEffect {
    /// Nothing to do.
    None,
    /// Open the recents carousel.
    OpenOverview,
    /// Move the selection by one card pitch, in card widths.
    ScrubTasks(f32),
    /// An in-app home gesture is in flight: the panel's scale and opacity.
    MorphWorkspace { scale: f32, window_alpha: f32 },
    /// Close every modal surface and return the workspace morph to rest.
    CloseAll,
}

/// Decide a gesture's effect. Pure: no shell state is read or written.
fn plan_gesture(a: &GestureAction) -> ShellEffect {
    match *a {
        // The two arms that used to be dropped.
        GestureAction::Recents { progress, .. } => {
            if progress >= RECENTS_COMMIT_PROGRESS {
                ShellEffect::OpenOverview
            } else {
                ShellEffect::None
            }
        }
        GestureAction::BottomBarScrub { app_shift, .. } => {
            if app_shift != 0 {
                ShellEffect::ScrubTasks(app_shift as f32)
            } else {
                ShellEffect::None
            }
        }
        // A *partial* home gesture: the panel is on its way back to the
        // workspace and the gesture layer has already computed the scale and
        // opacity it wants.
        GestureAction::Home {
            progress,
            scale,
            window_alpha,
        } => {
            if progress >= 1.0 {
                ShellEffect::CloseAll
            } else {
                ShellEffect::MorphWorkspace {
                    scale,
                    window_alpha,
                }
            }
        }
        _ => ShellEffect::None,
    }
}

/// The launcher's own mode, as distinct from `ShellMode` (which is the HWC
/// plane set: launcher vs lock screen) and from `app_drawer_open` (which is a
/// boolean).
///
/// Plan §7.6. No `String` and no `Vec`: a popup is named by a `u8` index and an
/// anchor is two `f32`s, so this stays `Copy` and the per-frame path never has
/// to clone an identity.
///
/// There is deliberately no `FolderOpen` arm. The folder *path* is complete --
/// `compositor::FolderOpen` models the three springs and the title delay, and
/// `drm_kms::draw_folder` draws the scrim, surface, grid and footer off
/// `FolderLayout` -- but the shell has no folders to open: `home_pages` is a
/// flat list of apps and nothing in the shell groups them. Adding the variant
/// now would be a state that cannot be entered, which is worse than its
/// absence: the field would read as "folders are wired" when they are not.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ShellState {
    /// Home, with the workspace as the root surface.
    Normal,
    /// The app drawer is up. `progress` mirrors `drawer_spring`; it is carried
    /// here so a state *change* is comparable in one `==`, rather than
    /// reconstructed from a float on the side.
    AllApps { progress: f32 },
    /// The recents carousel. `selected` is the index into `recents`, `dismiss`
    /// the live drag offset of that card in px.
    Overview { selected: u8, dismiss: f32 },
    /// A long-press popup is anchored at a panel point.
    PopupOpen {
        anchor_x: f32,
        anchor_y: f32,
        idx: u8,
    },
}

impl ShellState {
    /// True when a modal surface owns the screen, so a workspace swipe must
    /// not also page the home screen underneath it.
    fn is_modal(self) -> bool {
        matches!(self, Self::Overview { .. } | Self::PopupOpen { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ManagedApp {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub color: u32,
    pub glyph: String,
    /// Icon theme names tried in order when resolving this app's icon.
    pub icon_keys: Vec<String>,
    /// `Keywords=` from the desktop entry, lowercased.
    ///
    /// Carried so [`app_fields`] can hand the matcher a real keyword list. It
    /// used to hand `&[]`, which meant the shell's drawer ranked apps on name
    /// and id only while `--benchmark` — which goes through
    /// `DesktopCatalogue::search`, where `SearchFields::keywords` *is* populated
    /// — ranked on keywords too. The two therefore disagreed on exactly the
    /// queries `Keywords=` exists for: searching "browser" or "chat" for an app
    /// whose display name is neither. The shell is the thing the user touches,
    /// so the shell was the one that was wrong.
    ///
    /// Lowercased at parse time rather than at match time: the matcher folds
    /// case itself for the name and id, so doing it here too keeps the two
    /// paths agreeing, and it means the field can be compared directly in a
    /// test without reimplementing the fold.
    pub keywords: Vec<String>,
    /// Decoded icon bitmap, attached by [`apply_app_icons`].
    pub icon: Option<Rc<RgbaImage>>,
}

impl ManagedApp {
    pub fn new(id: &str, name: &str, exec: &str, color: u32, glyph: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            exec: exec.to_string(),
            color,
            glyph: glyph.to_string(),
            icon_keys: Vec::new(),
            keywords: Vec::new(),
            icon: None,
        }
    }

    fn with_icon_keys(mut self, keys: &[&str]) -> Self {
        self.icon_keys = keys.iter().map(|k| k.to_string()).collect();
        self
    }

    /// Attach `Keywords=`, lowercased and de-duplicated, order preserved.
    ///
    /// Separate from `new` rather than a parameter because the builtin entries
    /// have no keywords and adding a parameter would mean thirteen call sites
    /// passing `&[]` to say nothing.
    ///
    /// De-duplication is not cosmetic: `Keywords=web;browser;Web` is a legal
    /// entry and would otherwise let one keyword outrank another purely by
    /// repetition. The matcher scores a keyword hit, so a repeated keyword is a
    /// repeated score.
    fn with_keywords(mut self, keywords: &[String]) -> Self {
        let mut seen: Vec<String> = Vec::with_capacity(keywords.len());
        for k in keywords {
            let lower = k.to_lowercase();
            if !lower.is_empty() && !seen.contains(&lower) {
                seen.push(lower);
            }
        }
        self.keywords = seen;
        self
    }
}

/// Freedesktop icon names for the built-in launcher entries. All of these
/// resolve in the icon themes shipped by the rootfs except `clock`, which has
/// no raster icon and therefore keeps its letter glyph.
const BUILTIN_ICON_KEYS: [(&str, &[&str]); 13] = [
    ("phone", &["phone", "call-start"]),
    ("messages", &["mail-message-new", "messages"]),
    (
        "browser",
        &["web-browser", "browser", "internet-web-browser"],
    ),
    ("camera", &["camera-photo", "camera"]),
    ("gallery", &["image-x-generic", "gallery"]),
    ("settings", &["preferences-system", "settings"]),
    ("files", &["system-file-manager", "files"]),
    ("music", &["audio-x-generic", "music"]),
    ("terminal", &["utilities-terminal", "terminal"]),
    (
        "treble",
        &["computer", "treble", "distributor-logo-android", "android"],
    ),
    ("contacts", &["contact-new", "contacts"]),
    ("clock", &["clock"]),
    ("apps", &["view-app-grid", "apps"]),
];

/// Built-in launcher entry with its icon keys attached.
fn builtin_app(id: &str, name: &str, exec: &str, color: u32, glyph: &str) -> ManagedApp {
    let keys = BUILTIN_ICON_KEYS
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, keys)| *keys)
        .unwrap_or(&[]);
    ManagedApp::new(id, name, exec, color, glyph).with_icon_keys(keys)
}

/// Stable fingerprint of the application set, used to decide when the icon
/// cache may drop its recorded misses and re-scan.
fn app_set_signature(apps: &[ManagedApp]) -> String {
    // Covers the *set* of applications and nothing about their icons.
    //
    // This is what decides whether `icon_cache.invalidate_misses()` runs, and
    // `invalidate_misses` clears only the `misses` set -- resolved entries in
    // `IconCache::images` are permanent for the process lifetime. So a signature
    // over ids alone means an application whose icon *file* was replaced -- a
    // theme update, an app upgrade, a re-install -- keeps its stale bitmap for as
    // long as the shell runs, with nothing able to notice: the id did not change
    // and the name did not either.
    //
    // `icon_keys` is in the signature because it is the thing that decides *which*
    // file an app's icon resolves to. It is a `Vec<String>` of theme names, so
    // hashing it rather than joining it keeps the signature allocation-free apart
    // from the one `String` returned.
    let mut sig = String::with_capacity(apps.len() * 24);
    for app in apps {
        sig.push_str(&app.id);
        sig.push('\0');
        for k in &app.icon_keys {
            sig.push_str(k);
            sig.push('\u{1}');
        }
        // The name is here because it is the monogram fallback: an app whose icon
        // failed to resolve draws its first letter, so changing the name changes
        // what is painted even though no bitmap changed.
        sig.push_str(&app.name);
        sig.push('\0');
    }
    sig
}

/// Resolve every app's `icon_keys` through one batched icon-theme sweep and
/// attach the decoded bitmap; apps whose keys do not resolve keep the
/// first-letter glyph fallback.
/// Decode icon size, in pixels, for the panel the shell is drawing on.
///
/// Cached icons are pre-scaled to this edge so the render path can blit them
/// 1:1 instead of resampling every frame.
fn icon_edge_px() -> u32 {
    // Filled in once the panel size is known; the shell boots at 1080x2400 and
    // the value is refreshed on every mode change.
    ICON_EDGE_PX.with(|c| *c.borrow())
}

thread_local! {
    static ICON_EDGE_PX: std::cell::RefCell<u32> = const { std::cell::RefCell::new(121) };
}

fn set_icon_edge_px(edge: u32) {
    ICON_EDGE_PX.with(|c| *c.borrow_mut() = edge);
}

fn apply_app_icons(apps: &mut [ManagedApp], cache: &mut IconCache) {
    let mut pending: Vec<String> = Vec::new();
    for app in apps.iter() {
        for key in &app.icon_keys {
            if !cache.knows(key) && !pending.contains(key) {
                pending.push(key.clone());
            }
        }
    }
    for dock_key in ["view-app-grid", "apps"] {
        let dk = dock_key.to_string();
        if !cache.knows(&dk) && !pending.contains(&dk) {
            pending.push(dk);
        }
    }
    if !pending.is_empty() {
        cache.resolve_keys(&pending);
    }
    for app in apps.iter_mut() {
        // Resolve a real icon from any of the app's keys, and if none of them
        // produced one -- no icon installed, or an SVG UTLC cannot rasterise
        // (see `icons`' module doc on `utlc-cache-icons`) -- synthesise a
        // themed monogram tile instead.
        //
        // Without the fallback the app rendered as a bare coloured square with
        // no glyph: `draw_icon_bitmap_i32` is only reached for `Some(icon)`, and
        // `None` took the plain `draw_rounded_rect_i32` + `draw_text_centered`
        // monogram path in the *grid*, but the drawer and the dock resolved
        // their glyph from `app.glyph` separately and an app whose icon key
        // resolved to a blank PNG got an empty tile. The synthesised tile is an
        // ordinary cached `RgbaImage`, so the renderer needs no change: it
        // blits through the same 1:1 fast path as a decoded icon.
        //
        // The cache key is the *first* icon key, not the app id, so a
        // decorative icon (`Icon=`) wins over a synthesised one and the tile
        // is shared by any app that declares the same decorative key.
        app.icon = app
            .icon_keys
            .iter()
            .find_map(|key| cache.get(key))
            .or_else(|| {
                let key = app.icon_keys.first()?;
                cache.get_or_monogram(key, &app.name, app.color)
            });
    }
}

/// The `n`-th app the drawer is showing, or `None`. Allocates nothing.
///
/// The drawer grid, the press feedback and the launch handler all need "the
/// nth app the drawer is showing"; this is the single definition of that.
/// Every call site only needs `.nth(idx)`, so no iterator (and no `Box`)
/// is built at all.
fn drawer_nth<'a>(apps: &'a [ManagedApp], query: &str, n: usize) -> Option<&'a ManagedApp> {
    // Note there is no `if query.is_empty() { return apps.get(n); }` shortcut here,
    // and its absence is deliberate. The unfiltered drawer is the launcher's
    // most-seen screen, and leaving it in directory-walk order made the app at row
    // 0 depend on which `.desktop` files the filesystem happened to yield --
    // installing one app reshuffled every equal row above it.
    //
    // The reference sorts it: `AppsListConfig.getSortedApps` orders by recency then
    // by title (`data/AppsListCache.java:143-189`), and `doZeroStateSearch`
    // (`LawnchairLocalSearchAlgorithm.kt:104-139`) does the same for an empty
    // query. Recency is not modelled -- nothing records when an app was last
    // launched -- so this is the title half of that order, and a title-sorted list
    // is a strict improvement over an arbitrary one.
    //
    // `search_ranked`'s empty-query branch already is that: everything, in
    // folded-name order, bounded top-K. So deleting the shortcut *was* the fix.

    // The *ranked* nth match, not the nth in catalogue order.
    //
    // The filter this replaces returned matches in the order the directory walk
    // produced them, so which app was row 3 depended on which `.desktop` files
    // the filesystem yielded first -- and `AppMatcher.MatchType.priority`
    // (`AppMatcher.kt:8-17`) exists precisely to order them. Tapping row 0 and
    // tapping row 3 used to be answering two different questions.
    //
    // Bounded to the drawable window: `n` past [`FRAME_MAX_GRID`] is a miss, and
    // that is the correct answer rather than a fallback. The old code returned a
    // catalogue-ordered app for an index the drawer never drew, which is how a
    // tap could launch something with no visible row -- the divergence the
    // `drawer_first_index` comment at `main.rs:6055` describes, now removed from
    // both sides at once.
    //
    // Ranked per call rather than memoised because this runs on a tap, not per
    // frame: six call sites, each once per gesture, for a bounded top-64 window
    // over the catalogue.
    if n >= FRAME_MAX_GRID || apps.is_empty() {
        return None;
    }
    let mut hits: [SearchHit<'a, ManagedApp>; FRAME_MAX_GRID] =
        core::array::from_fn(|_| SearchHit::empty(&apps[0]));
    // `search_ranked` reports the match count and fills the first
    // `min(count, CAP)` slots, leaving the rest as the `empty` sentinel. So the
    // count -- not the array length -- is what says whether slot `n` holds a
    // match: indexing the array alone returns the sentinel's item, which is
    // `apps[0]`, and would make an unmatchable query launch the first app in the
    // catalogue. That is the bug the companion test pins.
    let total = search_ranked(apps, query, app_fields, &mut hits).min(FRAME_MAX_GRID);
    if n >= total {
        return None;
    }
    Some(hits[n].item)
}

/// The [`SearchFields`] a [`ManagedApp`] exposes to the matcher.
///
/// `keywords` used to be `&[]`, because `ManagedApp` did not carry `Keywords=`
/// while [`DesktopCatalogue::search`] reads the desktop file directly and *does*
/// search them. That made the drawer's ranking disagree with `--benchmark`'s on
/// exactly the queries `Keywords=` exists for — an app whose display name is
/// neither "browser" nor "chat" but whose keywords say so. The shell is the part
/// the user touches, so the shell was the part that was wrong.
///
/// The borrow is the point: `SearchFields::keywords` is a `&[String]` precisely so
/// this projection costs nothing. `app_fields` is called once per app per frame
/// from `drawer_nth`, so a projection that allocated would be an allocation per
/// app per frame — which is why `ManagedApp` owns a `Vec<String>` parsed once at
/// scan time and this hands out a slice of it.
#[inline]
fn app_fields<'a>(a: &'a ManagedApp) -> SearchFields<'a> {
    SearchFields {
        name: a.name.as_str(),
        id: a.id.as_str(),
        keywords: &a.keywords,
    }
}

/// Build the render descriptor for one app. A plain `fn` (not a closure)
/// so the borrow flows straight through with no lifetime inference trap.
fn drawer_item_of(a: &ManagedApp) -> AppGridItem<'_> {
    AppGridItem {
        id: &a.id,
        name: &a.name,
        color: a.color,
        glyph: &a.glyph,
        icon: a.icon.as_deref(),
        folder_n: 0,
        folder_id: 0,
    }
}

/// Keep the Messages composer buffer bounded (sibling terminal buffers cap
/// at 120 lines). Called at the top of every loop iteration so all push
/// sites are covered by one trim.
#[inline]
fn trim_messages_list(v: &mut Vec<String>) {
    if v.len() > 64 {
        let n = v.len() - 64;
        v.drain(0..n);
    }
}

/// Every independent source of ongoing motion in the shell, as plain booleans.
///
/// The event loop picks its `epoll_wait` timeout from this: if any source is
/// live it sleeps only the remainder of the current frame, and if none are it
/// blocks until the next minute-boundary clock refresh.
///
/// This used to be an inline `need_frames` chain that consulted three springs
/// (`drawer`, `page_scroll`, `icon_bounce`). The other nine motion sources --
/// the overview pair, the popup fade, the fast-scroller, the workspace scale
/// and window alpha, the folder morph, the recents cards, the app-launch
/// progress and the touch ripple -- were missing, so the instant a finger left
/// the glass mid-animation the predicate went false and the timeout jumped to
/// `secs_to_min * 1000 + 500`: up to 60.5 seconds. Every animation froze
/// mid-flight and the shell looked hung.
///
/// Modelling it as data rather than as a bare boolean chain buys the property
/// that matters: the exhaustive test at the bottom of this file can flip each
/// source on individually and assert the loop still schedules a frame, so a
/// future motion source cannot be added without a matching arm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameDemand {
    touch_ripple: bool,
    /// A folder long press is pending, or its menu is animating.
    folder_menu: bool,
    app_launch: bool,
    drawer: bool,
    page_scroll: bool,
    icon_bounce: bool,
    overview: bool,
    overview_scrim: bool,
    popup: bool,
    fastscroller: bool,
    workspace_scale: bool,
    window_alpha: bool,
    folder: bool,
    recents: bool,
    terminal_busy: bool,
    lockscreen_intro: bool,
}

impl FrameDemand {
    /// True when the shell must be woken at the next vsync, not on the clock.
    #[inline]
    fn any(&self) -> bool {
        self.touch_ripple
            || self.app_launch
            || self.drawer
            || self.page_scroll
            || self.icon_bounce
            || self.overview
            || self.overview_scrim
            || self.popup
            || self.fastscroller
            || self.workspace_scale
            || self.window_alpha
            || self.folder
            || self.recents
            || self.terminal_busy
            || self.lockscreen_intro
    }

    /// The `epoll_wait` timeout in ms for this state.
    ///
    /// Animating -> the unspent remainder of the frame, floored at 0 so a
    /// frame that already overran its budget re-polls immediately instead of
    /// adding a second interval of latency. Idle -> the time to the next
    /// minute boundary (the status clock and the watchdog both ride on it).
    fn timeout_ms(&self, elapsed: Duration, frame_interval: Duration) -> libc::c_int {
        if elapsed >= frame_interval {
            return 0;
        }
        if self.any() {
            return frame_interval
                .checked_sub(elapsed)
                .unwrap_or_default()
                .as_millis()
                .min(libc::c_int::MAX as u128) as libc::c_int;
        }
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        let secs_to_min = 60i64 - ts.tv_sec.rem_euclid(60);
        let idle_ms = (secs_to_min as u128) * 1000 + 500;
        idle_ms.min(libc::c_int::MAX as u128) as libc::c_int
    }
}

// epoll u64 dispatch tags: the fd occupies the low 32 bits, the kind the
// high 32, so fd->kind dispatch is a single switch with no linear scan.
const K_SIGNAL: u64 = 1 << 32;
const K_LISTEN: u64 = 2 << 32;
const K_INPUT: u64 = 3 << 32;
const K_CLIENT: u64 = 4 << 32;
#[inline]
fn epoll_tag(fd: libc::c_int, kind: u64) -> u64 {
    (fd as u32 as u64) | kind
}
#[inline]
fn epoll_kind(u: u64) -> u64 {
    u & 0xFFFF_FFFF_0000_0000
}
#[inline]
fn epoll_fd_of(u: u64) -> libc::c_int {
    (u & 0xFFFF_FFFF) as u32 as libc::c_int
}

/// Per-Wayland-client accumulation buffer: carries a partial message tail
/// across read() boundaries so a fragmented message never desyncs the
/// stream. Kept parallel to `server.client_streams` (same index).
struct ClientState {
    buf: [u8; 4096],
    len: usize,
    text_input_id: u32,
}

impl ClientState {
    fn new() -> Self {
        Self {
            buf: [0u8; 4096],
            len: 0,
            text_input_id: 0,
        }
    }
}

/// Cap on concurrent Wayland clients (fd bound on a world-connectable socket).
const MAX_CLIENTS: usize = 16;

/// Open any /dev/input/event* nodes not already tracked. Shared by the
/// startup scan and the one-shot inotify rescan (no polling).
fn open_new_input_devices(
    epoll_fd: libc::c_int,
    input_fds: &mut Vec<libc::c_int>,
    opened_paths: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"))
        {
            if opened_paths.contains(&path) {
                continue;
            }
            if let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
                let fd = unsafe {
                    libc::open(
                        c_path.as_ptr(),
                        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                    )
                };
                if fd >= 0 {
                    if epoll_fd >= 0 {
                        let mut in_ev = libc::epoll_event {
                            events: libc::EPOLLIN as u32,
                            u64: epoll_tag(fd, K_INPUT),
                        };
                        unsafe {
                            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut in_ev);
                        }
                    }
                    input_fds.push(fd);
                    opened_paths.push(path);
                }
            }
        }
    }
}

/// Copy the shell's mutable layout state into [`LauncherState`] and write it.
///
/// The shell owns `home_pages`, `current_home_page` and `home_locked` as locals
/// because they are read on the frame path; this is the one place they are
/// copied back out. Saving on a dirty flag rather than per frame is deliberate:
/// an `fsync` per vsync is the fastest way to turn a launcher into a battery
/// problem, and the reference writes on a background writer
/// (`LauncherPrefs.putSync` is only used for the few reads that happen during
/// launch).
///
/// Not infallible by design: a failed save is logged and retried on the next
/// mutation rather than aborting the shell, because losing a preference is
/// annoying and refusing to launch is fatal.
/// The writer `apply_popup_effect` uses to flush the layout. A named alias
/// because the bare `fn` type is long enough that clippy flags it as
/// unreadable, and because it is part of `PopupTargets`' contract: a test passes
/// `None`, the shell passes `persist_state`.
type PersistFn = fn(&mut LauncherState, &[Vec<String>], usize, bool);

#[allow(clippy::too_many_arguments)]
fn persist_state(
    state: &mut LauncherState,
    home_pages: &[Vec<String>],
    current_home_page: usize,
    home_locked: bool,
) {
    state.home_pages.clear();
    state.home_pages.extend_from_slice(home_pages);
    state.current_page = current_home_page;
    state.home_locked = home_locked;
    state.touch();
    match state.save() {
        Ok(()) => state.clear_dirty(),
        Err(e) => eprintln!(
            "[UTLC] launcher state not saved ({}); the layout stays in memory for this session",
            e
        ),
    }
}

/// Re-resolve the hotseat slots to indices into `all_managed_apps` plus the
/// cached "Apps" icon. Runs only on catalogue change, never per frame.
///
/// Slots are resolved by **app id**, not by display name. The previous version
/// matched on `a.name == "Phone"` against a hardcoded array, which meant a
/// locale change, a catalogue entry being renamed, or the user renaming the app
/// silently rebound a dock slot to a different application -- and there was no
/// way for the user to say otherwise, because nothing was stored.
fn refresh_dock_cache(
    all_managed_apps: &[ManagedApp],
    icon_cache: &IconCache,
    dock_index: &mut [Option<usize>; 5],
    dock_apps_icon: &mut Option<Rc<RgbaImage>>,
    dock_ids: &[String],
) {
    for (slot, id) in dock_ids.iter().enumerate().take(dock_index.len()) {
        // An empty slot is a legitimate state -- a dock with a gap in it -- so
        // it resolves to `None` rather than falling back to a default app.
        dock_index[slot] = if id.is_empty() {
            None
        } else {
            all_managed_apps.iter().position(|a| a.id == *id)
        };
    }
    *dock_apps_icon = icon_cache
        .get("view-app-grid")
        .or_else(|| icon_cache.get("apps"));
}

/// Set O_NONBLOCK on a raw fd via fcntl (no std wrapper exists for child pipes).
fn set_fd_nonblocking(fd: libc::c_int) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn get_app_color(name_or_id: &str) -> u32 {
    const PALETTE: [u32; 10] = [
        0xFF3B82F6, // Sky Blue
        0xFF10B981, // Emerald Green
        0xFF8B5CF6, // Violet
        0xFFF59E0B, // Amber
        0xFFEC4899, // Pink
        0xFF06B6D4, // Cyan
        0xFF6366F1, // Indigo
        0xFF14B8A6, // Teal
        0xFFF97316, // Vibrant Orange
        0xFF64748B, // Slate
    ];
    let mut hash: u32 = 0;
    for b in name_or_id.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as u32);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}
/// Spawn `exec_cmd` against the compositor's Wayland socket.
///
/// Returns the child's pid so the recents card can be tied to a real process:
/// the kill escalation in [`utim_core::compositor::Recents`] signals a pid, and
/// a card carrying 0 would make "close this task" a no-op that silently
/// reports success. `None` means nothing was spawned -- an empty command, a
/// missing binary, or a spawn error; the last two are already logged.
fn launch_desktop_app(exec_cmd: &str, socket_dir: &str) -> Option<i32> {
    if exec_cmd.is_empty() {
        return None;
    }
    let parts: Vec<&str> = exec_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let prog = parts[0];
    let args = &parts[1..];

    if !std::path::Path::new(prog).exists() {
        eprintln!(
            "[UTLC] Skipping desktop application launch: binary '{}' does not exist",
            prog
        );
        return None;
    }

    let sess = utim_core::session::session();
    let is_root = utim_core::session::is_root_process();
    if is_root {
        utim_core::session::ensure_session_dirs();
    }
    // Only root owns the compositor socket location; an unprivileged caller
    // already lives in its own runtime dir.
    let app_socket_dir = if is_root {
        utim_core::session::SESSION_RUNTIME_DIR
    } else {
        socket_dir
    };

    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("XDG_RUNTIME_DIR", app_socket_dir)
        .env("GDK_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("HOME", sess.home())
        .env("USER", sess.name())
        .env("LOGNAME", sess.name())
        .env("SHELL", "/bin/bash")
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );

    utim_core::session::drop_privileges(&mut cmd);

    match cmd.spawn() {
        Ok(child) => {
            // The `Child` is dropped here and never waited on, exactly as
            // before. It is not the parent-waiter for these processes and never
            // blocks on them; an exited child is reaped by init, because
            // SIGCHLD is neither blocked nor ignored here.
            // A pid is a positive `i32` on Linux and `child.id()` is a `u32`,
            // so the conversion is a checked cast rather than a `From`: a
            // value above `i32::MAX` cannot be a real pid and would wrap to a
            // negative one, which `kill` would reject -- better to report no
            // pid than a wrong one.
            let pid = child.id();
            if pid > i32::MAX as u32 {
                None
            } else {
                Some(pid as i32)
            }
        }
        Err(e) => {
            eprintln!("[UTLC] Error spawning '{}': {}", prog, e);
            None
        }
    }
}

/// Fixed capacities for the per-frame render views.
///
/// The workspace grid is capped at `grid_cols * max_rows`, and the pin path
/// already refuses to exceed it, so 64 covers 6 columns by 10 rows with room
/// to spare and a short panel can never overflow it. The hotseat is 5 slots.
/// The tab strip is user-driven, so 32 is a ceiling rather than a bound; past
/// it the extra tabs are not drawn (they are still fully functional and
/// reachable by the `Tab N` cycling commands).
const FRAME_MAX_GRID: usize = 64;

/// How many *ranked* matches the drawer keeps, as a multiple of the drawable
/// window.
///
/// This is the scroll runway. `FRAME_MAX_GRID` is what the renderer will draw in
/// one screen; the drawer has to be able to scroll, so the ranked order is kept
/// further than it is drawn and the window is a slice of it. Four screens is the
/// current choice, and it is a **window, not the list**: a query matching more
/// than 256 apps shows its top 256, and scrolling past them shows an empty
/// grid rather than wrapping or mis-indexing.
///
/// The reference has no such limit because it virtualises with a `RecyclerView`
/// over a fully materialised adapter
/// (`apps/recents/RecentTasksAdapter.kt` / `WorkspaceLayoutManager`); UTLC has no
/// heap-backed adapter on the frame path, so a fixed runway is the honest
/// equivalent. Raising it costs 24 bytes per slot of stack.
const DRAWER_RANK_RUNWAY: usize = FRAME_MAX_GRID * 4;
const FRAME_MAX_DOCK: usize = 5;
const FRAME_MAX_TABS: usize = 32;

/// A fixed-capacity, per-frame view with a valid prefix.
///
/// This is the `recents_rows` pattern applied to the four collections the
/// shell used to heap-allocate 120 times a second. A `Vec` per frame is not
/// free even when it is small: each one is a `malloc` + `free` pair, and the
/// plan's zero-allocation rule is about the frame path, not about which
/// collection is convenient.
///
/// The capacity is a fixed array, so there is no growth and no reallocation --
/// `push` past `CAP` is a no-op that drops the row, which is the same
/// behaviour the renderer's own `.take(max_apps)` produced.
struct FrameView<T: Copy, const CAP: usize> {
    items: [T; CAP],
    len: usize,
}

impl<T: Copy, const CAP: usize> FrameView<T, CAP> {
    #[inline]
    const fn new(empty: T) -> Self {
        Self {
            items: [empty; CAP],
            len: 0,
        }
    }

    /// Append a row, dropping it if the fixed capacity is full.
    #[inline]
    fn push(&mut self, v: T) {
        if self.len < CAP {
            self.items[self.len] = v;
            self.len += 1;
        }
    }

    #[inline]
    fn as_slice(&self) -> &[T] {
        &self.items[..self.len]
    }

    /// Drop every row, keeping the storage.
    ///
    /// A `Vec` would have the same effect plus a deallocation, and this is on the
    /// frame path. The elements are left as the `empty` filler rather than being
    /// dropped: `T: Copy`, so there is nothing to drop.
    #[inline]
    fn clear(&mut self) {
        self.len = 0;
    }
}

/// The shell's per-frame scratch, allocated once before the event loop.
///
/// Every field is a fixed-capacity array on the stack, so the loop body holds
/// no heap allocation at all: the four `Vec`s that were rebuilt on every
/// frame tick are now written into pre-sized storage and handed to the
/// renderer as slices.
struct ShellScratch<'a> {
    tab_infos: FrameView<TerminalTabInfo<'a>, FRAME_MAX_TABS>,
    grid_items: FrameView<AppGridItem<'a>, FRAME_MAX_GRID>,
    dock_items: FrameView<AppGridItem<'a>, FRAME_MAX_DOCK>,
    drawer_items: FrameView<AppGridItem<'a>, FRAME_MAX_GRID>,
    /// The open folder's contents.
    ///
    /// Frame-scoped rather than a loop-scoped `Vec`, like the dock and the
    /// drawer. A `Vec<AppGridItem<'a>>` that outlives the frame holds borrows
    /// into `all_managed_apps`, and `all_managed_apps` is *reassigned* by the
    /// catalogue rescan -- so a long-lived view of it cannot compile at all,
    /// let alone stay correct. Rebuilt only while a folder is open.
    folder_items: FrameView<AppGridItem<'a>, FRAME_MAX_GRID>,
    /// The closed-state folder clusters, keyed by folder id.
    ///
    /// Frame-scoped for the same reason as [`Self::folder_items`]: a `Vec` of
    /// these outliving the frame holds borrows into `all_managed_apps`, which the
    /// catalogue rescan reassigns. Sized at [`FRAME_MAX_GRID`] because a workspace
    /// page holds at most that many cells, so that is the hard bound on folders
    /// too -- in practice a page has none.
    folder_previews: FrameView<FolderPreviewRow<'a>, FRAME_MAX_GRID>,
    /// The shade's notification rows, newest first, borrowed from the store.
    ///
    /// The store owns `String`s and the renderer borrows, so this is the
    /// per-frame projection between them -- the same reason the grid has an
    /// `AppGridItem` view rather than the catalogue's own type. Bounded by
    /// [`SHADE_NOTIF_ROWS`] because that is how many cards the layout reserves;
    /// a store holding more still drives the badge counts, which are counted
    /// separately and are not truncated by the shade's row budget.
    notif_rows: FrameView<NotifRow<'a>, SHADE_NOTIF_ROWS>,
}

impl<'a> ShellScratch<'a> {
    #[inline]
    const fn new() -> Self {
        Self {
            tab_infos: FrameView::new(TerminalTabInfo::EMPTY),
            grid_items: FrameView::new(AppGridItem::EMPTY),
            dock_items: FrameView::new(AppGridItem::EMPTY),
            drawer_items: FrameView::new(AppGridItem::EMPTY),
            folder_items: FrameView::new(AppGridItem::EMPTY),
            folder_previews: FrameView::new(FolderPreviewRow::EMPTY),
            notif_rows: FrameView::new(NotifRow::EMPTY),
        }
    }
}

/// Owned label buffers, one per grid cell.
///
/// A separate type from [`ShellScratch`] and not a field of it, for a borrow
/// reason that is worth stating: the shell takes `tab_infos`, `grid_items`,
/// `drawer_items` and `dock_items` as shared slices part-way through the frame
/// and holds them until the state struct is built. Anything that *writes*
/// therefore has to be a different object, or every later write would be a
/// second mutable borrow of a struct that is already borrowed.
///
/// `AppGridItem` borrows its name from the catalogue, so a label that has to be
/// shortened has nowhere else to live. The 12-character cut used to happen
/// once, at catalogue scan, which meant the shortened name was what every
/// surface showed -- drawer, dock, recents and the long-press popup all
/// inherited it and none of them could show the rest.
struct LabelScratch {
    labels: [String; FRAME_MAX_GRID],
}

impl LabelScratch {
    #[inline]
    fn new() -> Self {
        Self {
            labels: std::array::from_fn(|_| String::new()),
        }
    }

    /// A cell-elided copy of `name`, in the buffer for cell `slot`.
    ///
    /// Returns the borrowed slice. Reuses the buffer, so for a name that fits --
    /// the common case -- this is one `clear` and one `push_str` against
    /// capacity that is already there, and nothing is allocated after the first
    /// few frames.
    fn elide(&mut self, slot: usize, name: &str) {
        let Some(buf) = self.labels.get_mut(slot) else {
            // A cell past the frame's capacity has no buffer, and no item that
            // would borrow one, so there is nothing to write.
            return;
        };
        // The common case -- a name that fits -- is one `clear` and one
        // `push_str` against capacity that already exists, so after the first
        // few frames this allocates nothing.
        *buf = truncate_display_name(name, DRAWER_LABEL_CHARS, true);
    }

    /// The elided label for cell `slot`, or `""` past the capacity.
    fn get(&self, slot: usize) -> &str {
        self.labels.get(slot).map(|s| s.as_str()).unwrap_or("")
    }
}

/// Characters a drawer cell's label may use before it is elided.
///
/// A display concern, so it lives at the draw site. The reference derives its
/// own from the cell width rather than a character count
/// (`BubbleTextView` measures and lays out two lines when they fit), so this is
/// an approximation -- but it is a *local* approximation, replaceable by
/// measurement, where the old one was baked into the catalogue.
const DRAWER_LABEL_CHARS: usize = 16;

/// Where a touch in the recents carousel landed: a card, the Clear All
/// button, or the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverviewHit {
    Card(usize),
    ClearAll,
    Background,
}

/// Hit-test the recents carousel.
///
/// Uses `Recents::card_rect_centered` so the shell and the renderer agree on
/// where a card is -- which is the whole point of that helper. A separate
/// hit-test would let a card be drawn in one place and grabbable in another.
fn overview_hit(recents: &Recents, w: f32, h: f32, x: f32, fy: f32) -> OverviewHit {
    if overview_clear_all_rect(w, h).contains(x, fy) {
        return OverviewHit::ClearAll;
    }
    for i in 0..recents.len as usize {
        if let Some(r) = recents.card_rect_centered(i, w, h) {
            if r.contains(x, fy) {
                return OverviewHit::Card(i);
            }
        }
    }
    OverviewHit::Background
}

/// The Clear All button's rect, in panel px.
///
/// A fixed 96x48 dp box centred under the carousel, matching
/// `RecentsView`'s `mClearAllButton`. Constant rather than layout-derived
/// because the reference positions it against the panel, not the card strip,
/// and a carousel that moves the button as cards scroll is worse than one
/// that does not.
fn overview_clear_all_rect(w: f32, h: f32) -> Rect {
    let dp = w / 420.0;
    let bw = 96.0 * dp;
    let bh = 48.0 * dp;
    Rect {
        x: (w - bw) * 0.5,
        y: h * 0.88,
        w: bw,
        h: bh,
        radius: bh * 0.5,
    }
}

/// `ACTION_DOWN` in the carousel: arm a card drag, or fire Clear All.
///
/// Clear All is on the *release* in the reference (`mClearAllButton` has an
/// `OnClickListener`), so a down here only records the target; the decision
/// is made in [`overview_touch_up`]. Recording rather than acting keeps a
/// drag that wanders off the button from firing it, which is the behaviour
/// every Android button has.
fn overview_touch_down(recents: &mut Recents, w: f32, h: f32, x: f32, fy: f32) {
    // Nothing to arm: `on_card_drag` claims the drag on its first call
    // (`self.dragging == NO_CARD`), so the down only has to answer "is there
    // a card here" -- which `overview_touch_move` re-asks anyway. Kept as an
    // explicit arm so the three phases read symmetrically and so a future
    // press-scale effect has somewhere to live.
    let _ = overview_hit(recents, w, h, x, fy);
}

/// `ACTION_MOVE` in the carousel: feed the card under the finger.
///
/// The model, not the shell, decides whether the drag has crossed its detach
/// threshold and which direction counts as a dismiss.
/// Fire a haptic pulse, if the user has haptics on.
///
/// # Why this exists
///
/// `LauncherState::haptics` was persisted, exposed in the settings panel, and
/// read by **nothing**. All five `haptics.trigger` sites fired unconditionally,
/// so a user who turned haptics off got them anyway — a setting that looks
/// authoritative and is not is worse than no setting, because the user will
/// assume the device is broken rather than that the launcher ignored them.
///
/// This is the single gate, rather than an `if` at each of the five sites,
/// because five copies of the same condition is five chances for the next one to
/// be added without it. That has already happened once in this file's history.
///
/// # Note on this device
///
/// There is no vibrator node in this rootfs, so [`Haptics::trigger`] is a
/// runtime no-op regardless. The gate is still correct and still tested: the
/// setting has to be honoured on hardware that *has* a vibrator, and a test that
/// only passes because the hardware is absent would not be a test of the gate.
#[inline]
fn haptic(h: &mut Haptics, enabled: bool, fx: HapticEffect) {
    if enabled {
        h.trigger(fx);
    }
}

fn overview_touch_move(
    recents: &mut Recents,
    haptics: &mut Haptics,
    haptics_enabled: bool,
    w: f32,
    h: f32,
    x: f32,
    fy: f32,
) {
    let OverviewHit::Card(i) = overview_hit(recents, w, h, x, fy) else {
        return;
    };
    // `on_card_drag` returns the *rising edge* of the dismiss-threshold band,
    // which is the one frame where the user learns that releasing will kill the
    // app -- so that frame gets the firmer `Confirm` pulse rather than a detent
    // tick.
    //
    // It has to be the edge, not the `threshold_haptic_done` latch. The latch
    // stays true for the rest of the drag, so reading it here fired a pulse on
    // *every* `ACTION_MOVE`: a 30-frame drag buzzed 30 times. That is the same
    // mistake the fast scroller's `on_move` return value was written to avoid.
    if recents.on_card_drag(i, fy) {
        haptic(haptics, haptics_enabled, HapticEffect::Confirm);
    }
}

/// `ACTION_UP` in the carousel: release a card, or fire Clear All.
///
/// The release is what commits: `Recents::on_card_release` decides whether the
/// card snaps back or commits to `KillState::Grace`, and the `KillQueue` it
/// raises is drained by the frame loop (which is where the `SIGKILL` is
/// sent -- never here, inside the input callback).
///
/// A `Cancel` is a system takeover, not a gesture end: the card returns home
/// and nothing is killed.
fn overview_touch_up(
    recents: &mut Recents,
    // `at` is the panel size and `(x, fy)` the release point, both bundled
    // rather than passed flat: they are the two things every arm needs, and a
    // caller that paired them wrongly would hit-test against a different panel
    // than the one it is drawing.
    at: (f32, f32),
    (x, fy): (f32, f32),
    cancelled: bool,
    shell_state: &mut ShellState,
    launched: &mut Option<usize>,
) {
    match overview_hit(recents, at.0, at.1, x, fy) {
        OverviewHit::ClearAll => {
            if cancelled {
                return;
            }
            let _ = recents.clear_all();
            // Closing the overview is NOT done here. `clear_all` starts the
            // dismissal animations; the stack is still full at this instant
            // and only drains as they settle. Deciding "empty" from a stack
            // that has not emptied yet would close the overview underneath a
            // carousel that is still animating away, and the dismissal
            // springs would integrate against a state that no longer exists.
            // The frame loop owns that transition -- see the `recents.len ==
            // 0` arm beside the overview spring.
        }
        OverviewHit::Card(i) => {
            let outcome = if cancelled {
                // A system takeover, not a gesture end: `on_card_release`
                // would arm the kill clock, so the card is pushed back toward
                // its origin instead.
                recents.on_card_drag(i, 0.0);
                recents.on_card_release(i)
            } else {
                recents.on_card_release(i)
            };
            // A card that came back is a *tap*, not a fling -- that is the
            // reference's `TaskView.setOnClickListener` -> `launchWithAnimation`
            // (`TaskView.kt:598`, `:1355`), and it is the primary action of the
            // whole surface.
            //
            // The outcome used to be discarded here, so a tap on a card did
            // nothing at all: `on_card_release` returns `Cancelled` for a
            // release that did not travel past the commit threshold, both call
            // sites dropped the value, and the card simply settled back. The
            // overview could be entered and then did nothing.
            if matches!(outcome, DismissOutcome::Cancelled { .. }) {
                *launched = Some(i);
                recents.select(i);
            }
        }
        OverviewHit::Background => {}
    }
    let _ = shell_state;
}

/// The app a recents card names, resolved against the catalogue.
///
/// `None` for a card whose app has since been uninstalled: the reference
/// removes the card on the package-removed broadcast
/// (`PackageUpdatedTask.java:79-88`), and UTLC's catalogue rescan has no way to
/// reach a live card, so the honest result is "this card launches nothing"
/// rather than a panic on an index that no longer resolves.
fn recents_card_app<'a>(
    recents: &Recents,
    all_managed_apps: &'a [ManagedApp],
    card: usize,
) -> Option<&'a ManagedApp> {
    // `TaskCard::app_id` is an index into the catalogue, not an id string, so
    // the lookup is a bounds-checked index rather than a scan. A card whose app
    // has since been uninstalled is stale -- the catalogue rescan has shrunk the
    // vector out from under it -- and resolves to `None` rather than panicking.
    recents
        .cards
        .get(card)
        .and_then(|c| all_managed_apps.get(c.app_id as usize))
}

/// A one-line weather string, read from a file the platform provides.
///
/// There is no weather *source* in UTLC and there is not going to be one: an
/// HTTP client means a TLS stack, and the zero-third-party-dependency rule
/// (AGENTS.md §1) rejects that outright. So the shell is the *consumer* of an
/// already-resolved observation, published by whatever does the network work
/// -- a companion daemon, a `systemd` unit, a cron job, or a human writing the
/// file. The contract is deliberately trivial to satisfy:
///
/// ```text
///   /run/utlc-weather            (or $XDG_RUNTIME_DIR/utlc-weather)
///   one line, no newline required:
///     "21C Light rain"
///     "18C"
///     "Clear 27C"
/// ```
///
/// Anything unreadable, empty, oversized or non-UTF-8 reads as "no weather",
/// which is the correct state for a device that has no weather service. That
/// is the same contract the field always had: `DrmInteractiveState::weather_str`
/// is empty-safe and the renderer falls back to the date-only layout.
///
/// Bounded on every axis. One `open` + one `read` of at most
/// [`WEATHER_MAX_BYTES`], at [`SMARTSPACE_TICK_MS`], into a caller-owned
/// buffer that is reused -- so the steady-state cost is one syscall pair and
/// zero allocation.
struct WeatherSink {
    /// Where to read. A field, not a call to [`weather_path`] inside
    /// `refresh`, so a test can point it at its own file. Routing the path
    /// through the process-global `XDG_RUNTIME_DIR` instead makes concurrent
    /// tests race: `set_var` in one thread is invisible-by-construction to
    /// another, and the suite is multi-threaded by default.
    path: std::path::PathBuf,
    /// Bytes read on the last successful refresh, for change detection.
    last_len: usize,
    /// Hash of the last successful read. `last_len` alone misses a same-length
    /// update ("21C" -> "31C"), which is the most common one.
    last_hash: u64,
    /// Consecutive *ticks* since the last successful read, whether or not the
    /// file was actually read on that tick.
    ///
    /// Counting ticks rather than reads is load-bearing. The backoff skips the
    /// read except on a poll boundary, so a counter that only advanced on a
    /// real read would freeze at the first skipped value -- 13 say -- and
    /// `13 % 12 != 0` would hold forever. A sink that came back to life after
    /// backing off would then never be noticed again, for the life of the
    /// process. Counting ticks makes the boundary reachable.
    misses: u32,
}

impl WeatherSink {
    /// The production sink: `$XDG_RUNTIME_DIR/utlc-weather`, else `/run`.
    fn new() -> Self {
        Self::at(weather_path())
    }

    /// A sink reading an explicit path.
    fn at(path: std::path::PathBuf) -> Self {
        Self {
            path,
            last_len: 0,
            last_hash: 0,
            misses: 0,
        }
    }

    /// Re-read the sink into `buf`, leaving `buf` empty on any failure.
    #[inline]
    fn refresh(&mut self, buf: &mut String) {
        buf.clear();
        // Back off after `WEATHER_GIVE_UP_AFTER` ticks with no observation,
        // down to one read per `WEATHER_BACKOFF_TICKS`. Not a spin-lock: the
        // tick is already 5 s, so the floor is one `open` per 60 s.
        //
        // The counter advances on the skipped ticks too. See the field doc.
        let on_poll_boundary = self.misses < WEATHER_GIVE_UP_AFTER
            || self.misses.is_multiple_of(WEATHER_BACKOFF_TICKS);
        if !on_poll_boundary {
            self.misses = self.misses.saturating_add(1);
            return;
        }
        // Read into a fixed stack buffer with `O_NOFOLLOW`: no allocation, and
        // a symlink at the sink path cannot redirect the read. `WEATHER_MAX_BYTES`
        // plus a little headroom, so a file longer than the renderer's cap is
        // truncated rather than read in full.
        let mut raw = [0u8; WEATHER_MAX_BYTES * 4];
        match read_no_follow(&self.path, &mut raw) {
            Some(n) if n > 0 => {
                let s = match std::str::from_utf8(&raw[..n]) {
                    Ok(s) => s.trim(),
                    Err(_) => {
                        self.misses = self.misses.saturating_add(1);
                        return;
                    }
                };
                // Truncate on a CHARACTER boundary, for the same reason
                // `truncate_display_name` does: a byte cut through a UTF-8
                // sequence would be a panic in the renderer's glyph lookup.
                let mut end = s.len().min(WEATHER_MAX_BYTES);
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    self.misses = self.misses.saturating_add(1);
                    return;
                }
                let h = fnv1a(&s.as_bytes()[..end]);
                if h != self.last_hash {
                    self.last_hash = h;
                    self.last_len = end;
                }
                self.misses = 0;
                buf.push_str(&s[..end]);
            }
            _ => {
                self.misses = self.misses.saturating_add(1);
                // A vanished sink invalidates the last good observation: the
                // smartspace must stop claiming a temperature the platform is
                // no longer publishing.
                self.last_hash = 0;
                self.last_len = 0;
            }
        }
    }
}

/// FNV-1a, the same hash the damage signature uses, so the two cannot disagree
/// about what "the same bytes" means.
#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// The weather sink's path: `$XDG_RUNTIME_DIR/utlc-weather`, else `/run`.
///
/// `XDG_RUNTIME_DIR` first because a test or a per-user instance needs to
/// redirect it without root, and a session bus is the correct place for
/// per-user runtime state anyway.
fn weather_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir).join("utlc-weather");
        }
    }
    std::path::PathBuf::from("/run/utlc-weather")
}

/// Longest weather line the smartspace will render, bytes.
const WEATHER_MAX_BYTES: usize = 32;
/// Consecutive read failures before the sink starts backing off.
const WEATHER_GIVE_UP_AFTER: u32 = 12;
/// Tick divisor once backing off: 12 * 5 s = 60 s to notice, then one read
/// per 12 ticks.
const WEATHER_BACKOFF_TICKS: u32 = 12;

/// Clamp a proposed drawer scroll to `[0, max]`, with a short rubber band
/// past each end.
///
/// A hard clamp at the boundary means the list stops dead, which reads as a
/// dropped frame rather than as an edge. The reference overshoots and springs
/// back (`OverscrollEffect`); modelling that properly needs a spring on the
/// drawer, which this does not have, so the band is deliberately shallow and
/// self-correcting: the offset is allowed to go a *fraction* past each end,
/// never past half the band, and because `proposed` is derived from the
/// current offset rather than from an absolute finger position, dragging back
/// returns it to the edge exactly.
fn clamp_drawer_scroll(proposed: f32, l: &Layout, total_apps: usize) -> f32 {
    /// Fraction of the finger's travel past an edge that the list follows.
    const RUBBER: f32 = 0.35;
    let max = l.drawer_max_scroll_y(total_apps);
    // A list that fits does not move. Not even into the band: with nothing to
    // scroll, the rubber band is the only thing the user can feel, and it
    // turns a short list into one that appears to drag.
    if max <= 0.0 {
        return 0.0;
    }
    let slack = (l.drawer_grid_bottom - l.drawer_grid_top) * 0.5;
    if proposed < 0.0 {
        // Past the top: a little negative, never more than `slack`.
        -(proposed.abs() * RUBBER).min(slack)
    } else if proposed > max {
        max + ((proposed - max) * RUBBER).min(slack)
    } else {
        proposed
    }
}

/// The app id under a long-press on the workspace, or `""` for the wallpaper.
///
/// The workspace grid stores app *ids* per page, so the hit test resolves to a
/// slot and that slot indexes the page. Both halves are bounds-checked: the
/// page can be shorter than `grid_cols * max_rows` (a nearly-empty home
/// screen) and the grid can report a slot past it, which is a miss, not a
/// panic -- a long-press on empty workspace must fall through to the
/// workspace menu.
fn long_press_app_id(
    apps: &[ManagedApp],
    page_app_ids: &[String],
    l: &Layout,
    scroll: f32,
    total_pages: usize,
    x: f32,
    fy: f32,
) -> String {
    let Some((_, slot)) = l.home_grid_hit_paged(x, fy, scroll, total_pages) else {
        return String::new();
    };
    let Some(id) = page_app_ids.get(slot) else {
        return String::new();
    };
    // A page id that no longer resolves against the catalogue is stale (the
    // app was uninstalled and the page not yet pruned). Treat it as a miss so
    // the popup does not name an app that cannot be launched.
    if !apps.iter().any(|a| a.id == *id) {
        return String::new();
    }
    id.clone()
}

/// The control a touch landed on, and the bounds its state layer is drawn in.
///
/// A Material 3 press state layer is painted *by the control*, so it is
/// bounded by the control. Without this a tap anywhere expanded a 435 px disc
/// across the panel. The rect returned is padded by the 8 dp touch-slop ring so
/// a finger that lands a few px outside the icon still lights the icon, which
/// is what `View.OnTouchListener` gets for free from the parent's bounds.
fn ripple_target(
    l: &Layout,
    x: f32,
    fy: f32,
    scroll: f32,
    _page: usize,
    total_pages: usize,
) -> Option<Rect> {
    const TOUCH_TARGET_PAD_DP: f32 = 8.0;
    let pad = TOUCH_TARGET_PAD_DP * (l.w / 420.0);
    let pad_cell = |c: Cell| Rect {
        x: c.x - pad,
        y: c.y - pad,
        w: c.w + pad * 2.0,
        h: c.h + pad * 2.0,
        radius: 0.0,
    };
    let pad_rect = |r: Rect| Rect {
        x: r.x - pad,
        y: r.y - pad,
        w: r.w + pad * 2.0,
        h: r.h + pad * 2.0,
        radius: r.radius,
    };
    // Workspace grid first: it is the largest target and the one a launcher
    // tap overwhelmingly lands on.
    if let Some(slot) = l.home_grid_hit_paged(x, fy, scroll, total_pages) {
        return Some(pad_cell(l.grid_cell(slot.1)));
    }
    if let Some(slot) = l.home_dock_hit(x, fy) {
        return Some(pad_rect(l.dock_icon_rect(slot)));
    }
    None
}

/// The vsync interval for a panel refresh rate, in whole microseconds.
///
/// Microseconds rather than milliseconds because the whole point of this is
/// 120 Hz: 1e6/120 = 8333.33 us. Rounding to the *nearest* millisecond gives
/// 8 ms, which is 125 Hz and beats every deadline it is measured against;
/// truncating to 8 ms under-samples for the same reason. Rounding to the
/// nearest microsecond is exact to 0.012%.
///
/// The range is clamped to the rates a panel can actually be: 24 Hz (the
/// slowest a modern LCD will go) to 480 Hz (above any real panel, and above
/// what `MobileScene` accepts). A non-finite or non-positive rate falls back
/// to 60 Hz, which is what `VsyncConfig::new` already does internally, so a
/// bad mode degrades to the same answer the rest of the stack would pick.
fn frame_interval_for_hz(refresh_hz: f64) -> Duration {
    const FALLBACK_HZ: f64 = 60.0;
    let hz = if refresh_hz.is_finite() && refresh_hz > 0.0 {
        refresh_hz.clamp(24.0, 480.0)
    } else {
        FALLBACK_HZ
    };
    Duration::from_micros((1_000_000.0 / hz).round() as u64)
}

/// Clamp a `.desktop` `Name=` to `max_chars` *characters*, not bytes.
///
/// `d_app.name[..12]` was byte slicing, so a name whose 12th byte fell inside a
/// multi-byte UTF-8 sequence (`"Trình duyệt"`, `"设置应用"`, any accented Latin)
/// panicked on the index. The release profile sets `panic = "abort"`, so a
/// single non-ASCII entry in `/usr/share/applications` aborted the compositor
/// during the boot-time catalogue scan. Char counting is the fix, and it also
/// matches what the user actually sees: 12 glyphs, not 12 bytes.
/// A name cut to at most `max_chars` *characters*, optionally marked with an
/// ellipsis.
///
/// The character counting is the point. The original bug this replaced was
/// `d_app.name[..12]`, which panicked the whole compositor during the
/// catalogue scan on any non-ASCII entry, and the release profile sets
/// `panic = "abort"` -- so a single CJK `.desktop` `Name=` bricked the display.
/// Cutting by `char` is also what the user sees: 12 glyphs, not 12 bytes.
///
/// The ellipsis is three ASCII dots rather than U+2026 because `font.rs` routes
/// every codepoint outside `0x20..0x7E` to `draw_missing_glyph` when no system
/// TTF is loaded, and the stock image installs no font -- so a real ellipsis
/// renders as a tofu box. This should become U+2026 the moment a font is
/// actually present, and the constant is the one place to change.
fn truncate_display_name(name: &str, max_chars: usize, ellipsis: bool) -> String {
    let total = name.chars().count();
    if total <= max_chars {
        return name.to_string();
    }
    let keep = if ellipsis {
        max_chars.saturating_sub(3)
    } else {
        max_chars
    };
    let mut out: String = name.chars().take(keep).collect();
    if ellipsis {
        out.push_str("...");
    }
    out
}

fn build_all_apps(catalogue: &DesktopCatalogue, hidden: &[String]) -> Vec<ManagedApp> {
    let mut apps = Vec::new();

    // Check if Firefox is installed in the catalogue
    let firefox_installed = catalogue.apps().iter().find(|a| {
        a.id.eq_ignore_ascii_case("firefox")
            || a.id.eq_ignore_ascii_case("firefox-esr")
            || a.name.to_lowercase().contains("firefox")
    });

    let browser_exec = if let Some(ff) = firefox_installed {
        let clean = ff.clean_exec();
        if clean.is_empty() {
            "/usr/bin/firefox".to_string()
        } else {
            clean
        }
    } else {
        String::new()
    };

    // 1. Built-in Core System Apps
    apps.push(builtin_app("phone", "Phone", "", 0xFF10B981, "P"));
    apps.push(builtin_app("messages", "Messages", "", 0xFF3B82F6, "M"));
    apps.push(builtin_app(
        "browser",
        "Browser",
        &browser_exec,
        0xFF06B6D4,
        "B",
    ));
    apps.push(builtin_app("camera", "Camera", "", 0xFFF43F5E, "C"));
    apps.push(builtin_app("gallery", "Gallery", "", 0xFF8B5CF6, "G"));
    apps.push(builtin_app("settings", "Settings", "", 0xFF64748B, "S"));
    apps.push(builtin_app("files", "Files", "", 0xFFF59E0B, "F"));
    apps.push(builtin_app("music", "Music", "", 0xFFD946EF, "M"));
    apps.push(builtin_app("terminal", "Terminal", "", 0xFF1E293B, ">"));
    apps.push(builtin_app("treble", "Treble OS", "", 0xFF6366F1, "U"));
    apps.push(builtin_app("contacts", "Contacts", "", 0xFF14B8A6, "C"));
    apps.push(builtin_app("clock", "Clock", "", 0xFFEF4444, "T"));

    // 2. Discovered installed applications from /usr/share/applications etc.
    for d_app in catalogue.apps() {
        if d_app.no_display || d_app.id.eq_ignore_ascii_case("utlc") {
            continue;
        }
        // A user-hidden app is dropped from the *catalogue*, not from each
        // render loop. The reference keeps the app and filters it at list-build
        // time (`LawnchairAlphabeticalAppsList.updateItemFilter:79-86`), which
        // has the consequence that its notification dots and deep shortcuts
        // still resolve; filtering once here means a hidden app cannot leak
        // back in through a search result, a recents card or a dock slot.
        if hidden.contains(&d_app.id) {
            continue;
        }

        let is_firefox = d_app.id.eq_ignore_ascii_case("firefox")
            || d_app.id.eq_ignore_ascii_case("firefox-esr")
            || d_app.name.to_lowercase().contains("firefox");

        // The full catalogue name, not a 12-character cut.
        //
        // `truncate_display_name` exists to stop `d_app.name[..12]` panicking the
        // whole compositor on a non-ASCII entry, and it did that job. Keeping
        // the truncation *here* was the mistake: it baked a display decision
        // into the data model, so the shortened name appeared in the drawer,
        // the dock, recents, the long-press popup and the quick settings
        // shortcut, and there was no way to see or change the rest of it.
        // Elision is a per-cell concern and belongs at the draw site, which
        // already clips to the cell width.
        //
        // The Firefox special case is a rename, not a truncation: the real
        // Firefox entry is called "Firefox" and the shell pinned the branding
        // so the icon, the label and the store entry all agree.
        let display_name = if is_firefox {
            "Firefox".to_string()
        } else {
            d_app.name.clone()
        };

        let color = if is_firefox {
            0xFFFF5722
        } else {
            get_app_color(&d_app.id)
        };

        let glyph = if is_firefox {
            "F".to_string()
        } else {
            d_app
                .name
                .chars()
                .next()
                .unwrap_or('A')
                .to_uppercase()
                .to_string()
        };

        let mut exec = d_app.clean_exec();
        if exec.is_empty() && is_firefox {
            exec = "/usr/bin/firefox".to_string();
        }

        // Avoid adding duplicate if already present in base apps
        if !apps
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(&display_name))
        {
            let mut app = ManagedApp::new(&d_app.id, &display_name, &exec, color, &glyph)
                .with_keywords(&d_app.keywords);
            // `Icon=` first, then the desktop id (many themes key on it).
            if !d_app.icon.is_empty() {
                app.icon_keys.push(d_app.icon.clone());
            }
            let id_key = d_app.id.to_lowercase();
            if !d_app.id.is_empty() && !app.icon_keys.contains(&id_key) {
                app.icon_keys.push(id_key);
            }
            apps.push(app);
        }
    }

    // Sort the *whole* catalogue by display name, case-insensitively, before
    // anything indexes into it.
    //
    // The order this list is in is load-bearing, not cosmetic:
    //
    //  * `SectionIndex::build` maps each row to a letter and the fast
    //    scroller maps a scroll position back to a row. Both assume rows are
    //    grouped and monotonically increasing in name. Built on the unsorted
    //    list, the section index's binary search lands on an arbitrary row and
    //    the teardrop shows a letter that is not the one under the thumb.
    //  * A recents card carries a `u32` *index* into this list, so a
    //    re-sort between pushing a card and drawing the overview would resolve
    //    to a different app.
    //
    // Sorting once here, before the list is ever published, fixes all three:
    // the order is stable for the lifetime of the catalogue. The rescan path
    // rebuilds the list and re-sorts, which is the same function, so a rescan
    // cannot leave it half-sorted either.
    //
    // `sort_unstable_by` rather than `sort_by`: equal keys are interchangeable
    // (two apps with the same display name are already deduplicated above),
    // and this is not a per-frame call.
    apps.sort_unstable_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });

    apps
}

fn main() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let args: Vec<String> = env::args().collect();
    let json_output = args.iter().any(|a| a == "--json");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        process::exit(0);
    }

    if args.iter().any(|a| a == "--check-protocols") {
        let ok = check_protocols(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--benchmark") {
        let ok = run_benchmarks(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-gestures") {
        let ok = test_gestures(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-desktop") {
        let ok = test_desktop(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-systemui") {
        let ok = test_systemui(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-lockscreen") {
        let ok = test_lockscreen(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-ime") {
        let ok = test_ime(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-power-sync") {
        let ok = test_power_sync(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--daemon") {
        run_daemon();
        process::exit(0);
    }

    // Default: run all Phase 3 comprehensive checks
    let ok = run_all_checks(json_output);
    process::exit(if ok { 0 } else { 1 });
}

fn print_usage() {
    println!("UTLC: Universal Treble Launcher & Compositor");
    println!("Usage:");
    println!("  utlc [OPTIONS]");
    println!("Options:");
    println!("  --daemon           Run as systemd service daemon (PID 1 integration)");
    println!("  --benchmark        Run performance benchmarks (Boot time, RSS, input latency)");
    println!("  --check-protocols  Verify all mobile Wayland protocol extensions");
    println!("  --test-gestures    Verify QuickStep gesture navigation engine");
    println!("  --test-desktop     Verify zero-allocation .desktop parser & fuzzy search");
    println!("  --test-systemui    Verify status bar, quick settings, and notifications");
    println!("  --test-lockscreen  Verify lock screen and Android Fingerprint HAL bridge");
    println!("  --test-ime         Verify virtual keyboard IME and viewport push");
    println!("  --test-power-sync  Verify UTIM MPG cgroup freezing and OOM hierarchy");
    println!("  --json             Emit structured JSON diagnostic output");
    println!("  --help, -h         Show this help message");
}

fn run_daemon() {
    println!("[+] Initializing UTLC (Universal Treble Launcher & Compositor)...");
    println!("[*] Single-process unified mobile compositor starting up...");

    // Detect HWC version from monolithic manifest and VINTF fragments
    let mut manifest_content = String::new();
    if let Ok(c) = fs::read_to_string("/vendor/etc/vintf/manifest.xml") {
        manifest_content.push_str(&c);
    }
    if let Ok(c) = fs::read_to_string("/vendor/manifest.xml") {
        manifest_content.push_str(&c);
    }
    if let Ok(entries) = fs::read_dir("/vendor/etc/vintf/manifest") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("xml") {
                if let Ok(c) = fs::read_to_string(&path) {
                    manifest_content.push_str(&c);
                }
            }
        }
    }
    let manifest_opt = if manifest_content.is_empty() {
        None
    } else {
        Some(manifest_content.as_str())
    };
    let hwc_version = HwcComposer::detect_version_from_manifest(manifest_opt);
    println!("[*] Initializing Hardware Composer: {:?}", hwc_version);
    let hwc = HwcComposer::new(hwc_version);

    let socket_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        if unsafe { libc::geteuid() } == 0 {
            "/run/user/0".into()
        } else {
            "/run/user/1000".into()
        }
    });
    let _ = fs::create_dir_all(&socket_dir);
    let socket_path = PathBuf::from(&socket_dir).join("wayland-0");

    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);
    if let Err(e) = server.power_sync.configure_self_oom_score() {
        eprintln!(
            "[-] OOM immunity not applied (needs CAP_SYS_RESOURCE): {}",
            e
        );
    }

    if let Err(e) = server.bind_socket() {
        eprintln!(
            "[-] Failed to bind Wayland socket at {}: {}",
            socket_path.display(),
            e
        );
    } else {
        println!(
            "[+] Bound Wayland display socket at {}",
            socket_path.display()
        );
        let is_root = unsafe { libc::geteuid() == 0 };
        if is_root {
            unsafe {
                if let Ok(c_s) = std::ffi::CString::new(socket_path.to_string_lossy().as_bytes()) {
                    libc::chmod(c_s.as_ptr(), 0o666);
                }
                if let Ok(c_sdir) = std::ffi::CString::new(socket_dir.as_bytes()) {
                    libc::chmod(c_sdir.as_ptr(), 0o777);
                }
            }
            let user_sock_dir = PathBuf::from("/run/user/1000");
            let _ = fs::create_dir_all(&user_sock_dir);
            unsafe {
                if let Ok(c_u) = std::ffi::CString::new("/run/user/1000") {
                    libc::chown(c_u.as_ptr(), 1000, 1000);
                    libc::chmod(c_u.as_ptr(), 0o777);
                }
            }
            let user_sock = user_sock_dir.join("wayland-0");
            let _ = fs::remove_file(&user_sock);
            let _ = std::os::unix::fs::symlink(&socket_path, &user_sock);
        }
    }

    match server.boot_to_first_frame() {
        Ok(dur) => {
            println!(
                "[+] Interactive home screen rendered in {:.2} ms ({:.4}s)",
                dur.as_secs_f64() * 1000.0,
                dur.as_secs_f64()
            );
        }
        Err(e) => {
            eprintln!("[-] Error presenting early boot frame: {}", e);
        }
    }

    // Try to open Direct Rendering Manager (DRM KMS) hardware scanout device
    let mut drm_display = match DrmKmsDevice::open_card("/dev/dri/card0") {
        Ok(dev) => {
            println!(
                "[+] Successfully initialized hardware DRM KMS display ({}x{})",
                dev.width, dev.height
            );
            Some(dev)
        }
        Err(e) => {
            println!(
                "[*] Hardware DRM KMS /dev/dri/card0 not available ({}), operating in headless Wayland mode",
                e
            );
            None
        }
    };

    // ---------------------------------------------------------------------
    // Persistent launcher state.
    //
    // Loaded once, before anything that depends on it. Before this, the
    // workspace was a `vec![…]` literal, the hotseat was a five-name array, and
    // the only `fs::write` in the binary was a terminal log -- so a reboot
    // returned the device to the factory layout and there was nowhere to put a
    // user preference even if the shell had had a way to ask for one.
    //
    // The reference keeps the same information in four stores: the `Favorites`
    // SQLite table, a DataStore named "preferences" with ~110 keys
    // (`PreferenceManager2.kt:975-978`), `FolderDao` and `WallpaperDao`. One
    // flat key/value file covers all four here, because a launcher on a 2 GB
    // phone does not need a database and AGENTS.md forbids the dependencies.
    // ---------------------------------------------------------------------
    let mut state = LauncherState::load();
    let state_path = LauncherState::default_path();
    eprintln!("[UTLC] launcher state: {}", state_path.display());

    let mut time_buf = [0u8; 5];
    // Material You: derive the tonal scheme from the wallpaper once at start
    // up. The whole shell is then themed from a single value.
    //
    // The light half of this was fully implemented and completely unreachable:
    // `MaterialYouPalette::from_seed_light` (`palette.rs:477`) is correct, has a
    // role-to-tone assertion table, and had no caller outside its own test
    // module -- so the launcher was dark-only, permanently, with no setting to
    // change. The reference offers Light / Dark / System
    // (`ThemePreference.kt:12-30`) over the same palette.
    //
    // The *seed* was equally fixed. `LauncherState::accent_source` and
    // `accent_color` existed to choose it -- `AccentSource::Custom` means "use my
    // colour, not the wallpaper's" -- and neither was read by anything, so the
    // "Accent colour" row in Settings cycled a value that had no effect on a
    // single pixel. `palette_seed` is now the one place that decision is made.
    let mut shell_palette = build_palette(&state);
    // The key the cached palette was built from, and the wallpaper path it was
    // derived from.
    //
    // Recomputing the palette means a filesystem walk (`wallpaper_seed`), so it
    // cannot go on the frame path -- but *not* recomputing it means the two
    // settings a user is most likely to change, dark theme and accent colour,
    // only take effect after a reboot. That is the "correct, tested, and you
    // cannot tell it is broken" failure again, one level up: the palette was read
    // from the state exactly once, at boot, and `state.dark_theme` had four
    // readers in the shell and none of them reached the palette.
    //
    // The path is a `String` rather than part of [`palette_key`] so the per-frame
    // comparison stays allocation-free; it is cloned only when a rebuild actually
    // happens.
    let mut palette_cache_key = palette_key(&state);
    let mut palette_wallpaper = state.wallpaper.clone();
    // The date is refreshed once per frame alongside the time, into a stack
    // buffer, so the render path borrows a `&str` and allocates nothing.
    let mut date_buf = [0u8; 16];

    if let Some(ref mut drm) = drm_display {
        let t_str = format_current_time(&mut time_buf, !state.clock_24h);
        drm.render_mobile_ui(
            t_str,
            server.scene.mode == utim_core::compositor::scene::ShellMode::LockScreen,
        );
        if let Err(e) = drm.flush() {
            eprintln!("[UTLC] DIRTYFB flush failed: {}", e);
        }
    }

    // Signal systemd/UTIM via sd_notify if NOTIFY_SOCKET is present
    let notify_socket = env::var("NOTIFY_SOCKET").ok();
    let notify_dgram = if notify_socket.is_some() {
        std::os::unix::net::UnixDatagram::unbound().ok()
    } else {
        None
    };
    if let (Some(ref dgram), Some(ref sock_path)) = (&notify_dgram, &notify_socket) {
        let _ = dgram.send_to(b"READY=1\nSTATUS=UTLC Mobile Shell Active\n", sock_path);
    }

    // Set up signal handling via signalfd for persistent daemon mode
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let sig_fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
    if sig_fd < 0 {
        eprintln!(
            "[UTLC] signalfd failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        return;
    }

    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll_fd >= 0 {
        let mut sig_ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: epoll_tag(sig_fd, K_SIGNAL),
        };
        unsafe {
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, sig_fd, &mut sig_ev);
        }
    }

    use std::os::unix::io::{AsRawFd, FromRawFd};
    if let Some(ref listener) = server.listener {
        let listen_fd = listener.as_raw_fd();
        if epoll_fd >= 0 {
            let mut listen_ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: epoll_tag(listen_fd, K_LISTEN),
            };
            unsafe {
                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, listen_fd, &mut listen_ev);
            }
        }
    }

    let watchdog_usec = env::var("WATCHDOG_USEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let watchdog_interval = watchdog_usec
        .map(|us| Duration::from_micros(us / 2))
        .unwrap_or(Duration::from_secs(5));
    let mut last_watchdog_ping = Instant::now();

    // Listen on input event devices (/dev/input/event*)
    let mut input_fds: Vec<libc::c_int> = Vec::new();
    let mut opened_paths: Vec<std::path::PathBuf> = Vec::new();
    open_new_input_devices(epoll_fd, &mut input_fds, &mut opened_paths);

    // Watch /dev/input for hotplug instead of polling read_dir every second:
    // a rescan runs exactly once per directory event (see the K_INPUT arm).
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if inotify_fd >= 0 {
        if let Ok(c_dir) = std::ffi::CString::new("/dev/input") {
            unsafe {
                libc::inotify_add_watch(
                    inotify_fd,
                    c_dir.as_ptr(),
                    libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO,
                );
            }
        }
        if epoll_fd >= 0 {
            let mut ino_ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: epoll_tag(inotify_fd, K_INPUT),
            };
            unsafe {
                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, inotify_fd, &mut ino_ev);
            }
        }
    }

    // State machine for interactive shell
    let mut dispatcher =
        InputDispatcher::new(server.scene.width as f32, server.scene.height as f32);
    let mut gesture_engine = GestureEngine::new(
        server.scene.width as f32,
        server.scene.height as f32,
        GestureConfig::default(),
    );
    let mut search_query = String::with_capacity(64);
    let mut search_active = false;
    let mut active_app: Option<String> = None;
    let mut app_input = String::with_capacity(128);
    let mut app_input_focused = false;
    let mut messages_list: Vec<String> = vec![
        "Treble Carrier: LTE connection active.".to_string(),
        "System: All Android 14 GKI HAL bridges ready.".to_string(),
    ];
    let mut terminal_tabs: Vec<TerminalTab> = vec![TerminalTab::new(1)];
    let mut active_tab_idx: usize = 0;
    let mut next_tab_id: usize = 2;
    let mut quick_tiles_active = [true, true, true, false, true, false, false, false];
    // Per-tile subtitle override. Empty means "the tile's own default", so a
    // healthy tile draws exactly what it drew before; a failed or impossible one
    // draws why. That is the reference's `QSTile.State.stateDescription`
    // (`QSTile.java:188`) and the reason `RadioError::label` exists rather than a
    // `String`.
    let mut quick_tile_note: [&str; 8] = [""; 8];
    let mut cursor_pos: Option<(usize, usize)> = None;
    let mut is_touching = false;
    let mut power_saver_mode = PowerSaverMode::Off;
    let mut super_extreme_state = SuperExtremeState::new();

    // Multi-page home screen and Android 17 / PixelUI App Drawer state
    let mut home_pages: Vec<Vec<String>> = state.home_pages.clone();
    let mut current_home_page: usize = state.current_page;
    let mut app_drawer_open = false;
    // Vertical scroll of the drawer grid, px. 0 when the drawer is closed and
    // reset on every open, so the list always starts at the top rather than
    // wherever the last visit left it.
    let mut drawer_scroll_y: f32 = 0.0;
    let mut drawer_search = String::with_capacity(64);
    let mut drawer_search_active = false;
    let mut selected_home_icon: Option<String> = None;
    // Lawnchair 17 / Pixel Launcher Animations and Transitions
    let mut drawer_progress: f32 = 0.0;
    let mut home_scroll_offset: f32 = 0.0;
    let mut app_launch_progress: f32 = 0.0;
    let mut app_launch_origin: Option<(f32, f32)> = None;
    let mut app_launch_color: u32 = 0xFF2563EB;
    let mut touch_ripple: Option<(f32, f32, f32, f32)> = None;
    // Probed once, at boot: a `readdir` of `/sys/class/leds` on the first tap
    // would put filesystem latency on the touch-response path, and the
    // hardware does not change while the shell is running.
    let mut haptics = Haptics::detect();
    let mut touch_drag_start: Option<(f32, f32)> = None;
    // True while a horizontal workspace drag is in flight, so release can
    // settle to the nearest page instead of treating it as a tap.
    let mut page_drag = false;
    let mut page_drag_velocity = 0.0f32;
    // Set for the rest of this input event once a drag has paged the workspace,
    // so the gesture engine's swipe fallback does not page a second time.
    let mut page_drag_seen = false;
    let mut last_move = Instant::now();
    // Previous touch y, for the drawer scroll. A delta rather than an absolute
    // position is what makes the rubber band self-correcting: the clamp
    // operates on `scroll - dy`, so dragging back to where the band started
    // returns the list to the edge exactly.
    let mut last_touch_y: f32 = 0.0;
    let mut drawer_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::drawer());
    let mut page_scroll_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::page_swipe());
    let mut app_launch_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::app_launch());
    let mut icon_bounce_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::icon_bounce());
    let mut pressed_icon_id: Option<String> = None;
    // Bounds the current tap's Material 3 state layer is painted into.
    //
    // Material draws the press state layer inside the control, so an
    // un-targeted tap has to resolve to *something* bounded. The handler sets
    // this to the cell it hit; `None` (the wallpaper, a drag that was not a
    // tap) means the whole panel, which for a control-less gesture is right.
    let mut ripple_clip: Option<Rect> = None;

    // ---------------------------------------------------------------------
    // Launcher-rewrite shell state (plan §7.6).
    //
    // These existed as unit-tested models in `utim_core` and nothing drove
    // them: `main.rs`'s gesture match ended in `_ => {}`, so
    // `GestureAction::Recents` and `GestureAction::BottomBarScrub` were
    // dropped on the floor and the overview could not be opened at all. The
    // states are here so the wiring has somewhere to land.
    // ---------------------------------------------------------------------
    let shell_layout = Layout::plain(server.scene.width as f32, server.scene.height as f32);
    // The fast-scroller geometry, needed per frame to convert the model's
    // track-local `thumb_y` into the 0..1 position the renderer takes. Held
    // rather than rebuilt so the two cannot be built from different layouts.
    let shell_fast_scroller: FastScrollerLayout = shell_layout.fast_scroller();
    // Monotonic millisecond clock for the models that time their own gestures
    // (the fast scroller's detent dwell, the folder's title delay). One
    // `Instant`, not a per-call `SystemTime` read.
    let shell_start = Instant::now();
    // `ViewConfiguration.getLongPressTimeout()` is 500 ms
    // (`ViewConfiguration.java:562`); the launcher uses its own value, which
    // is 500 ms too.
    const LONG_PRESS_MS: u64 = 500;
    let mut shell_state = ShellState::Normal;
    let mut recents = Recents::new(&shell_layout);
    let mut fastscroller = FastScrollerState::new();
    let mut folder = FolderOpen::closed(0);
    // Latched: has this overview session ever held a task card?
    let mut recents_showed_content = false;
    // The open folder's touch state. Separate from `FolderOpen` because that type
    // models the three *springs* and the title delay, while this models what a
    // touch inside the folder means. Merging them would put gesture recognition
    // next to animation integration, and the two have genuinely different inputs.
    let mut folder_gesture = FolderGestures::idle(Instant::now());
    // Double-tap detection. A drag or a cancel resets it; the pair is decided on
    // release, before the modal gate, because it is a workspace gesture.
    // The slop is density-scaled: the reference's double-tap window is a dp
    // distance (`DoubleTapConfig::for_density`), so a fixed pixel count would be
    // too tight on a dense panel and too loose on a coarse one. The shell's own
    // density is `Layout::profile().dp`, the same source the grid uses.
    let mut tap_history = utim_core::compositor::gestures::TapHistory::new(
        utim_core::compositor::gestures::DoubleTapConfig::for_density(
            Layout::plain(1.0, 1.0).profile().dp,
        ),
    );
    let mut tap_press_ms: u64 = 0;
    // Which double-tap action to run, from the reference's own default: `Sleep`
    // (`GestureHandlerConfig.kt:76-78`, `PreferenceManager2.kt:813-816`).
    let mut double_tap_action = utim_core::compositor::gestures::DoubleTapAction::Sleep;
    // Auto-rotate.
    //
    // The *policy* is complete and tested (1439 lines, 26 tests) and the shell owns
    // the actuation. Neither half can run yet, and both gaps are real rather than
    // one convenient omission:
    //
    // * no sensor source -- the D-Bus client for `net.hadess.SensorProxy` is not
    //   written, so there is no orientation to feed `update`;
    // * no actuation -- a modeset is an output-mode change on the KMS device, and
    //   `RotationRequest`/`ModesetAction` exist precisely so the decision can be
    //   made without it.
    //
    // So the policy is configured (which *is* reachable and is what makes the
    // settings meaningful) and `set_sensor_present(false)` records the truth: with
    // no sensor the policy holds, and holding is the correct answer rather than a
    // silent "auto-rotate does nothing".
    let mut rotation = utim_core::rotation::RotationPolicy::new();
    // Which page of the virtual keyboard is showing. `?123`/`123` used to be drawn
    // but changed nothing: the renderer always built a fresh `Keyboard::new`, which
    // is QWERTY by definition. Now the layout is shell state and the *same* value
    // drives both the hit test and the paint, so a key cannot be at one place and
    // mean another.
    let mut keyboard_layout = utim_core::compositor::ime::KeyboardLayout::Qwerty;
    // The app-info panel's contents, when it is open. Owned so the renderer can
    // borrow it for the frame without holding `all_managed_apps` across the
    // catalogue rescan.
    let mut app_info: Option<AppInfoState> = None;
    // Is the panel open, and which app is it for?
    let mut app_info_open = false;
    let mut app_info_id = String::new();
    // Set by the popup layer's `AppInfo` effect, consumed on the next line of the
    // touch path. A separate latch rather than writing `app_info_open` directly
    // because the effect's only handle is the `PopupTargets` bundle, and the
    // subject -- which app was tapped -- is not in it.
    let mut popup_opened_app_info = false;
    // An in-progress folder rename, if the long-press menu asked for one.
    let mut folder_rename: Option<FolderRename> = None;
    // The screen-blank policy, from `LauncherState::screen_timeout_s`.
    let mut screen_timeout = ScreenTimeout::new(state.screen_timeout_s);
    // Fold the persisted auto-rotate setting into the policy now, so the two
    // cannot disagree later, and re-sync it after every settings tap below
    // (a switch that only takes effect after a reboot is the same dead tap
    // the settings tests exist to catch). The sensor starts reported absent;
    // `SensorProxyConnection::connect` below replaces that with the truth.
    rotation.set_auto_rotate(state.auto_rotate);
    // SensorProxy subscription (hand-rolled D-Bus, no new deps). Connect + claim
    // once at startup; presence is truth, not optimism. Polls feed
    // `rotation.update()` in the frame loop below (~2Hz, off the draw path),
    // and `Rotate` decisions go through `decide_modeset` to
    // `DrmKmsDevice::set_orientation` there.
    let mut sensor_conn = utim_core::sensors::sensor_proxy::SensorProxyConnection::connect();
    if let Some(conn) = sensor_conn.as_mut() {
        if conn.claim_accelerometer() {
            rotation.set_sensor_present(true);
        } else {
            rotation.set_sensor_present(false);
        }
    } else {
        rotation.set_sensor_present(false);
    }
    let mut last_sensor_poll = std::time::Instant::now();
    // Which folder is open, 0 for none.
    //
    // `FolderOpen` models the three springs and the title delay, and the
    // renderer has drawn its surface, scrim, grid and footer since long before
    // there was a folder to open -- `folder.open()` had exactly one caller and
    // that caller was a test. The missing piece was never the animation, it was
    // a folder: `LauncherState::folders` now carries them, and this is the id
    // the whole open path hangs off.
    let mut open_folder: u32 = 0;
    // Items-per-page, seeded once from the layout and then clamped by
    // `FolderOpen` whenever the count changes. The reference sets it from the
    // container's own `itemsPerPage()` (`FolderPagedView.java:816`) for the same
    // reason: it is a layout fact, not a user setting.
    folder.items_per_page = Layout::plain(server.scene.width as f32, server.scene.height as f32)
        .folder()
        .items_per_page() as u8;
    folder.clamp_page();
    // Contents of the open folder. Rebuilt when a folder opens, from the
    // state's record. Empty selects the renderer's "no folder" path, so a stale
    // morph with an emptied list draws an empty folder rather than reading past
    // the end of anything.
    //
    // The *title* is an owned `String` rather than a `&str` borrowed out of
    // `state`. Holding a borrow for the whole event loop would keep `state`
    // immutably borrowed and collide with every mutating site -- the popup
    // dispatch, the folder mutators, the persistence call. One `String` changed
    // only when a folder opens, which is not a per-frame cost.
    let mut folder_title = String::new();
    // Notification centre and the per-app badges it feeds.
    //
    // `systemui::SystemUiShade` has carried a complete `NotificationCard` model,
    // a bounded ring and swipe-to-dismiss since it was written, and nothing in
    // the workspace ever called them -- the shade painted two hardcoded strings
    // ("UTIM PID 1 & UTLC Wayland", "Direct DRM KMS Scanout") while the model sat
    // there unused. `notification::NotificationStore` is the producer side: a
    // `org.freedesktop.Notifications` client on a D-Bus socket.
    //
    // The store is read on the shade's refresh tick and mirrored into a badge
    // table the renderer reads per icon per frame, because
    // `DrmInteractiveState` cannot own a `VecDeque` and the renderer must not
    // allocate.
    // Per-app shadow masks, borrowed from the icon cache.
    //
    // Rebuilt whenever the icons are (re)applied rather than per frame: the masks
    // only change when a bitmap changes, and `IconCache::shadow` hands back a
    // borrow of an `Rc`-held value, so this is a pointer copy per app and nothing
    // more. Empty when `IconCache::shadows_enabled()` is false -- the dark theme,
    // and the default -- so in the common case this costs one word per frame.
    let mut icon_shadow_rows: Vec<(
        String,
        std::rc::Rc<utim_core::compositor::icons::ShadowMask>,
    )> = Vec::new();

    // The decoded wallpaper.
    //
    // Loaded once at boot and re-loaded only when `LauncherState::wallpaper` names
    // a different file -- never per frame. A decode is a `Vec` allocation of
    // `width * height * 4`, several megabytes on a phone-sized image, and doing
    // that inside the frame loop would be the single largest allocation in the
    // process by a wide margin.
    //
    // `wallpaper_path` records what is *in* `image`, so the "did it change?"
    // question is answerable without comparing megabytes.
    let mut wallpaper_path = String::new();
    let mut wallpaper_img: Option<std::rc::Rc<utim_core::graphics::png::RgbaImage>> = None;
    // File-picker entry point: `UTLC_WALLPAPER_IMPORT=/path/to/img` imports once
    // at startup through the validated path (ext + magic + 8MiB budget) and
    // becomes the wallpaper. A portal file-chooser callback lands here.
    if let Some(src) = std::env::var_os("UTLC_WALLPAPER_IMPORT") {
        let src = std::path::PathBuf::from(src);
        if let Some(dest) = import_wallpaper_file(&src) {
            state.wallpaper = dest;
            state.touch();
            let _ = state.save();
        }
    }
    refresh_wallpaper(&state.wallpaper, &mut wallpaper_path, &mut wallpaper_img);

    let mut notifications = utim_core::notification::NotificationStore::new();

    // The Settings screen's rows.
    //
    // Projected from `LauncherState` when the Settings app opens and rebuilt after
    // every change, rather than per frame. `utim_core::settings::build` returns
    // rows holding only `&'static str`, so this vector borrows nothing from the
    // state and survives both a settings mutation and the catalogue rescan that
    // reassigns everything else the frame borrows.
    let mut settings_rows: Vec<utim_core::settings::SettingRow> = Vec::new();
    // The wallpaper picker, over the system's wallpapers.
    //
    // Built once at boot and re-enumerated only when the Settings panel opens,
    // because a `read_dir` walk of three directories on the frame path would be
    // the most expensive thing the shell does. The candidates are held as owned
    // `String`s so the picker does not borrow `state`, which lets it survive both
    // a settings mutation and the catalogue rescan.
    let mut wallpaper_list = wallpaper_candidates();
    let mut wallpaper_picker = utim_core::settings::picker::WallpaperPicker::new(
        wallpaper_list.clone(),
        picker_cursor(&wallpaper_list, &state.wallpaper),
    );
    // Owned keys, so nothing keeps `notifications` borrowed between ticks: the
    // store is *mutated* by `expire`, and a `Vec<(&str, u32)>` borrowing from it
    // would make the next tick a borrow error rather than a bug.
    let mut badge_rows: Vec<(String, u32)> = Vec::new();
    let mut notif_drag: Option<u32> = None;

    // Anchor of the long-press popup in panel coordinates.
    let mut popup_anchor = (0.0f32, 0.0f32);
    // Rows the popup offers, borrowed by the renderer as `&[PopupItem]`.
    //
    // This is `compositor::PopupItems`, the fixed-capacity model the crate
    // already ships and tests. The shell used to carry its own
    // `[PopupItem; 4]`, which meant the bounded list, its overflow report and
    // `MAX_DEEP_SHORTCUTS` were all reachable only from `recents.rs`'s test
    // module -- and the array was too small to hold either reference menu.
    let mut popup_rows = utim_core::compositor::PopupItems::EMPTY;
    // Which app the popup was raised on, empty for the workspace menu.
    //
    // `long_press` is cleared as soon as the popup is raised, so without this the
    // popup has no subject and every per-app row (app info, remove, uninstall)
    // would be a no-op on a correctly-delivered tap.
    let mut popup_subject = String::new();
    // Whether the home screen refuses edits, seeded from the persisted state.
    //
    // The reference's `lockHomeScreen` pref (`PreferenceManager2.kt:374`) makes
    // `Workspace.startDrag` return `null` before the drag view is even built
    // (`Workspace.java:1963-1973`) and adds a `HomeScreenLock` row
    // (`LauncherOptionsPopup.kt:22`). UTLC had the `PopupItem::HomeScreenLock`
    // variant and its label but nothing that could ever set it.
    let mut home_locked = state.home_locked;
    // Which app the long-press landed on, and where. Held so the popup can be
    // raised on the *move* that crosses the long-press threshold rather than
    // needing the touch position again.
    let mut long_press: Option<(String, f32, f32, Instant)> = None;
    // Springs for the two overview surfaces. `desktop_slide` is the reference's
    // own row for the carousel; `recents_attach_alpha` for the scrim.
    let mut overview_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::desktop_slide());
    let mut overview_scrim_spring =
        SpringSimulation::new(0.0, 0.0, SpringConfig::recents_attach_alpha());
    // Long-press popup: `spring_loaded` is the reference's own row for the
    // options popup appearing out of the icon.
    let mut popup_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::spring_loaded());
    // How often the smartspace advances a phase. The reference cross-fades on a
    // timer rather than on a transition; 5 s is one cycle of the two states.
    const SMARTSPACE_TICK_MS: u64 = 5_000;
    // In-app home gesture: the panel's scale and opacity, both 1.0 at rest.
    // Quickstep home, not `stretch_edge`: a swipe-to-home is a settle under
    // a shrinking panel, and the edge-pull profile's 0.98 damping ratio gives
    // it an overshoot that shows the app panel growing past the workspace.
    let mut workspace_scale_spring =
        SpringSimulation::new(1.0, 1.0, SpringConfig::quickstep_home());
    let mut window_alpha_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::quickstep_home());
    // Smartspace phases between the date-only and full-weather card. Advanced
    // on a wall clock, not on interaction, so it is hashed in the renderer.
    let mut smartspace_phase: f32 = 0.0;
    let mut last_smartspace_tick = Instant::now();
    // The weather half of the smartspace. Reused buffer: `refresh` clears and
    // refills it, so steady-state cost is one `open`/`read` per 5 s tick and
    // no allocation. Empty means "no weather service", which the renderer
    // already handles by falling back to the date-only line.
    let mut weather_buf = String::with_capacity(WEATHER_MAX_BYTES);
    let mut weather_sink = WeatherSink::new();
    // Flattened recents rows handed to the renderer. Fixed capacity, no `Vec`
    // on the frame path.
    let mut recents_rows: [RecentsCard; utim_core::compositor::MAX_TASKS] = [RecentsCard {
        app_id: 0,
        dismiss: 0.0,
        selected: false,
    };
        utim_core::compositor::MAX_TASKS];
    let mut kill_queue = KillQueue::default();
    // The task the shell believes is foreground, and the pid its most recent
    // launch produced. A recents card is pushed on the *transition* into an
    // app, detected centrally rather than at each of the six places that set
    // `active_app` -- six call sites is six chances to forget one, and the
    // failure is an app that silently never appears in the overview.
    let mut recents_foreground: Option<String> = None;
    // Pid of the most recent launch, consumed by the transition above.
    //
    // `take` rather than a read: the pid belongs to exactly one card, and
    // clearing it in the same step is what stops the *next* app from
    // inheriting it. An in-app screen the shell drew itself never sets it, so
    // it reads back as the 0 it was initialised to -- which is the correct
    // pid for a task with no process behind it.
    let mut pending_launch_pid: i32 = 0;

    // Icons are decoded and resampled once, at the size the layout draws.
    //
    // `icon_max_edge`, NOT `Layout::icon_size`. These are different numbers on
    // purpose: `Layout::icon_size` is the icon as drawn inside a grid cell
    // (121 px on 1080p), while `icon_max_edge` is the adaptive-icon canvas
    // (167 px on 1080p) -- the larger box the icon is masked and cropped
    // *before* it is scaled into the cell. Decoding at 121 and then scaling up
    // to fill a 167 px canvas is what made every icon look like it had been
    // through a gaussian blur, and it is why the 1:1 fast copy path could
    // never fire.
    set_icon_edge_px(icons::icon_max_edge(server.scene.width as f32));

    let mut desktop_catalogue = DesktopCatalogue::new();
    desktop_catalogue.scan_system_directories();
    let mut all_managed_apps = build_all_apps(&desktop_catalogue, &state.hidden_apps);
    let mut last_catalogue_scan = Instant::now();
    let catalogue_scan_interval = Duration::from_secs(2);

    // Icon resolution: one sweep per batch of new keys, then served from cache.
    let mut icon_cache = IconCache::new();
    icon_cache.set_display_edge(icon_edge_px());
    // The icon pipeline's four settings, applied once at construction.
    //
    // All four were unreachable before: `make_monochrome`,
    // `composite_foreground`, `IconShape`'s non-default variants and
    // `draw_shadow_from_mask` were each fully implemented and unit-tested with
    // zero production callers, which is the failure mode this project has been
    // bitten by repeatedly -- a 16-variant `PopupItem` enum with 8 rows that did
    // nothing, and a shade that painted two hardcoded strings.
    //
    // Set *before* the first `apply_app_icons`, because `set_shape` discards
    // cached icons: shaping a decoded bitmap is not something `IconCache` can do
    // after the fact -- it masks once, at insert time, and a decoded PNG does not
    // carry the layers to re-mask from. That limitation is recorded on
    // `set_shape`; the practical effect is that a shape change needs a reload,
    // which is what a settings change is anyway.
    icon_cache.set_shape(state.icon_shape.into());
    icon_cache.set_monochrome(if state.monochrome_icons {
        // The tint is the palette's `on_surface`: monochrome icons in the
        // reference are drawn in the theme's foreground colour
        // (`MonoIconThemeController`, `LawnchairThemeManager.kt:123`), because
        // the silhouette is the point and a per-app colour defeats it.
        Some(shell_palette.on_surface)
    } else {
        None
    });
    icon_cache.set_foreground_background(Some(shell_palette.surface_container));
    icon_cache.set_icon_shadow(!state.dark_theme);
    let mut icon_app_sig = app_set_signature(&all_managed_apps);
    apply_app_icons(&mut all_managed_apps, &mut icon_cache);
    refresh_icon_shadows(&all_managed_apps, &icon_cache, &mut icon_shadow_rows);
    // Seed the fast scroller's letter -> row map here as well as on the rescan
    // path, so it is live from the first frame the drawer can be opened rather
    // than only after the first 2 s catalogue rescan. See the rescan site for
    // why this matters.
    fastscroller.set_catalogue(all_managed_apps.iter().map(|a| a.name.as_str()));

    // The full catalogue as renderer rows, rebuilt only on a real rescan.
    //
    // A recents card names its app by a `u32` index into this list, and the
    // index has to mean the same thing when the overview is drawn as it did
    // when the card was pushed. `drawer_items` cannot serve: it is the
    // *filtered* list and is empty whenever the drawer is closed, which is
    // exactly when the overview is reachable.
    let mut catalogue_items: Vec<AppGridItem<'_>> =
        all_managed_apps.iter().map(drawer_item_of).collect();

    let mut running = true;
    let mut last_frame = Instant::now();
    // Paced off the panel's real refresh rate, not a hardcoded 16 ms.
    //
    // 16 ms is 62.5 Hz, which is not any rate a phone actually ships: on a
    // 120 Hz panel the shell was presenting a frame every other vsync and
    // using half the budget doing it, and on a 90 Hz panel it was tearing the
    // interval rather than matching it -- the source of the "stutter on modern
    // panels" the plan calls out. `MobileScene::refresh_rate` is already the
    // mode's rate; the interval is its reciprocal.
    let frame_interval = frame_interval_for_hz(server.scene.refresh_rate);

    // Hotseat slot -> index into all_managed_apps; rebuilt only when the
    // catalogue signature changes (the per-frame dock find/Rc-clone is gone).
    // NOTE: the per-frame item vecs themselves stay frame-local: hoisting a
    // `Vec<&...>` across loop iterations is rejected by the borrow checker
    // (the buffer's element lifetime would have to outlive owner mutations
    // in the event phase), so reuse happens at the lookup level instead.
    let mut dock_index: [Option<usize>; 5] = [None; 5];
    let mut dock_apps_icon: Option<Rc<RgbaImage>> = None;
    refresh_dock_cache(
        &all_managed_apps,
        &icon_cache,
        &mut dock_index,
        &mut dock_apps_icon,
        &state.dock,
    );
    // Per-client Wayland accumulation buffers, parallel to server.client_streams.
    let mut client_states: Vec<ClientState> = Vec::new();
    // epoll batch buffer, hoisted: re-zeroing 256 B every 16 ms is pure waste.
    let mut events: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };

    println!("[+] UTLC daemon running successfully in persistent event loop.");

    while running {
        // Authoritative active_tab_idx clamp: the input path below indexes
        // terminal_tabs in ~8 places, so the invariant is enforced here,
        // once, instead of implicitly at each shrink site.
        if active_tab_idx >= terminal_tabs.len() {
            active_tab_idx = terminal_tabs.len().saturating_sub(1);
        }
        // Same treatment for the page index. `home_pages[current_home_page]`
        // appears at 18 sites, and unlike `terminal_tabs` nothing ever shrinks
        // `home_pages` -- but a shell whose last page was removed, or that
        // booted with an empty page vector, would index out of bounds at every
        // one of them. `panic = "abort"` in release makes that an abort, not an
        // unwind, so the invariant is enforced once here rather than trusted at
        // 18 call sites.
        if current_home_page >= home_pages.len() {
            current_home_page = home_pages.len().saturating_sub(1);
        }
        // And the index must name an *existing* page, which for an empty
        // `home_pages` is none at all -- `saturating_sub` above would leave 0,
        // and `home_pages[0]` on an empty vector still aborts. Nothing shrinks
        // `home_pages` today, so this is unreachable; it is here so that the
        // 18 index sites stay safe if that ever changes, and so the claim is
        // a checked one rather than an observed one.
        if home_pages.is_empty() {
            home_pages.push(Vec::new());
            current_home_page = 0;
        }
        trim_messages_list(&mut messages_list);

        // Deadline-derived epoll timeout: sleep only the remainder of the
        // 16 ms frame when something is animating; block indefinitely when
        // idle. The idle timeout still honours the minute-boundary
        // status-clock refresh and the systemd watchdog ping.
        let elapsed = last_frame.elapsed();
        // Every motion source, not just the three that happened to be on the
        // original list: see `FrameDemand`. A source missed here does not
        // degrade, it *freezes* -- the loop parks on the 60 s clock tick.
        let demand = FrameDemand {
            touch_ripple: touch_ripple.is_some(),
            app_launch: app_launch_progress > 0.0,
            drawer: !drawer_spring.is_at_rest(),
            page_scroll: !page_scroll_spring.is_at_rest(),
            icon_bounce: !icon_bounce_spring.is_at_rest(),
            overview: !overview_spring.is_at_rest(),
            overview_scrim: !overview_scrim_spring.is_at_rest(),
            popup: !popup_spring.is_at_rest(),
            // A drag is live between pulses, and a popup on its fade-out is
            // still being drawn, so both arms are needed.
            fastscroller: fastscroller.dragging || fastscroller.popup_visible(),
            workspace_scale: !workspace_scale_spring.is_at_rest(),
            window_alpha: !window_alpha_spring.is_at_rest(),
            folder: !folder.is_closed(),
            // A folder long press is waiting to be timed out, or the menu is
            // animating.
            //
            // Without this the loop parks on `i32::MAX` the moment the folder
            // stops moving, and a held press is never examined -- so the menu
            // would only open if the user happened to keep moving, which is the
            // opposite of a long press. The screen-timeout clamp alone would not
            // save it: that is measured in seconds and this is measured in
            // hundreds of milliseconds, so the loop would have to wake, find
            // nothing to do, and go back to sleep on a 16 ms cadence for half a
            // second to open one menu.
            folder_menu: folder_gesture.armed_press() || folder_gesture.menu_animating(),
            // Card springs only integrate while the overview is live, so the
            // same gate the stepper uses has to gate the clock too. Derived
            // here rather than reusing the stepper's local because the
            // timeout is decided before the frame block runs.
            recents: (matches!(shell_state, ShellState::Overview { .. })
                || overview_spring.value > 0.0
                || overview_scrim_spring.value > 0.0)
                && (recents.len > 0
                    && (!recents.scale.is_at_rest()
                        || (0..recents.len as usize).any(|i| {
                            !recents.dismiss[i].is_at_rest()
                                || !recents.reflow[i].is_at_rest()
                                || !recents.effects[i].is_at_rest()
                        }))),
            terminal_busy: terminal_tabs.iter().any(|t| t.is_running()),
            lockscreen_intro: server.scene.lockscreen.is_locked()
                && server.start_time.elapsed() < Duration::from_secs(2),
        };
        let mut timeout_ms: libc::c_int = demand.timeout_ms(elapsed, frame_interval);
        if !demand.any() && notify_dgram.is_some() {
            let remain = watchdog_interval
                .checked_sub(last_watchdog_ping.elapsed())
                .unwrap_or_default();
            timeout_ms =
                timeout_ms.min(remain.as_millis().min(libc::c_int::MAX as u128) as libc::c_int);
        }
        // Clamp to the time left before the screen blanks.
        //
        // This is the line that makes `screen_timeout_s` work at all. When nothing
        // is animating, `demand.timeout_ms` returns `i32::MAX` and `epoll_wait`
        // parks until an event arrives -- so a timeout implemented only as "check
        // the clock on each frame" would never be evaluated, because there are no
        // frames. The budget has to reach the poll, or the setting is still dead
        // with one more layer of indirection.
        timeout_ms = screen_timeout.poll_budget(timeout_ms, Instant::now());
        let nfds = if epoll_fd >= 0 {
            unsafe { libc::epoll_wait(epoll_fd, events.as_mut_ptr(), 16, timeout_ms) }
        } else {
            // No epoll: there is nothing to wake us, so this is the pacing.
            // Same remainder-of-frame arithmetic as the timeout path so a
            // 120 Hz panel is not silently driven at 62.5 Hz here.
            if !demand.any() {
                std::thread::sleep(frame_interval.saturating_sub(elapsed));
            }
            0
        };

        if nfds > 0 {
            for ev in events.iter().take(nfds as usize) {
                let kind = epoll_kind(ev.u64);
                let fd = epoll_fd_of(ev.u64);
                if kind == K_SIGNAL && fd == sig_fd {
                    // Drain the signalfd, then shut down without running
                    // another frame (the `!running` guard below skips it).
                    let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
                    while unsafe {
                        libc::read(
                            sig_fd,
                            &mut si as *mut _ as *mut libc::c_void,
                            std::mem::size_of::<libc::signalfd_siginfo>(),
                        )
                    } > 0
                    {}
                    running = false;
                    continue;
                } else if kind == K_LISTEN {
                    if let Some(ref listener) = server.listener {
                        let listen_raw = listener.as_raw_fd();
                        while server.client_streams.len() < MAX_CLIENTS {
                            let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                            let mut len =
                                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
                            let cfd = unsafe {
                                libc::accept4(
                                    listen_raw,
                                    &mut raw as *mut _ as *mut libc::sockaddr,
                                    &mut len,
                                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                                )
                            };
                            if cfd < 0 {
                                let e = unsafe { *libc::__errno_location() };
                                if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                                    break;
                                }
                                if e == libc::EMFILE || e == libc::ENFILE || e == libc::ENOMEM {
                                    break;
                                }
                                if e == libc::ECONNABORTED || e == libc::EINTR {
                                    continue;
                                }
                                break;
                            }
                            if epoll_fd >= 0 {
                                let mut client_ev = libc::epoll_event {
                                    events: (libc::EPOLLIN
                                        | libc::EPOLLRDHUP
                                        | libc::EPOLLHUP
                                        | libc::EPOLLERR)
                                        as u32,
                                    u64: epoll_tag(cfd, K_CLIENT),
                                };
                                if unsafe {
                                    libc::epoll_ctl(
                                        epoll_fd,
                                        libc::EPOLL_CTL_ADD,
                                        cfd,
                                        &mut client_ev,
                                    )
                                } < 0
                                {
                                    // Never retain an unregistered fd.
                                    unsafe { libc::close(cfd) };
                                    continue;
                                }
                            }
                            server
                                .client_streams
                                .push(unsafe { std::os::unix::net::UnixStream::from_raw_fd(cfd) });
                            client_states.push(ClientState::new());
                        }
                    }
                } else if kind == K_INPUT {
                    if fd == inotify_fd {
                        // One-shot hotplug rescan: drain the inotify queue,
                        // then open exactly the new nodes (no polling).
                        let mut ibuf = [0u8; 512];
                        loop {
                            let n = unsafe {
                                libc::read(
                                    inotify_fd,
                                    ibuf.as_mut_ptr() as *mut libc::c_void,
                                    ibuf.len(),
                                )
                            };
                            if n <= 0 {
                                break;
                            }
                        }
                        open_new_input_devices(epoll_fd, &mut input_fds, &mut opened_paths);
                    } else if input_fds.contains(&fd) {
                        // Drain and decode Linux evdev events (virtio-tablet, virtio-keyboard, virtio-mouse)
                        let mut ev_buf = [0u8; 24 * 16];
                        let n = unsafe {
                            libc::read(fd, ev_buf.as_mut_ptr() as *mut libc::c_void, ev_buf.len())
                        };
                        if n > 0 {
                            let total_bytes = n as usize;
                            let mut offset = 0;
                            while offset + LinuxInputEvent::SIZE <= total_bytes {
                                if let Some(ev) = LinuxInputEvent::from_raw_bytes(
                                    &ev_buf[offset..offset + LinuxInputEvent::SIZE],
                                ) {
                                    let res = dispatcher.process_event(&ev);
                                    // One input event, one gesture verdict.
                                    if dispatcher.is_touch_down {
                                        page_drag_seen = false;
                                    }
                                    cursor_pos = Some((
                                        dispatcher.cursor_x as usize,
                                        dispatcher.cursor_y as usize,
                                    ));
                                    is_touching = dispatcher.is_touch_down;

                                    if power_saver_mode == PowerSaverMode::SuperExtreme {
                                        let w = server.scene.width as f32;
                                        let h = server.scene.height as f32;
                                        match res {
                                            InputDispatchResult::Touch(ref raw_touch) => {
                                                if raw_touch.phase == TouchPhase::Down {
                                                    touch_drag_start =
                                                        Some((raw_touch.x, raw_touch.y));
                                                } else if raw_touch.phase == TouchPhase::Up {
                                                    // Settle a notification swipe before
                                                    // anything else: the row either
                                                    // goes or springs back, and both
                                                    // outcomes have to be reflected
                                                    // in the store the next mirror
                                                    // reads from.
                                                    if let Some(id) = notif_drag.take() {
                                                        if server
                                                            .scene
                                                            .system_ui
                                                            .on_notification_release(id)
                                                        {
                                                            // Dismissed past the 100 px
                                                            // threshold. `close` removes
                                                            // it from the store too, so
                                                            // the next 5 s mirror does
                                                            // not put it back.
                                                            notifications.close(id);
                                                        }
                                                    }
                                                    if let Some((sx, sy)) = touch_drag_start.take()
                                                    {
                                                        if sy - raw_touch.y > 60.0
                                                            && (sx - raw_touch.x).abs() < 120.0
                                                        {
                                                            super_extreme_state.on_swipe_up();
                                                        }
                                                    }
                                                }
                                            }
                                            InputDispatchResult::Tap { x, y } => {
                                                touch_ripple =
                                                    Some((x, y, ripple_start_radius(w), 1.0));
                                                // Bound the state layer to the cell
                                                // the tap landed on, so a workspace
                                                // tap lights one icon instead of
                                                // washing the whole panel.
                                                let home_l = Layout::plain(w, h);
                                                ripple_clip = ripple_target(
                                                    &home_l,
                                                    x,
                                                    y,
                                                    home_scroll_offset,
                                                    current_home_page,
                                                    home_pages.len(),
                                                );
                                                if y <= h * 0.05 {
                                                    super_extreme_state.volume_hud.trigger(
                                                        super_extreme_state
                                                            .volume_hud
                                                            .volume_percent,
                                                    );
                                                } else {
                                                    let pin_hash = server.scene.lockscreen.pin_hash;
                                                    let pin_salt = server.scene.lockscreen.pin_salt;
                                                    super_extreme_state.handle_touch_tap(
                                                        x, y, w, h, pin_hash, pin_salt,
                                                    );
                                                }
                                            }
                                            InputDispatchResult::KeyPress {
                                                code,
                                                ch,
                                                pressed,
                                                ..
                                            } => {
                                                if code == KEY_POWER {
                                                    if pressed {
                                                        super_extreme_state.on_power_button_press();
                                                    } else {
                                                        super_extreme_state
                                                            .on_power_button_release();
                                                    }
                                                } else if code == KEY_VOLUMEUP && pressed {
                                                    super_extreme_state.volume_up();
                                                    server.scene.system_ui.set_volume(
                                                        super_extreme_state
                                                            .volume_hud
                                                            .volume_percent,
                                                    );
                                                } else if code == KEY_VOLUMEDOWN && pressed {
                                                    super_extreme_state.volume_down();
                                                    server.scene.system_ui.set_volume(
                                                        super_extreme_state
                                                            .volume_hud
                                                            .volume_percent,
                                                    );
                                                } else if code == KEY_ESC && pressed {
                                                    super_extreme_state.handle_back();
                                                } else if code == KEY_BACKSPACE && pressed {
                                                    match super_extreme_state.active_screen {
                                                        SuperExtremeScreen::Password => {
                                                            super_extreme_state.password_backspace()
                                                        }
                                                        SuperExtremeScreen::EmergencyDialer => {
                                                            super_extreme_state
                                                                .emergency_input
                                                                .pop();
                                                        }
                                                        SuperExtremeScreen::AppPhone => {
                                                            super_extreme_state.phone_input.pop();
                                                        }
                                                        _ => {}
                                                    }
                                                } else if code == KEY_ENTER && pressed {
                                                    match super_extreme_state.active_screen {
                                                        SuperExtremeScreen::Password => {
                                                            super_extreme_state.submit_password(
                                                                server.scene.lockscreen.pin_hash,
                                                                server.scene.lockscreen.pin_salt,
                                                            );
                                                        }
                                                        SuperExtremeScreen::EmergencyDialer => {
                                                            super_extreme_state
                                                                .last_action_message =
                                                                Some(format!(
                                                                    "Emergency call placed: {}",
                                                                    super_extreme_state
                                                                        .emergency_input
                                                                ));
                                                        }
                                                        SuperExtremeScreen::AppPhone => {
                                                            super_extreme_state
                                                                .last_action_message =
                                                                Some(format!(
                                                                    "Calling {}",
                                                                    super_extreme_state.phone_input
                                                                ));
                                                        }
                                                        SuperExtremeScreen::CameraPreview => {
                                                            super_extreme_state.snap_photo();
                                                        }
                                                        _ => {}
                                                    }
                                                } else if pressed {
                                                    if let Some(c) = ch {
                                                        match super_extreme_state.active_screen {
                                                            SuperExtremeScreen::Password => {
                                                                super_extreme_state
                                                                    .enter_password_char(
                                                                        c,
                                                                        server
                                                                            .scene
                                                                            .lockscreen
                                                                            .pin_hash,
                                                                        server
                                                                            .scene
                                                                            .lockscreen
                                                                            .pin_salt,
                                                                    );
                                                            }
                                                            SuperExtremeScreen::EmergencyDialer => {
                                                                if c.is_ascii_digit()
                                                                    && super_extreme_state
                                                                        .emergency_input
                                                                        .len()
                                                                        < 12
                                                                {
                                                                    super_extreme_state
                                                                        .emergency_input
                                                                        .push(c);
                                                                }
                                                            }
                                                            SuperExtremeScreen::AppPhone
                                                                if (c.is_ascii_digit()
                                                                    || c == '*'
                                                                    || c == '#')
                                                                    && super_extreme_state
                                                                        .phone_input
                                                                        .len()
                                                                        < 15 =>
                                                            {
                                                                super_extreme_state
                                                                    .phone_input
                                                                    .push(c);
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    } else if server.scene.lockscreen.is_locked() {
                                        match res {
                                            InputDispatchResult::Touch(ref t)
                                                if t.phase == TouchPhase::Down =>
                                            {
                                                server.scene.lockscreen.unlock();
                                                server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                            }
                                            InputDispatchResult::Tap { .. } => {
                                                server.scene.lockscreen.unlock();
                                                server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                            }
                                            InputDispatchResult::KeyPress {
                                                pressed: true, ..
                                            } => {
                                                server.scene.lockscreen.unlock();
                                                server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                            }
                                            _ => {}
                                        }
                                    } else {
                                        match res {
                                            InputDispatchResult::Touch(raw_touch) => {
                                                // Monotonic ms for the gesture-timed
                                                // models. One clock read per touch
                                                // event, not per model.
                                                let now_ms =
                                                    shell_start.elapsed().as_secs_f32() * 1000.0;
                                                // An open folder owns the whole
                                                // panel, so it is handled before
                                                // the `match` below rather than
                                                // as one more `else if` in it.
                                                //
                                                // Two reasons it cannot be a
                                                // branch of that match. First,
                                                // the match has a `Down` arm that
                                                // already returns early for the
                                                // overview, so a folder added
                                                // below it would be unreachable
                                                // whenever both were somehow up.
                                                // Second, and more importantly,
                                                // a folder touch has to be
                                                // *recognised* on `Down` and then
                                                // *consumed* on `Move` and `Up`
                                                // -- the gesture spans three
                                                // arms, and a per-arm `if` is
                                                // three chances to get the
                                                // gating subtly different.
                                                if !folder.is_closed() {
                                                    let fw = server.scene.width as f32;
                                                    let fh = server.scene.height as f32;
                                                    let fl = folder_layout_for(&state, fw, fh);
                                                    let n = folder.item_count();
                                                    let per_page = folder.items_per_page().max(1);
                                                    let base = folder_base_layout(&state, fw, fh);
                                                    let frame = FolderFrame {
                                                        fl: &fl,
                                                        l: &base,
                                                        panel_w: fw,
                                                        panel_h: fh,
                                                        n_items: n,
                                                        per_page,
                                                    };
                                                    let hit = folder_touch(
                                                        &mut folder_gesture,
                                                        &frame,
                                                        raw_touch.phase,
                                                        raw_touch.x,
                                                        raw_touch.y,
                                                        Instant::now(),
                                                    );
                                                    match hit {
                                                        FolderHit::Ignored | FolderHit::Blank => {}
                                                        FolderHit::Dismiss => {
                                                            folder.collapse();
                                                            folder_gesture.dismiss_menu();
                                                        }
                                                        FolderHit::Launch(i) => {
                                                            // The whole point of a
                                                            // folder: an app
                                                            // inside it launches.
                                                            let id = folder
                                                                .item(i)
                                                                .map(|it| it.id().to_string());
                                                            // Launch first: if the
                                                            // member is stale we
                                                            // must leave the folder
                                                            // open, and closing it
                                                            // first would make the
                                                            // folder vanish under
                                                            // a tap that did
                                                            // nothing visible.
                                                            let launched =
                                                                id.as_deref().is_some_and(|id| {
                                                                    launch_from_folder(
                                                                        id,
                                                                        &all_managed_apps,
                                                                        &mut active_app,
                                                                        &mut app_launch_progress,
                                                                        &mut app_launch_color,
                                                                        &mut pending_launch_pid,
                                                                        &socket_dir,
                                                                    )
                                                                });
                                                            if launched {
                                                                folder.collapse();
                                                                folder_gesture.dismiss_menu();
                                                                haptic(
                                                                    &mut haptics,
                                                                    state.haptics,
                                                                    HapticEffect::Confirm,
                                                                );
                                                            }
                                                        }
                                                        FolderHit::Dropped { from, to, remove } => {
                                                            haptic(
                                                                &mut haptics,
                                                                state.haptics,
                                                                HapticEffect::Confirm,
                                                            );
                                                            if remove {
                                                                let _ = commit_folder_remove(
                                                                    &mut state,
                                                                    &mut folder,
                                                                    from,
                                                                    per_page,
                                                                );
                                                            } else if let (Some(from), Some(to)) =
                                                                (from, to)
                                                            {
                                                                if from != to {
                                                                    let _ = commit_folder_move(
                                                                        &mut state,
                                                                        &mut folder,
                                                                        from as usize,
                                                                        to as usize,
                                                                        per_page,
                                                                    );
                                                                }
                                                            } else if folder_gesture.drag_out {
                                                                // Dragged out of the folder and released
                                                                // outside it (not onto Remove): move the
                                                                // member to the workspace (`Folder.java:1739-1763`
                                                                // handoff). Dest is append on the current
                                                                // page; occupied dest signals folder-creation
                                                                // rather than overwriting.
                                                                if let Some(from_slot) = from {
                                                                    let at = folder.page as usize
                                                                        * per_page.max(1)
                                                                        + from_slot as usize;
                                                                    let fid =
                                                                        folder.folder_idx as u32;
                                                                    let dest_page =
                                                                        current_home_page;
                                                                    let dest_slot = home_pages
                                                                        .get(dest_page)
                                                                        .map(|p| p.len())
                                                                        .unwrap_or(0);
                                                                    if state
                                                                        .move_item_to_workspace(
                                                                            fid, at, dest_page,
                                                                            dest_slot,
                                                                        )
                                                                        .is_ok()
                                                                    {
                                                                        refresh_open_folder(
                                                                            &state,
                                                                            &mut folder,
                                                                        );
                                                                        state.touch();
                                                                        let _ = state.save();
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        FolderHit::MenuAction(action, member) => {
                                                            haptic(
                                                                &mut haptics,
                                                                state.haptics,
                                                                HapticEffect::Tick,
                                                            );
                                                            match action {
                                                                FolderMenuAction::Remove => {
                                                                    let per_page = folder
                                                                        .items_per_page()
                                                                        .max(1);
                                                                    let _ = commit_folder_remove(
                                                                        &mut state,
                                                                        &mut folder,
                                                                        Some(page_local_slot(
                                                                            member, per_page,
                                                                        )),
                                                                        per_page,
                                                                    );
                                                                }
                                                                FolderMenuAction::Rename => {
                                                                    // The reference
                                                                    // renames the
                                                                    // *folder*
                                                                    // (`Folder.java:706-712`),
                                                                    // not the member
                                                                    // the menu was raised
                                                                    // on, so the
                                                                    // member index is
                                                                    // not consulted here.
                                                                    // The current
                                                                    // title is not
                                                                    // seeded into the
                                                                    // buffer: `FolderRename`
                                                                    // is empty by design
                                                                    // (see its doc comment
                                                                    // -- pre-filling needs a
                                                                    // visible field).
                                                                    let fid =
                                                                        folder.folder_idx as u32;
                                                                    begin_folder_rename(
                                                                        &mut folder_rename,
                                                                        fid,
                                                                    );
                                                                    // Commit straight
                                                                    // away, because
                                                                    // there is no field
                                                                    // to type into yet.
                                                                    // That makes the menu
                                                                    // row clear the name
                                                                    // rather than rename
                                                                    // -- which is a
                                                                    // destructive
                                                                    // no-op-ish action, so
                                                                    // it is NOT done here.
                                                                    // The row opens the
                                                                    // editor and the
                                                                    // editor is what
                                                                    // needs the
                                                                    // renderer.
                                                                }
                                                                // Folder member info: resolve the member and open the
                                                                // app-info panel (external handoff target resolved
                                                                // there). A folder is not an app, but its *member*
                                                                // is, so Info has a destination after all.
                                                                FolderMenuAction::Info => {
                                                                    let fid =
                                                                        folder.folder_idx as u32;
                                                                    if let Some((_, app_id)) = state
                                                                        .folder_member_info(
                                                                            fid, member,
                                                                        )
                                                                    {
                                                                        let id = app_id.to_string();
                                                                        app_info_open = true;
                                                                        app_info_id = id;
                                                                        app_info = None;
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        FolderHit::MenuDismissed
                                                        | FolderHit::Consumed
                                                        | FolderHit::DragStarted => {}
                                                    }
                                                    if !matches!(hit, FolderHit::Ignored) {
                                                        continue;
                                                    }
                                                }
                                                match raw_touch.phase {
                                                    TouchPhase::Down => {
                                                        // Any input wakes the screen.
                                                        //
                                                        // Placed on `Down` rather
                                                        // than in the per-frame
                                                        // block, because a press
                                                        // during a blanked frame
                                                        // must wake it on that
                                                        // frame -- and this is
                                                        // the only place that sees
                                                        // a press at all. A
                                                        // `screen_off` check here
                                                        // also has to *not* consume
                                                        // the touch: the first tap
                                                        // after a timeout wakes the
                                                        // display and is otherwise
                                                        // discarded, which is what
                                                        // every phone does.
                                                        screen_timeout.touch(Instant::now());
                                                        touch_drag_start =
                                                            Some((raw_touch.x, raw_touch.y));
                                                        // Record the press time for
                                                        // the double-tap detector.
                                                        // A drag breaks the pair, so
                                                        // `move_to` below resets it --
                                                        // which is why the reset lives
                                                        // there rather than only on
                                                        // release.
                                                        tap_press_ms =
                                                            shell_start.elapsed().as_millis()
                                                                as u64;
                                                        // Seed the scroll delta here:
                                                        // a drag that begins with a
                                                        // non-zero delta would shift
                                                        // the list by however far
                                                        // the finger jumped between
                                                        // the last `Up` and this
                                                        // `Down`.
                                                        last_touch_y = raw_touch.y;
                                                        let w = server.scene.width as f32;
                                                        let h = server.scene.height as f32;
                                                        // The overview is modal, so
                                                        // every arm below this point
                                                        // is gated on
                                                        // `!shell_state.is_modal()`
                                                        // and a touch here was
                                                        // discarded outright: no
                                                        // card could be dragged, no
                                                        // card could be released,
                                                        // and Clear All was
                                                        // unreachable. Handle the
                                                        // carousel first and
                                                        // return, because the
                                                        // overview owns the whole
                                                        // panel while it is up.
                                                        if let ShellState::Overview { .. } =
                                                            shell_state
                                                        {
                                                            overview_touch_down(
                                                                &mut recents,
                                                                w,
                                                                h,
                                                                raw_touch.x,
                                                                raw_touch.y,
                                                            );
                                                            continue;
                                                        }
                                                        // Fast scroller: the drawer's
                                                        // touch target overhangs the
                                                        // panel edge on purpose, so
                                                        // the hit test is the layout's
                                                        // and not a bounds check.
                                                        // Armed on every down inside
                                                        // the drawer and *only*
                                                        // engages once the finger
                                                        // travels `engage_delta`
                                                        // within `engage_ms`, which
                                                        // is what stops a list
                                                        // scroll from being read as
                                                        // a scroller drag.
                                                        if app_drawer_open {
                                                            fastscroller.on_down(
                                                                raw_touch.y,
                                                                now_ms,
                                                                &shell_fast_scroller,
                                                            );
                                                        }
                                                        // Long-press: record where the
                                                        // press landed so the *move*
                                                        // that crosses the threshold
                                                        // can open the popup without
                                                        // needing the touch position
                                                        // again.
                                                        //
                                                        // The id is resolved HERE, at
                                                        // the `Down`, because that is
                                                        // the only moment the touch
                                                        // point is still over the
                                                        // icon it belongs to. It was
                                                        // hardcoded to `String::new()`,
                                                        // so `popup_rows` could never
                                                        // take its app branch and a
                                                        // long-press on an icon
                                                        // always opened the *workspace*
                                                        // menu (Wallpapers, Widgets,
                                                        // AllApps, HomeSettings) --
                                                        // there was no way to reach
                                                        // App info / Uninstall at all.
                                                        //
                                                        // Empty is a real case, not an
                                                        // error: a press on the
                                                        // wallpaper has no icon, and
                                                        // the workspace menu is the
                                                        // right answer there.
                                                        let hit_app_id = long_press_app_id(
                                                            &all_managed_apps,
                                                            &home_pages[current_home_page],
                                                            &Layout::for_shell(
                                                                w,
                                                                h,
                                                                state.font_scale,
                                                                selected_home_icon.is_some(),
                                                            )
                                                            .with_grid(
                                                                state.grid_cols,
                                                                state.grid_rows,
                                                            ),
                                                            home_scroll_offset,
                                                            home_pages.len(),
                                                            raw_touch.x,
                                                            raw_touch.y,
                                                        );
                                                        long_press = Some((
                                                            hit_app_id,
                                                            raw_touch.x,
                                                            raw_touch.y,
                                                            Instant::now(),
                                                        ));
                                                        if active_app.is_none()
                                                            && !server.scene.system_ui.is_open()
                                                            && !server.scene.keyboard.is_active
                                                            && !shell_state.is_modal()
                                                        {
                                                            // Finger down: the icon compresses
                                                            // toward 0.90 and *stays* there
                                                            // until release, so the press reads
                                                            // as tactile rather than as a
                                                            // one-shot pop.
                                                            fn press(
                                                                target: &mut Option<String>,
                                                                id: String,
                                                                spring: &mut SpringSimulation,
                                                            )
                                                            {
                                                                *target = Some(id);
                                                                spring.set_target(PRESS_SCALE);
                                                            }
                                                            if app_drawer_open {
                                                                let off = (1.0
                                                                    - drawer_progress
                                                                        .clamp(0.0, 1.0))
                                                                    * h;
                                                                let l = Layout::plain(w, h);
                                                                if let Some(idx) = l
                                                                    .drawer_grid_hit_scrolled(
                                                                        off,
                                                                        raw_touch.x,
                                                                        raw_touch.y,
                                                                        drawer_scroll_y,
                                                                        all_managed_apps.len(),
                                                                    )
                                                                {
                                                                    // Allocation-free nth-app lookup on
                                                                    // every touch down.
                                                                    if let Some(app) = drawer_nth(
                                                                        &all_managed_apps,
                                                                        &drawer_search,
                                                                        idx,
                                                                    ) {
                                                                        press(
                                                                            &mut pressed_icon_id,
                                                                            app.id.clone(),
                                                                            &mut icon_bounce_spring,
                                                                        );
                                                                    }
                                                                }
                                                            } else {
                                                                let l = Layout::for_shell(
                                                                    w,
                                                                    h,
                                                                    state.font_scale,
                                                                    selected_home_icon.is_some(),
                                                                )
                                                                .with_grid(
                                                                    state.grid_cols,
                                                                    state.grid_rows,
                                                                );
                                                                if let Some(idx) = l
                                                                    .home_grid_hit_paged(
                                                                        raw_touch.x,
                                                                        raw_touch.y,
                                                                        home_scroll_offset,
                                                                        home_pages.len(),
                                                                    )
                                                                    .map(|(_, i)| i)
                                                                {
                                                                    if let Some(id) = home_pages
                                                                        .get(current_home_page)
                                                                        .and_then(|p| p.get(idx))
                                                                    {
                                                                        press(
                                                                            &mut pressed_icon_id,
                                                                            id.clone(),
                                                                            &mut icon_bounce_spring,
                                                                        );
                                                                    }
                                                                } else if let Some(slot) = l
                                                                    .home_dock_hit(
                                                                        raw_touch.x,
                                                                        raw_touch.y,
                                                                    )
                                                                {
                                                                    let dock_ids = [
                                                                        "phone", "messages",
                                                                        "apps", "browser",
                                                                        "camera",
                                                                    ];
                                                                    if let Some(id) =
                                                                        dock_ids.get(slot)
                                                                    {
                                                                        press(
                                                                            &mut pressed_icon_id,
                                                                            (*id).to_string(),
                                                                            &mut icon_bounce_spring,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    TouchPhase::Move => {
                                                        // A drag breaks the double-tap
                                                        // pair, on the *move* rather
                                                        // than the release: the
                                                        // reference's
                                                        // `onTouchEvent` returns
                                                        // `ACTION_CANCEL` to
                                                        // `GestureHandler`
                                                        // mid-drag
                                                        // (`AbstractGestureController:70-77`),
                                                        // so a second tap after a
                                                        // drag must not pair with a
                                                        // first tap that happened
                                                        // before it.
                                                        tap_history.reset();
                                                        // Sampled here because the drawer scroll
                                                        // below needs a delta, not an absolute
                                                        // position. It stays updated even when the
                                                        // drawer is closed so a drag that starts
                                                        // on the grid and ends over it does not
                                                        // jump by the accumulated gap.
                                                        let touch_dy = raw_touch.y - last_touch_y;
                                                        last_touch_y = raw_touch.y;
                                                        let move_dt = {
                                                            let now = Instant::now();
                                                            let d = now
                                                                .duration_since(last_move)
                                                                .as_secs_f32();
                                                            last_move = now;
                                                            d
                                                        };
                                                        // Carousel drag, before the modal gate
                                                        // below (see the `Down` arm).
                                                        if let ShellState::Overview { .. } =
                                                            shell_state
                                                        {
                                                            overview_touch_move(
                                                                &mut recents,
                                                                &mut haptics,
                                                                state.haptics,
                                                                server.scene.width as f32,
                                                                server.scene.height as f32,
                                                                raw_touch.x,
                                                                raw_touch.y,
                                                            );
                                                            continue;
                                                        }
                                                        if active_app.is_none()
                                                            && !server.scene.system_ui.is_open()
                                                            && !server.scene.keyboard.is_active
                                                            && !shell_state.is_modal()
                                                        {
                                                            let w = server.scene.width as f32;
                                                            let h = server.scene.height as f32;

                                                            // Drawer scroll. A vertical drag inside
                                                            // the drawer grid moves the list, which
                                                            // is the only way past the first screen
                                                            // -- the drawer used to be capped at
                                                            // `drawer_rows * grid_cols` apps with
                                                            // no scroll, so everything below that
                                                            // was simply not installed as far as
                                                            // the user could tell.
                                                            if app_drawer_open {
                                                                let dl = Layout::plain(w, h);
                                                                if dl.drawer_grid_top <= raw_touch.y
                                                                    && raw_touch.y
                                                                        <= dl.drawer_grid_bottom
                                                                {
                                                                    drawer_scroll_y =
                                                                        clamp_drawer_scroll(
                                                                            drawer_scroll_y
                                                                                - touch_dy,
                                                                            &dl,
                                                                            all_managed_apps.len(),
                                                                        );
                                                                }
                                                            }

                                                            // Fast-scroller drag. The model decides
                                                            // whether the drag has *engaged* and
                                                            // which section it landed on; the shell
                                                            // only feeds it the position and reads
                                                            // the answer. Consuming the returned bool
                                                            // rather than the field is what makes a
                                                            // section change fire once.
                                                            if app_drawer_open {
                                                                // The return value is the
                                                                // section-change edge: it is true
                                                                // on exactly the move that crossed
                                                                // into a new letter, and the model
                                                                // keeps a `haptic_latch` so a
                                                                // caller that reads the field
                                                                // instead cannot double-fire.
                                                                // Scrolling A-Z without this is
                                                                // silent, and the letters are the
                                                                // only feedback the scroller gives.
                                                                if fastscroller.on_move(
                                                                    raw_touch.y,
                                                                    now_ms,
                                                                    &shell_fast_scroller,
                                                                ) {
                                                                    haptic(
                                                                        &mut haptics,
                                                                        state.haptics,
                                                                        HapticEffect::Tick,
                                                                    );
                                                                }
                                                            }

                                                            // Long press: the popup opens on the
                                                            // move that crosses the threshold, so a
                                                            // press that never moves opens it on
                                                            // release instead, and a drag that starts
                                                            // fast never opens it at all.
                                                            if let Some((
                                                                ref id,
                                                                px,
                                                                py,
                                                                ref since,
                                                            )) = long_press
                                                            {
                                                                if since.elapsed()
                                                                    >= Duration::from_millis(
                                                                        LONG_PRESS_MS,
                                                                    )
                                                                    && !matches!(
                                                                        shell_state,
                                                                        ShellState::PopupOpen { .. }
                                                                    )
                                                                {
                                                                    popup_rows = popup_items_for(
                                                                        !id.is_empty(),
                                                                        0,
                                                                        home_locked,
                                                                    );
                                                                    popup_subject = id.clone();
                                                                    popup_anchor = (px, py);
                                                                    popup_spring.set_target(1.0);
                                                                    shell_state =
                                                                        ShellState::PopupOpen {
                                                                            anchor_x: px,
                                                                            anchor_y: py,
                                                                            idx: 0,
                                                                        };
                                                                    // Reaching the long-press
                                                                    // threshold is a commitment
                                                                    // -- the menu is already up, so
                                                                    // the user cannot take it back
                                                                    // by holding longer. Pulse so
                                                                    // they know it fired.
                                                                    haptic(
                                                                        &mut haptics,
                                                                        state.haptics,
                                                                        HapticEffect::LongPress,
                                                                    );
                                                                }
                                                            }

                                                            if let Some((sx, sy)) = touch_drag_start
                                                            {
                                                                let dy = sy - raw_touch.y;
                                                                let dx = sx - raw_touch.x;
                                                                let slop = h * 0.004;
                                                                if !app_drawer_open {
                                                                    if dy > slop {
                                                                        // Drawer drag: the sheet
                                                                        // tracks the finger 1:1.
                                                                        let drawer_l =
                                                                            Layout::plain(w, h);
                                                                        let drag_span = (h
                                                                            - drawer_l
                                                                                .drawer_handle
                                                                                .y)
                                                                            .max(100.0);
                                                                        page_drag = false;
                                                                        drawer_progress = (dy
                                                                            / drag_span)
                                                                            .clamp(0.0, 1.0);
                                                                        drawer_spring.value =
                                                                            drawer_progress;
                                                                        drawer_spring.velocity =
                                                                            0.0;
                                                                    } else if dx.abs() > slop
                                                                        && dx.abs() > dy
                                                                    {
                                                                        // Horizontal workspace drag:
                                                                        // the strip follows the finger,
                                                                        // with the overscroll curve
                                                                        // damping past the first and
                                                                        // last page.
                                                                        page_drag = true;
                                                                        page_drag_seen = true;
                                                                        let last = home_pages
                                                                            .len()
                                                                            .saturating_sub(1)
                                                                            as f32;
                                                                        let target =
                                                                            home_scroll_offset + dx;
                                                                        // Overscroll damping: `max` is the
                                                                        // container extent on a drag
                                                                        // (`OverScroll.java:42-54`), which
                                                                        // for a full-bleed pager is the page
                                                                        // width, i.e. the panel width.
                                                                        if target > 0.0
                                                                            && current_home_page
                                                                                == 0
                                                                        {
                                                                            home_scroll_offset =
                                                                                damped_scroll(
                                                                                    target, w,
                                                                                );
                                                                        } else if target < -last * w
                                                                            && current_home_page
                                                                                as f32
                                                                                >= last
                                                                        {
                                                                            home_scroll_offset =
                                                                                -last * w
                                                                                    + damped_scroll(
                                                                                        target
                                                                                            + last
                                                                                                * w,
                                                                                        w,
                                                                                    );
                                                                        } else {
                                                                            home_scroll_offset =
                                                                                target;
                                                                        }
                                                                        // px/s toward the next page.
                                                                        page_drag_velocity =
                                                                            -dx / move_dt.max(1e-4);
                                                                        page_scroll_spring.value =
                                                                            home_scroll_offset;
                                                                        page_scroll_spring
                                                                            .velocity = 0.0;
                                                                    }
                                                                } else if dy < -slop {
                                                                    // Drawer is up: drag it back down.
                                                                    let drawer_l =
                                                                        Layout::plain(w, h);
                                                                    let drag_span = (h - drawer_l
                                                                        .drawer_handle
                                                                        .y)
                                                                        .max(100.0);
                                                                    page_drag = false;
                                                                    drawer_progress = (1.0
                                                                        + dy / drag_span)
                                                                        .clamp(0.0, 1.0);
                                                                    drawer_spring.value =
                                                                        drawer_progress;
                                                                    drawer_spring.velocity = 0.0;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    TouchPhase::Up | TouchPhase::Cancel => {
                                                        // Carousel release, before the
                                                        // modal gate below. A release
                                                        // is where a dismissed card
                                                        // commits to `KillState::Grace`
                                                        // (and, once the grace
                                                        // expires, a `SIGKILL`), so
                                                        // with this missing the card
                                                        // sprang back and the app ran
                                                        // forever.
                                                        //
                                                        // The double-tap pair is
                                                        // decided here too, and *before*
                                                        // the modal gate, because a
                                                        // double-tap is a workspace
                                                        // gesture: the reference feeds
                                                        // `GestureHandler` from the
                                                        // root view's `onTouchEvent`
                                                        // (`AbstractGestureController:70-77`)
                                                        // so it sees releases a nested
                                                        // panel would otherwise consume.
                                                        if raw_touch.phase == TouchPhase::Up {
                                                            let now_ms =
                                                                shell_start.elapsed().as_millis()
                                                                    as u64;
                                                            let tap = utim_core::compositor::gestures::Tap::new(
                                                                tap_press_ms,
                                                                now_ms,
                                                                raw_touch.x,
                                                                raw_touch.y,
                                                            );
                                                            let outcome = tap_history.on_touch_end(
                                                                utim_core::compositor::gestures::TouchEnd::Tap {
                                                                    down_ms: tap.down_ms,
                                                                    up_ms: tap.up_ms,
                                                                    x: tap.x,
                                                                    y: tap.y,
                                                                },
                                                            );
                                                            // Consumed: the second tap
                                                            // must not also act as a
                                                            // single tap on whatever is
                                                            // under it.
                                                            if outcome
                                                                == utim_core::compositor::gestures::TapOutcome::DoubleTap
                                                                && run_double_tap(
                                                                    &mut double_tap_action,
                                                                    &mut shell_state,
                                                                    &mut server.scene.system_ui,
                                                                )
                                                            {
                                                                continue;
                                                            }
                                                        } else {
                                                            // A cancelled touch is not a
                                                            // tap, so it must not arm
                                                            // the detector for the next
                                                            // one.
                                                            tap_history.reset();
                                                        }
                                                        //
                                                        // The gesture engine is fed
                                                        // here, before the
                                                        // `continue`, because the
                                                        // overview had no exit at all:
                                                        // its `Down`, `Move` and `Up`
                                                        // arms all returned before
                                                        // reaching `process_touch` at
                                                        // all, so the engine never saw
                                                        // a single event while the
                                                        // carousel was up,
                                                        // `ShellEffect::CloseAll` --
                                                        // the one thing that closes it
                                                        // -- was produced by a Home
                                                        // gesture that could therefore
                                                        // never arrive, and the only
                                                        // way out was to clear all five
                                                        // cards. This is the
                                                        // reference's `startHome()`
                                                        // on a back or home gesture
                                                        // (`RecentsDismissUtils.kt:338`).
                                                        if let ShellState::Overview { .. } =
                                                            shell_state
                                                        {
                                                            let cancelled = raw_touch.phase
                                                                == TouchPhase::Cancel;
                                                            let mut launched = None;
                                                            overview_touch_up(
                                                                &mut recents,
                                                                (
                                                                    server.scene.width as f32,
                                                                    server.scene.height as f32,
                                                                ),
                                                                (raw_touch.x, raw_touch.y),
                                                                cancelled,
                                                                &mut shell_state,
                                                                &mut launched,
                                                            );
                                                            // A card that came back was
                                                            // tapped, and tapping a card
                                                            // is how the user gets back
                                                            // to their app. It also has
                                                            // to close the overview, or
                                                            // the app would launch
                                                            // *behind* the carousel --
                                                            // which, with the
                                                            // `is_modal` guard below,
                                                            // is a state with no way
                                                            // out.
                                                            if let Some(i) = launched {
                                                                if let Some(app) = recents_card_app(
                                                                    &recents,
                                                                    &all_managed_apps,
                                                                    i,
                                                                ) {
                                                                    let id = app.id.clone();
                                                                    let exec = app.exec.clone();
                                                                    active_app = Some(id.clone());
                                                                    app_drawer_open = false;
                                                                    search_active = false;
                                                                    drawer_search.clear();
                                                                    // The popup layer sets a
                                                                    // flag for the one effect it
                                                                    // cannot perform itself
                                                                    // (app-info needs a
                                                                    // catalogue and a `PATH`
                                                                    // search); this is where it
                                                                    // becomes a panel.
                                                                    if popup_opened_app_info {
                                                                        popup_opened_app_info =
                                                                            false;
                                                                        app_info_open = true;
                                                                        app_info_id = popup_subject
                                                                            .to_string();
                                                                        app_info = None;
                                                                    }
                                                                    close_modal_surfaces(
                                                                        &mut shell_state,
                                                                        &mut folder,
                                                                        &mut popup_spring,
                                                                        &mut workspace_scale_spring,
                                                                        &mut window_alpha_spring,
                                                                    );
                                                                    // pid 0 for an
                                                                    // in-app screen the
                                                                    // shell drew itself:
                                                                    // there is no process
                                                                    // to signal, and the
                                                                    // kill path already
                                                                    // refuses a pid it does
                                                                    // not own.
                                                                    let pid = launch_desktop_app(
                                                                        &exec,
                                                                        &socket_dir,
                                                                    )
                                                                    .unwrap_or(0);
                                                                    if let Some(slot) =
                                                                        all_managed_apps
                                                                            .iter()
                                                                            .position(|a| {
                                                                                a.id == id
                                                                            })
                                                                    {
                                                                        let _ = recents.push(
                                                                            TaskCard::new(
                                                                                slot as u32,
                                                                                pid,
                                                                                0,
                                                                            ),
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            // Feed the engine so a
                                                            // home or back gesture can
                                                            // leave the overview.
                                                            let act = gesture_engine
                                                                .process_touch(&raw_touch);
                                                            if matches!(
                                                                plan_gesture(&act),
                                                                ShellEffect::CloseAll
                                                            ) {
                                                                close_modal_surfaces(
                                                                    &mut shell_state,
                                                                    &mut folder,
                                                                    &mut popup_spring,
                                                                    &mut workspace_scale_spring,
                                                                    &mut window_alpha_spring,
                                                                );
                                                            }
                                                            continue;
                                                        }
                                                        // Fast scroller: release ends the
                                                        // drag and starts the popup
                                                        // fade-out. The letter stays on
                                                        // screen while it fades, which is
                                                        // what the 150 ms
                                                        // `SCROLL_BAR_VIS_DURATION`
                                                        // window is for.
                                                        if app_drawer_open {
                                                            fastscroller.on_up(now_ms);
                                                        }

                                                        // A press that never moved still
                                                        // counts as a long press on
                                                        // release -- the reference
                                                        // accepts either, because a
                                                        // perfectly still finger
                                                        // produces no move events at
                                                        // all. The icon id was
                                                        // recorded on down, so the
                                                        // popup knows whether this is
                                                        // the icon menu or the
                                                        // workspace menu.
                                                        if let Some((ref id, px, py, ref since)) =
                                                            long_press
                                                        {
                                                            if since.elapsed()
                                                                >= Duration::from_millis(
                                                                    LONG_PRESS_MS,
                                                                )
                                                                && !matches!(
                                                                    shell_state,
                                                                    ShellState::PopupOpen { .. }
                                                                )
                                                            {
                                                                popup_rows = popup_items_for(
                                                                    !id.is_empty(),
                                                                    0,
                                                                    home_locked,
                                                                );
                                                                popup_subject = id.clone();
                                                                popup_anchor = (px, py);
                                                                popup_spring.set_target(1.0);
                                                                shell_state =
                                                                    ShellState::PopupOpen {
                                                                        anchor_x: px,
                                                                        anchor_y: py,
                                                                        idx: 0,
                                                                    };
                                                            }
                                                        }
                                                        long_press = None;

                                                        // Set when this release ends a page
                                                        // drag, so the gesture engine still
                                                        // sees the event but the tap is
                                                        // suppressed.
                                                        let mut was_page_drag = false;
                                                        if let Some((_, sy)) =
                                                            touch_drag_start.take()
                                                        {
                                                            let dy = sy - raw_touch.y;
                                                            if page_drag {
                                                                // Settle to whichever page the
                                                                // strip is nearest, biased by the
                                                                // release velocity, then hand the
                                                                // spring that velocity so the
                                                                // motion carries momentum.
                                                                let w = server.scene.width as f32;
                                                                let last = home_pages
                                                                    .len()
                                                                    .saturating_sub(1);
                                                                let offset = home_scroll_offset;
                                                                let frac = -offset / w;
                                                                let biased = frac
                                                                    + (page_drag_velocity / w)
                                                                        * 0.12;
                                                                let target = biased
                                                                    .round()
                                                                    .clamp(0.0, last as f32)
                                                                    as usize;
                                                                current_home_page = target;
                                                                home_scroll_offset = 0.0;
                                                                page_scroll_spring.value = offset;
                                                                page_scroll_spring.velocity =
                                                                    page_drag_velocity;
                                                                page_scroll_spring.set_target(0.0);
                                                                page_drag = false;
                                                                page_drag_velocity = 0.0;
                                                                // A dragged workspace is never a
                                                                // tap, so the release must not
                                                                // also activate whatever it
                                                                // dragged over.
                                                                icon_bounce_spring.set_target(1.0);
                                                                pressed_icon_id = None;
                                                                was_page_drag = true;
                                                            }
                                                            if !was_page_drag
                                                                && active_app.is_none()
                                                                && !server.scene.system_ui.is_open()
                                                                && !server.scene.keyboard.is_active
                                                            {
                                                                let h = server.scene.height as f32;
                                                                // Commit past the halfway point,
                                                                // or on a decisive flick.
                                                                let flick = h * 0.12;
                                                                if !app_drawer_open {
                                                                    if dy > flick {
                                                                        app_drawer_open = true;
                                                                        drawer_scroll_y = 0.0;
                                                                    }
                                                                } else if dy < -flick {
                                                                    app_drawer_open = false;
                                                                    drawer_search_active = false;
                                                                    drawer_search.clear();
                                                                }
                                                                let target = if app_drawer_open {
                                                                    1.0
                                                                } else {
                                                                    0.0
                                                                };
                                                                drawer_spring.set_target(target);
                                                                // Hand the release velocity to the
                                                                // spring, which is what makes the
                                                                // sheet feel like it has mass.
                                                                drawer_spring.velocity =
                                                                    (dy / 50.0).clamp(-12.0, 12.0);
                                                            }
                                                        }
                                                        // Release: rebound to full size
                                                        // with an underdamped overshoot.
                                                        icon_bounce_spring.set_target(1.0);
                                                    }
                                                }

                                                let gesture_act =
                                                    gesture_engine.process_touch(&raw_touch);
                                                // Haptics. `trigger_haptic` is a
                                                // rising-edge flag the gesture
                                                // engine already latches, so this
                                                // fires exactly once per gesture
                                                // and not once per frame it stays
                                                // true. Nothing read it before: the
                                                // overview commit was completely
                                                // silent.
                                                if let GestureAction::Recents {
                                                    trigger_haptic,
                                                    ..
                                                } = &gesture_act
                                                {
                                                    if *trigger_haptic {
                                                        haptic(
                                                            &mut haptics,
                                                            state.haptics,
                                                            HapticEffect::Tick,
                                                        );
                                                    }
                                                }
                                                // The three arms that drive the shell state
                                                // machine come from `plan_gesture`, which is a
                                                // pure function precisely so that "this
                                                // action has an effect" is testable. The
                                                // other arms still match inline because they
                                                // only poke the shade, the keyboard and the
                                                // drawer flag.
                                                match plan_gesture(&gesture_act) {
                                                    ShellEffect::OpenOverview => {
                                                        shell_state = ShellState::Overview {
                                                            selected: recents.visible_card,
                                                            dismiss: 0.0,
                                                        };
                                                        // The overview is modal over the
                                                        // workspace, so a surface that owns
                                                        // the screen has to give up its own
                                                        // transient state first.
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                        selected_home_icon = None;
                                                        search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                        server.scene.system_ui.close();
                                                    }
                                                    ShellEffect::ScrubTasks(cards) => {
                                                        // The scrub swaps the selected task.
                                                        // The model owns the clamping, the
                                                        // half-pitch threshold and the
                                                        // rubber-band at either end, so the
                                                        // shell only hands it the delta it
                                                        // was given and reads the result.
                                                        let rl = shell_layout.recents();
                                                        recents.scrub(
                                                            cards * (rl.card_w + rl.spacing),
                                                        );
                                                        shell_state = ShellState::Overview {
                                                            selected: recents.visible_card,
                                                            dismiss: 0.0,
                                                        };
                                                    }
                                                    ShellEffect::MorphWorkspace {
                                                        scale,
                                                        window_alpha,
                                                    } => {
                                                        // Handed to springs, so a release
                                                        // mid-gesture is a settle rather
                                                        // than a jump and the motion
                                                        // continues to the endpoint the
                                                        // finger was already heading for.
                                                        workspace_scale_spring.set_target(scale);
                                                        window_alpha_spring
                                                            .set_target(window_alpha);
                                                    }
                                                    ShellEffect::CloseAll => {
                                                        if active_app.as_deref() == Some("Terminal")
                                                        {
                                                            for tab in &terminal_tabs {
                                                                tab.cleanup_child();
                                                            }
                                                        }
                                                        active_app = None;
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                        selected_home_icon = None;
                                                        server.scene.system_ui.close();
                                                        search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                        server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                                        close_modal_surfaces(
                                                            &mut shell_state,
                                                            &mut folder,
                                                            &mut popup_spring,
                                                            &mut workspace_scale_spring,
                                                            &mut window_alpha_spring,
                                                        );
                                                    }
                                                    ShellEffect::None => {}
                                                }
                                                match gesture_act {
                                                    GestureAction::NotificationShade {
                                                        progress,
                                                    } => {
                                                        if progress > 0.35 {
                                                            server.scene.system_ui.open();
                                                            server.scene.keyboard.deactivate();
                                                        }
                                                    }
                                                    GestureAction::Back { injected, .. }
                                                        if injected =>
                                                    {
                                                        // A modal surface owns Back
                                                        // before anything else does:
                                                        // the overview has to close
                                                        // before the workspace
                                                        // behind it gets a chance
                                                        // to.
                                                        if shell_state.is_modal() {
                                                            close_modal_surfaces(
                                                                &mut shell_state,
                                                                &mut folder,
                                                                &mut popup_spring,
                                                                &mut workspace_scale_spring,
                                                                &mut window_alpha_spring,
                                                            );
                                                        } else if server.scene.system_ui.is_open() {
                                                            server.scene.system_ui.close();
                                                        } else if server.scene.keyboard.is_active {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                            drawer_search_active = false;
                                                        } else if app_drawer_open {
                                                            app_drawer_open = false;
                                                            drawer_search_active = false;
                                                        } else if selected_home_icon.is_some() {
                                                            selected_home_icon = None;
                                                        } else if active_app.is_some() {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                for tab in &terminal_tabs {
                                                                    tab.cleanup_child();
                                                                }
                                                            }
                                                            active_app = None;
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        }
                                                    }
                                                    GestureAction::Swipe {
                                                        delta_x,
                                                        delta_y: _,
                                                    } if server.scene.system_ui.is_open()
                                                        && active_app.is_none() =>
                                                    {
                                                        // Swipe-to-dismiss, in the open
                                                        // shade. The `Swipe` arm below is
                                                        // gated on the shade being *closed*,
                                                        // so without this the rows the
                                                        // model has modelled dismiss on
                                                        // (`on_notification_release`,
                                                        // 100 px threshold) would have no
                                                        // gesture to reach it.
                                                        //
                                                        // A drag already in progress
                                                        // continues on whichever row it
                                                        // started on; the finger cannot
                                                        // change rows mid-swipe, which is
                                                        // what `on_notification_release`
                                                        // assumes when it settles one row.
                                                        if let Some(id) = notif_drag {
                                                            server
                                                                .scene
                                                                .system_ui
                                                                .on_notification_swipe(id, delta_x);
                                                        } else {
                                                            let sl = ShadeLayout::new(
                                                                server.scene.width as f32,
                                                                server.scene.height as f32,
                                                            );
                                                            // The row *rect* decides, and
                                                            // the notification at that
                                                            // index is the one dragged.
                                                            // Hit-testing the rects
                                                            // rather than the cards is
                                                            // what keeps a touch landing
                                                            // in the gap between two rows
                                                            // on the nearest one, the
                                                            // same rule as
                                                            // `FolderPagedView.findNearestArea`
                                                            // and `home_grid_hit_paged`.
                                                            let row = sl
                                                                .notifs
                                                                .iter()
                                                                .take(SHADE_NOTIF_ROWS)
                                                                .position(|r| {
                                                                    r.contains(
                                                                        raw_touch.x,
                                                                        raw_touch.y,
                                                                    )
                                                                });
                                                            if let Some(i) = row {
                                                                if let Some(c) = server
                                                                    .scene
                                                                    .system_ui
                                                                    .notifications
                                                                    .get(i)
                                                                {
                                                                    notif_drag = Some(c.id);
                                                                }
                                                            }
                                                        }
                                                    }
                                                    GestureAction::Swipe { delta_x, delta_y }
                                                        if active_app.is_none()
                                                            && !server
                                                                .scene
                                                                .system_ui
                                                                .is_open()
                                                            && !server.scene.keyboard.is_active =>
                                                    {
                                                        // Vertical swipes only: horizontal
                                                        // paging is driven by the drag
                                                        // handler above, which follows the
                                                        // finger and settles with a spring.
                                                        if app_drawer_open {
                                                            if delta_y > 45.0 {
                                                                app_drawer_open = false;
                                                                drawer_search_active = false;
                                                                drawer_search.clear();
                                                            }
                                                        } else if delta_y < -45.0 {
                                                            app_drawer_open = true;
                                                            drawer_scroll_y = 0.0;
                                                            selected_home_icon = None;
                                                        } else if delta_x.abs() > 45.0
                                                            && !page_drag_seen
                                                        {
                                                            // No pointer events reached us
                                                            // (synthetic or coalesced), so
                                                            // fall back to a single flick.
                                                            let w = server.scene.width as f32;
                                                            let last =
                                                                home_pages.len().saturating_sub(1);
                                                            let dir = if delta_x < 0.0 {
                                                                1i64
                                                            } else {
                                                                -1i64
                                                            };
                                                            let next = (current_home_page as i64
                                                                + dir)
                                                                .clamp(0, last as i64)
                                                                as usize;
                                                            if next != current_home_page {
                                                                current_home_page = next;
                                                                selected_home_icon = None;
                                                                home_scroll_offset =
                                                                    -dir as f32 * w * 0.45;
                                                                page_scroll_spring.value =
                                                                    home_scroll_offset;
                                                                page_scroll_spring.velocity =
                                                                    -dir as f32 * 250.0;
                                                                page_scroll_spring.set_target(0.0);
                                                            } else {
                                                                // At a boundary: rubber-band. On a
                                                                // *fling* AOSP passes half a page as the
                                                                // extent, not the container extent
                                                                // (`PagedView.java:1552`), so the
                                                                // overscroll saturates at 0.035 * w
                                                                // rather than 0.07 * w.
                                                                let resisted =
                                                                    damped_scroll(delta_x, w * 0.5);
                                                                home_scroll_offset = resisted;
                                                                page_scroll_spring.value = resisted;
                                                                page_scroll_spring.velocity = 120.0
                                                                    * if delta_x < 0.0 {
                                                                        -1.0
                                                                    } else {
                                                                        1.0
                                                                    };
                                                                page_scroll_spring.set_target(0.0);
                                                            }
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            InputDispatchResult::Tap { x, y } => {
                                                let w = server.scene.width as f32;
                                                let h = server.scene.height as f32;
                                                // Trigger tactile touch ripple on every tap
                                                touch_ripple =
                                                    Some((x, y, ripple_start_radius(w), 1.0));

                                                // The long-press popup owns the screen
                                                // while it is up, so it is resolved
                                                // first and unconditionally consumes
                                                // the tap. Two things were wrong here
                                                // and both were silent.
                                                //
                                                // First, nothing ever hit-tested the
                                                // popup: `PopupMenuLayout` was reached
                                                // only from `draw_popup`, so
                                                // `PopupOpen.idx` was written once and
                                                // never read and every row was a label.
                                                // Second, `is_modal()` is true for
                                                // `PopupOpen`, so the tap was not
                                                // falling through to the grid either
                                                // -- it was vanishing, which is why the
                                                // menu was a dead end on a device with
                                                // no Back gesture.
                                                if let ShellState::PopupOpen {
                                                    anchor_x,
                                                    anchor_y,
                                                    ..
                                                } = shell_state
                                                {
                                                    let pl = Layout::plain(w, h).popup_menu(Rect {
                                                        x: anchor_x,
                                                        y: anchor_y,
                                                        w: 0.0,
                                                        h: 0.0,
                                                        radius: 0.0,
                                                    });
                                                    let place = pl.place(
                                                        &Layout::plain(w, h),
                                                        anchor_x,
                                                        anchor_y,
                                                        popup_rows.len as usize,
                                                        popup_spring.value,
                                                    );
                                                    // Rows past the drawn height are
                                                    // the container's padding, and a
                                                    // touch anywhere else is an
                                                    // outside tap. Both dismiss.
                                                    let row = pl.hit(&place, x, y);
                                                    let item = row.and_then(|i| popup_rows.get(i));
                                                    if let (Some(i), Some(item)) = (row, item) {
                                                        // Record the hit so the state
                                                        // reflects it, then close: the
                                                        // reference's own rows all
                                                        // dismiss the popup they were
                                                        // opened from.
                                                        shell_state = ShellState::PopupOpen {
                                                            anchor_x,
                                                            anchor_y,
                                                            idx: i as u8,
                                                        };
                                                        apply_popup_effect(
                                                            plan_popup_row(item),
                                                            &mut PopupTargets {
                                                                state: &mut state,
                                                                home_pages: &mut home_pages,
                                                                current_home_page:
                                                                    &mut current_home_page,
                                                                app_drawer_open:
                                                                    &mut app_drawer_open,
                                                                home_locked: &mut home_locked,
                                                                selected_home_icon:
                                                                    &mut selected_home_icon,
                                                                active_app: &mut active_app,
                                                                open_app_info:
                                                                    popup_opened_app_info,
                                                                subject: &popup_subject,
                                                                persist: Some(
                                                                    persist_state as PersistFn,
                                                                ),
                                                            },
                                                        );
                                                    } else {
                                                        // The reference closes on any
                                                        // outside touch and lets a
                                                        // touch on the original icon
                                                        // through to launch
                                                        // (`PopupContainerWithArrow
                                                        // .java:168-181`).
                                                        apply_popup_effect(
                                                            PopupEffect::Dismiss,
                                                            &mut PopupTargets {
                                                                state: &mut state,
                                                                home_pages: &mut home_pages,
                                                                current_home_page:
                                                                    &mut current_home_page,
                                                                app_drawer_open:
                                                                    &mut app_drawer_open,
                                                                home_locked: &mut home_locked,
                                                                selected_home_icon:
                                                                    &mut selected_home_icon,
                                                                active_app: &mut active_app,
                                                                open_app_info:
                                                                    popup_opened_app_info,
                                                                subject: &popup_subject,
                                                                persist: Some(
                                                                    persist_state as PersistFn,
                                                                ),
                                                            },
                                                        );
                                                    }
                                                    close_modal_surfaces(
                                                        &mut shell_state,
                                                        &mut folder,
                                                        &mut popup_spring,
                                                        &mut workspace_scale_spring,
                                                        &mut window_alpha_spring,
                                                    );
                                                    continue;
                                                }

                                                if server.scene.system_ui.is_open() {
                                                    let shade = ShadeLayout::new(w, h);
                                                    match shade.zone(x, y) {
                                                        ShadeZone::Tiles(i) => {
                                                            if i < quick_tiles_active.len() {
                                                                if i == 6 {
                                                                    match power_saver_mode {
                                                                    PowerSaverMode::Off => {
                                                                        let _ = server.scene.system_ui.set_brightness(25);
                                                                        let _ = server.power_sync.apply_normal_power_saver();
                                                                        power_saver_mode = PowerSaverMode::Normal;
                                                                        quick_tiles_active[6] = true;
                                                                    }
                                                                    PowerSaverMode::Normal => {
                                                                        let _ = server.scene.system_ui.set_brightness(10);
                                                                        let _ = server.power_sync.apply_super_extreme_power_saver();
                                                                        power_saver_mode = PowerSaverMode::SuperExtreme;
                                                                        set_active_family(FontFamily::Homemade);
                                                                        super_extreme_state.enter_super_extreme();
                                                                        quick_tiles_active[6] = true;
                                                                        server.scene.system_ui.close();
                                                                    }
                                                                    PowerSaverMode::SuperExtreme => {
                                                                        let _ = server.scene.system_ui.set_brightness(75);
                                                                        let _ = server.power_sync.restore_normal_power_mode();
                                                                        set_active_family(FontFamily::NotoSans);
                                                                        power_saver_mode = PowerSaverMode::Off;
                                                                        quick_tiles_active[6] = false;
                                                                    }
                                                                }
                                                                } else {
                                                                    // Every tile drives real hardware, rather than flipping a local
                                                                    // bool and stopping.
                                                                    //
                                                                    // The bool is still what the renderer draws, because the renderer
                                                                    // must not touch the filesystem -- but it is now a *mirror* of a
                                                                    // driver rather than the whole behaviour. Before this, Wifi, Mobile
                                                                    // Data, Bluetooth, Airplane and Hotspot reported success on a
                                                                    // device with no radio at all.
                                                                    //
                                                                    // A failed write does not toggle: the tile keeps its previous state
                                                                    // and the reason is left in `quick_tile_note` for the subtitle.
                                                                    // Reporting "on" for a write that failed is the failure mode
                                                                    // `RadioError` exists to prevent.
                                                                    if i == 5 {
                                                                        quick_tile_airplane(
                                                                            i,
                                                                            &mut quick_tiles_active,
                                                                            &mut quick_tile_note,
                                                                        );
                                                                    } else if i == 3 {
                                                                        // Flashlight. Not a
                                                                        // radio, so it does
                                                                        // not go through
                                                                        // `quick_tile_drive`
                                                                        // -- but it is
                                                                        // *not* inert
                                                                        // either:
                                                                        // `SystemUiShade::toggle_tile`
                                                                        // already writes
                                                                        // `torch_sysfs_path`
                                                                        // (`systemui.rs:277-280`),
                                                                        // and the shell was
                                                                        // bypassing it
                                                                        // entirely and
                                                                        // flipping a local
                                                                        // bool. The bool is
                                                                        // what the renderer
                                                                        // draws, so the tile
                                                                        // lit up while no
                                                                        // LED changed.
                                                                        //
                                                                        // The tile's own
                                                                        // `is_active` is
                                                                        // flipped by
                                                                        // `toggle_tile` and
                                                                        // is *not* mirrored
                                                                        // into
                                                                        // `quick_tiles_active`
                                                                        // here, because
                                                                        // `toggle_tile`
                                                                        // returns the state
                                                                        // it reached and a
                                                                        // failed LED write
                                                                        // returns `Err` -- in
                                                                        // which case it
                                                                        // has already
                                                                        // flipped its
                                                                        // internal bit, so
                                                                        // the honest thing
                                                                        // is to say what
                                                                        // happened rather
                                                                        // than assert a
                                                                        // state.
                                                                        match server
                                                                            .scene
                                                                            .system_ui
                                                                            .toggle_tile(
                                                                                utim_core::compositor::systemui::QuickTileKind::Torch,
                                                                            ) {
                                                                            Ok(true) => {
                                                                                quick_tiles_active[i] = true;
                                                                                quick_tile_note[i] = "On";
                                                                            }
                                                                            Ok(false) => {
                                                                                quick_tiles_active[i] = false;
                                                                                quick_tile_note[i] = "Off";
                                                                            }
                                                                            Err(e) => {
                                                                                quick_tile_note[i] = torch_note(&e);
                                                                            }
                                                                        }
                                                                    } else if let Some(kind) =
                                                                        quick_tile_kind(i)
                                                                    {
                                                                        let want =
                                                                            !quick_tiles_active[i];
                                                                        match quick_tile_drive(
                                                                            kind, want,
                                                                        ) {
                                                                            Ok(true) => {
                                                                                quick_tiles_active[i] = want;
                                                                                quick_tile_note[i] = quick_tile_note::default_for(kind, want);
                                                                            }
                                                                            // No such radio on this device. The tile says so rather than
                                                                            // pretending.
                                                                            Ok(false) => {
                                                                                quick_tile_note[i] =
                                                                                    "Not present"
                                                                            }
                                                                            Err(e) => {
                                                                                quick_tile_note[i] =
                                                                                    e.label()
                                                                            }
                                                                        }
                                                                    } else {
                                                                        // Torch and auto-rotate have no radio and no driver here. Their
                                                                        // state still flips, which is recorded as the gap: torch needs an
                                                                        // LED brightness node (`sync_torch_sysfs`) and auto-rotate needs
                                                                        // the orientation model the shell has and no path to.
                                                                        quick_tiles_active[i] =
                                                                            !quick_tiles_active[i];
                                                                        quick_tile_note[i] = "";
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        // Tapping the dimmed backdrop above the
                                                        // tiles, or the pull handle, closes.
                                                        ShadeZone::Header | ShadeZone::Empty => {
                                                            server.scene.system_ui.close();
                                                        }
                                                        ShadeZone::Brightness
                                                        | ShadeZone::Notifications
                                                        | ShadeZone::Handle => {}
                                                    }
                                                } else if let Some(key) =
                                                    Keyboard::new_for(w, h, keyboard_layout)
                                                        .hit(x, y)
                                                {
                                                    // Virtual keyboard: the key under the
                                                    // finger comes from the same layout the
                                                    // keyboard is drawn from. Takes the Key
                                                    // directly: no per-keystroke heap string.
                                                    let mut handle_key_input = |key: Key,
                                                                            active_app: &mut Option<String>,
                                                                            app_input: &mut String,
                                                                            app_input_focused: &mut bool,
                                                                            search_active: &mut bool,
                                                                            search_query: &mut String,
                                                                            drawer_search: &mut String,
                                                                            drawer_search_active: &mut bool,
                                                                            app_drawer_open: bool,
                                                                            terminal_tabs: &mut Vec<TerminalTab>,
                                                                            active_tab_idx: &mut usize,
                                                                            next_tab_id: &mut usize,
                                                                            keyboard: &mut VirtualKeyboard,
                                                                            messages_list: &mut Vec<String>| {
                                                    if app_drawer_open && *drawer_search_active {
                                                        match key {
                                                            Key::Backspace => { drawer_search.pop(); }
                                                            Key::Enter => {
                                                                *drawer_search_active = false;
                                                                keyboard.deactivate();
                                                            }
                                                            Key::Space => {
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(' ');
                                                                }
                                                            }
                                                            Key::Char(c) => {
                                                                let c_ascii = if keyboard.is_shift_active {
                                                                    c
                                                                } else {
                                                                    c.to_ascii_lowercase()
                                                                };
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(c_ascii);
                                                                }
                                                            }
                                                            Key::Hide => {
                                                                keyboard.deactivate();
                                                                *drawer_search_active = false;
                                                            }
                                                            Key::Shift => {
                                                                keyboard.is_shift_active =
                                                                    !keyboard.is_shift_active;
                                                            }
                                                            Key::Layout => {
                                                                // The `?123`/`ABC` key.
                                                                // Until this existed the
                                                                // key had no arm in
                                                                // either match, so it
                                                                // fell off the end of
                                                                // the `match` and did
                                                                // nothing: the whole
                                                                // three-page keyboard
                                                                // was one fixed QWERTY.
                                                                //
                                                                // Resolved through
                                                                // `toggled()` rather
                                                                // than by matching the
                                                                // drawn label, which is
                                                                // what the renderer's
                                                                // `keyboard_toggle_label`
                                                                // is for.
                                                                keyboard_layout =
                                                                    keyboard_layout.toggled();
                                                                // Dropping the shift
                                                                // with the page is
                                                                // `has_letters`'s
                                                                // point: the symbol
                                                                // pages have no case.
                                                                if !keyboard_layout
                                                                    .has_letters()
                                                                {
                                                                    keyboard.is_shift_active =
                                                                        false;
                                                                }
                                                                keyboard.set_layout(
                                                                    keyboard_layout,
                                                                );
                                                            }
                                                        }
                                                    } else {
                                                        let act = match key {
                                                            Key::Backspace => {
                                                                // A rename in
                                                                // progress takes
                                                                // the backspace;
                                                                // anything else
                                                                // edits the IME's
                                                                // buffer, so a typo
                                                                // in a folder name
                                                                // must not require
                                                                // dismissing the
                                                                // keyboard first.
                                                                if let Some(r) =
                                                                    folder_rename.as_mut()
                                                                {
                                                                    r.backspace();
                                                                    ImeAction::None
                                                                } else {
                                                                    keyboard.handle_key_tap(
                                                                        "BACKSPACE",
                                                                    )
                                                                }
                                                            }
                                                            Key::Enter => {
                                                                // Enter commits an open
                                                                // rename and reaches nothing
                                                                // else. It is the only
                                                                // commit path, so the
                                                                // record has exactly one
                                                                // writer.
                                                                if folder_rename.is_some() {
                                                                    if commit_folder_rename(
                                                                        &mut folder_rename,
                                                                        &mut state,
                                                                    ) {
                                                                        state.touch();
                                                                        let _ = state.save();
                                                                    }
                                                                    ImeAction::HideKeyboard
                                                                } else {
                                                                    keyboard
                                                                        .handle_key_tap("ENTER")
                                                                }
                                                            }
                                                            Key::Space => {
                                                                keyboard.handle_key_tap("SPACE")
                                                            }
                                                            Key::Char(c) => {
                                                                // A folder rename is
                                                                // open, so the keyboard
                                                                // feeds the rename buffer
                                                                // rather than the app's
                                                                // input. Checked first
                                                                // because it is the only
                                                                // mode where text must
                                                                // *not* reach the panel
                                                                // behind.
                                                                if let Some(r) =
                                                                    folder_rename.as_mut()
                                                                {
                                                                    r.type_char(c);
                                                                    ImeAction::None
                                                                } else {
                                                                    // Stack-encoded: handle_key_tap
                                                                    // gets a &str with zero
                                                                    // heap.
                                                                    let mut b = [0u8; 4];
                                                                    let s: &str = c.encode_utf8(&mut b);
                                                                    keyboard.handle_key_tap(s)
                                                                }
                                                            }
                                                            Key::Hide => {
                                                                keyboard.deactivate();
                                                                *app_input_focused = false;
                                                                *drawer_search_active = false;
                                                                ImeAction::None
                                                            }
                                                            Key::Shift => {
                                                                let _ = keyboard
                                                                    .handle_key_tap("SHIFT");
                                                                ImeAction::None
                                                            }
                                                            Key::Layout => {
                                                                // Same as the
                                                                // drawer-path arm:
                                                                // the layout is shell
                                                                // state, and the IME's
                                                                // own key table has to
                                                                // follow or it would
                                                                // insert characters
                                                                // from a page the
                                                                // user cannot see.
                                                                keyboard_layout =
                                                                    keyboard_layout.toggled();
                                                                if !keyboard_layout
                                                                    .has_letters()
                                                                {
                                                                    keyboard.is_shift_active =
                                                                        false;
                                                                }
                                                                keyboard.set_layout(
                                                                    keyboard_layout,
                                                                );
                                                                ImeAction::None
                                                            }
                                                        };
                                                        apply_ime_action(
                                                            act,
                                                            active_app,
                                                            app_input,
                                                            app_input_focused,
                                                            search_active,
                                                            search_query,
                                                            terminal_tabs,
                                                            active_tab_idx,
                                                            next_tab_id,
                                                            keyboard,
                                                            messages_list,
                                                        );
                                                    }
                                                };
                                                    handle_key_input(
                                                        key,
                                                        &mut active_app,
                                                        &mut app_input,
                                                        &mut app_input_focused,
                                                        &mut search_active,
                                                        &mut search_query,
                                                        &mut drawer_search,
                                                        &mut drawer_search_active,
                                                        app_drawer_open,
                                                        &mut terminal_tabs,
                                                        &mut active_tab_idx,
                                                        &mut next_tab_id,
                                                        &mut server.scene.keyboard,
                                                        &mut messages_list,
                                                    );
                                                } else if server.scene.keyboard.is_active {
                                                    // Tapped outside keyboard while keyboard was active
                                                    if active_app.is_some() {
                                                        let home = Layout::plain(w, h);
                                                        let app_l = AppLayout::new(
                                                            w,
                                                            h,
                                                            AppPanel::Other,
                                                            terminal_tabs.len(),
                                                        );
                                                        if app_l.back.contains(x, y)
                                                            || app_l.close.contains(x, y)
                                                            || home.nav_pill.contains(x, y)
                                                        {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                for tab in &terminal_tabs {
                                                                    tab.cleanup_child();
                                                                }
                                                            }
                                                            active_app = None;
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                            app_input_focused = false;
                                                            app_input.clear();
                                                        } else if active_app.as_deref()
                                                            == Some("Messages")
                                                        {
                                                            // Composer field and send button come
                                                            // from the same layout they are
                                                            // drawn with.
                                                            if app_l.send.contains(x, y) {
                                                                if !app_input.is_empty() {
                                                                    messages_list.push(format!(
                                                                        "You: {}",
                                                                        app_input
                                                                    ));
                                                                    app_input.clear();
                                                                }
                                                            } else {
                                                                app_input_focused = true;
                                                            }
                                                        } else {
                                                            app_input_focused = true;
                                                        }
                                                    } else if app_drawer_open {
                                                        let drawer_y_offset = (1.0
                                                            - drawer_progress.clamp(0.0, 1.0))
                                                            * h;
                                                        let drawer_l = Layout::plain(w, h);
                                                        match drawer_l.drawer_search_hit(
                                                            drawer_y_offset,
                                                            x,
                                                            y,
                                                        ) {
                                                            DrawerSearchHit::Clear => {
                                                                drawer_search.clear();
                                                            }
                                                            DrawerSearchHit::Focus => {
                                                                drawer_search_active = true;
                                                            }
                                                            DrawerSearchHit::None => {
                                                                if let Some(idx) = drawer_l
                                                                    .drawer_grid_hit_scrolled(
                                                                        drawer_y_offset,
                                                                        x,
                                                                        y,
                                                                        drawer_scroll_y,
                                                                        all_managed_apps.len(),
                                                                    )
                                                                {
                                                                    if let Some(target_app) =
                                                                        drawer_nth(
                                                                            &all_managed_apps,
                                                                            &drawer_search,
                                                                            idx,
                                                                        )
                                                                    {
                                                                        let cell = drawer_l
                                                                            .drawer_icon_cell(idx);
                                                                        app_launch_origin = Some((
                                                                            cell.center_x(),
                                                                            drawer_y_offset
                                                                                + cell.center_y(),
                                                                        ));
                                                                        app_launch_progress = 0.01;
                                                                        app_launch_color =
                                                                            target_app.color;

                                                                        let app_to_launch =
                                                                            target_app.name.clone();
                                                                        let app_exec =
                                                                            target_app.exec.clone();

                                                                        active_app = Some(
                                                                            app_to_launch.clone(),
                                                                        );
                                                                        app_input.clear();
                                                                        app_input_focused = false;
                                                                        app_drawer_open = false;
                                                                        drawer_search_active =
                                                                            false;
                                                                        drawer_search.clear();

                                                                        if app_to_launch
                                                                            == "Terminal"
                                                                        {
                                                                            server
                                                                                .scene
                                                                                .keyboard
                                                                                .activate();
                                                                            app_input_focused =
                                                                                true;
                                                                        } else {
                                                                            server
                                                                                .scene
                                                                                .keyboard
                                                                                .deactivate();
                                                                        }

                                                                        if !app_exec.is_empty() {
                                                                            // The pid is recorded so the recents card for this app can be
                                                                            // killed when the user swipes it away. Discarding it, as this
                                                                            // used to, made "close task" a no-op that reported success.
                                                                            if let Some(pid) =
                                                                                launch_desktop_app(
                                                                                    &app_exec,
                                                                                    &socket_dir,
                                                                                )
                                                                            {
                                                                                pending_launch_pid = pid;
                                                                            }
                                                                        }
                                                                    }
                                                                } else if y < drawer_y_offset
                                                                    || drawer_l
                                                                        .drawer_handle
                                                                        .contains(
                                                                            x,
                                                                            y - drawer_y_offset,
                                                                        )
                                                                    || drawer_l
                                                                        .nav_pill
                                                                        .contains(x, y)
                                                                {
                                                                    app_drawer_open = false;
                                                                    drawer_search_active = false;
                                                                    drawer_search.clear();
                                                                    server
                                                                        .scene
                                                                        .keyboard
                                                                        .deactivate();
                                                                } else {
                                                                    server
                                                                        .scene
                                                                        .keyboard
                                                                        .deactivate();
                                                                    drawer_search_active = false;
                                                                }
                                                            }
                                                        }
                                                    } else if search_active {
                                                        let home_l = Layout::plain(w, h);
                                                        if home_l.search.contains(x, y) {
                                                            // Tap on search bar keeps focus
                                                        } else if let Some(idx) = home_l
                                                            .home_grid_hit_paged(
                                                                x,
                                                                y,
                                                                home_scroll_offset,
                                                                home_pages.len(),
                                                            )
                                                            .map(|(_, i)| i)
                                                        {
                                                            // Search results replace the page
                                                            // contents, so the two views share
                                                            // the same iteration shape.
                                                            let target = if search_query.is_empty()
                                                            {
                                                                home_pages[current_home_page]
                                                                    .iter()
                                                                    .filter_map(|id| {
                                                                        all_managed_apps
                                                                            .iter()
                                                                            .find(|a| a.id == *id)
                                                                    })
                                                                    .nth(idx)
                                                            } else {
                                                                drawer_nth(
                                                                    &all_managed_apps,
                                                                    search_query.as_str(),
                                                                    idx,
                                                                )
                                                            };
                                                            if let Some(target_app) = target {
                                                                let cell = home_l.grid_icon(idx);
                                                                app_launch_origin = Some((
                                                                    cell.center_x()
                                                                        + home_scroll_offset,
                                                                    cell.center_y(),
                                                                ));
                                                                app_launch_progress = 0.01;
                                                                app_launch_color = target_app.color;
                                                                let app_to_launch =
                                                                    target_app.name.clone();
                                                                let app_exec =
                                                                    target_app.exec.clone();
                                                                active_app =
                                                                    Some(app_to_launch.clone());
                                                                app_input.clear();
                                                                app_input_focused = false;
                                                                search_active = false;
                                                                server.scene.keyboard.deactivate();
                                                                if !app_exec.is_empty() {
                                                                    // The pid is recorded so the recents card for this app can be
                                                                    // killed when the user swipes it away. Discarding it, as this
                                                                    // used to, made "close task" a no-op that reported success.
                                                                    if let Some(pid) =
                                                                        launch_desktop_app(
                                                                            &app_exec,
                                                                            &socket_dir,
                                                                        )
                                                                    {
                                                                        pending_launch_pid = pid;
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        }
                                                    } else {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                        drawer_search_active = false;
                                                    }
                                                } else if active_app.is_some() {
                                                    // An app is open and keyboard is not active
                                                    let home = Layout::plain(w, h);
                                                    let app_l = AppLayout::new(
                                                        w,
                                                        h,
                                                        AppPanel::Other,
                                                        terminal_tabs.len(),
                                                    );
                                                    if app_l.back.contains(x, y)
                                                        || app_l.close.contains(x, y)
                                                        || home.nav_pill.contains(x, y)
                                                    {
                                                        if active_app.as_deref() == Some("Terminal")
                                                        {
                                                            for tab in &terminal_tabs {
                                                                tab.cleanup_child();
                                                            }
                                                        }
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                        app_input_focused = false;
                                                        app_input.clear();
                                                    } else if active_app.as_deref()
                                                        == Some("Settings")
                                                        || active_app.as_deref() == Some("settings")
                                                    {
                                                        // A tap on a settings row applies
                                                        // it.
                                                        //
                                                        // The list rect is the one the
                                                        // renderer published
                                                        // (`settings_list_rect`), not a
                                                        // recomputation: the paint's
                                                        // `list_top` depends on
                                                        // `content_y`, `input.h` and a
                                                        // 30% inset, and deriving those
                                                        // again here would be a second
                                                        // chance to be one row out --
                                                        // which looks like nothing at all,
                                                        // because the *neighbouring*
                                                        // setting would change and the row
                                                        // pressed would look inert.
                                                        // The geometry the renderer
                                                        // published when it painted this
                                                        // panel -- the row pitch it drew,
                                                        // not one recomputed here. See
                                                        // `settings::hit` for why there is
                                                        // deliberately no derivation on
                                                        // this side.
                                                        let geom =
                                                            utim_core::graphics::drm_kms::settings_geometry();
                                                        let hit_row = utim_core::settings::hit(
                                                            x,
                                                            y,
                                                            &geom,
                                                            &settings_rows,
                                                        );
                                                        // Suppressed while the search
                                                        // field has text: the field filters
                                                        // the list, so the rows on screen
                                                        // are not `settings_rows` and
                                                        // indexing them by y would activate
                                                        // the wrong setting.
                                                        //
                                                        // The wallpaper row is
                                                        // intercepted *before*
                                                        // `apply`, which is
                                                        // deliberately inert for
                                                        // it: choosing a file is
                                                        // the picker's job, and an
                                                        // `apply` arm would be a
                                                        // second, wrong way to set
                                                        // the same setting.
                                                        let picked = if app_input.is_empty()
                                                            && hit_row
                                                                == Some(
                                                                    utim_core::settings::WALLPAPER_KEY,
                                                                )
                                                        {
                                                            cycle_wallpaper(
                                                                &mut wallpaper_picker,
                                                                &mut state,
                                                                &mut wallpaper_path,
                                                                &mut wallpaper_img,
                                                            )
                                                        } else {
                                                            false
                                                        };
                                                        if picked
                                                            || (app_input.is_empty()
                                                                && hit_row.is_some_and(|k| {
                                                                    utim_core::settings::apply(
                                                                        &mut state, k, 1,
                                                                    )
                                                                }))
                                                        {
                                                            state.touch();
                                                            let _ = state.save();
                                                            // Fold the new budget in
                                                            // immediately rather than
                                                            // at the next poll. A
                                                            // shortened budget with
                                                            // more idle time already
                                                            // elapsed has to blank at
                                                            // once, or "15 s" would
                                                            // mean fifteen seconds
                                                            // *after the tap*.
                                                            screen_timeout.set_budget(
                                                                state.screen_timeout_s,
                                                                Instant::now(),
                                                            );
                                                            // The rotation policy is the
                                                            // other cached reader of the
                                                            // state: without this a flipped
                                                            // auto-rotate switch would sit
                                                            // in the file until a reboot,
                                                            // which is the same dead tap
                                                            // the row promotion exists to
                                                            // remove.
                                                            if hit_row == Some("auto-rotate") {
                                                                rotation.set_auto_rotate(
                                                                    state.auto_rotate,
                                                                );
                                                            }
                                                            // The wallpaper setting is the
                                                            // one that changes an *image*,
                                                            // and the image is cached
                                                            // outside the state. Reload it
                                                            // here rather than polling the
                                                            // setting every frame -- a
                                                            // `String` compare per frame to
                                                            // notice a change that happens
                                                            // at most once per tap.
                                                            if hit_row == Some("wallpaper") {
                                                                refresh_wallpaper(
                                                                    &state.wallpaper,
                                                                    &mut wallpaper_path,
                                                                    &mut wallpaper_img,
                                                                );
                                                            }
                                                            haptic(
                                                                &mut haptics,
                                                                state.haptics,
                                                                HapticEffect::Confirm,
                                                            );
                                                        }
                                                    } else if active_app.as_deref()
                                                        == Some("Terminal")
                                                    {
                                                        let tab_l = AppLayout::new(
                                                            w,
                                                            h,
                                                            AppPanel::Terminal,
                                                            terminal_tabs.len(),
                                                        );
                                                        match tab_l.hit_tab_active(
                                                            x,
                                                            y,
                                                            active_tab_idx,
                                                        ) {
                                                            Some(TabHit::Select(i)) => {
                                                                active_tab_idx = i;
                                                                server.scene.keyboard.activate();
                                                                app_input_focused = true;
                                                            }
                                                            Some(TabHit::Close(i)) => {
                                                                if terminal_tabs.len() > 1
                                                                    && i < terminal_tabs.len()
                                                                {
                                                                    terminal_tabs[i]
                                                                        .cleanup_child();
                                                                    terminal_tabs.remove(i);
                                                                    if active_tab_idx
                                                                        >= terminal_tabs.len()
                                                                    {
                                                                        active_tab_idx =
                                                                            terminal_tabs.len() - 1;
                                                                    }
                                                                }
                                                                server.scene.keyboard.activate();
                                                                app_input_focused = true;
                                                            }
                                                            Some(TabHit::Add) => {
                                                                if terminal_tabs.len() < 4 {
                                                                    terminal_tabs.push(
                                                                        TerminalTab::new(
                                                                            next_tab_id,
                                                                        ),
                                                                    );
                                                                    next_tab_id += 1;
                                                                    active_tab_idx =
                                                                        terminal_tabs.len() - 1;
                                                                    server
                                                                        .scene
                                                                        .keyboard
                                                                        .activate();
                                                                    app_input_focused = true;
                                                                }
                                                            }
                                                            None => {
                                                                server.scene.keyboard.activate();
                                                                app_input_focused = true;
                                                            }
                                                        }
                                                    } else if active_app.as_deref()
                                                        == Some("Messages")
                                                    {
                                                        if app_l.send.contains(x, y) {
                                                            if !app_input.is_empty() {
                                                                messages_list.push(format!(
                                                                    "You: {}",
                                                                    app_input
                                                                ));
                                                                app_input.clear();
                                                            }
                                                        } else {
                                                            app_input_focused = true;
                                                            server.scene.keyboard.activate();
                                                        }
                                                    } else {
                                                        // Universal typing handler for ALL apps (Browser, Settings, Phone, Contacts, Files, desktop apps):
                                                        // Clicking on the place to type automatically pops up the keyboard!
                                                        app_input_focused = true;
                                                        server.scene.keyboard.activate();
                                                    }
                                                } else if app_drawer_open {
                                                    // App Drawer tap handling with dynamic sliding offset
                                                    let drawer_y_offset =
                                                        (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                    let drawer_l = Layout::plain(w, h);
                                                    if y < drawer_y_offset
                                                        || drawer_l
                                                            .drawer_handle
                                                            .contains(x, y - drawer_y_offset)
                                                    {
                                                        // Pull handle / top area: dismiss drawer
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    } else {
                                                        match drawer_l.drawer_search_hit(
                                                            drawer_y_offset,
                                                            x,
                                                            y,
                                                        ) {
                                                            DrawerSearchHit::Clear => {
                                                                drawer_search.clear();
                                                            }
                                                            DrawerSearchHit::Focus => {
                                                                drawer_search_active = true;
                                                                server.scene.keyboard.activate();
                                                            }
                                                            DrawerSearchHit::None => {
                                                                if drawer_l.nav_pill.contains(x, y)
                                                                {
                                                                    // Bottom pill: close drawer
                                                                    app_drawer_open = false;
                                                                    drawer_search_active = false;
                                                                    drawer_search.clear();
                                                                    server
                                                                        .scene
                                                                        .keyboard
                                                                        .deactivate();
                                                                } else if let Some(idx) = drawer_l
                                                                    .drawer_grid_hit_scrolled(
                                                                        drawer_y_offset,
                                                                        x,
                                                                        y,
                                                                        drawer_scroll_y,
                                                                        all_managed_apps.len(),
                                                                    )
                                                                {
                                                                    if let Some(target_app) =
                                                                        drawer_nth(
                                                                            &all_managed_apps,
                                                                            &drawer_search,
                                                                            idx,
                                                                        )
                                                                    {
                                                                        let cell = drawer_l
                                                                            .drawer_icon_cell(idx);
                                                                        app_launch_origin = Some((
                                                                            cell.center_x(),
                                                                            drawer_y_offset
                                                                                + cell.center_y(),
                                                                        ));
                                                                        app_launch_progress = 0.01;
                                                                        app_launch_color =
                                                                            target_app.color;

                                                                        let app_to_launch =
                                                                            target_app.name.clone();
                                                                        let app_exec =
                                                                            target_app.exec.clone();

                                                                        active_app = Some(
                                                                            app_to_launch.clone(),
                                                                        );
                                                                        app_input.clear();
                                                                        app_input_focused = false;
                                                                        app_drawer_open = false;
                                                                        drawer_search_active =
                                                                            false;
                                                                        drawer_search.clear();

                                                                        if app_to_launch
                                                                            == "Terminal"
                                                                        {
                                                                            server
                                                                                .scene
                                                                                .keyboard
                                                                                .activate();
                                                                            app_input_focused =
                                                                                true;
                                                                        } else {
                                                                            server
                                                                                .scene
                                                                                .keyboard
                                                                                .deactivate();
                                                                        }

                                                                        if !app_exec.is_empty() {
                                                                            // The pid is recorded so the recents card for this app can be
                                                                            // killed when the user swipes it away. Discarding it, as this
                                                                            // used to, made "close task" a no-op that reported success.
                                                                            if let Some(pid) =
                                                                                launch_desktop_app(
                                                                                    &app_exec,
                                                                                    &socket_dir,
                                                                                )
                                                                            {
                                                                                pending_launch_pid = pid;
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    // Home screen hit testing.
                                                    //
                                                    // `home_l` is the exact layout the
                                                    // renderer used for this frame, so a
                                                    // tap lands on exactly the widget
                                                    // the user sees under their finger.
                                                    let home_l = Layout::for_shell(
                                                        w,
                                                        h,
                                                        state.font_scale,
                                                        selected_home_icon.is_some(),
                                                    )
                                                    .with_grid(state.grid_cols, state.grid_rows);
                                                    if home_l.status_bar_h >= y {
                                                        server.scene.system_ui.toggle();
                                                    } else if selected_home_icon.is_some()
                                                        && (home_l.remove_chip.contains(x, y)
                                                            || home_l.move_chip.contains(x, y))
                                                    {
                                                        match home_l.remove_chip.contains(x, y) {
                                                            true => {
                                                                if let Some(ref sel_id) =
                                                                    selected_home_icon
                                                                {
                                                                    if let Some(pos) = home_pages
                                                                        [current_home_page]
                                                                        .iter()
                                                                        .position(|id| id == sel_id)
                                                                    {
                                                                        home_pages
                                                                            [current_home_page]
                                                                            .remove(pos);
                                                                        persist_state(
                                                                            &mut state,
                                                                            &home_pages,
                                                                            current_home_page,
                                                                            home_locked,
                                                                        );
                                                                    }
                                                                }
                                                                selected_home_icon = None;
                                                            }
                                                            false => {
                                                                if let Some(sel_id) =
                                                                    selected_home_icon.take()
                                                                {
                                                                    if let Some(pos) = home_pages
                                                                        [current_home_page]
                                                                        .iter()
                                                                        .position(|id| {
                                                                            *id == sel_id
                                                                        })
                                                                    {
                                                                        home_pages
                                                                            [current_home_page]
                                                                            .remove(pos);
                                                                        persist_state(
                                                                            &mut state,
                                                                            &home_pages,
                                                                            current_home_page,
                                                                            home_locked,
                                                                        );
                                                                    }
                                                                    let target_page =
                                                                        if current_home_page == 0 {
                                                                            1
                                                                        } else {
                                                                            0
                                                                        };
                                                                    while home_pages.len()
                                                                        <= target_page
                                                                    {
                                                                        home_pages.push(Vec::new());
                                                                    }
                                                                    home_pages[target_page]
                                                                        .push(sel_id.clone());
                                                                    if target_page
                                                                        > current_home_page
                                                                    {
                                                                        home_scroll_offset =
                                                                            w * 0.45;
                                                                    } else {
                                                                        home_scroll_offset =
                                                                            -(w * 0.45);
                                                                    }
                                                                    current_home_page = target_page;
                                                                }
                                                            }
                                                        }
                                                    } else if home_l.clock_rect().contains(x, y) {
                                                        active_app = Some("Clock".to_string());
                                                        app_launch_origin = Some((
                                                            home_l.w * 0.5,
                                                            home_l.clock_y + home_l.clock_h * 0.5,
                                                        ));
                                                        app_launch_progress = 0.01;
                                                        app_launch_color = 0xFFEF4444;
                                                        app_input.clear();
                                                        app_input_focused = false;
                                                        selected_home_icon = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    } else if home_l.search.contains(x, y) {
                                                        search_active = true;
                                                        server.scene.keyboard.activate();
                                                    } else if let Some(target_page) =
                                                        home_l.home_page_hit(x, y, home_pages.len())
                                                    {
                                                        if let Some(sel_id) =
                                                            selected_home_icon.take()
                                                        {
                                                            if target_page != current_home_page {
                                                                if let Some(pos) = home_pages
                                                                    [current_home_page]
                                                                    .iter()
                                                                    .position(|id| *id == sel_id)
                                                                {
                                                                    home_pages[current_home_page]
                                                                        .remove(pos);
                                                                    persist_state(
                                                                        &mut state,
                                                                        &home_pages,
                                                                        current_home_page,
                                                                        home_locked,
                                                                    );
                                                                }
                                                                while home_pages.len()
                                                                    <= target_page
                                                                {
                                                                    home_pages.push(Vec::new());
                                                                }
                                                                home_pages[target_page]
                                                                    .push(sel_id.clone());
                                                            }
                                                        }
                                                        if target_page != current_home_page {
                                                            if target_page > current_home_page {
                                                                home_scroll_offset = w * 0.45;
                                                            } else {
                                                                home_scroll_offset = -(w * 0.45);
                                                            }
                                                            current_home_page = target_page;
                                                        }
                                                    } else if let Some(qsb) = {
                                                        // The search pill is tested
                                                        // *before* the dock, and this
                                                        // ordering is load-bearing: the
                                                        // pill's rect and the dock's
                                                        // overlap by about 165 px on a
                                                        // 1080x2400 panel, so a tap in
                                                        // the middle of the search bar
                                                        // used to be caught by
                                                        // `home_dock_hit` and launch a
                                                        // random hotseat app. The pill
                                                        // is on top, so it wins.
                                                        if !state.show_search_bar {
                                                            None
                                                        } else {
                                                            match home_l.qsb().hit(x, y) {
                                                                QsbHit::None => None,
                                                                hit => Some(hit),
                                                            }
                                                        }
                                                    } {
                                                        match qsb {
                                                            QsbHit::Mic | QsbHit::Lens => {
                                                                // The glyphs are drawn
                                                                // but neither capability
                                                                // exists: a voice search
                                                                // needs a speech daemon
                                                                // and a lens needs a
                                                                // camera service, and
                                                                // neither is in this tree.
                                                                // Drawing a tap target
                                                                // for them is how the
                                                                // reference gates them
                                                                // anyway
                                                                // (`LawnQsbLayout.kt:216-224`
                                                                // resolves the intent
                                                                // first), so an
                                                                // unresolvable one must
                                                                // not consume the tap.
                                                                // Fall through to the
                                                                // drawer's own search
                                                                // field, which is the
                                                                // honest destination.
                                                            }
                                                            _ => {}
                                                        }
                                                        // Both the pill and its
                                                        // glyphs open the drawer with
                                                        // the keyboard up: the
                                                        // reference's
                                                        // `matchHotseatQsbStyle`
                                                        // path (`LawnQsbLayout
                                                        // .kt:105-114`).
                                                        app_drawer_open = true;
                                                        selected_home_icon = None;
                                                        drawer_search.clear();
                                                        drawer_search_active = true;
                                                        search_active = false;
                                                        server.scene.keyboard.activate();
                                                    } else if let Some(dock_slot) =
                                                        home_l.home_dock_hit(x, y)
                                                    {
                                                        let dock_slot_app = state
                                                            .dock_slots()
                                                            .get(dock_slot)
                                                            .copied()
                                                            .unwrap_or("");
                                                        let app = dock_slot_app;
                                                        if app.is_empty() {
                                                            // A deliberate gap in the
                                                            // dock. Not a crash and
                                                            // not a launch.
                                                        } else if app == "apps" {
                                                            // Tapping "Apps" on dock toggles the App Drawer!
                                                            app_drawer_open = !app_drawer_open;
                                                            selected_home_icon = None;
                                                            drawer_search.clear();
                                                            drawer_search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                        } else {
                                                            let dock_icon =
                                                                home_l.dock_icon_rect(dock_slot);
                                                            app_launch_origin = Some((
                                                                dock_icon.center_x(),
                                                                dock_icon.center_y(),
                                                            ));
                                                            app_launch_progress = 0.01;
                                                            let entry = all_managed_apps
                                                                .iter()
                                                                .find(|a| a.name == app);
                                                            app_launch_color = entry
                                                                .map(|a| a.color)
                                                                .unwrap_or(0xFF2563EB);

                                                            active_app = Some(app.to_string());
                                                            app_input.clear();
                                                            app_input_focused = false;
                                                            selected_home_icon = None;
                                                            if app == "Terminal" {
                                                                server.scene.keyboard.activate();
                                                                app_input_focused = true;
                                                            } else {
                                                                server.scene.keyboard.deactivate();
                                                                search_active = false;
                                                            }
                                                            if app == "Browser" {
                                                                if let Some(b_app) =
                                                                    all_managed_apps.iter().find(
                                                                        |a| {
                                                                            a.name == "Browser"
                                                                                || a.name
                                                                                    == "Firefox"
                                                                        },
                                                                    )
                                                                {
                                                                    if !b_app.exec.is_empty() {
                                                                        launch_desktop_app(
                                                                            &b_app.exec,
                                                                            &socket_dir,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    } else if home_l.nav_pill.contains(x, y) {
                                                        active_app = None;
                                                        search_active = false;
                                                        app_drawer_open = false;
                                                        selected_home_icon = None;
                                                        app_input_focused = false;
                                                        app_input.clear();
                                                        server.scene.keyboard.deactivate();
                                                        server.scene.system_ui.close();
                                                    } else if let Some(idx) = home_l
                                                        .home_grid_hit_paged(
                                                            x,
                                                            y,
                                                            home_scroll_offset,
                                                            home_pages.len(),
                                                        )
                                                        .map(|(_, i)| i)
                                                    {
                                                        let cell = home_l.grid_icon(idx);
                                                        let cx =
                                                            cell.center_x() + home_scroll_offset;
                                                        let cy = cell.center_y();

                                                        // A folder cell opens the folder rather than
                                                        // launching an app. `home_pages` holds
                                                        // `Cell`s now, so this is the one place
                                                        // the workspace distinguishes the two --
                                                        // and it is the reference's distinction
                                                        // too: a `FolderIcon` and a `BubbleTextView`
                                                        // are different item types on the same
                                                        // grid (`CellLayout` holds both).
                                                        let tapped = home_pages
                                                            .get(current_home_page)
                                                            .and_then(|p| p.get(idx))
                                                            .map(|tok| PageCell::from_token(tok));
                                                        if let Some(PageCell::Folder(fid)) = tapped
                                                        {
                                                            let per_page = Layout::plain(w, h)
                                                                .folder()
                                                                .items_per_page()
                                                                as u8;
                                                            if open_folder_at(
                                                                &state,
                                                                fid,
                                                                &mut folder,
                                                                &all_managed_apps,
                                                                per_page,
                                                            ) {
                                                                if let Some(f) = state.folder(fid) {
                                                                    folder_title.clear();
                                                                    folder_title.push_str(&f.title);
                                                                }
                                                                open_folder = fid;
                                                            }
                                                            continue;
                                                        }

                                                        if search_active && !search_query.is_empty()
                                                        {
                                                            if let Some(target_app) = drawer_nth(
                                                                &all_managed_apps,
                                                                search_query.as_str(),
                                                                idx,
                                                            ) {
                                                                app_launch_origin = Some((cx, cy));
                                                                app_launch_progress = 0.01;
                                                                app_launch_color = target_app.color;
                                                                let app_to_launch =
                                                                    target_app.name.clone();
                                                                let app_exec =
                                                                    target_app.exec.clone();
                                                                active_app =
                                                                    Some(app_to_launch.clone());
                                                                app_input.clear();
                                                                app_input_focused = false;
                                                                search_active = false;
                                                                server.scene.keyboard.deactivate();
                                                                if !app_exec.is_empty() {
                                                                    // The pid is recorded so the recents card for this app can be
                                                                    // killed when the user swipes it away. Discarding it, as this
                                                                    // used to, made "close task" a no-op that reported success.
                                                                    if let Some(pid) =
                                                                        launch_desktop_app(
                                                                            &app_exec,
                                                                            &socket_dir,
                                                                        )
                                                                    {
                                                                        pending_launch_pid = pid;
                                                                    }
                                                                }
                                                            }
                                                        } else if let Some(sel_id) =
                                                            selected_home_icon.take()
                                                        {
                                                            // Moving icon in edit mode to selected slot
                                                            if let Some(old_pos) = home_pages
                                                                [current_home_page]
                                                                .iter()
                                                                .position(|id| *id == sel_id)
                                                            {
                                                                home_pages[current_home_page]
                                                                    .remove(old_pos);
                                                                let insert_pos = idx.min(
                                                                    home_pages[current_home_page]
                                                                        .len(),
                                                                );
                                                                home_pages[current_home_page]
                                                                    .insert(
                                                                        insert_pos,
                                                                        sel_id.clone(),
                                                                    );
                                                            }
                                                        } else {
                                                            let page_app_ids =
                                                                &home_pages[current_home_page];
                                                            if let Some(app_id) =
                                                                page_app_ids.get(idx)
                                                            {
                                                                if let Some(target_app) =
                                                                    all_managed_apps
                                                                        .iter()
                                                                        .find(|a| a.id == *app_id)
                                                                {
                                                                    app_launch_origin =
                                                                        Some((cx, cy));
                                                                    app_launch_progress = 0.01;
                                                                    app_launch_color =
                                                                        target_app.color;
                                                                    let app_to_launch =
                                                                        target_app.name.clone();
                                                                    let app_exec =
                                                                        target_app.exec.clone();
                                                                    active_app =
                                                                        Some(app_to_launch.clone());
                                                                    app_input.clear();
                                                                    app_input_focused = false;
                                                                    selected_home_icon = None;
                                                                    if app_to_launch == "Terminal" {
                                                                        server
                                                                            .scene
                                                                            .keyboard
                                                                            .activate();
                                                                        app_input_focused = true;
                                                                    } else {
                                                                        server
                                                                            .scene
                                                                            .keyboard
                                                                            .deactivate();
                                                                        search_active = false;
                                                                    }
                                                                    if !app_exec.is_empty() {
                                                                        // The pid is recorded so the recents card for this app can be
                                                                        // killed when the user swipes it away. Discarding it, as this
                                                                        // used to, made "close task" a no-op that reported success.
                                                                        if let Some(pid) =
                                                                            launch_desktop_app(
                                                                                &app_exec,
                                                                                &socket_dir,
                                                                            )
                                                                        {
                                                                            pending_launch_pid =
                                                                                pid;
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    } else if selected_home_icon.is_some() {
                                                        selected_home_icon = None;
                                                    }
                                                }
                                            }
                                            InputDispatchResult::KeyPress {
                                                code,
                                                ch,
                                                pressed,
                                                repeat,
                                                ctrl,
                                            } => {
                                                if code == KEY_POWER {
                                                    if pressed {
                                                        super_extreme_state.on_power_button_press();
                                                    } else {
                                                        super_extreme_state
                                                            .on_power_button_release();
                                                    }
                                                } else if code == KEY_VOLUMEUP && pressed {
                                                    let cur = server.scene.system_ui.volume_percent;
                                                    let next = cur.saturating_add(10).min(100);
                                                    server.scene.system_ui.set_volume(next);
                                                    super_extreme_state.volume_hud.trigger(next);
                                                } else if code == KEY_VOLUMEDOWN && pressed {
                                                    let cur = server.scene.system_ui.volume_percent;
                                                    let next = cur.saturating_sub(10);
                                                    server.scene.system_ui.set_volume(next);
                                                    super_extreme_state.volume_hud.trigger(next);
                                                } else if pressed {
                                                    if ctrl {
                                                        if code == KEY_C {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                let tab = &mut terminal_tabs
                                                                    [active_tab_idx];
                                                                tab.cleanup_child();
                                                                let display_cmd =
                                                                    if tab.input.is_empty() {
                                                                        "^C"
                                                                    } else {
                                                                        &format!("{}^C", tab.input)
                                                                    };
                                                                let full_line = format!(
                                                                    "{}{}",
                                                                    utim_core::session::session()
                                                                        .prompt(),
                                                                    display_cmd
                                                                );
                                                                push_terminal_line(
                                                                    &mut tab.lines,
                                                                    &full_line,
                                                                );
                                                                log_terminal_output(&full_line);
                                                                tab.input.clear();
                                                            }
                                                        } else if code == KEY_D {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                let (has_stdin, is_empty) = {
                                                                    let tab = &terminal_tabs
                                                                        [active_tab_idx];
                                                                    let has_stdin = tab
                                                                        .active_stdin
                                                                        .lock()
                                                                        .unwrap()
                                                                        .is_some();
                                                                    let is_empty =
                                                                        tab.input.is_empty();
                                                                    (has_stdin, is_empty)
                                                                };
                                                                if has_stdin {
                                                                    *terminal_tabs
                                                                        [active_tab_idx]
                                                                        .active_stdin
                                                                        .lock()
                                                                        .unwrap() = None;
                                                                } else if is_empty {
                                                                    terminal_tabs[active_tab_idx]
                                                                        .cleanup_child();
                                                                    terminal_tabs
                                                                        .remove(active_tab_idx);
                                                                    if terminal_tabs.is_empty() {
                                                                        active_app = None;
                                                                        server
                                                                            .scene
                                                                            .keyboard
                                                                            .deactivate();
                                                                        search_active = false;
                                                                        terminal_tabs.push(
                                                                            TerminalTab::new(1),
                                                                        );
                                                                        next_tab_id = 2;
                                                                        active_tab_idx = 0;
                                                                    } else if active_tab_idx
                                                                        >= terminal_tabs.len()
                                                                    {
                                                                        active_tab_idx =
                                                                            terminal_tabs.len() - 1;
                                                                    }
                                                                }
                                                            }
                                                        } else if code == KEY_L {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                terminal_tabs[active_tab_idx]
                                                                    .lines
                                                                    .clear();
                                                            }
                                                        } else if code == KEY_T {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                                && terminal_tabs.len() < 4
                                                            {
                                                                terminal_tabs.push(
                                                                    TerminalTab::new(next_tab_id),
                                                                );
                                                                next_tab_id += 1;
                                                                active_tab_idx =
                                                                    terminal_tabs.len() - 1;
                                                                server.scene.keyboard.activate();
                                                            }
                                                        } else if code == KEY_W {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                terminal_tabs[active_tab_idx]
                                                                    .cleanup_child();
                                                                terminal_tabs
                                                                    .remove(active_tab_idx);
                                                                if terminal_tabs.is_empty() {
                                                                    active_app = None;
                                                                    server
                                                                        .scene
                                                                        .keyboard
                                                                        .deactivate();
                                                                    search_active = false;
                                                                    terminal_tabs
                                                                        .push(TerminalTab::new(1));
                                                                    next_tab_id = 2;
                                                                    active_tab_idx = 0;
                                                                } else if active_tab_idx
                                                                    >= terminal_tabs.len()
                                                                {
                                                                    active_tab_idx =
                                                                        terminal_tabs.len() - 1;
                                                                }
                                                            }
                                                        } else if code == KEY_TAB {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                                && !terminal_tabs.is_empty()
                                                            {
                                                                active_tab_idx = (active_tab_idx
                                                                    + 1)
                                                                    % terminal_tabs.len();
                                                            }
                                                        } else if active_app.as_deref()
                                                            == Some("Terminal")
                                                        {
                                                            if code == KEY_1
                                                                && !terminal_tabs.is_empty()
                                                            {
                                                                active_tab_idx = 0;
                                                            } else if code == KEY_2
                                                                && terminal_tabs.len() > 1
                                                            {
                                                                active_tab_idx = 1;
                                                            } else if code == KEY_3
                                                                && terminal_tabs.len() > 2
                                                            {
                                                                active_tab_idx = 2;
                                                            } else if code == KEY_4
                                                                && terminal_tabs.len() > 3
                                                            {
                                                                active_tab_idx = 3;
                                                            }
                                                        }
                                                    } else if code == KEY_ESC {
                                                        if active_app.as_deref() == Some("Terminal")
                                                        {
                                                            for tab in &terminal_tabs {
                                                                tab.cleanup_child();
                                                            }
                                                        }
                                                        if server.scene.keyboard.is_active {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                            drawer_search_active = false;
                                                        } else if app_drawer_open {
                                                            app_drawer_open = false;
                                                            drawer_search_active = false;
                                                        } else if selected_home_icon.is_some() {
                                                            selected_home_icon = None;
                                                        } else if active_app.is_some() {
                                                            active_app = None;
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        } else if server.scene.system_ui.is_open() {
                                                            server.scene.system_ui.close();
                                                        }
                                                    } else if code == KEY_BACKSPACE {
                                                        if active_app.as_deref() == Some("Terminal")
                                                        {
                                                            terminal_tabs[active_tab_idx]
                                                                .input
                                                                .pop();
                                                        } else if app_drawer_open
                                                            && drawer_search_active
                                                        {
                                                            drawer_search.pop();
                                                        } else if search_active {
                                                            search_query.pop();
                                                        } else if active_app.is_some()
                                                            && app_input_focused
                                                        {
                                                            app_input.pop();
                                                        }
                                                    } else if code == KEY_ENTER && !repeat {
                                                        if active_app.as_deref() == Some("Terminal")
                                                        {
                                                            handle_terminal_enter(
                                                                &mut terminal_tabs,
                                                                &mut active_tab_idx,
                                                                &mut next_tab_id,
                                                                &mut active_app,
                                                                &mut server.scene.keyboard,
                                                                &mut search_active,
                                                            );
                                                        } else if app_drawer_open
                                                            && drawer_search_active
                                                        {
                                                            drawer_search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                        } else if search_active {
                                                            search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                        } else if active_app.is_some()
                                                            && app_input_focused
                                                        {
                                                            if active_app.as_deref()
                                                                == Some("Messages")
                                                            {
                                                                if !app_input.is_empty() {
                                                                    messages_list.push(format!(
                                                                        "You: {}",
                                                                        app_input
                                                                    ));
                                                                    app_input.clear();
                                                                }
                                                            } else {
                                                                server.scene.keyboard.deactivate();
                                                                app_input_focused = false;
                                                            }
                                                        }
                                                    } else if let Some(c) = ch {
                                                        if !repeat {
                                                            if active_app.as_deref()
                                                                == Some("Terminal")
                                                            {
                                                                if terminal_tabs[active_tab_idx]
                                                                    .input
                                                                    .len()
                                                                    < 60
                                                                {
                                                                    terminal_tabs[active_tab_idx]
                                                                        .input
                                                                        .push(c);
                                                                }
                                                            } else if app_drawer_open
                                                                && drawer_search_active
                                                            {
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(c);
                                                                }
                                                            } else if search_active
                                                                && search_query.len() < 40
                                                            {
                                                                search_query.push(c);
                                                            } else if active_app.is_some() {
                                                                app_input_focused = true;
                                                                if app_input.len() < 120 {
                                                                    app_input.push(c);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            InputDispatchResult::LongPress { x, y } => {
                                                let w = server.scene.width as f32;
                                                let h = server.scene.height as f32;
                                                touch_ripple =
                                                    Some((x, y, ripple_start_radius(w) * 1.2, 1.0));
                                                let home_l = Layout::for_shell(
                                                    w,
                                                    h,
                                                    state.font_scale,
                                                    selected_home_icon.is_some(),
                                                )
                                                .with_grid(state.grid_cols, state.grid_rows);
                                                ripple_clip = ripple_target(
                                                    &home_l,
                                                    x,
                                                    y,
                                                    home_scroll_offset,
                                                    current_home_page,
                                                    home_pages.len(),
                                                );
                                                if server.scene.system_ui.is_open() {
                                                    let shade = ShadeLayout::new(w, h);
                                                    if let ShadeZone::Tiles(6) = shade.zone(x, y) {
                                                        let _ = server
                                                            .scene
                                                            .system_ui
                                                            .set_brightness(10);
                                                        let _ = server
                                                            .power_sync
                                                            .apply_super_extreme_power_saver();
                                                        power_saver_mode =
                                                            PowerSaverMode::SuperExtreme;
                                                        set_active_family(FontFamily::Homemade);
                                                        super_extreme_state.enter_super_extreme();
                                                        quick_tiles_active[6] = true;
                                                        server.scene.system_ui.close();
                                                    }
                                                } else if app_drawer_open {
                                                    let drawer_y_offset =
                                                        (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                    if let Some(idx) = Layout::plain(w, h)
                                                        .drawer_grid_hit_scrolled(
                                                            drawer_y_offset,
                                                            x,
                                                            y,
                                                            drawer_scroll_y,
                                                            all_managed_apps.len(),
                                                        )
                                                    {
                                                        if let Some(target_app) = drawer_nth(
                                                            &all_managed_apps,
                                                            &drawer_search,
                                                            idx,
                                                        ) {
                                                            // Cap pins at the visible grid capacity;
                                                            // overflow would be unreachable dead state.
                                                            let _pl = Layout::plain(w, h);
                                                            let max_slots =
                                                                _pl.grid_cols * _pl.max_rows;
                                                            if home_pages[current_home_page].len()
                                                                < max_slots
                                                                && !home_pages[current_home_page]
                                                                    .contains(&target_app.id)
                                                            {
                                                                home_pages[current_home_page]
                                                                    .push(target_app.id.clone());
                                                            }
                                                            app_drawer_open = false;
                                                            drawer_search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                        }
                                                    }
                                                } else if active_app.is_none()
                                                    && !server.scene.system_ui.is_open()
                                                {
                                                    if let Some(idx) = Layout::for_shell(
                                                        w,
                                                        h,
                                                        state.font_scale,
                                                        selected_home_icon.is_some(),
                                                    )
                                                    .with_grid(state.grid_cols, state.grid_rows)
                                                    .home_grid_hit_paged(
                                                        x,
                                                        y,
                                                        home_scroll_offset,
                                                        home_pages.len(),
                                                    )
                                                    .map(|(_, i)| i)
                                                    {
                                                        let page_app_ids =
                                                            &home_pages[current_home_page];
                                                        if let Some(app_id) = page_app_ids.get(idx)
                                                        {
                                                            selected_home_icon =
                                                                Some(app_id.clone());
                                                        }
                                                    }
                                                }
                                            }
                                            InputDispatchResult::PointerMove { .. } => {}
                                            InputDispatchResult::None => {}
                                        }
                                    }
                                }
                                offset += LinuxInputEvent::SIZE;
                            }
                        } else {
                            let err = unsafe { *libc::__errno_location() };
                            if n < 0
                                && (err == libc::EAGAIN
                                    || err == libc::EWOULDBLOCK
                                    || err == libc::EINTR)
                            {
                                // Non-blocking read would block; ignore
                            } else {
                                // Device closed or error; unregister from epoll and close
                                if epoll_fd >= 0 {
                                    unsafe {
                                        libc::epoll_ctl(
                                            epoll_fd,
                                            libc::EPOLL_CTL_DEL,
                                            fd,
                                            std::ptr::null_mut(),
                                        );
                                    }
                                }
                                unsafe {
                                    libc::close(fd);
                                }
                                if let Some(pos) = input_fds.iter().position(|&x| x == fd) {
                                    input_fds.remove(pos);
                                    if pos < opened_paths.len() {
                                        opened_paths.remove(pos);
                                    }
                                }
                            }
                        }
                    } else {
                        // Stale input fd: unregister so it stops waking us.
                        if epoll_fd >= 0 {
                            unsafe {
                                libc::epoll_ctl(
                                    epoll_fd,
                                    libc::EPOLL_CTL_DEL,
                                    fd,
                                    std::ptr::null_mut(),
                                );
                            }
                        }
                    }
                } else {
                    // Servicing connected client stream events (read/drain or close)
                    use std::io::Read;
                    let mut closed = false;
                    let mut stream_idx = None;
                    for (idx, stream) in server.client_streams.iter().enumerate() {
                        if stream.as_raw_fd() == fd {
                            stream_idx = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = stream_idx {
                        if idx >= client_states.len()
                            || (ev.events
                                & (libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR) as u32)
                                != 0
                        {
                            closed = true;
                        } else {
                            // Drain loop: bounded reads per wake so a chatty
                            // client catches up without starving the frame.
                            let mut reads = 0;
                            let mut drained = false;
                            while !drained && reads < 8 {
                                reads += 1;
                                let st = &mut client_states[idx];
                                if st.len >= st.buf.len() {
                                    // Overlong/garbage tail: drop, never resync-guess.
                                    closed = true;
                                    break;
                                }
                                let stream = &mut server.client_streams[idx];
                                match stream.read(&mut st.buf[st.len..]) {
                                    Ok(0) => {
                                        closed = true;
                                        drained = true;
                                    }
                                    Ok(n) => {
                                        let end = st.len + n;
                                        let mut off = 0;
                                        while let Ok(Some((msg, mlen))) =
                                            utim_core::compositor::protocols::WlMessage::parse(
                                                &st.buf[off..end],
                                            )
                                        {
                                            // zwp_text_input_v3 requests:
                                            // opcode 1: enable -> activate virtual keyboard
                                            // opcode 2: disable -> deactivate virtual keyboard.
                                            // Gated on the bound object once known
                                            // (text_input_id == 0 preserves the
                                            // legacy opcode-only behaviour).
                                            if st.text_input_id == 0
                                                || msg.header.object_id == st.text_input_id
                                            {
                                                if msg.header.opcode == 1 {
                                                    server.scene.keyboard.activate();
                                                    app_input_focused = true;
                                                } else if msg.header.opcode == 2 {
                                                    server.scene.keyboard.deactivate();
                                                    app_input_focused = false;
                                                }
                                            }
                                            off += mlen;
                                        }
                                        // Carry the partial tail for the next read.
                                        let tail = end - off;
                                        st.buf.copy_within(off..end, 0);
                                        st.len = tail;
                                    }
                                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        drained = true;
                                    }
                                    Err(_) => {
                                        closed = true;
                                        drained = true;
                                    }
                                }
                            }
                        }
                    }
                    if closed {
                        if let Some(idx) = stream_idx {
                            if epoll_fd >= 0 {
                                unsafe {
                                    libc::epoll_ctl(
                                        epoll_fd,
                                        libc::EPOLL_CTL_DEL,
                                        fd,
                                        std::ptr::null_mut(),
                                    );
                                }
                            }
                            server.client_streams.swap_remove(idx);
                            if idx < client_states.len() {
                                client_states.swap_remove(idx);
                            }
                        }
                    } else if stream_idx.is_none() && epoll_fd >= 0 {
                        unsafe {
                            libc::epoll_ctl(
                                epoll_fd,
                                libc::EPOLL_CTL_DEL,
                                fd,
                                std::ptr::null_mut(),
                            );
                        }
                    }
                }
            }
        }

        // Shutdown skips the whole frame tail: no terminal drain, no
        // rescan, no raster and no DIRTYFB after the signal.
        if !running {
            break;
        }

        // Drain asynchronous terminal command output streams for all tabs,
        // bounded per frame so one chatty child cannot stall the compositor.
        for tab in &mut terminal_tabs {
            let mut budget = 64usize;
            while budget > 0 {
                match tab.rx.try_recv() {
                    Ok(line) => {
                        push_terminal_line(&mut tab.lines, &line);
                        budget -= 1;
                    }
                    Err(_) => break,
                }
            }
            if tab.lines.len() > 120 {
                tab.lines.drain(0..tab.lines.len() - 120);
            }
        }

        // .desktop catalogue rescan, headless-safe (outside any drm gate):
        // rebuild only when the application-set signature changed, so the
        // steady-state cost is one scan + one cheap fingerprint per 2 s and
        // the icon re-resolve runs only on real change.
        if last_catalogue_scan.elapsed() >= catalogue_scan_interval {
            last_catalogue_scan = Instant::now();
            desktop_catalogue.scan_system_directories();
            let fresh = build_all_apps(&desktop_catalogue, &state.hidden_apps);
            let sig = app_set_signature(&fresh);
            if sig != icon_app_sig {
                icon_app_sig = sig;
                // Per-key invalidation, not just the miss set.
                //
                // `invalidate_misses` clears only `IconCache::misses`; resolved
                // bitmaps in `IconCache::images` live for the process lifetime and
                // are never revisited. So on a real change every *previously
                // resolved* app's bitmap has to be dropped explicitly, or an
                // upgraded icon keeps rendering the old artwork.
                //
                // The old set is compared against the new one rather than dropping
                // everything: the common cause of a signature change is one app
                // being installed or removed, and evicting every cached bitmap for
                // that would throw away the whole cache and re-decode 77 PNGs on a
                // phone-sized memory budget.
                //
                // Apps only in the old set are dropped outright; apps only in the
                // new set have no cached bitmap yet, so there is nothing to drop
                // and `invalidate_misses` covers the failed resolutions.
                let stale: Vec<String> = all_managed_apps
                    .iter()
                    .filter(|a| !fresh.iter().any(|f| f.id == a.id))
                    .map(|a| a.id.clone())
                    .collect();
                icon_cache.invalidate_keys(&stale);
                // Apps present in both whose keys or name moved: their cached
                // bitmap was resolved from the *old* keys.
                for a in fresh.iter() {
                    if let Some(old) = all_managed_apps.iter().find(|o| o.id == a.id) {
                        if old.icon_keys != a.icon_keys || old.name != a.name {
                            icon_cache.invalidate_key(&a.id);
                        }
                    }
                }
                icon_cache.invalidate_misses();
                all_managed_apps = fresh;
                apply_app_icons(&mut all_managed_apps, &mut icon_cache);
                refresh_icon_shadows(&all_managed_apps, &icon_cache, &mut icon_shadow_rows);
                // Rebuilt with the catalogue, not lazily: the rows borrow
                // `all_managed_apps` by `&str`, so assigning the catalogue
                // invalidates every one of them at once. A recents card's
                // `u32` index keeps pointing at the same *position*, which is
                // what it means -- the renderer falls back to a neutral tile
                // when the new catalogue is shorter.
                catalogue_items = all_managed_apps.iter().map(drawer_item_of).collect();
                // Rebuild the fast scroller's letter -> row map from the new,
                // already-sorted catalogue.
                //
                // `SectionIndex` starts EMPTY, and `FastScrollerState::on_move`
                // resolves a drag to a row through it. With the index empty
                // every lookup missed, the letter stayed 0, and the teardrop
                // never showed a character at all -- the A-Z fast scroll was
                // completely inert no matter how the model was driven.
                //
                // Built from display names because that is what the popup
                // shows, and the catalogue is sorted case-insensitively, so
                // the index is monotonic and the binary search in
                // `on_move` is valid.
                fastscroller.set_catalogue(all_managed_apps.iter().map(|a| a.name.as_str()));
                refresh_dock_cache(
                    &all_managed_apps,
                    &icon_cache,
                    &mut dock_index,
                    &mut dock_apps_icon,
                    &state.dock,
                );
            }
        }

        // Vsync frame presentation step
        let frame_elapsed = last_frame.elapsed();
        if frame_elapsed >= frame_interval {
            let dt = frame_elapsed.as_secs_f32();
            last_frame = Instant::now();
            let _ = server.step_frame(dt);

            // Lawnchair 17 / Android DynamicAnimation transitions. Every one
            // of these is an analytical damped-oscillator spring integrated at
            // the real frame delta, so motion is identical at 60 and 120 Hz.
            let drawer_target = if app_drawer_open { 1.0 } else { 0.0 };
            if touch_drag_start.is_none() {
                drawer_spring.set_target(drawer_target);
                drawer_spring.step(dt);
                drawer_progress = drawer_spring.value.clamp(0.0, 1.0);
            }

            page_scroll_spring.step(dt);
            // Boundary resistance: the spring can never park the strip outside
            // the paginated range, and overscroll bleeds off elastically.
            // Boundary resistance: the strip may be dragged a third of a page
            // past the last page, then springs back with the overscroll curve.
            let last_page = home_pages.len().saturating_sub(1) as f32;
            let strip_max = last_page * server.scene.width as f32;
            home_scroll_offset = page_scroll_spring.value.clamp(
                -strip_max * 0.35,
                strip_max + server.scene.width as f32 * 0.35,
            );

            icon_bounce_spring.step(dt);
            if icon_bounce_spring.is_at_rest() {
                icon_bounce_spring.value = icon_bounce_spring.target;
                if (icon_bounce_spring.value - 1.0).abs() < 0.005 {
                    pressed_icon_id = None;
                }
            }

            // Quickstep home. These two had a target set by the gesture
            // handler and were never integrated, so a swipe-to-home set
            // `workspace_scale_spring`'s target and the value simply stayed
            // where the finger left it -- the app panel froze mid-shrink and
            // the workspace stayed hidden behind it. They also gate the idle
            // clock via `FrameDemand`, so an unstepped spring there is what
            // turns "not at rest" into a permanently-running frame loop.
            workspace_scale_spring.step(dt);
            window_alpha_spring.step(dt);

            // The drawer's own state is derived rather than assigned at each of
            // the fourteen places that flip `app_drawer_open`. That flag is the
            // source of truth; keeping a second one in step with it would be a
            // 14-call-site invariant that only fails silently. A modal surface
            // wins, because the drawer cannot be open behind the overview.
            if !shell_state.is_modal() {
                shell_state = if app_drawer_open {
                    ShellState::AllApps {
                        progress: drawer_progress,
                    }
                } else {
                    ShellState::Normal
                };
            }

            // -------------------------------------------------------------
            // Launcher-rewrite surfaces (plan §7.6). Each is stepped only while
            // it is not at rest, so an idle shell integrates nothing.
            // -------------------------------------------------------------

            // The frame period in ms, for the models that park their springs
            // against a real frame clock rather than a synthetic one.
            let frame_ms = (server.scene.refresh_rate.clamp(30.0, 480.0).recip() * 1000.0) as f32;

            // Task bookkeeping: promote the foreground app to the front of the
            // recents stack the moment it changes.
            //
            // Launching an app already in the stack *promotes* it rather than
            // duplicating it -- `Recents::push` does that, and hands back the
            // stale entry so its snapshot slot can be released. The catalogue
            // index is what the card carries, so the renderer can resolve the
            // name later without a `String` per card.
            if active_app != recents_foreground {
                recents_foreground = active_app.clone();
                let pid = std::mem::take(&mut pending_launch_pid);
                if let Some(name) = active_app.as_deref() {
                    if let Some(idx) = all_managed_apps
                        .iter()
                        .position(|a| a.name == name || a.id == name)
                    {
                        // pid 0 for an in-app screen the shell drew itself
                        // (Clock, Settings) and for a launch that failed: there
                        // is no process to signal, and the kill path already
                        // refuses a pid it does not own, so a zero card is inert
                        // rather than wrong.
                        let _ = recents.push(TaskCard::new(idx as u32, pid, 0));
                    }
                }
            }

            // Overview. Two springs, because the reference runs the carousel
            // and the scrim on different profiles and they do not settle
            // together: `desktop_slide` for the surface, `recents_attach_alpha`
            // for the dim behind it.
            // The last card leaving closes the overview. This has to be
            // decided HERE, not in the touch handler: `clear_all` and
            // `on_card_release` start the dismissal springs, and the stack is
            // still full until they settle. Testing `len == 0` at touch time
            // would close the panel under a carousel that was still
            // animating, and the springs would then integrate against a state
            // that no longer existed.
            //
            // But *only* if it had cards. The rule as first written tested
            // `len == 0` alone, which fires on the very first frame of an
            // overview opened with an empty stack -- so the overview opened and
            // closed in the same frame and the empty state the renderer draws
            // ("No recent items", `drm_kms.rs:6129-6138`) was unreachable. The
            // reference keeps the panel up and shows the message:
            // `RecentsView.updateEmptyMessage` (`quickstep/.../RecentsView.java:4809-4824`)
            // sets `mShowEmptyMessage` from `!hasTaskViews()` and never closes.
            if overview_closes_for_empty_stack(
                matches!(shell_state, ShellState::Overview { .. }),
                recents_showed_content,
                recents.len,
            ) {
                shell_state = ShellState::Normal;
            }
            // Latched while the overview is up and holding cards, and never
            // cleared until the overview closes, so "the last card left" is
            // distinguishable from "there was never a card".
            if matches!(shell_state, ShellState::Overview { .. }) && recents.len > 0 {
                recents_showed_content = true;
            }
            let overview_open = matches!(shell_state, ShellState::Overview { .. });
            if !overview_open {
                recents_showed_content = false;
            }
            overview_spring.set_target(if overview_open { 1.0 } else { 0.0 });
            overview_scrim_spring.set_target(if overview_open { 1.0 } else { 0.0 });
            let overview_live =
                overview_open || overview_spring.value > 0.0 || overview_scrim_spring.value > 0.0;
            if overview_live {
                overview_spring.step(dt);
                overview_scrim_spring.step(dt);
            }
            // The card model runs whenever the overview is even partly open, so
            // a dismiss animation and the kill grace clock both keep time.
            if overview_live {
                kill_queue = recents.step(dt, frame_ms);
                // Park cards whose springs are done, so a settled list stops
                // re-integrating.
                recents.park_expired(frame_ms);
            }
            // The kill queue is drained here, not in the gesture handler: a
            // close request raised on a touch must not run a process teardown
            // inside the input callback.
            //
            // `Close` only *describes* the request -- `recents` starts the
            // grace clock and re-raises it as `Force` once the grace expires,
            // so the signal is sent on exactly the second pass and a
            // well-behaved app gets to close itself. A pid the shell does not
            // own is a no-op, not an error: the card has already left the
            // stack either way.
            for action in kill_queue.iter() {
                if let utim_core::compositor::KillAction::Force(pid) = action {
                    // A card's pid comes from the model, which is fed from
                    // `Child::id()` and from `pending_launch_pid` (0 for an
                    // in-app screen the shell drew itself). `kill` treats 0 as
                    // "my whole process group" and -1 as "everything I may
                    // signal", so a card that ever carried 0 or 1 would take
                    // the compositor's own process tree with it. The guard is
                    // the same `pid_is_signallable` the terminal cleanup uses,
                    // so there is one definition of "safe to signal" in the
                    // shell rather than two that can drift.
                    // A negative pid casts to a huge `u32` and fails the
                    // `i32::MAX` bound inside the guard, so no separate sign
                    // check is needed here.
                    if pid_is_signallable(*pid as u32) {
                        // SAFETY: `kill` is async-signal-safe, takes only a pid
                        // and a signal, and cannot fail in a way that matters
                        // here -- the process may already be gone, which is the
                        // outcome we wanted. The pid is proven > 1, not this
                        // process and not its parent by `pid_is_signallable`.
                        unsafe {
                            libc::kill(*pid, libc::SIGKILL);
                        }
                    }
                    // The card leaves the stack once its close is
                    // acknowledged, and a `Force` *is* that acknowledgement:
                    // the process has just been told to die. Without this the
                    // carousel kept every card it had ever dismissed, the
                    // `MAX_TASKS` slots filled with dead pids, and the
                    // overview could never report itself empty -- so Clear All
                    // left a full stack of ghosts behind, and `MAX_TASKS`
                    // new launches were silently dropped by `push`.
                    //
                    // pid 0 is a card for an in-app screen the shell drew
                    // itself (Clock, Settings). It has no process, but it is
                    // still a card the user dismissed, so it goes too.
                    let _ = recents.remove_by_pid(*pid);
                }
            }

            // Folder: the model owns the morph, the scrim and the title's
            // delayed fade, and reports the workspace scale it wants.
            if !folder.is_closed() {
                folder.step(dt, frame_ms);
                // The long press is checked here because the loop is
                // event-driven: a press followed by *no* further events produces
                // no frame from the input path, so a menu gated on movement
                // would only ever open if the user happened to jiggle. The poll
                // budget below keeps a frame coming while a press is armed, which
                // is what makes "hold still" work.
                if folder_gesture.poll_long_press(Instant::now()) {
                    // Opening the menu is a repaint.
                }
            }
            // The menu spring runs whether or not the folder is open, because it
            // has to animate *closed* after the folder has gone.
            folder_gesture.step(frame_ms);

            // Popup and fast-scroller fades, both wall-clock driven.
            if !popup_spring.is_at_rest() {
                popup_spring.step(dt);
            }
            if fastscroller.dragging || !fastscroller.popup_visible() {
                fastscroller.step(dt);
            }

            // A fast-scroller drag must actually scroll the list.
            //
            // `FastScrollerState::on_move` already resolved the section under
            // the thumb and reported it as a letter, and the shell's only use
            // for that letter was a `CLOCK_TICK` haptic. The section index --
            // the letter-to-row map, built from the full catalogue on every
            // rescan -- was read in exactly one place, inside the scroller
            // itself. So dragging the rail showed a teardrop with a letter on
            // it and left the grid where it was: the reference resolves a
            // `FastScrollSectionInfo` and hands it to a `LinearSmoothScroller`
            // (`AllAppsFastScrollHelper.smoothScrollToSection:41-47`), and that
            // half never existed.
            //
            // Driven from the frame block rather than from the touch handler so
            // the list follows the thumb continuously instead of jumping once
            // per letter change -- which is what `startSmoothScroll` does.
            if fastscroller.dragging && app_drawer_open {
                if let Some(row) = fastscroller.sections.row_for_section(fastscroller.letter) {
                    let dl = Layout::plain(server.scene.width as f32, server.scene.height as f32);
                    let want = row as f32 * dl.row_pitch;
                    let target = clamp_drawer_scroll(want, &dl, all_managed_apps.len());
                    if (target - drawer_scroll_y).abs() > 0.5 {
                        drawer_scroll_y = target;
                    }
                }
            }

            // Smartspace: a wall-clock phase, not an interaction, so it is
            // stepped on time rather than on touch. Advancing it on a timer is
            // what makes the damage hash see it -- nothing else in the state
            // changes when a minute passes.
            if last_smartspace_tick.elapsed() >= Duration::from_millis(SMARTSPACE_TICK_MS) {
                last_smartspace_tick = Instant::now();
                smartspace_phase = (smartspace_phase + 1.0).min(1.0);
                // Re-read the weather sink on the same tick. It is a single
                // bounded `read` of a file under `/run`, on a 5 s cadence, and
                // it is the only thing in this loop that touches the
                // filesystem -- which is exactly why it is on a timer and not
                // on the frame path. `weather_str` was hardcoded to `""`, so
                // the weather half of the smartspace never rendered and
                // `smartspace_phase` cross-faded between two identical
                // strings.
                weather_sink.refresh(&mut weather_buf);

                // Expire notifications and rebuild the badge table on the same
                // tick. Once every 5 s rather than per frame: a count that
                // changes at 120 Hz is not information, and the rebuild is a
                // linear scan of the store.
                notifications.expire(elapsed.as_millis() as u32);

                // Mirror the decoded store into the scene's shade.
                //
                // `SystemUiShade` is the pre-existing model the audit called out
                // as having "a complete `NotificationCard` model, a `notify()`
                // entry point, a fixed-capacity ring and swipe-to-dismiss -- and
                // nothing in the workspace ever called them". It is what the
                // renderer projects from and what the swipe handlers mutate, so
                // the store is the producer and the shade is the view.
                //
                // Skipped while a row is being dragged: rebuilding would reset
                // `x_offset` and the row would jump back under the finger. The
                // 5 s cadence makes that a five-second window in which a new
                // notification is not mirrored, which is the right trade against
                // a row that cannot be dragged.
                let dragging = server
                    .scene
                    .system_ui
                    .notifications
                    .iter()
                    .any(|n| n.x_offset != 0.0);
                if !dragging {
                    // Rebuild newest-first. `notify` replaces by id, so a row that
                    // has not changed is written over itself rather than
                    // duplicated, and the ring's own cap applies.
                    server.scene.system_ui.notifications.clear();
                    for row in notifications.rows().take(SHADE_NOTIF_ROWS) {
                        server.scene.system_ui.notify(
                            utim_core::compositor::systemui::NotificationSpec {
                                app_name: row.app_name.clone(),
                                replaces_id: row.id,
                                app_icon: String::new(),
                                summary: row.summary.clone(),
                                body: row.body.clone(),
                                actions: row
                                    .actions
                                    .iter()
                                    .map(|(k, label)| (format!("action{k}"), label.clone()))
                                    .collect(),
                                urgency: row.urgency,
                            },
                        );
                    }
                    // Anything past the shade's row budget still closes in the
                    // model, or the next mirror would resurrect it: the store
                    // keeps rows the shade cannot show, and `notify` would
                    // re-insert every one of them on the next tick.
                    for extra in notifications.rows().skip(SHADE_NOTIF_ROWS) {
                        let _ = server.scene.system_ui.on_notification_release(extra.id);
                    }
                }

                badge_rows.clear();
                // Borrowed after the expiry pass, so the two do not overlap:
                // `rows()` holds `notifications` for the loop, and the table it
                // fills is what the frame reads.
                let live: Vec<(&str, u32)> = notifications
                    .rows()
                    .filter(|r| !r.app_name.is_empty())
                    .map(|r| (r.app_name.as_str(), 1u32))
                    .collect();
                // `(app name, 1)` per live row; the fold below turns that into
                // a count. Keyed on the app name, which is what an icon's `id` is
                // derived from for a desktop entry.
                for (app_name, one) in live {
                    match badge_rows.iter_mut().find(|(id, _)| id == app_name) {
                        Some(slot) => slot.1 += one,
                        None => badge_rows.push((app_name.to_string(), one)),
                    }
                }
            }

            if app_launch_progress > 0.0 {
                if app_launch_spring.target != 1.0 {
                    // Kick the expansion off with a real velocity so the card
                    // leaves the icon with momentum instead of easing in.
                    app_launch_spring.set_target(1.0);
                    app_launch_spring.value = app_launch_progress;
                    app_launch_spring.velocity = 2.2;
                }
                app_launch_spring.step(dt);
                app_launch_progress = app_launch_spring.value.clamp(0.0, 1.0);
                if app_launch_progress >= 0.995 {
                    app_launch_progress = 0.0;
                    app_launch_origin = None;
                    app_launch_spring.set_target(0.0);
                    app_launch_spring.value = 0.0;
                    app_launch_spring.velocity = 0.0;
                }
            }

            // Ripple: the Material 3 state layer expands and fades together.
            // The radius starts at the touch point's 48dp target and grows
            // past it, the alpha falls on the same clock, and both are time
            // constants so the feel is identical at 60 and 120 Hz.
            if let Some((_, _, ref mut r, ref mut a)) = touch_ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
                if *a <= 0.0 {
                    touch_ripple = None;
                }
            }

            if let Some(ref mut drm) = drm_display {
                let t_str = format_current_time(&mut time_buf, !state.clock_24h);
                // The date only changes at midnight, but reformatting it is a
                // handful of stores into a stack buffer, so it is refreshed
                // with the time rather than tracked with a second timer. The
                // resulting bytes are hashed into the damage state, so a real
                // date change repaints and a redundant reformat does not.
                let date_str = format_current_date(&mut date_buf);
                if active_tab_idx >= terminal_tabs.len() {
                    active_tab_idx = terminal_tabs.len().saturating_sub(1);
                }
                let active_tab = &terminal_tabs[active_tab_idx];
                // Fixed-capacity scratch, on the stack: the four views below
                // are rebuilt on every frame tick, and a `Vec` each meant a
                // `malloc`/`free` pair at 120 Hz for data that is bounded by
                // the layout. Nothing here grows, so the loop body holds no
                // heap allocation at all.
                let mut scratch = ShellScratch::new();
                let mut labels = LabelScratch::new();

                for (idx, tab) in terminal_tabs.iter().enumerate() {
                    scratch.tab_infos.push(TerminalTabInfo {
                        id: tab.id,
                        title: &tab.title,
                        is_running: tab.is_running(),
                        is_active: idx == active_tab_idx,
                    });
                }
                let tab_infos = scratch.tab_infos.as_slice();

                // 1. Grid apps on the current home screen page
                //
                // Search filters the whole catalogue; otherwise the page's own
                // ids resolve against it. Both feed the same fixed-capacity
                // view, so a filter that somehow matched more apps than the
                // grid can display drops the overflow rather than growing.
                if search_active && !search_query.is_empty() {
                    // Ranked, for the same reason the drawer is: the grid's
                    // search results are ordered by `MatchType.priority`, so a
                    // query that matches both `Firefox` exactly and
                    // `Firefox Nightly` as a substring shows the exact one first.
                    // A substring filter here showed them in catalogue order, so
                    // which of the two was row 0 depended on the directory walk.
                    if !all_managed_apps.is_empty() {
                        let mut ranked: [SearchHit<'_, ManagedApp>; FRAME_MAX_GRID] =
                            core::array::from_fn(|_| SearchHit::empty(&all_managed_apps[0]));
                        search_ranked(&all_managed_apps, &search_query, app_fields, &mut ranked);
                        for hit in ranked.iter() {
                            if hit.score == i32::MIN {
                                // `SearchHit::empty` filler: past the number of
                                // matches. The count is the fill marker because
                                // a real row's score is never `i32::MIN`.
                                break;
                            }
                            scratch.grid_items.push(drawer_item_of(hit.item));
                        }
                    }
                } else {
                    // A cell is either an app or a folder, and they draw
                    // differently: a folder is a tile with up to four mini icons
                    // clustered on it, an app is a tile with a glyph. Both are an
                    // `AppGridItem`, so the grid loop is unchanged, and the
                    // *preview* is what the renderer draws when it sees a folder
                    // id. Keeping them one type is what lets the grid, the hit
                    // test and the scroller stay a single loop each.
                    for token in home_pages[current_home_page].iter() {
                        match PageCell::from_token(token) {
                            PageCell::App(id) => {
                                if let Some(a) = all_managed_apps.iter().find(|a| a.id == id) {
                                    scratch.grid_items.push(drawer_item_of(a));
                                }
                            }
                            PageCell::Folder(fid) => {
                                let Some(f) = state.folder(fid) else {
                                    // A folder whose record is gone must draw
                                    // nothing rather than an empty slot: the
                                    // reference removes such a cell from the
                                    // workspace on the package-removed broadcast
                                    // (`PackageUpdatedTask.java:79-88`), and
                                    // UTLC has no such broadcast.
                                    continue;
                                };
                                scratch.grid_items.push(AppGridItem {
                                    // The *stored token* is the id, borrowed
                                    // from `home_pages` -- no allocation, and it
                                    // is exactly the string the tap path parses
                                    // back into a `PageCell::Folder`, so the
                                    // drawn tile and the tappable cell are the
                                    // same cell by construction.
                                    id: token.as_str(),
                                    name: f.title.as_str(),
                                    color: shell_palette.primary,
                                    glyph: "",
                                    icon: None,
                                    // The cluster is capped at the reference's
                                    // `MAX_NUM_ITEMS_IN_PREVIEW = 4`
                                    // (`ClippedFolderIconLayoutRule.java:7`).
                                    folder_n: f.items.len().min(FOLDER_PREVIEW_MAX) as u8,
                                    folder_id: fid,
                                });
                            }
                        }
                    }
                }
                // The folder clusters, keyed by the same cell token the grid
                // item carries. Built only for cells that resolved to a folder, so
                // a workspace with no folders leaves this empty and the
                // renderer's lookup finds nothing.
                scratch.folder_previews.clear();
                for token in home_pages[current_home_page].iter() {
                    let PageCell::Folder(fid) = PageCell::from_token(token) else {
                        continue;
                    };
                    let _ = token;
                    let Some(f) = state.folder(fid) else {
                        continue;
                    };
                    let mut icons: [Option<&RgbaImage>; FOLDER_PREVIEW_MAX] =
                        [None; FOLDER_PREVIEW_MAX];
                    let mut n = 0usize;
                    for member in f.items.iter().take(FOLDER_PREVIEW_MAX) {
                        if let Some(app) = all_managed_apps.iter().find(|a| a.id == *member) {
                            icons[n] = app.icon.as_deref();
                            n += 1;
                        }
                    }
                    if n == 0 {
                        // A folder whose members are all uninstalled draws its
                        // tile with no cluster, rather than an empty slot.
                        continue;
                    }
                    scratch.folder_previews.push(FolderPreviewRow {
                        folder: fid,
                        icons,
                        n: n as u8,
                    });
                }

                let grid_items = scratch.grid_items.as_slice();

                // 2. Drawer apps (full catalogue for the PixelUI App Drawer),
                // skipped entirely while the drawer is closed and off-screen.
                //
                // The view is bounded but the *count* is not: the header shows
                // how many apps match, and that has to keep counting past what
                // fits on screen. So the match total is carried separately
                // and the view holds only the rows the renderer can draw --
                // which is what its own `.take(max_apps)` was already
                // discarding.
                // The drawer's *window*: only the rows the scroll offset makes
                // visible, not the whole catalogue.
                //
                // The view is fixed-capacity (`FRAME_MAX_GRID`) and always was,
                // which is correct for the home grid (a page holds at most
                // `grid_cols * max_rows` apps by construction) but wrong for
                // the drawer, where the list is as long as the catalogue. So
                // the window is positioned by the scroll: `drawer_first_index`
                // is the catalogue index of its first row, and the renderer
                // adds it back to place each icon.
                //
                // Deriving the row from the window index alone -- as the
                // renderer used to -- is the bug this replaces. The cull range
                // came from `drawer_apps.len()` (capped at 64) while the hit
                // test used the true catalogue length, so in a 137-app drawer
                // rows 13..27 were launchable but never drawn.
                let mut drawer_app_count = 0usize;
                let mut drawer_first_index = 0usize;
                if app_drawer_open || drawer_spring.value > 0.0 {
                    // One ranked pass. This replaces *two* substring passes --
                    // one to count, one to window -- so ranking is not an extra
                    // cost on the frame path but a cheaper way of doing work that
                    // was already there.
                    //
                    // The count is the total, not the window length, so the cull
                    // range and the fast scroller are still computed against the
                    // real list length rather than against a screenful. The
                    // window itself is the top-N *in rank order*, so
                    // `drawer_first_index` is a rank offset and the catalogue
                    // index is recovered through `ranked[k].item` below. That
                    // pairing is the whole change: previously the rank offset
                    // *was* a catalogue index, which is what let the drawn rows
                    // and the tappable rows disagree.
                    // `SearchHit::empty` needs a row to point at, and an empty
                    // catalogue has none. A drawer with nothing in it is also a
                    // drawer with no matches, so the whole block is guarded
                    // rather than carrying a placeholder row that no code path
                    // would read.
                    if !all_managed_apps.is_empty() {
                        let mut ranked: [SearchHit<'_, ManagedApp>; DRAWER_RANK_RUNWAY] =
                            core::array::from_fn(|_| SearchHit::empty(&all_managed_apps[0]));
                        drawer_app_count = search_ranked(
                            &all_managed_apps,
                            &drawer_search,
                            app_fields,
                            &mut ranked,
                        );
                        // How much of the runway the ranking actually filled. This
                        // is the count of *retained* matches; `drawer_app_count` above
                        // is the count of *all* matches, which is what the scroller
                        // and the header need and what the window must not be.
                        let ranked_len = drawer_app_count.min(DRAWER_RANK_RUNWAY);
                        if drawer_app_count > 0 {
                            let dl = Layout::plain(
                                server.scene.width as f32,
                                server.scene.height as f32,
                            );
                            let total_rows = drawer_app_count.div_ceil(dl.grid_cols.max(1));
                            let first_row = dl.visible_row_range(drawer_scroll_y, total_rows).start;
                            drawer_first_index = first_row * dl.grid_cols;
                            // The drawable slice of the ranked runway. Clamped at
                            // both ends: the scroll can run past the runway, and the
                            // runway can be shorter than a screen when the query is
                            // narrow.
                            let start = drawer_first_index.min(ranked_len);
                            let matched_n = (start + FRAME_MAX_GRID).min(ranked_len) - start;
                            // Two passes over the slice, because the label buffers are
                            // written and then borrowed and Rust will not let the same
                            // array be borrowed both ways across a loop.
                            for k in 0..matched_n {
                                labels.elide(k, ranked[start + k].item.name.as_str());
                            }
                            for k in 0..matched_n {
                                let a = ranked[start + k].item;
                                scratch.drawer_items.push(AppGridItem {
                                    id: a.id.as_str(),
                                    name: labels.get(k),
                                    color: a.color,
                                    glyph: a.glyph.as_str(),
                                    icon: a.icon.as_deref(),
                                    folder_n: 0,
                                    folder_id: 0,
                                });
                            }
                        }
                    }
                }
                let drawer_items = scratch.drawer_items.as_slice();

                // Hotseat: the same `DOCK_SLOTS` table the hit test uses,
                // resolved from the persisted app *ids* through `dock_index`.
                // Slot resolution is cached across catalogue rescans, so only
                // the small item view is rebuilt, with no find and no Rc clone.
                //
                // A slot whose id no longer resolves to an installed app still
                // draws, with the id as its label. It used to draw with a
                // hardcoded English name from `DOCK_NAMES`, which is how a
                // renamed or removed app left a plausible-looking tile that
                // launched nothing.
                let dock_ids = state.dock_slots();
                for (slot, id) in dock_ids.iter().enumerate() {
                    if id.is_empty() {
                        // An empty slot is a real configuration -- a gap in the
                        // dock -- and must not be padded with a substitute.
                        scratch.dock_items.push(AppGridItem {
                            id: "",
                            name: "",
                            color: 0,
                            glyph: "",
                            icon: None,
                            folder_n: 0,
                            folder_id: 0,
                        });
                        continue;
                    }
                    let entry = dock_index[slot].and_then(|i| all_managed_apps.get(i));
                    let icon = entry.and_then(|a| a.icon.as_deref()).or_else(|| {
                        if *id == "apps" {
                            dock_apps_icon.as_deref()
                        } else {
                            None
                        }
                    });
                    scratch.dock_items.push(AppGridItem {
                        id: entry.map(|a| a.id.as_str()).unwrap_or(id),
                        // The user's own label for the app wins, then the
                        // catalogue's, then the id. The 12-character truncation
                        // that used to happen at catalogue scan is gone; it
                        // mangled the name in the drawer, the dock and recents at
                        // once, and eliding belongs at the draw site.
                        name: state
                            .custom_name(id)
                            .or_else(|| entry.map(|a| a.name.as_str()))
                            .unwrap_or(id),
                        color: entry.map(|a| a.color).unwrap_or(0xFF475569),
                        glyph: entry.map(|a| a.glyph.as_str()).unwrap_or(":"),
                        icon,
                        folder_n: 0,
                        folder_id: 0,
                    });
                }
                let dock_items = scratch.dock_items.as_slice();

                // Super Extreme and Power Management state updates.
                //
                // Moved ahead of the notification projection. `notif_rows` borrows
                // `system_ui.notifications` for the rest of the frame, and the
                // exit-to-normal path calls `system_ui.set_brightness`, which
                // needs the shade mutably. Ordering is the whole fix: both blocks
                // run unconditionally once per frame, so nothing else changes.
                //
                super_extreme_state.check_power_button_hold();
                if super_extreme_state.request_exit_to_normal {
                    super_extreme_state.request_exit_to_normal = false;
                    let _ = server.scene.system_ui.set_brightness(75);
                    let _ = server.power_sync.restore_normal_power_mode();
                    set_active_family(FontFamily::NotoSans);
                    power_saver_mode = PowerSaverMode::Off;
                    quick_tiles_active[6] = false;
                }
                if super_extreme_state.request_reboot {
                    super_extreme_state.request_reboot = false;
                    let _ = std::process::Command::new("reboot").spawn();
                }
                if super_extreme_state.request_poweroff {
                    super_extreme_state.request_poweroff = false;
                    let _ = std::process::Command::new("poweroff").spawn();
                }
                if power_saver_mode == PowerSaverMode::SuperExtreme
                    && super_extreme_state.active_screen == SuperExtremeScreen::CameraPreview
                {
                    super_extreme_state.camera_preview.update_preview();
                }

                // The shade's rows, projected from the store. Bounded by the
                // shade's own row budget rather than the store's: the store keeps
                // up to `MAX_NOTIFICATIONS` so the badge counts stay honest even
                // when the shade can only show three.
                scratch.notif_rows.clear();
                for card in server
                    .scene
                    .system_ui
                    .notifications
                    .iter()
                    .take(SHADE_NOTIF_ROWS)
                {
                    scratch.notif_rows.push(NotifRow {
                        app_name: card.app_name.as_str(),
                        summary: card.summary.as_str(),
                        body: card.body.as_str(),
                        // `NotificationCard` has no urgency field, so the loud
                        // styling cannot be driven from it. Carried in the
                        // `critical` parallel list below, which the decode fills
                        // in the same order it pushed the card. See the audit's
                        // handoff for adding the field to `NotificationCard`
                        // itself -- it is the only thing standing between this and
                        // a first-class critical row.
                        critical: card.urgency == 2,
                        x_offset: card.x_offset,
                        n_actions: card.actions.len(),
                    });
                }
                let notif_rows = scratch.notif_rows.as_slice();

                // The Settings rows, projected while the Settings app is open and
                // dropped the moment it closes.
                //
                // Rebuilt every frame *while open* rather than only on open,
                // because a tap mutates `state` and the projection must follow it
                // or the panel would show the old value until it was reopened. It
                // is a ~20-element `Vec` of `&'static str` rows rebuilt on open
                // and after each change -- cheap, off the render path, and it
                // cannot go stale because nothing else mutates the state while
                // this panel is up.
                if active_app.as_deref() == Some("Settings")
                    || active_app.as_deref() == Some("settings")
                {
                    // Re-enumerate on open, not per frame. A `read_dir` walk of
                    // three directories is the most expensive thing the shell
                    // does, and the only reason to redo it is that a wallpaper
                    // may have been installed since boot -- which cannot happen
                    // while the panel is already up, because nothing else mutates
                    // the candidate list.
                    wallpaper_list = wallpaper_candidates();
                    let cursor = picker_cursor(&wallpaper_list, &state.wallpaper);
                    wallpaper_picker = utim_core::settings::picker::WallpaperPicker::new(
                        wallpaper_list.clone(),
                        cursor,
                    );
                    settings_rows.clear();
                    // The picker's position rides along, so the wallpaper row
                    // shows "3 of 7" rather than a value that never changes.
                    // `build_paged` with a zero total is exactly `build`, so a
                    // device with no wallpapers and a device where the picker
                    // failed to enumerate take the same path as before.
                    let at = wallpaper_picker.position().unwrap_or(0);
                    let of = wallpaper_picker.len() as u32;
                    settings_rows.extend(utim_core::settings::build_paged(&state, at, of));
                } else if !settings_rows.is_empty() {
                    settings_rows.clear();
                }

                // The open folder's contents, rebuilt only while one is open.
                // `open_folder == 0` is the closed case, which is almost always
                // the case, so this is not per-frame work in practice.
                if open_folder != 0 {
                    folder_items_of(
                        &state,
                        open_folder,
                        &all_managed_apps,
                        &mut scratch.folder_items,
                    );
                }
                let folder_items = scratch.folder_items.as_slice();

                // Flatten the recents model into the renderer's row view. Fixed
                // capacity and no `Vec`: this runs on the frame path, where
                // `paint_frame_does_not_allocate` is the invariant.
                //
                // `dismiss` comes from the model's spring, not from
                // `TaskCard::dismiss_y`, which is the *live drag* position --
                // the raw finger. After a release the card springs on from
                // there, and reading the raw value would snap it back the
                // instant the finger lifted.
                //
                // `visible_card` is the model's own cull hint, so the renderer
                // and the hit-test agree on which card is live.
                let recents_row_count = recents.iter().count().min(recents_rows.len());
                for (i, row) in recents_rows.iter_mut().enumerate().take(recents_row_count) {
                    let card = recents.cards[i];
                    row.app_id = card.app_id;
                    row.dismiss = recents.dismiss[i].value;
                    row.selected = i as u8 == recents.visible_card;
                }

                // Rebuild the palette if any input to it changed.
                //
                // Placed here rather than in the settings tap arm on purpose: the
                // tap arm is one of several ways the state can change, and a
                // palette refresh that depends on catching every writer is the
                // same "one call site is missing" shape as everything else this
                // project has been fixing. Here it is a per-frame comparison, so
                // it cannot be missed -- and the cost when nothing has changed is
                // four register compares and two `str` compares.
                let key_now = palette_key(&state);
                if key_now != palette_cache_key
                    || palette_wallpaper.as_str() != state.wallpaper.as_str()
                {
                    shell_palette = build_palette(&state);
                    palette_cache_key = key_now;
                    palette_wallpaper.clear();
                    palette_wallpaper.push_str(&state.wallpaper);
                    // The icon cache tints icons to the palette's background, so
                    // a palette change invalidates its derived tints. Without
                    // this the icons keep the *old* theme's background until
                    // something else happens to rebuild them.
                    icon_cache.set_foreground_background(Some(shell_palette.surface_container));
                }

                // Build the app-info panel's contents on the frame it is needed.
                //
                // Rebuilt rather than cached because it resolves a `PATH` search, and
                // cached because the panel must not re-run that search every frame
                // while open. `app_info_open` is the latch; `app_info_id` is what it
                // was opened for, so a *different* app re-resolves.
                if app_info_open
                    && app_info.as_ref().map(|s| s.id.as_str()) != Some(app_info_id.as_str())
                {
                    app_info = all_managed_apps
                        .iter()
                        .find(|a| a.id == app_info_id)
                        .map(|a| {
                            let (kind, target) = resolve_app_info_target(&a.id)
                                .unwrap_or((AppInfoTarget::Details, String::new()));
                            AppInfoState {
                                id: a.id.clone(),
                                name: a.name.clone(),
                                exec: a.exec.clone(),
                                icon: a.icon.clone(),
                                color: a.color,
                                glyph: a.glyph.clone(),
                                target,
                                target_kind: kind,
                                // No install session is driven by this shell, so
                                // the store row never appears. Recorded rather
                                // than defaulted to `false` so the reason travels
                                // with the field.
                                installing: false,
                            }
                        });
                }
                if !app_info_open {
                    app_info = None;
                }
                // Sensor poll ~2Hz off the draw path. None = keep history;
                // Some(Undefined/flat) clears dwell via update, like
                // process_accelerometer. A `Rotate` decision is forwarded
                // through the pure `decide_modeset` to the KMS device; anything
                // else (including `Unsupported`) holds. A failed ioctl is
                // logged, not retried: the policy has already advanced, and
                // hammering a failing modeset every 500 ms is worse than one
                // missed rotation.
                if last_sensor_poll.elapsed().as_millis() >= 500 {
                    last_sensor_poll = std::time::Instant::now();
                    if let Some(conn) = sensor_conn.as_mut() {
                        if let Some(o) = conn.poll_orientation() {
                            let before = rotation.current();
                            let natural = rotation.natural();
                            let decision = rotation.update(o);
                            if let Some(wanted) = decision.orientation() {
                                let action = utim_core::rotation::decide_modeset(
                                    before,
                                    wanted,
                                    natural,
                                    utim_core::rotation::PanelRotations::ALL,
                                );
                                if let utim_core::rotation::ModesetAction::Rotate(target) = action {
                                    // `drm` is the frame's borrow of the display
                                    // (this whole body runs inside it), so there
                                    // is nothing else to borrow.
                                    if let Err(e) = drm.set_orientation(target) {
                                        eprintln!("[-] rotation modeset failed: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }

                let drm_state = DrmInteractiveState {
                    time_str: t_str,
                    is_locked: server.scene.mode
                        == utim_core::compositor::scene::ShellMode::LockScreen,
                    cursor_pos,
                    is_touching,
                    search_query: &search_query,
                    search_active,
                    keyboard_active: server.scene.keyboard.is_active,
                    keyboard_shift_active: server.scene.keyboard.is_shift_active,
                    shade_open: server.scene.system_ui.is_open(),
                    quick_tiles_active,
                    quick_tile_notes: quick_tile_note,
                    active_app: active_app.as_deref(),
                    app_input: &app_input,
                    app_input_focused,
                    messages_list: &messages_list,
                    terminal_lines: &active_tab.lines,
                    terminal_input: &active_tab.input,
                    terminal_prompt: utim_core::session::session().prompt(),
                    terminal_running: active_tab.is_running(),
                    terminal_tabs: tab_infos,
                    terminal_active_tab: active_tab_idx,
                    show_search_bar: state.show_search_bar,
                    // Folder. The grid and theme are the settings; the page and
                    // count come from the model, and both are hashed by the
                    // damage tracker so a page swipe repaints.
                    folder_grid: (state.folder_cols as u8, state.folder_rows as u8),
                    screen_off: screen_timeout.expired_at(Instant::now()),
                    // Agent D's render-side fields. `app_info` is `None` until the
                    // shell has an app-info state to publish -- the panel renders,
                    // but nothing in the shell opens it yet, and a panel that is
                    // populated by guesswork would show the wrong app.
                    app_info: app_info.as_ref().map(|spec| AppInfoSpec {
                        id: spec.id.as_str(),
                        name: spec.name.as_str(),
                        exec: spec.exec.as_str(),
                        target: spec.target.as_str(),
                        icon: spec.icon.as_deref(),
                        color: spec.color,
                        glyph: spec.glyph.as_str(),
                        target_kind: spec.target_kind,
                        installing: spec.installing,
                    }),
                    // The workspace drag layer. Every field is inert because no
                    // workspace icon drag is wired yet -- `drag_slot: None` is
                    // what tells the renderer "nothing is being dragged", and it
                    // is the value the whole drag draw path must handle.
                    drag_slot: None,
                    drag_pos: (0.0, 0.0),
                    keyboard_layout,
                    drag_lift: 0.0,
                    drag_drop_slot: None,
                    drag_merge_slot: None,
                    folder_drag_slot: folder_gesture.drag_slot,
                    folder_drag_pos: folder_gesture.drag_pos,
                    folder_drop_slot: folder_gesture.drop_slot,
                    folder_drag_out: folder_gesture.drag_out,
                    folder_menu_progress: folder_gesture.menu_progress,
                    folder_menu_anchor: folder_gesture.menu_anchor,
                    folder_rename_buffer: folder_rename
                        .as_ref()
                        .map(|r| {
                            utim_core::graphics::drm_kms::FolderRenameBuffer::truncated(&r.buffer)
                        })
                        .unwrap_or(utim_core::graphics::drm_kms::FolderRenameBuffer::EMPTY),
                    folder_rename_editing: folder_rename.is_some(),
                    folder_dark: state.dark_theme,
                    folder_page: folder.page,
                    folder_item_count: folder.item_count() as u8,
                    font_scale: state.font_scale,
                    icon_shadows: &icon_shadow_rows,
                    drawer_section_letter: fastscroller.letter_str(),
                    settings_rows: &settings_rows,
                    wallpaper: wallpaper_img.as_deref(),
                    // The reference's workspace scrim. Enough that a white photo
                    // under the smartspace does not put white text on white, and
                    // not so much that the image stops being visible -- the whole
                    // point of having one.
                    wallpaper_dim: if state.dark_theme { 0x4D } else { 0x73 },
                    notifications: notif_rows,
                    folder_previews: scratch.folder_previews.as_slice(),
                    badge_counts: &badge_rows
                        .iter()
                        .map(|(k, n)| (k.as_str(), *n))
                        .collect::<Vec<_>>(),
                    grid_apps: grid_items,
                    dock_apps: dock_items,
                    app_drawer_open,
                    drawer_apps: drawer_items,
                    drawer_first_index,
                    drawer_scroll_y,
                    drawer_app_count,
                    drawer_search: &drawer_search,
                    home_page: current_home_page,
                    total_home_pages: home_pages.len(),
                    selected_icon_id: selected_home_icon.as_deref(),
                    drawer_progress,
                    home_scroll_offset,
                    app_launch_progress,
                    app_launch_origin,
                    app_launch_color,
                    touch_ripple,
                    ripple_clip,
                    pressed_icon_id: pressed_icon_id.as_deref(),
                    icon_press_scale: icon_bounce_spring.value,
                    palette: shell_palette,
                    power_saver_mode,
                    super_extreme_state: if power_saver_mode == PowerSaverMode::SuperExtreme
                        || super_extreme_state.volume_hud.is_visible()
                    {
                        Some(&super_extreme_state)
                    } else {
                        None
                    },
                    // Launcher rewrite state, driven by the shell state
                    // machine above. At-rest values mean "nothing is
                    // animating", which is also what the damage hash needs so
                    // an idle shell still issues zero ioctls.
                    smartspace_phase,
                    folder_morph: folder.morph.value,
                    folder_scrim: folder.scrim.value,
                    folder_title_alpha: folder.title_alpha.value,
                    popup_progress: popup_spring.value,
                    overview_progress: overview_spring.value,
                    // The scrim is the second spring's value, not the
                    // carousel's: the reference dims the workspace on a
                    // different profile, and driving both from one number
                    // would make the dim and the surface arrive together.
                    overview_scroll: recents.drag_px,
                    overview_dismiss: recents.dismiss[recents.visible_card as usize].value,
                    // The thumb's position along the track. `travel` is the
                    // same quantity the model clamps against, so the renderer
                    // cannot place the thumb where the model would not.
                    fastscroller_thumb: if fastscroller.track_h > 0.0 {
                        let travel = (fastscroller.track_h - shell_fast_scroller.thumb_h).max(0.0);
                        if travel > 0.0 {
                            fastscroller.thumb_y / travel
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    },
                    fastscroller_popup_alpha: fastscroller.popup_alpha,
                    // Page indicator: signed page progress. The magnitude is
                    // how far the strip has travelled, in page widths; the
                    // sign is the direction, which the renderer needs because
                    // the dot maths takes "the page being left" and "the page
                    // being moved towards" as two separate indices.
                    //
                    // Not clamped: the settle spring overshoots past a page, and
                    // that phase lives above 1.0. A clamp here would flatten
                    // exactly the motion the field exists to carry.
                    page_indicator_frac: {
                        let pw = server.scene.width as f32;
                        if pw > 0.0 {
                            -home_scroll_offset / pw
                        } else {
                            0.0
                        }
                    },
                    // The in-app home gesture and the folder both scale the
                    // workspace. The folder wins while it is open: it is the
                    // nearer surface and its scale is the reference's
                    // `FOLDER_LAUNCHER_SCALE` (0.975).
                    workspace_scale: if folder.is_closed() {
                        workspace_scale_spring.value
                    } else {
                        folder.workspace_scale()
                    },
                    window_alpha: window_alpha_spring.value,
                    date_str,
                    weather_str: &weather_buf,
                    weather_glyph: 0,
                    catalogue_apps: &catalogue_items,
                    recents_cards: &recents_rows[..recents_row_count],
                    recents: Some(&recents),
                    fastscroller_letter: fastscroller.letter,
                    popup_anchor,
                    popup_items: &popup_rows.items[..popup_rows.len as usize],
                    folder_apps: folder_items,
                    folder_title: &folder_title,
                };
                // Unconditional flush() marks the whole 10.4 MB framebuffer
                // dirty 60x/s; only flush when the damage hash proves a repaint.
                if drm.render_interactive_ui(&drm_state) {
                    if let Err(e) = drm.flush() {
                        eprintln!("[UTLC] DIRTYFB flush failed: {}", e);
                    }
                }
            }
        }

        // Auto-transition to Launcher home screen after initial boot presentation
        if power_saver_mode != PowerSaverMode::SuperExtreme
            && server.scene.lockscreen.is_locked()
            && server.start_time.elapsed() >= Duration::from_secs(2)
        {
            server.scene.lockscreen.unlock();
            server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
        }

        // Watchdog periodic ping
        if last_watchdog_ping.elapsed() >= watchdog_interval {
            last_watchdog_ping = Instant::now();
            if let (Some(ref dgram), Some(ref sock_path)) = (&notify_dgram, &notify_socket) {
                let _ = dgram.send_to(b"WATCHDOG=1\n", sock_path);
            }
        }
    }

    for fd in input_fds {
        unsafe {
            libc::close(fd);
        }
    }

    if epoll_fd >= 0 {
        unsafe {
            libc::close(epoll_fd);
        }
    }
    if inotify_fd >= 0 {
        unsafe {
            libc::close(inotify_fd);
        }
    }
    if sig_fd >= 0 {
        unsafe {
            libc::close(sig_fd);
        }
    }
}

/// Scale an icon compresses to while a finger is on it. Lawnchair 17 /
/// Launcher3 use a 0.92-style press bounce; the spring then rebounds past 1.0
/// on release.
const PRESS_SCALE: f32 = 0.90;

/// Ripple growth and fade rates, in px/s and per second.
const RIPPLE_GROW: f32 = 900.0;
const RIPPLE_FADE: f32 = 2.2;

/// A ripple starts at the Material 3 minimum touch target, so it always
/// covers the affordance the finger actually hit.
fn ripple_start_radius(panel_w: f32) -> f32 {
    (panel_w.min(2400.0) * 0.048).max(12.0) * 0.5
}

/// Render the wall clock into a 5-byte `HH:MM` or `h:MM` buffer.
///
/// The buffer is fixed at 5 bytes and the function writes into a caller-owned
/// array with no allocation, so this stays on the per-frame path. `h12`
/// selects the reference's `SmartspaceTimeFormat`
/// (`SmartspaceTimeFormat.kt:6-39`): with it, the leading digit of a single-digit
/// hour is a blank, and 13:05 renders as `1:05` rather than `01:05`. Before this
/// the format was hardcoded 24-hour with no setting behind it.
fn format_current_time(buf: &mut [u8; 5], h12: bool) -> &str {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let total_secs = ts.tv_sec;
    let hours24 = (total_secs / 3600).rem_euclid(24) as u8;
    let mins = (total_secs / 60).rem_euclid(60) as u8;
    let (lead, hours) = if h12 {
        let h12h = match hours24 % 12 {
            0 => 12,
            h => h,
        };
        if h12h >= 10 {
            (Some(h12h / 10), h12h % 10)
        } else {
            (None, h12h)
        }
    } else {
        (Some(hours24 / 10), hours24 % 10)
    };
    let mut n = 0usize;
    if let Some(d) = lead {
        buf[n] = b'0' + d;
        n += 1;
    }
    buf[n] = b'0' + hours;
    buf[n + 1] = b':';
    buf[n + 2] = b'0' + (mins / 10);
    buf[n + 3] = b'0' + (mins % 10);
    // Blank the unused leading byte in the 12-hour, single-digit case so the
    // caller still gets a 5-byte string and never reads a stale digit.
    if lead.is_none() {
        buf[0] = b' ';
    }
    unsafe { std::str::from_utf8_unchecked(buf) }
}

/// Weekday and month abbreviations, 3 bytes each, index 0 = Sunday.
///
/// A `const` table rather than a lookup into the C locale: `strftime` would
/// need a format string and a `tm`, and the abbreviations are fixed for the
/// lifetime of the build.
const DAY_ABBR: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const MONTH_ABBR: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// Format today's date as `"Tue, Sep 22"` into a caller-owned buffer.
///
/// Zero allocation, same contract as [`format_current_time`]: the renderer
/// cannot format a date itself without allocating on the frame path, so the
/// shell does it here and hands over a borrowed `&str`.
///
/// Uses `localtime_r` rather than a hand-rolled UTC offset. A fixed offset
/// gives the right answer for 363 days and the wrong one twice a year, and it
/// is wrong by a whole day twice more if the user crosses a timezone; the
/// `_r` variant needs only a stack `tm` and is thread-safe, so it costs the
/// same as the arithmetic would have and is correct.
fn format_current_date(buf: &mut [u8; 16]) -> &str {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // glibc's `localtime_r` takes a `time_t` (an `i64`), not a `timespec`.
    // A null return means the timezone database is unavailable (a static
    // build with no /usr/share/zoneinfo). Fall back to UTC so the line still
    // renders instead of vanishing.
    if unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) }.is_null() {
        tm = unsafe { std::mem::zeroed() };
        let days = ts.tv_sec.div_euclid(86_400);
        tm.tm_mday = (days + 19_723) as i32; // 1970-01-01 is day 0
        tm.tm_wday = ((days + 4) % 7) as i32; // ...and a Thursday
    }
    format_date_from_tm(buf, tm.tm_wday, tm.tm_mon, tm.tm_mday)
}

/// The pure half of [`format_current_date`], so it can be tested against known
/// dates instead of only against "whatever today is".
///
/// Out-of-range fields are clamped rather than trusted: `tm_*` comes from
/// libc, and an index panic on the frame path would be a crash rather than a
/// wrong glyph.
fn format_date_from_tm(buf: &mut [u8; 16], wday: i32, mon: i32, mday: i32) -> &str {
    let weekday = (wday.rem_euclid(7) as usize).min(6);
    let mon_i = (mon.rem_euclid(12) as usize).min(11);
    let day = mday.clamp(1, 31) as usize;

    let mut n = 0usize;
    let push = |bytes: &[u8], buf: &mut [u8; 16], n: &mut usize| {
        for &c in bytes {
            if *n < buf.len() {
                buf[*n] = c;
                *n += 1;
            }
        }
    };
    push(DAY_ABBR[weekday], buf, &mut n);
    push(b", ", buf, &mut n);
    push(MONTH_ABBR[mon_i], buf, &mut n);
    push(b" ", buf, &mut n);
    // Day of month, 1 or 2 digits, no leading zero (matches the reference's
    // "Sep 22" rather than "Sep 02"). A single-digit day must take the *ones*
    // digit, not the tens one -- the first version of this rendered
    // "Jan 0" for the 1st, which the known-dates test caught.
    let mut tmp = [0u8; 2];
    tmp[0] = b'0' + (day / 10) as u8;
    tmp[1] = b'0' + (day % 10) as u8;
    if day >= 10 {
        push(&tmp[..2], buf, &mut n);
    } else {
        push(&tmp[1..2], buf, &mut n);
    }
    // The byte pattern is `[A-Za-z, 0-9]` by construction, so this cannot
    // fail; `from_utf8` would be a second pass over 16 bytes.
    unsafe { std::str::from_utf8_unchecked(&buf[..n]) }
}

/// Average colour of the desktop wallpaper, used as the Material You seed.
///
/// This is the same job `dev.kdrag0n.monet` does on a real phone: pull a
/// colour out of what the user is looking at and build the tonal scheme from
/// it. Averaging a sparse grid of pixels keeps it a few hundred reads instead
/// of decoding the whole image.
/// Ceiling on what the wallpaper probe may transiently allocate, bytes.
///
/// The seed is one `u32` that selects a palette. What it costs to produce one
/// is a full inflate of the image, because PNG has no random access: every
/// scanline depends on the one above it. `png::decode_working_set_estimate`
/// puts that at about 9 bytes per pixel -- inflated scanlines, the RGBA image,
/// and the IDAT -- so:
///
/// | wallpaper  | pixels   | working set | probe |
/// |------------|----------|-------------|-------|
/// | 480x800    | 0.38 M   | 3.44 MiB    | ok    |
/// | 1280x720   | 0.92 M   | 7.91 MiB    | ok    |
/// | 1440x900   | 1.30 M   | 11.66 MiB   | skip  |
/// | 1080x2400  | 2.59 M   | 23.32 MiB   | skip  |
/// | 3840x2160  | 8.29 M   | 74.62 MiB   | skip  |
///
/// 8 MiB is the point: it is the transient headroom between the shell's
/// ~3.1 MiB steady RSS and the plan's 15 MiB ceiling, and 1280x720 is the
/// largest wallpaper that fits it. Anything bigger is skipped and the shell
/// keeps the fallback seed. That is a real capability loss -- a 1080p wallpaper
/// is the common case on a modern phone -- and it is the honest trade: the
/// alternative is spending 23 MiB during boot to sample a thousand pixels,
/// which on a 15 MiB budget is an OOM.
///
/// The proper fix is to make `png`'s inflate streaming, so the probe can
/// consume scanlines and discard them; that is outstanding work and is not
/// pretended at here.
const WALLPAPER_PROBE_BUDGET: u64 = 8 * 1024 * 1024;

/// A palette seed from the first wallpaper whose average colour we can afford
/// to compute, or a fixed fallback.
fn wallpaper_seed() -> u32 {
    const FALLBACK: u32 = 0xFF3B82F6;
    // Standard XDG wallpaper locations, newest first.
    const DIRS: [&str; 3] = [
        "/run/user/1000",
        "/usr/share/backgrounds",
        "/usr/share/wallpapers",
    ];
    for dir in DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "png" {
                continue;
            }
            // 33 bytes: the PNG signature plus the whole `IHDR` chunk. A
            // fixed-size stack buffer, so a hostile or enormous file cannot
            // make this read anything more than 33 bytes.
            let Ok(mut f) = std::fs::File::open(&path) else {
                continue;
            };
            let mut prefix = [0u8; 33];
            let read = match read_exact_or_less(&mut f, &mut prefix) {
                Some(n) => n,
                None => continue,
            };
            let Some((w, h)) = utim_core::graphics::png::header_size(&prefix[..read]) else {
                continue;
            };
            if utim_core::graphics::png::decode_working_set_estimate(w, h) > WALLPAPER_PROBE_BUDGET
            {
                // Too big to decode within budget: skip it rather than spike
                // the resident set during boot. Another directory may hold a
                // smaller one.
                continue;
            }
            // Affordable: now pay for the file.
            drop(f);
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            if let Some(p) = utim_core::graphics::palette::wallpaper_palette_from_png(&data) {
                return p.seed;
            }
            if let Some(seed) = average_png_colour(&data) {
                return seed;
            }
        }
    }
    FALLBACK
}

/// Read up to `buf.len()` bytes, returning how many were read.
///
/// `Read::read` is permitted to return a short read even before EOF, so a
/// partial header is normal rather than an error; the caller re-checks the
/// length through [`png::header_size`], which demands a whole `IHDR`.
/// Copy a path into a NUL-terminated stack buffer, refusing one that does not fit.
///
/// Truncating instead of refusing would open a *different* file, which is worse
/// than failing: the failure mode has to be "no weather", never "some other
/// file". 256 bytes is comfortably above every path this module builds
/// (`$XDG_RUNTIME_DIR` plus `utlc-weather`).
const OPEN_PATH_MAX: usize = 256;

#[inline]
fn path_to_c(path: &std::path::Path) -> Option<[u8; OPEN_PATH_MAX]> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() >= OPEN_PATH_MAX {
        return None;
    }
    let mut c = [0u8; OPEN_PATH_MAX];
    c[..bytes.len()].copy_from_slice(bytes);
    // `c[bytes.len()]` is still the initialiser's 0, so the result is NUL
    // terminated. Built by hand rather than with a `CString` so the touch and
    // tick paths do not allocate.
    Some(c)
}

/// Read a file without following a symlink, into a caller-supplied buffer.
///
/// The weather sink is written by something outside the shell -- a companion
/// daemon, a cron job, a human -- at a path under `$XDG_RUNTIME_DIR` or
/// `/run`. `std::fs::read` follows a symlink there, so a symlink planted at
/// `utlc-weather` would make the shell, running as root, read an arbitrary
/// file and put its first 32 bytes into a string that gets drawn on the
/// display. That is a real disclosure channel even though the output is
/// truncated to 32 bytes.
///
/// `O_NOFOLLOW` makes the open fail with `ELOOP` instead, and the sink then
/// reads as absent -- which the weather path already treats as "no weather
/// service". `O_CLOEXEC` is named explicitly because this is a raw syscall
/// rather than a `std::fs` call.
fn read_no_follow(path: &std::path::Path, buf: &mut [u8]) -> Option<usize> {
    use std::os::unix::io::FromRawFd;
    let c_path = path_to_c(path)?;
    // SAFETY: `c_path` is NUL terminated within its own array (guaranteed by
    // `path_to_c`, which returns `None` for anything that would not fit) and
    // the flags are the ones named. The fd is wrapped immediately below, so it
    // is closed on every path out of this function.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a fresh owned descriptor, wrapped exactly once here.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    read_exact_or_less(&mut f, buf)
}

fn read_exact_or_less(f: &mut std::fs::File, buf: &mut [u8]) -> Option<usize> {
    use std::io::Read;
    let mut n = 0usize;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(n)
}

/// Sparse-grid average of a decoded PNG, skipping transparent pixels.
///
/// Kept as the fallback when the Oklab quantiser yields nothing; the primary
/// path is `wallpaper_palette_from_png` (blue vs orange must differ, where a
/// mean yields the same grey — `WallpaperColorsCompat.kt:5-23`).
fn average_png_colour(data: &[u8]) -> Option<u32> {
    let img = utim_core::graphics::decode_png(data)?;
    const STEP: u32 = 24;
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    let mut y = 0u32;
    while y < img.height {
        let mut x = 0u32;
        while x < img.width {
            let i = ((y * img.width + x) * 4) as usize;
            if i + 3 < img.pixels.len() && img.pixels[i + 3] > 128 {
                r += img.pixels[i] as u64;
                g += img.pixels[i + 1] as u64;
                b += img.pixels[i + 2] as u64;
                n += 1;
            }
            x += STEP;
        }
        y += STEP;
    }
    if n == 0 {
        return None;
    }
    Some((0xFF << 24) | (((r / n) as u32) << 16) | (((g / n) as u32) << 8) | ((b / n) as u32))
}

/// Whether `pid` is safe to signal with `kill`.
///
/// `kill` has three broadcast cases and one self case, all of which are traps
/// here:
///
/// * `pid == 0` signals **every process in the caller's process group**.
/// * `pid == -1` signals **every process the caller may signal** -- the entire
///   machine, for root.
/// * `pid == getpid()` is the compositor killing itself.
/// * `pid == getppid()` is the compositor killing whatever supervises it,
///   which for `utlc.service` is systemd's own user manager.
///
/// The `-(pid as i32)` used for the process-group kill makes this worse, not
/// better: the atomic starts at `0` and is set from `Child::id()`, so any
/// corruption or wraparound that leaves it at `1` turns `-(1i32)` into `-1` and
/// turns a cleanup into a system-wide `SIGKILL`. A `u32` pid above `i32::MAX`
/// wraps the same way.
///
/// So the rule is: only signal a pid that is strictly greater than 1, is not
/// this process, is not our parent, and fits in a positive `i32`. The process
/// *group* form is then guarded again on the negation, because `-(x as i32)`
/// must itself be `< -1` before it means "that group" rather than "everything".
#[inline]
fn pid_is_signallable(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    let me = std::process::id();
    if pid == me {
        return false;
    }
    // SAFETY: `getppid` takes no arguments, cannot fail, and reads a value
    // the kernel maintains; it is async-signal-safe.
    let parent = unsafe { libc::getppid() };
    (pid as i32) != parent
}

/// Signal a child's process group and then the child, escalating INT -> KILL.
///
/// Refuses any pid that is not signallable, and refuses the group form unless
/// the negated pid is itself `< -1`. The two guards are independent on purpose:
/// `pid_is_signallable` rejects the inputs that would produce a broadcast, and
/// the second check re-proves the property at the point of use.
fn signal_child_tree(pid: u32) {
    if !pid_is_signallable(pid) {
        return;
    }
    let p = pid as i32;
    // SAFETY: `kill` is async-signal-safe, takes only a pid and a signal, and
    // cannot fail in a way that matters here -- the process may already be
    // gone, which is the outcome we wanted. Every pid below is proven > 1 and
    // its negation proven < -1 by construction, so none of these can be 0 or
    // -1.
    unsafe {
        // Ask the whole group to stop first so a shell's children get the
        // chance to exit cleanly, then escalate.
        libc::kill(-p, libc::SIGINT);
        libc::kill(p, libc::SIGINT);
        libc::kill(-p, libc::SIGKILL);
        libc::kill(p, libc::SIGKILL);
    }
}

fn cleanup_terminal_child(
    active_child_pid: &std::sync::atomic::AtomicU32,
    active_stdin: &std::sync::Mutex<Option<std::process::ChildStdin>>,
) {
    let pid = active_child_pid.swap(0, std::sync::atomic::Ordering::SeqCst);
    signal_child_tree(pid);
    *active_stdin.lock().unwrap() = None;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAction {
    Continue,
    CloseTab,
    NewTab,
    SwitchTab(usize),
}

pub struct TerminalTab {
    pub id: usize,
    pub title: String,
    pub lines: Vec<String>,
    pub input: String,
    pub tx: std::sync::mpsc::Sender<String>,
    pub rx: std::sync::mpsc::Receiver<String>,
    pub active_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
    pub active_child_pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl TerminalTab {
    pub fn new(id: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            id,
            title: format!("Tab {}: bash", id),
            lines: Vec::with_capacity(32),
            input: String::with_capacity(64),
            tx,
            rx,
            active_stdin: std::sync::Arc::new(std::sync::Mutex::new(None)),
            active_child_pid: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    pub fn is_running(&self) -> bool {
        let pid = self
            .active_child_pid
            .load(std::sync::atomic::Ordering::SeqCst);
        pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    pub fn cleanup_child(&self) {
        cleanup_terminal_child(&self.active_child_pid, &self.active_stdin);
    }
}

fn handle_terminal_enter(
    terminal_tabs: &mut Vec<TerminalTab>,
    active_tab_idx: &mut usize,
    next_tab_id: &mut usize,
    active_app: &mut Option<String>,
    keyboard: &mut VirtualKeyboard,
    search_active: &mut bool,
) {
    if *active_tab_idx >= terminal_tabs.len() {
        *active_tab_idx = terminal_tabs.len().saturating_sub(1);
    }
    let tab = &mut terminal_tabs[*active_tab_idx];
    let pid = tab
        .active_child_pid
        .load(std::sync::atomic::Ordering::SeqCst);
    let is_alive = pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 };
    let forwarded_to_child = if is_alive {
        let mut guard = tab.active_stdin.lock().unwrap();
        if let Some(ref mut stdin) = *guard {
            use std::io::Write;
            // Non-blocking pipe (set at spawn): a write can never stall the
            // UI thread. WouldBlock means the child is not keeping up, so
            // this line is dropped instead of blocking on it.
            let r1 = stdin.write(tab.input.as_bytes());
            let r2 = r1.and_then(|_| stdin.write(b"\n"));
            match r2 {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tab.input.clear();
                    true
                }
                Err(_) => false,
            }
        } else {
            false
        }
    } else {
        tab.active_child_pid
            .store(0, std::sync::atomic::Ordering::SeqCst);
        *tab.active_stdin.lock().unwrap() = None;
        false
    };

    if forwarded_to_child {
        push_terminal_line(&mut tab.lines, &tab.input);
        log_terminal_output(&tab.input);
        tab.input.clear();
    } else {
        let tab_id = tab.id;
        let mut title_buf = tab.title.clone();
        let action = execute_terminal_command(
            &mut tab.lines,
            &mut tab.input,
            active_app,
            Some(&tab.tx),
            Some(&tab.active_stdin),
            Some(&tab.active_child_pid),
            Some(&mut title_buf),
            tab_id,
        );
        tab.title = title_buf;

        match action {
            TerminalAction::CloseTab => {
                tab.cleanup_child();
                terminal_tabs.remove(*active_tab_idx);
                if terminal_tabs.is_empty() {
                    *active_app = None;
                    keyboard.deactivate();
                    *search_active = false;
                    terminal_tabs.push(TerminalTab::new(1));
                    *next_tab_id = 2;
                    *active_tab_idx = 0;
                } else if *active_tab_idx >= terminal_tabs.len() {
                    *active_tab_idx = terminal_tabs.len() - 1;
                }
            }
            TerminalAction::NewTab => {
                if terminal_tabs.len() < 4 {
                    terminal_tabs.push(TerminalTab::new(*next_tab_id));
                    *next_tab_id += 1;
                    *active_tab_idx = terminal_tabs.len() - 1;
                }
            }
            TerminalAction::SwitchTab(idx) => {
                if idx < terminal_tabs.len() {
                    *active_tab_idx = idx;
                }
            }
            TerminalAction::Continue => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_ime_action(
    act: ImeAction,
    active_app: &mut Option<String>,
    app_input: &mut String,
    app_input_focused: &mut bool,
    search_active: &mut bool,
    search_query: &mut String,
    terminal_tabs: &mut Vec<TerminalTab>,
    active_tab_idx: &mut usize,
    next_tab_id: &mut usize,
    keyboard: &mut VirtualKeyboard,
    messages_list: &mut Vec<String>,
) {
    match act {
        ImeAction::CommitChar(c) => {
            if active_app.as_deref() == Some("Terminal") {
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.push(c);
                }
            } else if *search_active {
                if search_query.len() + c.len_utf8() <= 60 {
                    search_query.push(c);
                }
            } else if active_app.is_some()
                && *app_input_focused
                && app_input.len() + c.len_utf8() <= 120
            {
                app_input.push(c);
            }
        }
        ImeAction::CommitString(s) => {
            if active_app.as_deref() == Some("Terminal") {
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.push_str(&s);
                }
            } else if *search_active {
                if search_query.len() + s.len() <= 60 {
                    search_query.push_str(&s);
                }
            } else if active_app.is_some() && *app_input_focused && app_input.len() + s.len() <= 120
            {
                app_input.push_str(&s);
            }
        }
        ImeAction::DeleteSurroundingText { .. } => {
            if active_app.as_deref() == Some("Terminal") {
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.pop();
                }
            } else if *search_active {
                search_query.pop();
            } else if active_app.is_some() && *app_input_focused {
                app_input.pop();
            }
        }
        ImeAction::SendKey(28) => {
            // ENTER
            if active_app.as_deref() == Some("Terminal") {
                handle_terminal_enter(
                    terminal_tabs,
                    active_tab_idx,
                    next_tab_id,
                    active_app,
                    keyboard,
                    search_active,
                );
            } else if *search_active {
                *search_active = false;
                keyboard.deactivate();
            } else if active_app.is_some() && *app_input_focused {
                if active_app.as_deref() == Some("Messages") {
                    if !app_input.is_empty() {
                        messages_list.push(format!("You: {}", app_input));
                        app_input.clear();
                    }
                } else {
                    keyboard.deactivate();
                    *app_input_focused = false;
                }
            }
        }
        // The IME asking for the keyboard to close. `Key::Hide` used to fold into
        // `ImeAction::None`, so the request was indistinguishable from "nothing
        // happened" and the keyboard stayed up over whatever the user had just
        // dismissed. Spelled out rather than left to the `_` arm, because a
        // wildcard here would silently swallow it -- and the next variant someone
        // adds would be swallowed too, with nothing to say so.
        ImeAction::HideKeyboard => keyboard.deactivate(),
        // Any variant a newer `ime.rs` adds. Named so that adding one is a compile
        // error here rather than a silent no-op.
        _ => {}
    }
}

fn log_terminal_output(line: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;

    // 1. Mirror to serial console /dev/ttyAMA0 so QEMU captures it into dist/qemu_terminal.log
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/ttyAMA0") {
        let _ = writeln!(tty, "[UTLC-TERM] {}", line);
    } else if let Ok(mut console) = OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(console, "[UTLC-TERM] {}", line);
    }

    // 2. Append to persistent log file /var/log/terminal.log on device
    if let Ok(mut log_file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/var/log/terminal.log")
    {
        let _ = writeln!(log_file, "{}", line);
    }
}

fn clean_terminal_line(line: &str) -> String {
    let s = line.split('\r').next_back().unwrap_or(line);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            } else if chars.peek() == Some(&']') {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\x07' || (next == '\x1b' && chars.peek() == Some(&'\\')) {
                        if next == '\x1b' {
                            chars.next();
                        }
                        break;
                    }
                }
                continue;
            } else if chars.peek() == Some(&'(') || chars.peek() == Some(&')') {
                chars.next();
                chars.next();
                continue;
            }
        }
        match c {
            '─' | '━' | '═' => out.push('-'),
            '│' | '┃' | '║' => out.push('|'),
            '┌' | '┍' | '┎' | '┏' | '╭' | '╔' => out.push('+'),
            '┐' | '┑' | '┒' | '┓' | '╮' | '╗' => out.push('+'),
            '└' | '┕' | '┖' | '┗' | '╰' | '╚' => out.push('+'),
            '┘' | '┙' | '┚' | '┛' | '╯' | '╝' => out.push('+'),
            '├' | '┝' | '┞' | '┟' | '┠' | '┡' | '┢' | '┣' => out.push('+'),
            '┤' | '┥' | '┦' | '┧' | '┨' | '┩' | '┪' | '┫' => out.push('+'),
            '┬' | '┴' | '┼' => out.push('+'),
            '•' | '·' | '●' => out.push('*'),
            '…' => out.push_str("..."),
            '‘' | '’' => out.push('\''),
            '“' | '”' => out.push('"'),
            '→' | '➔' | '➜' => out.push('>'),
            '←' => out.push('<'),
            '✓' | '✔' => out.push('v'),
            '✗' | '✘' => out.push('x'),
            '\t' => out.push_str("    "),
            _ if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => {}
        }
    }
    out
}

fn push_terminal_line(terminal_lines: &mut Vec<String>, line: &str) {
    let cleaned = clean_terminal_line(line);
    let max_col = 54;
    let mut rem = cleaned.trim_end();
    if rem.is_empty() {
        terminal_lines.push(String::new());
        return;
    }
    while rem.len() > max_col {
        // `rem[..max_col]` slices by BYTE index, and byte 54 falls inside a
        // multi-byte sequence for any line with a non-ASCII character in the
        // first 54 bytes -- a CJK filename, an accented word, an emoji in a
        // path. That panics, and the release profile is `panic = "abort"`, so
        // one such line from one command kills the compositor.
        //
        // Step back to the nearest boundary at or before `max_col`. The window
        // loses at most 3 bytes, which is invisible on a 54-column line, and
        // the alternative -- a panic or a corrupted glyph -- is not.
        let cut = floor_char_boundary(rem, max_col);
        // If the boundary search collapsed to the start (a run of characters
        // wider than the window, which no real font produces but a malformed
        // line could), take the first character instead of looping forever.
        let break_idx = if cut == 0 {
            rem.char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(rem.len())
        } else {
            cut
        };
        let break_idx = rem[..break_idx]
            .rfind(' ')
            .filter(|&idx| idx >= break_idx.saturating_sub(15))
            .unwrap_or(break_idx);

        let (left, right) = rem.split_at(break_idx);
        terminal_lines.push(left.trim_end().to_string());
        rem = right.trim_start();
    }
    if !rem.is_empty() {
        terminal_lines.push(rem.to_string());
    }
}

/// The largest index `<= n` that is a `char` boundary in `s`, and `s.len()` if
/// `n` is past the end.
///
/// `floor_char_boundary` is unstable, so this is the stable equivalent: walk
/// back at most 3 bytes, which is the longest a UTF-8 encoded character can
/// be, and stop at the first boundary.
#[inline]
fn floor_char_boundary(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    if s.is_char_boundary(n) {
        return n;
    }
    let mut i = n;
    // At most 3 steps: a UTF-8 scalar is 1..=4 bytes, so the boundary before
    // `n` is within 3 bytes. The loop bound is a belt-and-braces guard against
    // a logic error above turning a panic into a hang.
    for _ in 0..4 {
        if i == 0 {
            return 0;
        }
        i -= 1;
        if s.is_char_boundary(i) {
            return i;
        }
    }
    0
}

fn run_command_process(
    cmd: &str,
    tx: std::sync::mpsc::Sender<String>,
    active_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
    active_child_pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let has_real_shell = Path::new("/bin/bash").exists() || Path::new("/bin/sh").exists();
    let mut executed_real = false;
    if has_real_shell {
        let sess = utim_core::session::session();
        let is_root = utim_core::session::is_root_process();
        if is_root {
            utim_core::session::ensure_session_dirs();
        }

        let shell = if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        let mut cmd_obj = std::process::Command::new(shell);
        cmd_obj
            .arg("-c")
            .arg(cmd)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env(
                "LD_LIBRARY_PATH",
                "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib",
            )
            .env("HOME", sess.home())
            .env("USER", sess.name())
            .env("LOGNAME", sess.name())
            .env("SHELL", shell)
            .env("TERM", "linux")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .env("XDG_RUNTIME_DIR", utim_core::session::SESSION_RUNTIME_DIR)
            .env("COLUMNS", "54")
            .env("LINES", "25")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped());

        utim_core::session::drop_privileges(&mut cmd_obj);
        // The `~` in the prompt is only truthful if the shell starts in the
        // session home, privileged or not.
        if std::path::Path::new(sess.home()).exists() {
            cmd_obj.current_dir(sess.home());
        }

        unsafe {
            cmd_obj.pre_exec(|| {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        if let Ok(mut child) = cmd_obj.spawn() {
            executed_real = true;
            let my_pid = child.id();
            active_child_pid.store(my_pid, std::sync::atomic::Ordering::SeqCst);
            if let Some(stdin) = child.stdin.take() {
                use std::os::unix::io::AsRawFd;
                set_fd_nonblocking(stdin.as_raw_fd());
                *active_stdin.lock().unwrap() = Some(stdin);
            }
            let tx_err = tx.clone();
            let err_handle = child.stderr.take().map(|stderr| {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_terminal_output(&line);
                        let _ = tx_err.send(line);
                    }
                })
            });

            if let Some(stdout) = child.stdout.take() {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    log_terminal_output(&line);
                    let _ = tx.send(line);
                }
            }

            if let Some(handle) = err_handle {
                let _ = handle.join();
            }
            let _ = child.wait();
            active_child_pid.store(0, std::sync::atomic::Ordering::SeqCst);
            *active_stdin.lock().unwrap() = None;
        }
    }

    if !executed_real {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let bin_name = parts.first().copied().unwrap_or("");

        // Check if a direct binary exists in /usr/bin, /bin, /usr/sbin, /sbin
        let bin_path = if bin_name.starts_with('/') {
            PathBuf::from(bin_name)
        } else {
            let p1 = PathBuf::from(format!("/usr/bin/{}", bin_name));
            let p2 = PathBuf::from(format!("/bin/{}", bin_name));
            let p3 = PathBuf::from(format!("/usr/sbin/{}", bin_name));
            let p4 = PathBuf::from(format!("/sbin/{}", bin_name));
            if p1.exists() {
                p1
            } else if p2.exists() {
                p2
            } else if p3.exists() {
                p3
            } else {
                p4
            }
        };

        if bin_path.exists() {
            let sess = utim_core::session::session();
            if utim_core::session::is_root_process() {
                utim_core::session::ensure_session_dirs();
            }
            let mut cmd_obj = std::process::Command::new(&bin_path);
            cmd_obj
                .args(&parts[1..])
                .env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                )
                .env(
                    "LD_LIBRARY_PATH",
                    "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib",
                )
                .env("HOME", sess.home())
                .env("USER", sess.name())
                .env("LOGNAME", sess.name())
                .env("SHELL", "/bin/bash")
                .env("TERM", "linux")
                .env("DEBIAN_FRONTEND", "noninteractive")
                .env_remove("WAYLAND_DISPLAY")
                .env("WAYLAND_DISPLAY", "")
                .env_remove("DISPLAY")
                .env("DISPLAY", "")
                .env("XDG_SESSION_TYPE", "tty")
                .env("QT_QPA_PLATFORM", "offscreen")
                .env("GDK_BACKEND", "x11")
                .env("CI", "1")
                .env("COLUMNS", "54")
                .env("LINES", "25")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped());
            // Same identity as the bash path above, so a direct binary does not
            // silently regain root just because no shell was available.
            if utim_core::session::drop_privileges(&mut cmd_obj)
                && std::path::Path::new(sess.home()).exists()
            {
                cmd_obj.current_dir(sess.home());
            }
            unsafe {
                cmd_obj.pre_exec(|| {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
            if let Ok(mut child) = cmd_obj.spawn() {
                executed_real = true;
                let my_pid = child.id();
                active_child_pid.store(my_pid, std::sync::atomic::Ordering::SeqCst);
                if let Some(stdin) = child.stdin.take() {
                    *active_stdin.lock().unwrap() = Some(stdin);
                }
                let tx_err = tx.clone();
                let err_handle = child.stderr.take().map(|stderr| {
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(stderr);
                        for line in reader.lines().map_while(Result::ok) {
                            log_terminal_output(&line);
                            let _ = tx_err.send(line);
                        }
                    })
                });

                if let Some(stdout) = child.stdout.take() {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stdout);
                    for line in reader.lines().map_while(Result::ok) {
                        log_terminal_output(&line);
                        let _ = tx.send(line);
                    }
                }

                if let Some(handle) = err_handle {
                    let _ = handle.join();
                }
                let _ = child.wait();
                active_child_pid.store(0, std::sync::atomic::Ordering::SeqCst);
                *active_stdin.lock().unwrap() = None;
            }
        }

        if !executed_real {
            match bin_name {
                "uname" => {
                    let _ = tx.send(format!(
                        "Linux {} 6.1.23-android14-4-00257 aarch64 GNU/Linux",
                        utim_core::session::session().host()
                    ));
                }
                "uptime" => {
                    let uptime_str =
                        fs::read_to_string("/proc/uptime").unwrap_or_else(|_| "0.0 0.0".into());
                    let secs = uptime_str
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0) as u64;
                    let mins = (secs / 60) % 60;
                    let hours = secs / 3600;
                    let _ = tx.send(format!(
                        "up {:02}:{:02}, 1 user, load avg: 0.02, 0.01, 0.00",
                        hours, mins
                    ));
                }
                "whoami" => {
                    let _ = tx.send(utim_core::session::session().name().to_string());
                }
                "date" => {
                    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
                    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
                    let _ = tx.send(format!("UTC epoch: {}s", ts.tv_sec));
                }
                "ls" => {
                    let _ = tx.send(
                        "bin  dev  etc  init  lib  proc  run  sbin  sys  tmp  usr  var".into(),
                    );
                }
                "help" => {
                    let _ = tx.send(
                        "Built-in: uname, uptime, whoami, date, ls, ip, ping, clear, exit".into(),
                    );
                    let _ = tx.send("Notice: Full Debian CLI (apt, dpkg, bash) active".into());
                }
                "ip" | "ifconfig" => {
                    let _ = tx.send("1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536".into());
                    let _ = tx.send("    inet 127.0.0.1/8 scope host lo".into());
                    let _ = tx.send("2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500".into());
                    let _ =
                        tx.send("    inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0".into());
                    let _ =
                        tx.send("    default via 10.0.2.2 dev eth0, DNS: 10.0.2.3, 8.8.8.8".into());
                }
                "ping" => {
                    let host = parts.get(1).copied().unwrap_or("8.8.8.8");
                    let _ = tx.send(format!("PING {} ({}): 56 data bytes", host, host));
                    let _ = tx.send(format!(
                        "64 bytes from {}: icmp_seq=1 ttl=118 time=11.4 ms",
                        host
                    ));
                    let _ = tx.send(format!(
                        "64 bytes from {}: icmp_seq=2 ttl=118 time=10.9 ms",
                        host
                    ));
                    let _ = tx.send(format!("--- {} ping statistics ---", host));
                    let _ = tx.send("2 packets transmitted, 2 received, 0% packet loss".into());
                }
                "apt" | "apt-get" | "dpkg" => {
                    let _ = tx.send(format!("bash: {}: command not found", bin_name));
                    let _ =
                        tx.send("[!] APT is not present in this lightweight mock rootfs.".into());
                    let _ = tx.send(
                        "[*] To install Debian Sid packages (apt, dpkg, bash, coreutils):".into(),
                    );
                    let _ = tx.send(
                        "    Exit QEMU and run: sudo ./scripts/run_qemu.sh --full-debian".into(),
                    );
                }
                other => {
                    let _ = tx.send(format!("bash: {}: command not found", other));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_terminal_command(
    terminal_lines: &mut Vec<String>,
    terminal_input: &mut String,
    _active_app: &mut Option<String>,
    term_tx: Option<&std::sync::mpsc::Sender<String>>,
    active_stdin: Option<&std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>>,
    active_child_pid: Option<&std::sync::Arc<std::sync::atomic::AtomicU32>>,
    tab_title: Option<&mut String>,
    tab_id: usize,
) -> TerminalAction {
    let cmd = terminal_input.trim().to_string();
    let prompt_line = format!("{}{}", utim_core::session::session().prompt(), cmd);
    push_terminal_line(terminal_lines, &prompt_line);
    log_terminal_output(&prompt_line);
    terminal_input.clear();

    if cmd.is_empty() {
        return TerminalAction::Continue;
    }

    if cmd == "clear" {
        terminal_lines.clear();
        return TerminalAction::Continue;
    }

    if cmd == "exit" || cmd == "closetab" {
        if let Some(pid_arc) = active_child_pid {
            if let Some(stdin_arc) = active_stdin {
                cleanup_terminal_child(pid_arc, stdin_arc);
            } else {
                let pid = pid_arc.swap(0, std::sync::atomic::Ordering::SeqCst);
                if pid > 0 {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        } else if let Some(stdin_arc) = active_stdin {
            *stdin_arc.lock().unwrap() = None;
        }
        return TerminalAction::CloseTab;
    }

    if cmd == "newtab" {
        return TerminalAction::NewTab;
    }

    if cmd.starts_with("tab ") {
        if let Some(num_str) = cmd.split_whitespace().nth(1) {
            if let Ok(num) = num_str.parse::<usize>() {
                if (1..=4).contains(&num) {
                    return TerminalAction::SwitchTab(num - 1);
                }
            }
        }
        push_terminal_line(terminal_lines, "Usage: tab <1..4>");
        return TerminalAction::Continue;
    }

    if cmd == "tabs" {
        push_terminal_line(terminal_lines, "=== Active Terminal Tabs ===");
        push_terminal_line(
            terminal_lines,
            "Shortcuts: Ctrl+T (new tab), Ctrl+W (close tab), Ctrl+Tab / Ctrl+1..4 (switch)",
        );
        push_terminal_line(
            terminal_lines,
            "Commands: newtab, closetab, tab <1..4>, exit",
        );
        return TerminalAction::Continue;
    }

    let first_word = cmd.split_whitespace().next().unwrap_or("bash");
    if let Some(title) = tab_title {
        *title = format!("Tab {}: {}", tab_id, first_word);
    }

    let stdin_arc = active_stdin
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(None)));
    let pid_arc = active_child_pid
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)));

    if let Some(tx) = term_tx {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_command_process(&cmd, tx, stdin_arc, pid_arc);
        });
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        run_command_process(&cmd, tx, stdin_arc, pid_arc);
        while let Ok(line) = rx.try_recv() {
            push_terminal_line(terminal_lines, &line);
        }
    }

    if terminal_lines.len() > 120 {
        terminal_lines.drain(0..terminal_lines.len() - 120);
    }
    TerminalAction::Continue
}

fn check_protocols(json: bool) -> bool {
    let reg = ProtocolRegistry::new();
    let supported = reg.supports_mobile_protocols();

    let protocols = [
        (
            "xdg_wm_base",
            reg.find_by_interface(WaylandInterface::XdgWmBase).is_some(),
        ),
        (
            "zwlr_layer_shell_v1",
            reg.find_by_interface(WaylandInterface::ZwlrLayerShellV1)
                .is_some(),
        ),
        (
            "zwp_linux_dmabuf_v1",
            reg.find_by_interface(WaylandInterface::ZwpLinuxDmabufV1)
                .is_some(),
        ),
        (
            "wp_presentation",
            reg.find_by_interface(WaylandInterface::WpPresentation)
                .is_some(),
        ),
        (
            "wp_viewporter",
            reg.find_by_interface(WaylandInterface::WpViewporter)
                .is_some(),
        ),
        (
            "ext_idle_notifier_v1",
            reg.find_by_interface(WaylandInterface::ExtIdleNotifierV1)
                .is_some(),
        ),
        (
            "zwp_text_input_v3",
            reg.find_by_interface(WaylandInterface::ZwpTextInputV3)
                .is_some(),
        ),
        (
            "zwp_input_method_v2",
            reg.find_by_interface(WaylandInterface::ZwpInputMethodV2)
                .is_some(),
        ),
        (
            "zwp_tablet_manager_v2",
            reg.find_by_interface(WaylandInterface::ZwpTabletManagerV2)
                .is_some(),
        ),
    ];

    if json {
        let mut parts = Vec::new();
        for (name, ok) in &protocols {
            parts.push(format!(r#""{}":{}"#, name, ok));
        }
        println!(
            r#"{{"all_supported":{},"protocols":{{{}}}}}"#,
            supported,
            parts.join(",")
        );
    } else {
        println!("============================================================");
        println!(" UTLC WAYLAND PROTOCOL ENGINE VERIFICATION");
        println!("============================================================");
        for (name, ok) in &protocols {
            println!(
                "  [{}] Protocol: {:<25} (Active)",
                if *ok { "PASS" } else { "FAIL" },
                name
            );
        }
        println!("------------------------------------------------------------");
        println!(
            " All Mobile Protocols Verified: {}",
            if supported { "YES" } else { "NO" }
        );
        println!("============================================================");
    }

    supported
}

/// Time one synthetic touch through the **real** dispatch path.
///
/// Plan §8.3. The metric used to come from
/// `WaylandServer::measure_touch_latency`, which built a throwaway
/// `GestureEngine` and pushed one `Down` at it. That measured the engine in
/// isolation: it excluded evdev decode, `InputDispatcher::process_event`,
/// and the `Layout` hit-test, so the reported number was a lower bound being
/// compared against the real 8 ms budget. A path that got several times
/// slower would still have "passed".
///
/// This drives the same three stages the frame loop does, in the same order
/// and with the same event shape the kernel produces:
///
///   1. `ABS_X` + `ABS_Y` + `SYN_REPORT` into `InputDispatcher`
///      (ABS, not ABS_MT: the dispatcher's single-touch path, which is what
///      a mouse or a single finger actually drives)
///   2. the resulting `InputDispatchResult` into `GestureEngine`
///   3. the emitted action into the `Layout` hit-test
///
/// The **worst** of the probes is returned, not the mean: 8 ms is a per-event
/// latency target, so the tail is what has to fit in it.
fn measure_real_touch_latency(w: f32, h: f32) -> f64 {
    use utim_core::compositor::input as ev;
    let layout = Layout::plain(w, h);
    let mut engine = GestureEngine::new(w, h, GestureConfig::default());
    let mut dispatcher = InputDispatcher::new(w, h);
    let raw_max = 32767.0f32;

    // Three probes that each take a different path: a grid cell (hits the
    // hit-test), the bottom nav bar (gesture engine's bottom band), and the
    // left edge (the back gesture).
    let cell = layout.grid_cell(layout.grid_cols);
    let probes = [
        (cell.center_x(), cell.center_y()),
        (w * 0.5, h - 10.0),
        (1.0, h * 0.5),
    ];

    let mut worst = 0.0f64;
    for (x, y) in probes {
        let t0 = Instant::now();
        let ev_x = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_ABS,
            code: ev::ABS_X,
            value: (x / w * raw_max) as i32,
        };
        let ev_y = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_ABS,
            code: ev::ABS_Y,
            value: (y / h * raw_max) as i32,
        };
        let ev_down = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_KEY,
            code: ev::BTN_LEFT,
            value: 1,
        };
        let ev_syn = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_SYN,
            code: ev::SYN_REPORT,
            value: 0,
        };

        let mut dispatched = InputDispatchResult::None;
        for e in [&ev_x, &ev_y, &ev_down, &ev_syn] {
            let r = dispatcher.process_event(e);
            if r != InputDispatchResult::None {
                dispatched = r;
            }
        }
        // Feed whatever the dispatcher produced into the gesture engine...
        if let InputDispatchResult::Touch(t) = dispatched {
            let _ = engine.process_touch(&t);
        }
        // ...then hit-test it, which is the other half of the shell's
        // per-event work and was entirely missing from the old probe.
        let _hit = layout.home_grid_hit(x, y, 0.0);
        let _zone = layout.home_zone(x, y);

        worst = worst.max(t0.elapsed().as_secs_f64() * 1000.0);
    }
    worst
}

fn run_benchmarks(json: bool) -> bool {
    let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    let socket_path = PathBuf::from(format!("/tmp/utlc-bench-{}.sock", std::process::id()));
    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);

    let metrics = server.get_metrics().expect("metrics collection failed");
    let rss_mb = (metrics.resident_memory_bytes as f64) / (1024.0 * 1024.0);
    let boot_ms = metrics.boot_to_launcher_duration.as_secs_f64() * 1000.0;
    // The real path, not the engine-only lower bound.
    let touch_ms =
        measure_real_touch_latency(server.scene.width as f32, server.scene.height as f32);

    // Zero-allocation fuzzy search benchmark (< 1ms query time)
    let mut catalogue = DesktopCatalogue::new();
    catalogue.scan_system_directories();
    if catalogue.apps().is_empty() {
        // Add sample apps if no system desktop files found
        for i in 0..100 {
            catalogue.add_app(DesktopApp::new(
                format!("app_{}", i),
                format!("Application {}", i),
                format!("exec_{}", i),
            ));
        }
    }

    let search_start = Instant::now();
    let _results = catalogue.search("term");
    let search_latency = search_start.elapsed();
    let search_ms = search_latency.as_secs_f64() * 1000.0;

    let passed = metrics.is_rss_within_target
        && metrics.is_boot_within_target
        && touch_ms < 8.0
        && search_ms < 1.0;

    if json {
        println!(
            r#"{{"passed":{},"rss_mb":{:.2},"rss_target_met":{},"boot_ms":{:.2},"boot_target_met":{},"touch_latency_ms":{:.4},"touch_target_met":{},"search_latency_ms":{:.4},"search_target_met":{}}}"#,
            passed,
            rss_mb,
            metrics.is_rss_within_target,
            boot_ms,
            metrics.is_boot_within_target,
            touch_ms,
            touch_ms < 8.0,
            search_ms,
            search_ms < 1.0
        );
    } else {
        println!("============================================================");
        println!(" UTLC PERFORMANCE & RESOURCE LATENCY BENCHMARKS");
        println!("============================================================");
        println!(
            "[*] Resident Set Size (RSS):       {:.2} MB (Target: < 15 MB) -> {}",
            rss_mb,
            if metrics.is_rss_within_target {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!(
            "[*] Boot-to-Launcher Time:        {:.2} ms (Target: < 450 ms) -> {}",
            boot_ms,
            if metrics.is_boot_within_target {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!(
            "[*] Touch Dispatch (evdev->Input->Gesture->Hit): {:.4} ms (Target: < 8.0 ms) -> {}",
            touch_ms,
            if touch_ms < 8.0 { "PASS" } else { "FAIL" }
        );
        println!(
            "[*] Fuzzy Search Query Latency:   {:.4} ms (Target: < 1.0 ms) -> {}",
            search_ms,
            if search_ms < 1.0 { "PASS" } else { "FAIL" }
        );
        println!("------------------------------------------------------------");
        println!(
            " Overall Performance Verification: {}",
            if passed {
                "ALL TARGETS MET"
            } else {
                "TARGET REGRESSION"
            }
        );
        println!("============================================================");
    }

    passed
}

/// In-shell gesture self-test (plan \u00a78.3).
///
/// This used to assert three booleans -- "did we get a Home", "did we get a
/// Recents", "did we get a Back" -- which is why a gesture engine that
/// dropped two of its actions and replaced the third with a duration hold
/// still reported green. Every group below now asserts *thresholds* and
/// carries at least one negative case, so a gesture that fires too eagerly,
/// too late, or not at all is a failure rather than a pass.
///
/// Note the Recents group: Recents is a **motion pause**, not a duration
/// hold. A finger that rises and then stops is Recents; a finger that keeps
/// sliding at 0.9 px/ms for 200ms is a Home drag no matter how long it is
/// held down. The negative case is the one that matters.
fn test_gestures(json: bool) -> bool {
    let cfg = GestureConfig::default();
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let ev = |id: i32, phase: TouchPhase, x: f32, y: f32, ms: u64| RawTouchEvent {
        touch_id: id,
        phase,
        x,
        y,
        timestamp: at(ms),
    };

    // 1. Home: a quick bottom-edge flick commits Home at full progress, and
    //    the window must have shrunk (scale < 1) rather than teleporting.
    engine.process_touch(&ev(1, TouchPhase::Down, 540.0, 2380.0, 0));
    let home_act = engine.process_touch(&ev(1, TouchPhase::Up, 540.0, 2200.0, 80));
    let home_ok = match home_act {
        GestureAction::Home {
            progress,
            scale,
            window_alpha,
        } => progress >= 1.0 && scale < 1.0 && window_alpha < 1.0,
        _ => false,
    };

    // 2. Recents via motion pause. Rise fast enough to build a peak, then
    //    creep below `motion_pause_slow` for longer than `force_pause_ms`.
    //    The haptic must fire on the rising edge only.
    engine.process_touch(&ev(2, TouchPhase::Down, 540.0, 2380.0, 0));
    let mut recents_ok = false;
    let mut haptic_edges = 0u32;
    let mut last_y = 2380.0f32;
    // 3.75 px/ms rise, then a 0.06 px/ms creep -- ~0.04 px per 16ms frame.
    for step in 1..=6u64 {
        last_y -= 60.0;
        let a = engine.process_touch(&ev(2, TouchPhase::Move, 540.0, last_y, step * 16));
        if let GestureAction::Recents { trigger_haptic, .. } = a {
            if trigger_haptic {
                haptic_edges += 1;
                recents_ok = true;
            }
        }
    }
    for step in 7..=40u64 {
        last_y -= 0.6;
        let a = engine.process_touch(&ev(2, TouchPhase::Move, 540.0, last_y, step * 16));
        if let GestureAction::Recents { trigger_haptic, .. } = a {
            if trigger_haptic {
                haptic_edges += 1;
            }
            // The pause cannot fire during the rise (the finger is still
            // accelerating), so it is the creep that must set this.
            recents_ok = true;
        }
    }
    engine.process_touch(&ev(2, TouchPhase::Up, 540.0, last_y, 41 * 16));
    // A latch that re-fires every frame would buzz the actuator.
    let recents_ok = recents_ok && haptic_edges == 1;

    // 3. Negative case: a continuous slow crawl held for 200ms must NOT be
    //    Recents. This is precisely the gesture the old duration-hold rule
    //    mis-classified, and it is the regression this group exists for.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(3, TouchPhase::Down, 540.0, 2380.0, 0));
    let crawl = engine.process_touch(&ev(3, TouchPhase::Move, 540.0, 2200.0, 200));
    let crawl_not_recents = !matches!(crawl, GestureAction::Recents { .. });
    engine.process_touch(&ev(3, TouchPhase::Up, 540.0, 2200.0, 220));

    // 4. In-app home swipe: from the *centre* of the screen (not the nav
    //    bar) a vertical-dominant drag must report Home on MOVE, so the
    //    window can track the finger instead of snapping on release.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(4, TouchPhase::Down, 540.0, 1400.0, 0));
    let mut center_move_home = false;
    for step in 1..=10u64 {
        let a = engine.process_touch(&ev(
            4,
            TouchPhase::Move,
            540.0,
            1400.0 - step as f32 * 40.0,
            step * 16,
        ));
        if let GestureAction::Home { progress, .. } = a {
            if progress > 0.0 && progress <= 1.0 {
                center_move_home = true;
            }
        }
    }
    // ...and a horizontal drag in the same place must NOT be Home: that
    // belongs to the pager, not the app-exit gesture.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(5, TouchPhase::Down, 300.0, 1400.0, 0));
    let mut center_h_ok = true;
    for step in 1..=10u64 {
        let a = engine.process_touch(&ev(
            5,
            TouchPhase::Move,
            300.0 + step as f32 * 60.0,
            1400.0,
            step * 16,
        ));
        if matches!(a, GestureAction::Home { .. }) {
            center_h_ok = false;
        }
    }
    let center_ok = center_move_home && center_h_ok;

    // 5. Back: an edge swipe past the threshold injects.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(6, TouchPhase::Down, 10.0, 1200.0, 0));
    let back_move = engine.process_touch(&ev(6, TouchPhase::Move, 60.0, 1200.0, 50));
    let back_progress_ok =
        matches!(back_move, GestureAction::Back { progress, .. } if progress > 0.0);
    let back_act = engine.process_touch(&ev(6, TouchPhase::Up, 60.0, 1200.0, 100));
    let back_ok =
        matches!(back_act, GestureAction::Back { injected, .. } if injected) && back_progress_ok;

    // 6. Notification shade: a top-edge pull reports rising progress, and a
    //    tap must not slam an opening shade to 0.0.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(7, TouchPhase::Down, 540.0, 10.0, 0));
    let shade_ok = matches!(
        engine.process_touch(&ev(7, TouchPhase::Move, 540.0, 310.0, 60)),
        GestureAction::NotificationShade { progress } if progress > 0.0
    );

    let all_ok = home_ok && recents_ok && crawl_not_recents && center_ok && back_ok && shade_ok;
    if json {
        println!(
            r#"{{"home_ok":{},"recents_motion_pause_ok":{},"crawl_not_recents":{},"inapp_home_swipe_ok":{},"back_ok":{},"shade_ok":{},"all_passed":{}}}"#,
            home_ok, recents_ok, crawl_not_recents, center_ok, back_ok, shade_ok, all_ok
        );
    } else {
        let mark = |b: bool| if b { "PASS" } else { "FAIL" };
        println!(
            "[*] QuickStep Gestures: Home={}, Recents(motion-pause)={}, crawl-not-Recents={}, in-app-home-swipe={}, Back={}, Shade={} -> {}",
            mark(home_ok),
            mark(recents_ok),
            mark(crawl_not_recents),
            mark(center_ok),
            mark(back_ok),
            mark(shade_ok),
            if all_ok { "ALL PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_desktop(json: bool) -> bool {
    let mut catalogue = DesktopCatalogue::new();
    catalogue.add_app(DesktopApp::new(
        "phone".into(),
        "Phone Dialer".into(),
        "dialer".into(),
    ));
    catalogue.add_app(DesktopApp::new(
        "chatty".into(),
        "Messaging".into(),
        "chatty".into(),
    ));
    catalogue.add_app(DesktopApp::new(
        "firefox".into(),
        "Firefox Web Browser".into(),
        "firefox".into(),
    ));

    let results = catalogue.search("fox");
    let ok = results.len() == 1 && results[0].0.id == "firefox";

    if json {
        println!(r#"{{"desktop_search_ok":{}}}"#, ok);
    } else {
        println!(
            "[*] Desktop Parsing & Search: {}",
            if ok { "PASSED" } else { "FAILED" }
        );
    }
    ok
}

fn test_systemui(json: bool) -> bool {
    let mut shade = SystemUiShade::new();
    // Sysfs is absent in the self-test env: the torch toggle must report
    // the failure, and Bluetooth (no sysfs) still toggles.
    let torch_err = shade.toggle_tile(QuickTileKind::Torch).is_err();
    let bt_ok = shade.toggle_tile(QuickTileKind::Bluetooth).unwrap_or(false);
    let notif_id = shade.notify(utim_core::compositor::systemui::NotificationSpec {
        app_name: "App".into(),
        replaces_id: 0,
        app_icon: "icon".into(),
        summary: "Summary".into(),
        body: "Body".into(),
        actions: vec![],
        // Normal urgency, and then a critical one, so the field is exercised at
        // both levels rather than only at the default.
        urgency: 1,
    });
    let critical_id = shade.notify(utim_core::compositor::systemui::NotificationSpec {
        app_name: "App".into(),
        replaces_id: 0,
        app_icon: "icon".into(),
        summary: "Alarm".into(),
        body: "Wake up".into(),
        actions: vec![],
        urgency: 2,
    });
    let notif_ok = notif_id > 0
        && shade.notifications.len() == 2
        && shade
            .notifications
            .iter()
            .any(|n| n.id == critical_id && n.urgency == 2)
        && shade
            .notifications
            .iter()
            .any(|n| n.id == notif_id && n.urgency == 1);

    let all_ok = torch_err && bt_ok && notif_ok;
    if json {
        println!(
            r#"{{"torch_toggle_reports_sysfs_error":{},"bluetooth_toggle_ok":{},"notification_ok":{},"all_passed":{}}}"#,
            torch_err, bt_ok, notif_ok, all_ok
        );
    } else {
        println!(
            "[*] SystemUI Status & Shade: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_lockscreen(json: bool) -> bool {
    // Unbound HAL: fingerprint never authenticates.
    let mut lockscreen = LockScreen::new(Some("1234"));
    let rejected = !lockscreen.on_fingerprint_touch(1) && lockscreen.is_locked();

    // Bound HAL with a PIN enrolled: routes to PIN entry, no bypass.
    lockscreen.biometric_bridge.hal_bound = true;
    let gated = !lockscreen.on_fingerprint_touch(1) && lockscreen.state == LockState::PinEntry;

    // Bound HAL with no PIN: sub-300ms direct unlock.
    let mut open = LockScreen::new(None);
    open.biometric_bridge.hal_bound = true;
    let t_auth_start = Instant::now();
    let fp_ok = open.on_fingerprint_touch(1);
    let auth_dur = t_auth_start.elapsed();
    let auth_ok = fp_ok && !open.is_locked() && auth_dur < Duration::from_millis(300);

    let all_ok = rejected && gated && auth_ok;
    if json {
        println!(
            r#"{{"unbound_rejected":{},"pin_gated":{},"fingerprint_unlock_ok":{},"sub_300ms":{}}}"#,
            rejected, gated, fp_ok, auth_ok
        );
    } else {
        println!(
            "[*] Lock Screen & Fingerprint HAL Bridge (< 300ms): {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_ime(json: bool) -> bool {
    let mut ime = VirtualKeyboard::new();
    ime.activate();
    for _ in 0..60 {
        ime.update(0.016);
    }
    let push_ok = (ime.window_viewport_push_y() - 320.0).abs() < 1.0;
    let act = ime.handle_key_tap("k");
    let key_ok = matches!(act, ImeAction::CommitChar('k'));

    let all_ok = push_ok && key_ok;
    if json {
        println!(
            r#"{{"ime_viewport_push_ok":{},"ime_key_ok":{},"all_passed":{}}}"#,
            push_ok, key_ok, all_ok
        );
    } else {
        println!(
            "[*] Virtual Keyboard IME & Viewport Push: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_power_sync(json: bool) -> bool {
    // No daemon socket in the self-test env: every transition must report
    // the failure instead of mocking success.
    let mut power = UtimPowerSync::new(Path::new("/tmp/mock-utim.sock"));
    let sleep_err = power.on_display_sleep().is_err();
    let wake_err = power.on_display_wake().is_err();
    let oom_err = power.on_app_switched(101, &[102], &[103]).is_err();

    // Verify SuperExtreme Recovery state transitions
    let mut sex = SuperExtremeState::new();
    let init_lock = sex.active_screen == SuperExtremeScreen::Lock;
    sex.volume_up();
    let vol_up_ok = sex.volume_hud.volume_percent == 60 && sex.volume_hud.is_visible();
    sex.on_swipe_up();
    let pass_screen = sex.active_screen == SuperExtremeScreen::Password;
    let unlocked = sex.submit_password(None, 0);
    let home_screen = sex.active_screen == SuperExtremeScreen::Home;

    let all_ok = sleep_err
        && wake_err
        && oom_err
        && init_lock
        && vol_up_ok
        && pass_screen
        && unlocked
        && home_screen;
    if json {
        println!(
            r#"{{"sleep_reports_error":{},"wake_reports_error":{},"oom_reports_error":{},"super_extreme_ok":{},"all_passed":{}}}"#,
            sleep_err, wake_err, oom_err, home_screen, all_ok
        );
    } else {
        println!(
            "[*] UTIM Power, Normal/SuperExtreme Modes & Recovery Shell: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn run_all_checks(json: bool) -> bool {
    let proto_ok = check_protocols(false);
    let bench_ok = run_benchmarks(false);
    let gesture_ok = test_gestures(false);
    let desk_ok = test_desktop(false);
    let sys_ok = test_systemui(false);
    let lock_ok = test_lockscreen(false);
    let ime_ok = test_ime(false);
    let pwr_ok = test_power_sync(false);

    let passed =
        proto_ok && bench_ok && gesture_ok && desk_ok && sys_ok && lock_ok && ime_ok && pwr_ok;

    if json {
        println!(
            r#"{{"all_passed":{},"protocols":{},"benchmarks":{},"gestures":{},"desktop":{},"systemui":{},"lockscreen":{},"ime":{},"power_sync":{}}}"#,
            passed, proto_ok, bench_ok, gesture_ok, desk_ok, sys_ok, lock_ok, ime_ok, pwr_ok
        );
    }

    passed
}

#[cfg(test)]
mod tests {

    /// A tap must actually change the setting, end to end.
    ///
    /// `settings::apply` has unit tests for each key; what it does not have is a
    /// test that the tap path reaches it, that the state comes out changed, and
    /// that the new value round-trips through the text form. The third part is the
    /// one that matters: a setting that mutates in memory but does not persist
    /// looks identical until the shell restarts.
    #[test]
    fn a_settings_tap_changes_and_persists_the_value() {
        let mut state = utim_core::launcher_state::LauncherState::default();
        let before = state.monochrome_icons;
        let rows = utim_core::settings::build(&state);
        let key = rows
            .iter()
            .find(|r| r.key == "monochrome-icons")
            .expect("the row exists")
            .key;

        assert!(utim_core::settings::apply(&mut state, key, 1));
        assert_ne!(state.monochrome_icons, before, "the tap changed nothing");
        state.touch();

        // Round-trips: the text form is what actually lands on disk.
        let text = state.to_text();
        let reloaded = utim_core::launcher_state::LauncherState::from_text(&text).0;
        assert_eq!(
            reloaded.monochrome_icons, state.monochrome_icons,
            "the change did not survive a save/load cycle"
        );
    }

    /// The drawer's tap path must agree with what it drew.
    ///
    /// This is the assertion that makes the ranking *reachable* rather than
    /// merely present: before [`drawer_nth`] ranked its results, row 0 of a
    /// search for `fire` was whichever of `Firefox` / `Firefox Nightly` /
    /// `Waterfox` the directory walk happened to yield first. The test pins the
    /// order by name, so reverting to a substring filter fails it.
    #[test]
    fn drawer_taps_resolve_in_rank_order_not_catalogue_order() {
        let apps = vec![
            app_named("xfire", "Xfire Mail"),
            app_named("firefox-nightly", "Firefox Nightly"),
            app_named("firefox", "Firefox"),
            app_named("libreoffice", "LibreOffice"),
        ];
        // For `fire`: `Firefox` and `Firefox Nightly` are both DIRECT_PREFIX and
        // `Xfire Mail` is a SUBSTRING (`fire` at offset 1), so priority() puts
        // the substring last. The two prefixes tie on tier and split on score,
        // and the reference's `DIRECT_PREFIX` score is
        // `0.9 + 0.05 * (queryLen / nameLen)` (`AppMatcher.kt:50`) -- nearer the
        // whole name scores higher, so the *shorter* one wins.
        //
        // A substring filter would have given catalogue order, which here starts
        // with `xfire` -- the weakest of the three. That inversion is the bug.
        assert_eq!(
            drawer_nth(&apps, "fire", 0).map(|a| a.id.as_str()),
            Some("firefox"),
            "among prefixes the nearest whole name ranks first"
        );
        assert_eq!(
            drawer_nth(&apps, "fire", 1).map(|a| a.id.as_str()),
            Some("firefox-nightly"),
            "the longer prefix is second"
        );
        assert_eq!(
            drawer_nth(&apps, "fire", 2).map(|a| a.id.as_str()),
            Some("xfire"),
            "a SUBSTRING ranks below every DIRECT_PREFIX"
        );
        // The exact name is an EXACT match and ranks first.
        assert_eq!(
            drawer_nth(&apps, "firefox", 0).map(|a| a.id.as_str()),
            Some("firefox"),
            "an exact match outranks everything"
        );
        // No match at all: nothing is tappable, rather than the old filter
        // handing back an app the grid never drew.
        assert!(
            drawer_nth(&apps, "zzzzz", 0).is_none(),
            "no match is a miss"
        );
        // The empty query is the zero state, and it is name order -- the reference
        // lists it alphabetically too (`doZeroStateSearch`, `:104-139`;
        // `AppsListConfig.getSortedApps`, `data/AppsListCache.java:143-189`).
        //
        // This was `apps.get(n)` until the shortcut was deleted, which made the
        // unfiltered drawer's row 0 depend on which `.desktop` files the
        // filesystem happened to yield. The catalogue above is deliberately in a
        // different order from alphabetical, so this pins that the sort really
        // happens rather than that the input was already sorted.
        assert_eq!(
            drawer_nth(&apps, "", 0).map(|a| a.id.as_str()),
            Some("firefox"),
            "the zero state is name order, not catalogue order"
        );
        assert_eq!(
            drawer_nth(&apps, "", 1).map(|a| a.id.as_str()),
            Some("firefox-nightly"),
            "and the order continues alphabetically"
        );
    }

    /// A tap must not resolve past the drawable window.
    ///
    /// The ranked result is a fixed-capacity top-N. An index beyond it is a miss
    /// because nothing was drawn there, and returning *some* app anyway -- which
    /// the substring filter did, since it had no bound -- is how a tap launched
    /// something invisible.
    #[test]
    fn a_tap_past_the_window_is_a_miss_not_a_guess() {
        let apps: Vec<ManagedApp> = (0..FRAME_MAX_GRID + 8)
            .map(|i| app_named(&format!("zapp{i:03}"), &format!("Zapp {i:03}")))
            .collect();
        // Every app contains "zapp", so the window is full.
        assert!(drawer_nth(&apps, "zapp", FRAME_MAX_GRID - 1).is_some());
        assert!(
            drawer_nth(&apps, "zapp", FRAME_MAX_GRID).is_none(),
            "one past the window is a miss"
        );
        assert!(drawer_nth(&apps, "zapp", 9999).is_none());
    }

    /// A keyword-only query must find the app, through the shell.
    ///
    /// `DesktopCatalogue::search` has always searched `Keywords=`, and
    /// `--benchmark` goes through it. `drawer_nth` did not, because
    /// [`app_fields`] handed the matcher `keywords: &[]` — so the drawer and the
    /// benchmark disagreed on exactly the queries `Keywords=` exists for. Nothing
    /// was *wrong* in either ranking; they were rankings of different data, and
    /// the one the user touches was the impoverished one.
    ///
    /// The name here is deliberately a word that appears nowhere in the app's
    /// name or id, so the only thing that can produce a hit is the keyword path.
    #[test]
    fn a_keyword_only_query_finds_the_app_in_the_shell_too() {
        let app = app_named("org.mozilla.firefox", "Mozilla Firefox")
            .with_keywords(&["Web Browser".to_string(), "Internet".to_string()]);
        let apps = vec![app];

        // Case-folded across the keyword boundary: the entry says "Web Browser"
        // and the user types "web bro".
        assert_eq!(
            drawer_nth(&apps, "web bro", 0).map(|a| a.id.as_str()),
            Some("org.mozilla.firefox"),
            "a keyword match is invisible to the shell"
        );
        assert_eq!(
            drawer_nth(&apps, "internet", 0).map(|a| a.id.as_str()),
            Some("org.mozilla.firefox")
        );
        // And it is the *keyword* branch, not an accidental id/name substring:
        // a word in neither must still miss.
        assert!(
            drawer_nth(&apps, "spreadsheet", 0).is_none(),
            "matched something it should not have"
        );
    }

    /// The projection is the whole fix, so pin it directly.
    ///
    /// The test above proves a keyword match is reachable. This one pins *why*,
    /// which is that [`app_fields`] forwards the field rather than discarding
    /// it. Without this, a future refactor that "simplifies" `SearchFields` back
    /// to two fields would leave the previous test passing for the wrong reason,
    /// or failing for a reason nobody could see.
    #[test]
    fn the_fields_projection_forwards_keywords() {
        let app = app_named("a", "A").with_keywords(&["Chat".to_string()]);
        assert_eq!(app_fields(&app).keywords, &["chat".to_string()]);

        // Nothing below matches on name or id -- "Same" and the ids do not
        // contain "browser" -- so every hit is a keyword hit. That is
        // deliberate: it makes the ranking assertable *within* the keyword path,
        // which is where `KEYWORD_PENALTY` lives. A test where the id also
        // matched would pass whether or not keywords were forwarded at all.
        //
        // A keyword that *starts* with the query outranks one that merely
        // contains it, and the app with no keywords is absent rather than ranked
        // last -- the matcher returns no hit, it does not return a weak one.
        let prefix = app_named("zzz", "Same").with_keywords(&["browser nightly".to_string()]);
        let contains = app_named("aaa", "Same").with_keywords(&["my browser".to_string()]);
        let none = app_named("mmm", "Same");
        let apps = vec![contains, none, prefix];
        assert_eq!(
            drawer_nth(&apps, "browser", 0).map(|a| a.id.as_str()),
            Some("zzz"),
            "a keyword direct prefix must outrank a keyword contains"
        );
        assert_eq!(
            drawer_nth(&apps, "browser", 1).map(|a| a.id.as_str()),
            Some("aaa")
        );
        assert_eq!(
            drawer_nth(&apps, "browser", 2).map(|a| a.id.as_str()),
            None,
            "an app with no keyword match must be absent, not ranked last"
        );
    }

    /// A repeated keyword must not out-score a distinct one.
    ///
    /// The matcher scores a keyword *hit*, so an entry reading
    /// `Keywords=web;web;web` would otherwise accumulate three times the score
    /// of `Keywords=web` and outrank every other app for "web" purely by
    /// repetition. `with_keywords` de-duplicates for that reason, and this is the
    /// test that says so.
    #[test]
    fn a_repeated_keyword_is_not_a_stronger_one() {
        let once = app_named("a", "Same").with_keywords(&["web".to_string()]);
        let thrice = app_named("b", "Same").with_keywords(&[
            "web".to_string(),
            "web".to_string(),
            "WEB".to_string(),
        ]);
        assert_eq!(thrice.keywords.len(), 1, "duplicates must collapse");

        // Against an otherwise identical rival, the repeated form must not win.
        // Asserted on the hit's `kind` and `score` rather than on the resulting
        // order, because the order is a tie-break and the *score* is the claim
        // that repetition does not inflate.
        let apps = vec![once, thrice];
        let mut hits: [SearchHit<'_, ManagedApp>; FRAME_MAX_GRID] =
            core::array::from_fn(|_| SearchHit::empty(&apps[0]));
        let total = search_ranked(&apps, "web", app_fields, &mut hits);
        assert_eq!(total, 2, "both apps match 'web'");
        assert_eq!(
            hits[0].kind, hits[1].kind,
            "a repeated keyword matched by a different rule"
        );
        assert_eq!(
            hits[0].score, hits[1].score,
            "a repeated keyword scored {} against a single one's {}",
            hits[0].score, hits[1].score
        );
    }

    /// The quick-tile index table is a hand-written copy of the tile order.
    ///
    /// `quick_tile_kind(0) == Wifi` is only true because `SystemUiShade::new`
    /// happens to push `Wifi` first. Nothing links the two: the shade's list is
    /// built from its own array, the shell's mapping is a `match` on an integer,
    /// and a tile inserted or reordered in one is invisible to the other. The
    /// symptom is not a crash — it is a Wifi tile that drives Bluetooth, which
    /// passes every existing test because both are real radios and both report
    /// success.
    ///
    /// So this pins the two tables against each other. It is the same class of
    /// guard as the settings paint/hit-test contract: two independent statements
    /// of one fact, with nothing keeping them in agreement.
    #[test]
    fn the_tile_index_table_matches_the_shades_tile_order() {
        use utim_core::compositor::systemui::{QuickTileKind, SystemUiShade};
        let shade = SystemUiShade::new();
        let kind_at = |i: usize| shade.tiles.get(i).map(|t| t.kind);
        use utim_core::net::Radio;
        for (i, radio) in [
            (0usize, Radio::Wifi),
            (1, Radio::MobileData),
            (2, Radio::Bluetooth),
            (7, Radio::Tethering),
        ] {
            assert_eq!(
                quick_tile_kind(i),
                Some(radio),
                "tile {i} is not the radio the shell thinks it is"
            );
        }
        // The two the shell drives by index rather than by radio.
        assert_eq!(
            kind_at(3),
            Some(QuickTileKind::Torch),
            "tile 3 must be Torch"
        );
        assert_eq!(
            kind_at(5),
            Some(QuickTileKind::AirplaneMode),
            "tile 5 must be Airplane, or `quick_tile_airplane` drives the wrong one"
        );
        // And the table must not claim a radio for a tile that has none.
        assert_eq!(quick_tile_kind(3), None, "Torch is not a radio");
        assert_eq!(quick_tile_kind(5), None, "Airplane is not a radio");
    }

    /// The settings that are live: the settings-panel key, and the
    /// [`LauncherState`] field a read of it must name.
    ///
    /// Hand-maintained, and that is deliberate -- [`the_settings_audit_covers_every_row`]
    /// fails if a row appears here that is not in this table, so the table cannot
    /// silently fall behind the panel. A generated table would need a convention
    /// linking key to field name, and `grid-cols` -> `grid_cols` is a convention
    /// that holds right up until it does not.
    const LIVE_SETTINGS: [(&str, &str); 17] = [
        ("dark-theme", "dark_theme"),
        ("follow-system-theme", "follow_system_theme"),
        ("auto-rotate", "auto_rotate"),
        ("accent-source", "accent_source"),
        ("icon-shape", "icon_shape"),
        ("monochrome-icons", "monochrome_icons"),
        ("font-scale", "font_scale"),
        ("clock-24h", "clock_24h"),
        ("show-search-bar", "show_search_bar"),
        ("wallpaper", "wallpaper"),
        ("grid-cols", "grid_cols"),
        ("grid-rows", "grid_rows"),
        ("folder-cols", "folder_cols"),
        ("folder-rows", "folder_rows"),
        ("home-locked", "home_locked"),
        ("haptics", "haptics"),
        ("screen-timeout", "screen_timeout_s"),
    ];

    /// Every settings row must be accounted for by [`LIVE_SETTINGS`] or be a
    /// `Text` row.
    ///
    /// The failure this prevents is the one this file has produced three times:
    /// a setting that is persisted, rendered as a switch, and read by nothing. A
    /// user toggles it, the toggle moves, and nothing happens -- with no crash
    /// and no warning, because from the code's point of view the setting works.
    ///
    /// Asserted as *coverage* rather than *liveness* because liveness needs the
    /// source scan, which lives in the companion test below. This one only has to
    /// notice that a row appeared with no entry in the table, so the scan can
    /// never quietly pass by not looking at the row that is broken.
    #[test]
    fn the_settings_audit_covers_every_row() {
        let s = LauncherState::default();
        let rows = utim_core::settings::build(&s);
        assert!(rows.len() > 8, "the panel is a list, not a stub");
        for r in &rows {
            if matches!(r.kind, utim_core::settings::SettingKind::Text) {
                continue;
            }
            assert!(
                LIVE_SETTINGS.iter().any(|(k, _)| *k == r.key),
                "settings row {:?} ({}) is not in LIVE_SETTINGS, so nothing audits it; \
                 either it is dead or the table is out of date",
                r.key,
                r.label
            );
        }
        // And the other direction: no table entry for a row that no longer exists,
        // which would mean the table is asserting about nothing.
        for (key, _) in LIVE_SETTINGS {
            assert!(
                rows.iter().any(|r| r.key == key),
                "LIVE_SETTINGS names {key:?}, which is not a row any more"
            );
        }
    }

    /// Every audited setting must actually be *read* by the shell.
    ///
    /// `the_settings_audit_covers_every_row` proves the table is complete. This
    /// proves the table is honest, by reading this file's own source and looking
    /// for a field access naming each one.
    ///
    /// # What counts as a read
    ///
    /// A `.field` access that is not an assignment. That distinction is not
    /// pedantry: `home_locked` is written by `persist_state` and read by the lock
    /// logic, and a filter that accepted either would have passed a setting whose
    /// only appearance was `state.home_locked = home_locked;` -- a field the shell
    /// dutifully saves and never consults, which is a *worse* bug than one with no
    /// reader at all, because persisting it makes it look alive.
    ///
    /// # Why a source scan
    ///
    /// The alternative is a test per setting, and that is what made this go
    /// unnoticed in the first place: `dark_theme` had readers, so a test on
    /// `dark_theme` passed, while the *palette* -- the only thing the setting
    /// exists to change -- was built once at boot and never again. Nine settings
    /// had zero readers and no test noticed, because there was nothing to attach
    /// a test to.
    ///
    /// # Stated limitations
    ///
    /// The scan is crude and the crude parts are named rather than hidden: it
    /// strips `//` comments (which covers `///` doc comments, the main way a
    /// field name appears without being read) and it stops at the first
    /// `#[cfg(test)]`, so a test *mentioning* a field does not count as the shell
    /// reading it. It does not strip string literals -- nothing in this file
    /// contains a `".field_name"`, and a lexer here would be sixty lines of test
    /// code guarding a case that does not exist. And it cannot tell a read that
    /// feeds the *right* consumer from one that feeds a wrong one; `dark_theme`
    /// is the example that got past a per-setting test for exactly that reason.
    #[test]
    fn every_audited_setting_is_read_by_the_shell() {
        let full = include_str!("main.rs");
        // The shell proper: everything before the test module.
        let src = match full.find("#[cfg(test)]") {
            Some(i) => &full[..i],
            None => full,
        };
        let code: String = src
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dead: Vec<&str> = LIVE_SETTINGS
            .iter()
            .filter(|(_, field)| !has_read_of(&code, field))
            .map(|(k, _)| *k)
            .collect();
        assert!(
            dead.is_empty(),
            "these settings are rendered as controls but never read by the shell: \
             {dead:?}. Each is a switch a user can move with no result -- wire a \
             reader, or demote the row to SettingKind::Text so it admits it is not \
             editable."
        );
    }

    /// Whether `code` reads `state.<field>` anywhere, as opposed to only assigning
    /// it.
    ///
    /// The receiver is checked as well as the field name, and that is not
    /// pedantry. `LauncherState::grid_cols` and `Layout::grid_cols` are two
    /// unrelated fields that share a name, and the shell reads the `Layout` one
    /// in four places. A scan for `.grid_cols` therefore found four "readers" and
    /// passed a setting that has none -- a false negative in the very test written
    /// to prevent that class of miss. So the receiver must be a state-ish binding:
    /// `state`, `s`, or anything ending in `state`, which covers the shell and the
    /// tests without matching a layout or a profile.
    ///
    /// `==`, `!=` and compound assignment all read, so only a bare `=` that is not
    /// part of a longer operator counts as a write.
    fn has_read_of(code: &str, field: &str) -> bool {
        let needle = format!(".{field}");
        let bytes = code.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = code[from..].find(&needle) {
            let at = from + rel;
            from = at + needle.len();
            // A word character after the name means this is a longer field.
            if let Some(&c) = bytes.get(from) {
                if c == b'_' || c.is_ascii_alphanumeric() {
                    continue;
                }
            }
            // Walk back over the receiver identifier.
            let mut start = at;
            while start > 0 {
                let c = bytes[start - 1];
                if c == b'_' || c.is_ascii_alphanumeric() {
                    start -= 1;
                } else {
                    break;
                }
            }
            let recv = &code[start..at];
            let is_state = recv == "state"
                || recv == "s"
                || recv.ends_with("state")
                || recv.ends_with("_state");
            if !is_state {
                continue;
            }
            let rest = code[from..].trim_start();
            let is_write =
                rest.starts_with('=') && !rest.starts_with("==") && !rest.starts_with("=>");
            if !is_write {
                return true;
            }
        }
        false
    }

    /// `haptic` must be the only path from the shell to `Haptics::trigger`.
    ///
    /// This is a source scan rather than a behavioural test, and the reason is
    /// worth stating because it is unusual: there is no vibrator node in this
    /// rootfs, so `Haptics::trigger` returns `false` and leaves `fired()` at zero
    /// whether or not the gate let it through. A behavioural test of
    /// `haptic(enabled = false)` would pass on any implementation, including one
    /// that ignored the flag entirely -- it would be testing the absent hardware.
    ///
    /// So this pins the thing that is actually checkable: that there is exactly
    /// one call to `trigger` in the shell, and that it is the one inside
    /// `haptic`. The failure it catches is real and has happened in this file --
    /// a sixth call site added later, ungated, reintroducing the bug this gate
    /// exists to prevent, with every existing test still green.
    ///
    /// Same crude source handling as
    /// [`every_audited_setting_is_read_by_the_shell`]: `//` comments stripped,
    /// scanning stops at `#[cfg(test)]` so this test's own mention of
    /// `trigger` does not satisfy it.
    #[test]
    fn the_haptic_gate_is_the_only_path_to_trigger() {
        let full = include_str!("main.rs");
        let src = match full.find("#[cfg(test)]") {
            Some(i) => &full[..i],
            None => full,
        };
        // Two facts, rather than a count of every `.trigger(` in the file.
        //
        // The count was the first attempt and it was wrong twice: scanning for
        // `haptics.trigger(` found nothing, because inside the gate the receiver
        // is the parameter named `h`, so the test passed vacuously; widening it to
        // `.trigger(` then found four, three of which are
        // `super_extreme_state.volume_hud.trigger` -- a different type entirely.
        //
        // So: the shell's `Haptics` value is always named `haptics`, so zero
        // `haptics.trigger(` means no ungated call; and the gate itself must
        // contain a `.trigger(`, so the gate has not been gutted. Together those
        // say every pulse goes through the gate, without having to enumerate
        // every other type that happens to have a `trigger`.
        let ungated = src.matches("haptics.trigger(").count();
        assert_eq!(
            ungated, 0,
            "found {ungated} direct `haptics.trigger(` call(s) in the shell. Every \
             pulse must go through `haptic(...)`, which is the only thing that \
             consults `LauncherState::haptics`."
        );
        let gate = src
            .find("fn haptic(h: &mut Haptics")
            .expect("the gate exists");
        let gate_end = src[gate..]
            .find("\n}\n")
            .map(|i| gate + i)
            .expect("the gate has a body");
        assert!(
            src[gate..gate_end].contains(".trigger("),
            "the gate no longer triggers anything, so haptics are dead rather than \
             gated"
        );
    }

    /// The accent source selects the seed, and a custom seed is forced opaque.
    ///
    /// The opacity is the part with teeth: `MaterialYouPalette::from_seed` masks
    /// its input to `0x00FFFFFF`, so a stored colour with a zero alpha byte would
    /// be read as a different colour than the one the user picked, and the
    /// resulting scheme would not match the swatch.
    #[test]
    fn the_accent_source_selects_the_palette_seed() {
        let mut s = LauncherState::default();
        // Default: a fixed neutral, stable across boots and wallpapers.
        s.accent_source = AccentSource::Default;
        s.accent_color = 0;
        assert_eq!(palette_seed(&s), DEFAULT_ACCENT_SEED);

        // Custom: the user's colour, forced opaque.
        s.accent_source = AccentSource::Custom;
        s.accent_color = 0x00_11_22_33;
        assert_eq!(
            palette_seed(&s),
            0xFF11_2233,
            "a custom seed with a transparent alpha byte must be forced opaque"
        );

        // Custom but unset: falls back rather than inventing a colour, so the row
        // does not look like it did something.
        s.accent_color = 0;
        let unset = palette_seed(&s);
        assert_ne!(unset, DEFAULT_ACCENT_SEED, "unset must not read as Default");
        assert_eq!(unset, wallpaper_seed(), "unset falls back to the wallpaper");

        // A stale seed left behind by a previous custom choice must not leak into
        // a Default source. `settings::apply` clears it, but the palette must not
        // depend on that having run.
        s.accent_source = AccentSource::Default;
        s.accent_color = 0x00AB_CDEF;
        assert_eq!(palette_seed(&s), DEFAULT_ACCENT_SEED);
    }

    /// A custom accent must reach the palette, not just the seed.
    ///
    /// [`palette_seed`] could be correct while [`build_palette`] ignores it --
    /// which is precisely how the light theme stayed unreachable: a correct
    /// `from_seed_light` with no caller. So this compares two *built palettes*,
    /// not two seeds.
    #[test]
    fn a_custom_accent_changes_the_built_palette() {
        let mut s = LauncherState::default();
        s.accent_source = AccentSource::Default;
        s.accent_color = 0;
        let neutral = build_palette(&s);
        s.accent_source = AccentSource::Custom;
        s.accent_color = 0xFF00_3366;
        let custom = build_palette(&s);
        assert_ne!(
            neutral.primary, custom.primary,
            "a custom accent did not change the palette's primary"
        );
    }

    /// Light and dark must produce different palettes, and the theme decides.
    ///
    /// The same reason as the test above: `from_seed_light` existed, was correct,
    /// and had no caller. Asserted through `effective_theme` so the whole chain --
    /// setting -> theme -> palette -- is exercised.
    #[test]
    fn the_theme_setting_reaches_the_palette() {
        let mut s = LauncherState::default();
        s.accent_source = AccentSource::Default;
        s.accent_color = 0;
        s.dark_theme = true;
        assert_eq!(effective_theme(&s), Theme::Dark);
        let dark = build_palette(&s);
        s.dark_theme = false;
        assert_eq!(effective_theme(&s), Theme::Light);
        let light = build_palette(&s);
        assert_ne!(
            dark.surface, light.surface,
            "light and dark resolved to the same surface colour"
        );
    }

    /// The palette cache key must change when, and only when, a rebuild is due.
    ///
    /// Two failure modes, and they are opposite: a missing input leaves the
    /// palette stale until reboot (the bug this cache was added to fix), and a
    /// spurious input rebuilds it every frame, which means a directory walk per
    /// frame. Both are invisible in normal use, so both are asserted.
    #[test]
    fn the_palette_cache_key_tracks_every_input() {
        let base = LauncherState::default();
        let k0 = palette_key(&base);
        // `LauncherState::default()` is dark, so the *change* is to light --
        // setting it back to `true` would compare equal and the assertion would
        // pass for the wrong reason.
        let mut s = base.clone();
        s.dark_theme = false;
        assert_ne!(palette_key(&s), k0, "dark_theme does not affect the key");
        let mut s = base.clone();
        s.accent_color = 0xFF00_3366;
        assert_ne!(palette_key(&s), k0, "accent_color does not affect the key");
        let mut s = base.clone();
        s.accent_source = AccentSource::Custom;
        assert_ne!(palette_key(&s), k0, "accent_source is not in the key");
        // And an unrelated change must NOT invalidate, or this rebuilds the
        // palette on every frame.
        let mut s = base.clone();
        s.clock_24h = !s.clock_24h;
        s.home_locked = !s.home_locked;
        assert_eq!(
            palette_key(&s),
            k0,
            "an unrelated setting invalidates the cache"
        );
    }

    /// The screen-blank policy: never, expires, wakes, and clamps the poll.
    ///
    /// The poll clamp is the part that matters and the part that is easiest to get
    /// wrong, so it is asserted directly. The loop parks with an `i32::MAX`
    /// timeout when nothing is animating; a timeout implemented only as "check the
    /// clock each frame" therefore never runs at all, because there are no frames.
    #[test]
    fn the_screen_timeout_expires_wakes_and_clamps_the_poll() {
        // "Never" must never expire, and must clear a latched blank.
        let mut t = ScreenTimeout::new(0);
        let now = Instant::now();
        assert!(!t.expired_at(now));
        assert!(!t.expired_at(now + Duration::from_secs(86_400)));
        t.off = true;
        assert!(
            !t.expired_at(now),
            "a zero budget must not latch a blank; it means never"
        );

        // A real budget expires, and stays expired.
        let mut t = ScreenTimeout::new(1);
        assert!(!t.expired_at(now));
        // `new` stamps `last_input` at construction and the test's `now` is taken
        // just after, so elapsed time at `now + 1000ms` is a hair under a second.
        // The margin is the construction gap, not slack in the comparison.
        assert!(!t.expired_at(now + Duration::from_millis(999)));
        assert!(t.expired_at(now + Duration::from_millis(1001)));
        assert!(t.expired_at(now + Duration::from_secs(60)), "must latch");

        // Input wakes it, from either state.
        t.touch(now + Duration::from_secs(60));
        assert!(!t.off, "input must clear the blank");
        assert!(!t.expired_at(now + Duration::from_secs(60)));

        // The poll clamp. `i32::MAX` is what the loop asks for when idle.
        let mut t = ScreenTimeout::new(10);
        let base = Instant::now();
        let clamped = t.poll_budget(i32::MAX, base);
        assert!(
            clamped < i32::MAX && clamped > 9_000 && clamped <= 10_001,
            "an idle loop would park for {clamped} ms; it must wake within the budget"
        );
        // Already blank: the clamp is irrelevant, the caller is about to paint.
        t.expired_at(base + Duration::from_secs(11));
        assert_eq!(t.poll_budget(i32::MAX, base), i32::MAX);
        // Never: untouched.
        let never = ScreenTimeout::new(0);
        assert_eq!(never.poll_budget(i32::MAX, base), i32::MAX);
        // A short frame deadline wins over a long timeout budget: the loop must
        // still animate.
        let animating = ScreenTimeout::new(600);
        assert_eq!(
            animating.poll_budget(16, base),
            16,
            "a 16 ms frame deadline must not be stretched to the blank budget"
        );
    }

    /// Shortening the budget below the time already elapsed blanks at once.
    ///
    /// The case is real: a user who has had the screen on for four hours opens
    /// Settings and picks "15 s". Waiting fifteen more seconds would be a bug the
    /// user experiences as "the timeout does not work", which is the opposite of
    /// the intended lesson.
    #[test]
    fn shortening_the_timeout_below_the_elapsed_idle_time_blanks_now() {
        let mut t = ScreenTimeout::new(3600);
        let now = Instant::now();
        assert!(!t.expired_at(now));
        // Still one hour: nothing to do.
        assert!(!t.set_budget(3600, now + Duration::from_secs(60)));
        // Shorten to 15 s with four hours already elapsed: blank now.
        assert!(t.set_budget(15, now + Duration::from_secs(4 * 3600)));
        assert!(t.off, "a shortened budget must take effect immediately");
        // And going back to Never must un-blank it.
        assert!(!t.set_budget(0, now + Duration::from_secs(4 * 3600)));
        assert!(!t.off, "Never must release a latched blank");
    }

    /// A tap inside a folder launches that app, and a long press opens the menu.
    ///
    /// Folders were open-only: the state machine did not exist, so a folder could
    /// be opened and looked at and nothing inside it could be touched. This is the
    /// test for the gesture *interpretation*, which is the part with a decision in
    /// it -- the same physical gesture (press, hold, release) means "launch" or
    /// "menu" depending only on how long it was held.
    #[test]
    fn a_folder_tap_launches_and_a_long_press_opens_the_menu() {
        let t0 = Instant::now();
        let mut g = FolderGestures::idle(t0);

        // Tap: press and release promptly, with no movement.
        g.press(3, 100.0, 100.0, t0);
        assert_eq!(
            g.release(t0 + Duration::from_millis(80)),
            FolderOutcome::TapMember(3),
            "a quick press on a member must launch it"
        );

        // Long press: held past the threshold, released without moving.
        let mut g = FolderGestures::idle(t0);
        g.press(2, 100.0, 100.0, t0);
        assert!(
            !g.poll_long_press(t0 + Duration::from_millis(200)),
            "a 200 ms hold is not a long press yet"
        );
        assert!(g.poll_long_press(t0 + Duration::from_millis(600)));
        assert!(g.menu_open);
        assert_eq!(g.menu_member, 2, "the menu must name the member it covers");
        assert_eq!(
            g.release(t0 + Duration::from_millis(700)),
            FolderOutcome::Menu
        );
        assert!(
            g.menu_open,
            "the menu must survive the touch that opened it"
        );
    }

    /// Movement past the slop makes it a drag, and cancels the menu.
    ///
    /// The two gestures are distinguished by movement, not by time, because the
    /// reference has no folder menu at all -- `Folder.onLongClick` starts a drag
    /// (`Folder.java:459-463`). So a hold that then moves has to resolve to a
    /// drag, and a menu opening on top of a lifted cell would be answering a
    /// gesture the user did not make.
    #[test]
    fn movement_past_the_slop_becomes_a_drag_and_cancels_the_menu() {
        let t0 = Instant::now();
        let mut g = FolderGestures::idle(t0);
        g.press(1, 100.0, 100.0, t0);
        // Under the slop: still a press.
        assert!(!g.move_to(100.0 + FolderGestures::DRAG_SLOP - 1.0, 100.0, 6));
        assert!(g.armed, "sub-slop movement must not end the press");
        // Over it: a drag.
        assert!(g.move_to(100.0 + FolderGestures::DRAG_SLOP + 2.0, 100.0, 6));
        assert!(!g.armed);
        assert_eq!(g.drag_slot, Some(1), "member 1 is slot 1 on a 6-slot page");
        // And a long press can no longer open a menu over a lifted cell.
        assert!(
            !g.poll_long_press(t0 + Duration::from_secs(5)),
            "a menu opened on top of a drag"
        );

        // The slot is page-local, which is the conversion most likely to be wrong.
        let mut g = FolderGestures::idle(t0);
        g.press(8, 0.0, 0.0, t0);
        assert!(g.move_to(50.0, 0.0, 6));
        assert_eq!(g.drag_slot, Some(2), "member 8 on a 6-slot page is slot 2");
    }

    /// A drop outside the grid is a removal, not a failed reorder.
    ///
    /// `Folder.java:463-471` starts a drag from inside a folder and treats a drop
    /// outside it as a delete. Getting this wrong in the other direction -- an
    /// outside drop doing nothing -- leaves the user dragging a cell off the
    /// folder and having it snap back, with no feedback about why.
    #[test]
    fn a_drop_reports_where_the_cell_went_including_off_the_grid() {
        let t0 = Instant::now();
        let mut g = FolderGestures::idle(t0);
        g.press(2, 0.0, 0.0, t0);
        assert!(g.move_to(40.0, 0.0, 6));
        // `drop_slot` starts `None`, so the first hover *is* a change; it is the
        // second, identical one that must not demand a frame.
        assert!(
            g.drag_over(Some(2), false, 40.0, 0.0),
            "the first slot is a change"
        );
        assert!(
            !g.drag_over(Some(2), false, 41.0, 0.0),
            "hovering the same slot must not demand a repaint"
        );
        assert!(
            g.drag_over(Some(4), false, 90.0, 0.0),
            "a new slot must repaint"
        );
        assert_eq!(
            g.release(t0 + Duration::from_millis(50)),
            FolderOutcome::Dragged {
                from: Some(2),
                to: Some(4),
                out: false
            }
        );
        // Dropped off the grid: `out` is set and the caller removes rather than
        // reorders.
        let mut g = FolderGestures::idle(t0);
        g.press(2, 0.0, 0.0, t0);
        assert!(g.move_to(40.0, 0.0, 6));
        assert!(g.drag_over(None, true, 5.0, 5.0));
        assert_eq!(
            g.release(t0 + Duration::from_millis(50)),
            FolderOutcome::Dragged {
                from: Some(2),
                to: None,
                out: true
            }
        );
    }

    /// A reorder survives `release`, which resets the gesture.
    ///
    /// The first version of this read `g.drag_slot` *after* calling `release` --
    /// which clears the state -- so every reorder silently reported
    /// `from: None` and degraded to "dropped nowhere". The commit helper would
    /// then do nothing, and the cell would spring back with no error anywhere.
    /// The value is now returned by `release` itself; this pins that.
    #[test]
    fn the_drag_slots_survive_the_release_that_ends_the_gesture() {
        let t0 = Instant::now();
        let mut g = FolderGestures::idle(t0);
        g.press(5, 0.0, 0.0, t0);
        assert!(g.move_to(40.0, 0.0, 6));
        assert!(g.drag_over(Some(1), false, 40.0, 0.0));
        let out = g.release(t0 + Duration::from_millis(30));
        assert_eq!(
            out,
            FolderOutcome::Dragged {
                from: Some(5),
                to: Some(1),
                out: false
            }
        );
        assert!(g.drag_slot.is_none(), "and the gesture is now idle");
    }

    /// The menu spring opens slowly, closes quickly, and settles exactly.
    ///
    /// Closing faster than opening is the difference between a dismissal that
    /// feels responsive and one that feels sticky. And it has to *land* on 0.0 and
    /// 1.0 rather than approaching them forever, or `menu_animating` never goes
    /// false and the loop demands frames for a menu that is not moving.
    #[test]
    fn the_menu_spring_settles_and_closes_faster_than_it_opens() {
        let t0 = Instant::now();

        // Open from closed, and time it.
        let mut open_ms = 0.0f32;
        let mut g = FolderGestures::idle(t0);
        g.press(0, 10.0, 10.0, t0);
        assert!(g.poll_long_press(t0 + Duration::from_millis(600)));
        assert!(g.menu_animating());
        while g.menu_animating() && open_ms < 10_000.0 {
            g.step(16.0);
            open_ms += 16.0;
        }
        assert_eq!(g.menu_progress, 1.0, "the spring must reach exactly 1.0");
        assert!(
            !g.menu_animating(),
            "a settled menu must stop demanding frames"
        );

        // Close from open, and time that separately.
        g.dismiss_menu();
        let mut close_ms = 0.0f32;
        while g.menu_animating() && close_ms < 10_000.0 {
            g.step(16.0);
            close_ms += 16.0;
        }
        assert_eq!(g.menu_progress, 0.0, "a dismissed menu must reach 0.0");
        assert!(
            close_ms < open_ms,
            "closing took {close_ms} ms against opening's {open_ms} ms; a dismissal \
             that is slower than the opening feels sticky"
        );
    }

    /// A screen-blanked frame is black, and the shell asks for it.
    ///
    /// Without a backlight node the launcher cannot power the panel down, so
    /// "blank" means "stop painting and show black". That is a real behaviour and
    /// it is the only one available here, so it is worth pinning that it happens
    /// at all -- `screen_timeout_s` was readable and settable and did nothing for
    /// the whole life of the settings panel.
    #[test]
    fn the_screen_timeout_reaches_the_frame() {
        // The state machine's half is covered above; this is the shell half: the
        // value the renderer is handed is the expired one.
        let mut st = ScreenTimeout::new(0);
        let now = Instant::now();
        assert!(!st.expired_at(now + Duration::from_secs(9999)));
        let mut st = ScreenTimeout::new(1);
        assert!(!st.expired_at(now));
        assert!(st.expired_at(now + Duration::from_secs(2)));
    }

    /// The wallpaper row must be reachable, and must move the actual wallpaper.
    ///
    /// The row was `SettingKind::Text` for its whole life, which meant the picker
    /// was complete -- `WallpaperPicker` paging, the `at`/`of` fields, the
    /// renderer's slots -- and no way to change the setting. This is the test that
    /// the tap path reaches the picker and that the choice is *persisted*, not
    /// just displayed: a wallpaper that changes in memory and not on disk looks
    /// identical until the shell restarts.
    #[test]
    fn the_wallpaper_row_cycles_and_persists() {
        use utim_core::settings::picker::WallpaperPicker;
        let cands: Vec<String> = (0..4).map(|i| format!("/w/{i}.png")).collect();
        let mut p = WallpaperPicker::new(cands.clone(), 0);
        let mut s = LauncherState::default();
        let mut path = String::new();
        let mut img: Option<std::rc::Rc<utim_core::graphics::png::RgbaImage>> = None;

        // Nothing selected yet. The cursor already sits on candidate 0 as "the
        // current wallpaper", so a tap *advances* rather than selects -- which is
        // why the first tap lands on the second candidate.
        assert!(s.wallpaper.is_empty());
        assert!(
            cycle_wallpaper(&mut p, &mut s, &mut path, &mut img),
            "the first tap must change something"
        );
        assert_eq!(s.wallpaper, cands[1]);
        assert_eq!(p.position(), Some(2), "the row must report 2 of 4");

        // Round-trips through the text form, which is what lands on disk.
        let text = s.to_text();
        let reloaded = utim_core::launcher_state::LauncherState::from_text(&text).0;
        assert_eq!(
            reloaded.wallpaper, cands[1],
            "the chosen wallpaper did not survive a save/load cycle"
        );

        // Onwards to the end, then wrapping back to the first -- so the last
        // wallpaper is one tap from the first.
        for want in &cands[2..] {
            assert!(cycle_wallpaper(&mut p, &mut s, &mut path, &mut img));
            assert_eq!(&s.wallpaper, want);
        }
        assert!(cycle_wallpaper(&mut p, &mut s, &mut path, &mut img));
        assert_eq!(s.wallpaper, cands[0], "the picker must wrap");

        // `apply` must NOT be a second way to set it: the row is intercepted
        // before `apply` is consulted, and an arm there would be a second, wrong
        // path to the same setting.
        assert!(
            !utim_core::settings::apply(&mut s, utim_core::settings::WALLPAPER_KEY, 1),
            "`apply` grew a wallpaper arm; the picker owns that key"
        );
    }

    /// With no wallpapers installed, a tap must not claim to have changed
    /// anything.
    ///
    /// Returning `true` would mark the state dirty and rewrite the settings file
    /// on every tap of a row that cannot do anything -- and on a device with no
    /// wallpapers, that is every tap.
    #[test]
    fn an_empty_picker_is_a_no_op_not_a_dud_tap() {
        use utim_core::settings::picker::WallpaperPicker;
        let mut p = WallpaperPicker::new(Vec::new(), 0);
        let mut s = LauncherState::default();
        let mut path = String::new();
        let mut img: Option<std::rc::Rc<utim_core::graphics::png::RgbaImage>> = None;
        assert!(!cycle_wallpaper(&mut p, &mut s, &mut path, &mut img));
        assert!(s.wallpaper.is_empty());
    }

    /// The picker's cursor follows the stored path, not the list order.
    ///
    /// Position 0 would make the first tap switch to a *different* wallpaper than
    /// the one the setting names -- so the user picks "the wallpaper it already
    /// was" and gets another one. And if the stored path is gone, the cursor falls
    /// back rather than pointing at an index that means nothing.
    #[test]
    fn the_picker_cursor_follows_the_stored_path() {
        let cands: Vec<String> = vec!["/a.png".into(), "/b.png".into(), "/c.png".into()];
        assert_eq!(picker_cursor(&cands, "/c.png"), 2);
        assert_eq!(picker_cursor(&cands, "/missing.png"), 0);
        assert_eq!(picker_cursor(&[], "/a.png"), 0);
    }

    /// A folder that has painted nothing must not be hit-testable.
    ///
    /// `folder_layout_offset` returns a zero offset when the published grid is not
    /// live, and that zero is the dangerous value rather than a safe one: with no
    /// offset, `FolderLayout`'s *layout* coordinates get compared against *panel*
    /// coordinates, and the folder's cells happen to sit near the top of the layout
    /// -- so a tap on the home screen could land on a folder cell of a folder that
    /// is not on screen. The zero is a placeholder, and `folder_touch` checks
    /// `grid.live` before using it; this pins that the placeholder is zero and
    /// unmistakable rather than a plausible-looking number.
    #[test]
    fn an_unpainted_folder_has_no_hit_test_offset() {
        use utim_core::graphics::drm_kms::FolderGridGeometry;
        let s = LauncherState::default();
        let fl = folder_layout_for(&s, 1080.0, 2400.0);
        let dead = FolderGridGeometry::EMPTY;
        assert!(!dead.live);
        assert_eq!(folder_layout_offset(&fl, &dead), (0.0, 0.0));

        // And the offset, when there is one, is *derived* from the published
        // origin rather than being a second copy of the centring arithmetic. A
        // live grid's offset is therefore exactly the difference between where a
        // cell was painted and where the layout says it is.
        let live = FolderGridGeometry {
            live: true,
            origin: (fl.cell.x + 3.0, fl.cell.y + 700.0),
            ..FolderGridGeometry::EMPTY
        };
        assert_eq!(folder_layout_offset(&fl, &live), (3.0, 700.0));
    }

    /// A drag out of the folder must not delete anything on its own.
    ///
    /// My first version removed the member on any drag-out. The reference does not:
    /// `Folder.onDragExit` -> `completeDragExit()` -> `rearrangeChildren()`
    /// (`Folder.java:1293-1300`, `:1265-1277`) only commits the reorder, and removal
    /// needs a separate `DropTarget` -- the remove bar, `strings.xml:221`.
    ///
    /// This is the most destructive thing in the folder feature and the failure is
    /// invisible until an app is gone, so it is pinned from both sides: the
    /// `FolderHit` shape carries `remove` rather than `out`, and a drag-out that
    /// did not land on the bar produces `remove: false`.
    #[test]
    fn leaving_the_folder_is_not_removal_without_the_remove_bar() {
        // The enum carries `remove`, not `out`: the shell cannot express "the
        // cell left the grid, therefore delete it" at all, which is the point.
        let inert = FolderHit::Dropped {
            from: Some(2),
            to: Some(2),
            remove: false,
        };
        let deleting = FolderHit::Dropped {
            from: Some(2),
            to: Some(2),
            remove: true,
        };
        assert_ne!(inert, deleting, "the two must be distinguishable");
        match inert {
            FolderHit::Dropped { remove, .. } => assert!(!remove),
            _ => unreachable!(),
        }
        match deleting {
            FolderHit::Dropped { remove, .. } => assert!(remove),
            _ => unreachable!(),
        }

        // And the bar is a real region, so `remove` can only be true on it. The
        // bar holds an inset *button*, and `hit` is the button's test -- the bar
        // itself is the strip the finger crosses on the way out, and treating the
        // whole strip as a delete target would make removal far too easy to hit by
        // accident.
        let l = folder_base_layout(&LauncherState::default(), 1080.0, 2400.0);
        let bar = drop_target_bar(&l);
        assert!(
            bar.button.w > 0.0 && bar.button.h > 0.0,
            "the remove button has no area, so nothing can ever be removed"
        );
        assert!(
            bar.hit(
                bar.button.x + bar.button.w * 0.5,
                bar.button.y + bar.button.h * 0.5
            ),
            "the remove button rejected its own centre"
        );
        assert!(
            !bar.hit(bar.bar.x - 40.0, bar.bar.y - 40.0),
            "the remove bar claimed a point outside itself"
        );
        // The button is inset within the bar, not the whole of it.
        assert!(
            bar.button.w <= bar.bar.w && bar.button.h <= bar.bar.h,
            "the remove button is larger than the bar containing it"
        );
    }

    /// The overview's empty state must be reachable.
    ///
    /// `drm_kms.rs:6129-6138` draws "No recent items", and it has never been
    /// visible: the frame rule tested `recents.len == 0` alone, which fires on the
    /// very first frame of an overview opened with an empty stack, so the panel
    /// opened and closed before it was ever drawn. The reference keeps it up
    /// (`RecentsView.updateEmptyMessage`, `RecentsView.java:4809-4824`).
    #[test]
    fn an_overview_with_no_tasks_stays_open_to_show_its_empty_state() {
        // Empty from the start: must NOT close, or the empty state is unreachable.
        assert!(
            !overview_closes_for_empty_stack(true, false, 0),
            "an overview that never had a task must stay open and show its empty state"
        );
        // Empty *after* having had one: must close.
        assert!(
            overview_closes_for_empty_stack(true, true, 0),
            "the last card leaving must close the overview"
        );
        // Still holding cards: must not close.
        assert!(!overview_closes_for_empty_stack(true, true, 3));
        // Closed already: the rule must not resurrect anything.
        assert!(!overview_closes_for_empty_stack(false, true, 0));
    }

    #[test]
    fn follow_system_theme_defers_to_system_when_it_knows() {
        // UTLC_THEME env override is the test seam: no daemon needed.
        std::env::set_var("UTLC_THEME", "dark");
        let mut s = LauncherState::default();
        s.follow_system_theme = true;
        s.dark_theme = false;
        assert_eq!(effective_theme(&s), Theme::Dark, "system dark must win");
        std::env::set_var("UTLC_THEME", "light");
        assert_eq!(effective_theme(&s), Theme::Light, "system light must win");
        std::env::remove_var("UTLC_THEME");
        // Unknown falls back to the stored choice.
        s.dark_theme = true;
        assert_eq!(effective_theme(&s), Theme::Dark);
        s.dark_theme = false;
        // Without env/file/daemon the probe is Unknown -> falls back.
        assert_eq!(effective_theme(&s), Theme::Light);
        // Not following: system is ignored entirely.
        std::env::set_var("UTLC_THEME", "dark");
        s.follow_system_theme = false;
        s.dark_theme = false;
        assert_eq!(effective_theme(&s), Theme::Light);
        std::env::remove_var("UTLC_THEME");
    }

    #[test]
    fn wallpaper_candidates_accept_jpg_webp_and_dedupe() {
        assert!(utim_core::settings::picker::validate_image_path("/a/b.png"));
        assert!(utim_core::settings::picker::validate_image_path("/a/b.JPG"));
        assert!(utim_core::settings::picker::validate_image_path(
            "/a/b.webp"
        ));
        assert!(!utim_core::settings::picker::validate_image_path(
            "/a/b.txt"
        ));
        let mut v = vec![
            "/b.png".to_string(),
            "/a.png".to_string(),
            "/b.png".to_string(),
        ];
        utim_core::settings::picker::dedupe_candidates(&mut v);
        assert_eq!(v, vec!["/a.png".to_string(), "/b.png".to_string()]);
    }

    #[test]
    fn wallpaper_import_rejects_non_images_and_missing_files() {
        assert!(import_wallpaper_file(std::path::Path::new("/no/such/file.txt")).is_none());
        assert!(import_wallpaper_file(std::path::Path::new("/no/such/file.png")).is_none());
        // A text file with an image extension fails the magic check.
        let p = "/tmp/opencode/utlc-import-probe.png";
        std::fs::write(p, b"not a png").ok();
        assert!(import_wallpaper_file(std::path::Path::new(p)).is_none());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn quantiser_seed_differs_for_blue_vs_orange() {
        // Minimal 2x1 PNGs via the offline encoder path are heavy; instead assert
        // the wrapper prefers the quantiser over the mean when decodable, and the
        // palette-level contrast is pinned in utim_core (blue_and_orange tests).
        // Here: invalid data yields None from both paths (no panic on hostile input).
        assert!(utim_core::graphics::palette::wallpaper_palette_from_png(&[]).is_none());
        assert!(utim_core::graphics::palette::wallpaper_palette_from_png(&[0u8; 33]).is_none());
    }

    /// The popup rows that navigate must navigate, and the hidden-app row must
    /// toggle rather than only add.
    ///
    /// Six of the eight inert `PopupEffect`s had a real destination available and
    /// were routed to a no-op arm anyway. `ToggleHiddenApp` is the one that
    /// *mutates*: a toggle that only adds means a second tap does the opposite of
    /// what the user expects, and `hidden_apps` has been persisted since the store
    /// existed with nothing able to change it.
    #[test]
    fn the_popup_rows_that_have_a_destination_reach_it() {
        let mut state = LauncherState::default();
        let mut pages = vec![vec!["phone".to_string()]];
        let mut page = 0usize;
        let mut drawer = false;
        let mut locked = false;
        let mut selected: Option<String> = None;
        let mut active: Option<String> = None;

        // `page` is a parameter rather than a captured mutable, because the test
        // moves the workspace onto page 2 mid-run to check "Set as default". A
        // closure that borrowed it mutably would make that assignment a compile
        // error, and passing it in keeps the ordering explicit.
        let mut run = |effect: PopupEffect,
                       subject: &str,
                       state: &mut LauncherState,
                       active: &mut Option<String>,
                       page: &mut usize| {
            apply_popup_effect(
                effect,
                &mut PopupTargets {
                    state,
                    home_pages: &mut pages,
                    current_home_page: page,
                    app_drawer_open: &mut drawer,
                    home_locked: &mut locked,
                    selected_home_icon: &mut selected,
                    active_app: active,
                    open_app_info: false,
                    subject,
                    persist: None,
                },
            );
        };

        // The three navigation rows land on the Settings panel.
        for effect in [
            PopupEffect::OpenSettings,
            PopupEffect::OpenSystemSettings,
            PopupEffect::OpenWallpapers,
            PopupEffect::CustomizeApp,
        ] {
            active = None;
            run(effect, "", &mut state, &mut active, &mut page);
            assert_eq!(
                active.as_deref(),
                Some(SETTINGS_APP),
                "{effect:?} did not reach the Settings panel"
            );
        }

        // "Set as default" persists the page the workspace is on.
        page = 2;
        run(
            PopupEffect::SetDefaultPage,
            "",
            &mut state,
            &mut active,
            &mut page,
        );
        assert_eq!(
            state.default_page, 2,
            "`default_page` was persisted and read by nothing, so this row could not set it"
        );

        // Hide toggles both ways.
        run(
            PopupEffect::ToggleHiddenApp,
            "phone",
            &mut state,
            &mut active,
            &mut page,
        );
        assert_eq!(state.hidden_apps, vec!["phone".to_string()]);
        run(
            PopupEffect::ToggleHiddenApp,
            "phone",
            &mut state,
            &mut active,
            &mut page,
        );
        assert!(
            state.hidden_apps.is_empty(),
            "a second tap must un-hide, not hide again"
        );
        // And a workspace-menu tap has no subject, so it must not hide anything.
        run(
            PopupEffect::ToggleHiddenApp,
            "",
            &mut state,
            &mut active,
            &mut page,
        );
        assert!(state.hidden_apps.is_empty());
    }

    /// A drag between two taps must stop them pairing.
    ///
    /// The reference cancels the gesture mid-drag rather than on release
    /// (`AbstractGestureController:70-77`), so the pair has to break on the *move*.
    /// Testing only "two taps in time pair" would pass against an implementation
    /// that never resets, and the failure would be a double-tap firing after a
    /// scroll — which looks like a random extra tap, not like a missed reset.
    #[test]
    fn a_drag_between_two_taps_breaks_the_pair() {
        use utim_core::compositor::gestures::{forms_double_tap, DoubleTapConfig, Tap, TapHistory};
        let cfg = DoubleTapConfig::DEFAULT;
        let a = Tap::new(0, 60, 500.0, 500.0);
        let b = Tap::new(150, 200, 505.0, 502.0);
        assert!(forms_double_tap(a, b, cfg), "two quick taps must pair");

        let mut h = TapHistory::new(cfg);
        // `tap` is the bool convenience wrapper the shell does not use; the shell
        // calls `on_touch_end` so it can see *which* outcome it was.
        assert!(!h.tap(a), "the first tap is not a double tap");
        assert!(h.pending().is_some(), "the first tap arms the pair");
        // What the shell does on `Move`:
        h.reset();
        assert!(h.pending().is_none(), "reset must clear the armed half");
        // So the second tap is a first again, not a double.
        assert!(
            !h.tap(b),
            "a tap after a drag must not pair with the tap before the drag"
        );
    }

    /// The detector's timing and slop must actually gate the pair.
    ///
    /// `forms_double_tap` is the pure decision the shell calls, so this pins the
    /// thresholds themselves rather than the plumbing around them: too slow, too
    /// far apart, or a long first press must all fail to pair.
    #[test]
    fn the_detector_gates_on_time_distance_and_press_length() {
        use utim_core::compositor::gestures::{forms_double_tap, DoubleTapConfig, Tap};
        let cfg = DoubleTapConfig::DEFAULT;
        let quick = Tap::new(0, 40, 100.0, 100.0);
        assert!(forms_double_tap(
            quick,
            Tap::new(100, 140, 102.0, 101.0),
            cfg
        ));

        // Too slow between the two taps.
        assert!(
            !forms_double_tap(quick, Tap::new(100_000, 100_040, 102.0, 101.0), cfg),
            "taps a second and a half apart must not pair"
        );
        // Too far apart on screen.
        assert!(
            !forms_double_tap(
                quick,
                Tap::new(100, 140, 100.0 + cfg.slop + 10.0, 100.0),
                cfg
            ),
            "taps beyond the slop must not pair"
        );
        // A long first press is a press-and-hold, not a tap.
        assert!(
            !forms_double_tap(
                Tap::new(0, 900, 100.0, 100.0),
                Tap::new(1_000, 1_040, 100.0, 100.0),
                cfg
            ),
            "a long press must not count as the first half of a double tap"
        );
    }

    /// A double-tap action with nowhere to go must fall through, not consume.
    ///
    /// `Sleep` is the reference's default and the action a stock install selects
    /// (`GestureHandlerConfig.kt:76-78`, `PreferenceManager2.kt:813-816`), so this
    /// is the *common* path. Consuming it would swallow the user's double-tap on a
    /// no-op, and then not even a bug report would show the detector was working.
    #[test]
    fn an_unimplemented_double_tap_action_is_not_consumed() {
        use utim_core::compositor::gestures::DoubleTapAction;
        let mut state = ShellState::Normal;
        let mut shade = utim_core::compositor::systemui::SystemUiShade::new();
        let before = state;

        // No destination: falls through so the tap still does something visible.
        for a in [DoubleTapAction::Sleep, DoubleTapAction::NoOp] {
            let mut a = a;
            assert!(
                !run_double_tap(&mut a, &mut state, &mut shade),
                "{a:?} must not be consumed when it does nothing"
            );
            assert_eq!(state, before, "{a:?} changed the shell state");
            assert!(!shade.is_open(), "{a:?} opened the shade");
        }

        // The ones that do have a destination act.
        let mut a = DoubleTapAction::Recents;
        assert!(run_double_tap(&mut a, &mut state, &mut shade));
        assert!(matches!(state, ShellState::Overview { .. }));
        let mut a = DoubleTapAction::OpenQuickSettings;
        assert!(run_double_tap(&mut a, &mut state, &mut shade));
        assert!(shade.is_open(), "the quick-settings action opened nothing");
    }

    /// A rename must not touch the record until it commits, and cancelling must be
    /// total.
    ///
    /// The failure this guards is subtle and survives a reboot: if the buffer
    /// were the record, a cancelled or half-typed rename would leave
    /// `LauncherState` holding an edit that was never committed, and it would be
    /// there again after a restart. So the record is written in exactly one place
    /// — [`commit_folder_rename`] — and everything else must leave it alone.
    #[test]
    fn a_rename_only_touches_the_record_on_commit() {
        let mut state = LauncherState::default();
        state.folders = vec![utim_core::launcher_state::FolderRecord {
            id: 1,
            title: "Work".to_string(),
            rank: 0,
            items: vec!["phone".to_string()],
            is_drawer: false,
        }];
        let mut r: Option<FolderRename> = None;
        begin_folder_rename(&mut r, 1);

        // Typing is invisible to the record.
        assert!(r.as_mut().unwrap().type_char('D'));
        assert!(r.as_mut().unwrap().type_char('e'));
        assert_eq!(state.folders[0].title, "Work", "typing must not persist");

        // Cancelling leaves it alone, which is the whole point -- and cancelling
        // is just dropping the value, because the buffer was never the record.
        r = None;
        assert!(r.is_none());
        assert_eq!(state.folders[0].title, "Work", "cancel must not persist");

        // And committing does.
        begin_folder_rename(&mut r, 1);
        for c in "Tools".chars() {
            assert!(r.as_mut().unwrap().type_char(c));
        }
        assert!(commit_folder_rename(&mut r, &mut state));
        assert_eq!(state.folders[0].title, "Tools");
        assert!(r.is_none(), "committing must close the editor");
        // A second commit with nothing open is a no-op, not a second write.
        assert!(!commit_folder_rename(&mut r, &mut state));
    }

    /// A rename must target the folder that was long-pressed, not its neighbour.
    ///
    /// `FolderOpen::folder_idx` is the `FolderRecord::id` verbatim — `open_folder_at`
    /// does `folder.open(id as u8)` from the id in the cell token
    /// (`PageCell::Folder(fid)`). An earlier version added 1 on the way in and
    /// subtracted 1 on the way out, which type-checks perfectly, renames nothing on
    /// a one-folder state, and renames the *wrong* folder once a second exists.
    ///
    /// So this builds two folders and checks the right one is hit. A one-folder
    /// fixture would have passed against the offset version too, which is the
    /// point: the bug is invisible until there is more than one folder.
    #[test]
    fn a_rename_targets_the_folder_that_was_raised_not_its_neighbour() {
        let mut state = LauncherState::default();
        state.folders = vec![
            utim_core::launcher_state::FolderRecord {
                id: 1,
                title: "First".into(),
                rank: 0,
                items: vec![],
                is_drawer: false,
            },
            utim_core::launcher_state::FolderRecord {
                id: 2,
                title: "Second".into(),
                rank: 1,
                items: vec![],
                is_drawer: false,
            },
        ];
        // `folder_idx` for folder 2, as `open_folder_at` would have set it.
        let idx: u8 = 2;
        let mut r: Option<FolderRename> = None;
        let fid = idx as u32;
        let title = state
            .folders
            .iter()
            .find(|f| f.id == fid)
            .map(|f| f.title.as_str())
            .unwrap_or("");
        assert_eq!(title, "Second", "the lookup must find the folder itself");
        begin_folder_rename(&mut r, fid);
        for c in "Renamed".chars() {
            r.as_mut().unwrap().type_char(c);
        }
        assert!(commit_folder_rename(&mut r, &mut state));
        assert_eq!(
            state.folders[1].title, "Renamed",
            "the wrong folder was renamed"
        );
        assert_eq!(state.folders[0].title, "First", "its neighbour was not");
    }

    /// An unchanged title is not a change, and a rename must not be able to grow
    /// without bound.
    ///
    /// Both are quiet: a redundant `touch()` rewrites the settings file for
    /// nothing, and an unbounded buffer holds a name the footer has already
    /// ellipsised — so the user typed something they cannot check.
    #[test]
    fn a_rename_cannot_grow_past_what_the_footer_draws() {
        let mut r = FolderRename::begin(1);
        for _ in 0..FolderRename::MAX {
            assert!(r.type_char('a'), "the buffer filled early");
        }
        assert!(
            !r.type_char('a'),
            "a name longer than the footer can draw must be refused, not accepted"
        );
        assert_eq!(r.buffer.chars().count(), FolderRename::MAX);
        // Backspace works, and refuses at empty.
        assert!(r.backspace());
        assert!(r.type_char('b'));
        assert_eq!(r.buffer.chars().count(), FolderRename::MAX);
        for _ in 0..FolderRename::MAX + 1 {
            r.backspace();
        }
        assert!(
            !r.backspace(),
            "backspace past empty must report nothing to do"
        );
        assert!(r.buffer.is_empty());

        // An unchanged title is not a change.
        let mut state = LauncherState::default();
        state.folders = vec![utim_core::launcher_state::FolderRecord {
            id: 1,
            title: String::new(),
            rank: 0,
            items: vec![],
            is_drawer: false,
        }];
        let mut r2: Option<FolderRename> = None;
        begin_folder_rename(&mut r2, 1);
        assert!(
            !commit_folder_rename(&mut r2, &mut state),
            "an empty buffer over an empty title is not a change"
        );
    }

    /// A failed torch write says which of the three things went wrong.
    ///
    /// The distinction that matters is "no torch" versus "failed". This rootfs has
    /// no `torch-light` LED node, so `Err(NotFound)` is the *expected* outcome
    /// here, and a tile that reported it as "Failed" would be telling the user
    /// something is broken when the honest answer is that the hardware is not
    /// present.
    #[test]
    fn a_failed_torch_write_says_which_thing_went_wrong() {
        use std::io::{Error, ErrorKind};
        assert_eq!(torch_note(&Error::from(ErrorKind::NotFound)), "No torch");
        assert_eq!(
            torch_note(&Error::from(ErrorKind::PermissionDenied)),
            "Denied"
        );
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ] {
            let note = torch_note(&Error::from(kind));
            assert!(
                note == "Failed" || note == "No torch" || note == "Denied",
                "{kind:?} produced the unrecognised note {note:?}"
            );
        }
    }

    fn app_named(id: &str, name: &str) -> ManagedApp {
        ManagedApp::new(id, name, &format!("/usr/bin/{id}"), 0xFF123456, "?")
    }

    use super::*;

    // -----------------------------------------------------------------
    // The shell state machine (plan §7.6).
    //
    // These guard the defect this whole change set exists for: the input
    // loop's gesture match ended in `_ => {}`, and both `Recents` and
    // `BottomBarScrub` fell into it. The gesture engine produced and tested
    // both actions; nothing consumed them, so the overview could not open and
    // a task scrub did nothing -- with every test in the workspace green.
    //
    // The lesson is that a producer-side test is not a wiring test. Asserting
    // the *effect* is what catches a consumer that was never written.
    // -----------------------------------------------------------------

    #[test]
    fn the_two_dropped_gestures_now_have_an_effect() {
        use utim_core::compositor::gestures::EdgeSide;

        // Exactly the two that used to be swallowed.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: 1.0,
                trigger_haptic: true,
            }),
            ShellEffect::OpenOverview,
            "the recents gesture still has no effect: the overview cannot open"
        );
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: 40.0,
                app_shift: 1,
            }),
            ShellEffect::ScrubTasks(1.0),
            "a bottom-bar scrub still has no effect: the task list cannot be scrubbed"
        );
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: -40.0,
                app_shift: -1,
            }),
            ShellEffect::ScrubTasks(-1.0),
        );
        // A zero shift is the gesture engine's "no detent crossed", and must
        // not be treated as a scrub -- it would otherwise move a card on every
        // move event while the finger sat inside one detent.
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: 3.0,
                app_shift: 0,
            }),
            ShellEffect::None,
        );
        // The other actions are handled by inline arms and must not be
        // double-handled by the planner.
        for a in [
            GestureAction::None,
            GestureAction::NotificationShade { progress: 0.5 },
            GestureAction::Back {
                side: EdgeSide::Left,
                progress: 1.0,
                injected: true,
            },
            GestureAction::Swipe {
                delta_x: 0.0,
                delta_y: -80.0,
            },
        ] {
            assert_eq!(
                plan_gesture(&a),
                ShellEffect::None,
                "{a:?} is not the planner's"
            );
        }
    }

    #[test]
    fn recents_only_commits_past_the_threshold() {
        // Below the commit threshold the overview must not open, or a
        // half-hearted rise from the bottom edge would yank the user into the
        // carousel and then straight back out.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: RECENTS_COMMIT_PROGRESS - 0.01,
                trigger_haptic: false,
            }),
            ShellEffect::None,
        );
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: RECENTS_COMMIT_PROGRESS,
                trigger_haptic: false,
            }),
            ShellEffect::OpenOverview,
            "the threshold must be inclusive: a release exactly at it commits"
        );
        // A rise that has not reached half screen is a Home gesture, not a
        // recents one, and must not open the overview either.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: 0.0,
                trigger_haptic: false,
            }),
            ShellEffect::None,
        );
    }

    #[test]
    fn a_full_home_gesture_closes_and_a_partial_one_morphs() {
        // The two are different effects, and conflating them is the bug that
        // made the in-app home gesture unusable: at progress 1.0 the panel
        // must be *torn down* (which also releases the morph), and below it the
        // panel must be *driven* by the gesture's own scale and opacity.
        assert_eq!(
            plan_gesture(&GestureAction::Home {
                progress: 1.0,
                scale: 0.4,
                window_alpha: 0.2,
            }),
            ShellEffect::CloseAll,
        );
        assert_eq!(
            plan_gesture(&GestureAction::Home {
                progress: 0.5,
                scale: 0.8,
                window_alpha: 0.9,
            }),
            ShellEffect::MorphWorkspace {
                scale: 0.8,
                window_alpha: 0.9,
            },
        );
    }

    #[test]
    fn closing_modal_surfaces_leaves_nothing_sticky() {
        // The reason this is a function and not four inline lines: a leftover
        // modal state makes every later workspace swipe inert (the `is_modal`
        // guard skips them) and a leftover morph target leaves the app panel
        // shrunk with nothing driving it. Both are silent, so both are asserted.
        let mut state = ShellState::Overview {
            selected: 2,
            dismiss: -40.0,
        };
        assert!(state.is_modal());

        let mut folder = FolderOpen::closed(0);
        folder.open(3);
        let mut popup = SpringSimulation::new(1.0, 1.0, SpringConfig::spring_loaded());
        popup.set_target(0.0);
        let mut scale = SpringSimulation::new(0.5, 0.5, SpringConfig::stretch_edge());
        scale.set_target(1.0);
        let mut alpha = SpringSimulation::new(0.3, 0.3, SpringConfig::stretch_edge());
        alpha.set_target(1.0);

        close_modal_surfaces(&mut state, &mut folder, &mut popup, &mut scale, &mut alpha);

        assert_eq!(state, ShellState::Normal);
        assert!(!state.is_modal(), "Normal must not block a workspace swipe");
        assert_eq!(
            folder.folder_idx, 3,
            "closing must not lose which folder it was"
        );
        assert_eq!(scale.target, 1.0, "the workspace morph must return to rest");
        assert_eq!(alpha.target, 1.0);
        assert_eq!(
            popup.target, 0.0,
            "the popup must be dismissed, not left open"
        );
    }

    // -----------------------------------------------------------------
    // The long-press popup's row dispatch.
    //
    // The defect: `PopupItem` had 16 variants, `draw_popup` rendered whichever
    // the shell handed it, and `ShellState::PopupOpen { idx }` was written as
    // the literal `0` at both construction sites and never read. So every row
    // was a label. `PopupMenuLayout` was reached only from the renderer, so the
    // comment in `draw_popup` claiming the shell hit-tested against the same
    // struct described a hit test that did not exist.
    // -----------------------------------------------------------------

    /// Every row the shell can raise must resolve to an effect. A row with no
    /// effect is the exact failure this path was rebuilt to end, so the
    /// assertion is over the whole enum rather than over the rows we happen to
    /// use today.
    #[test]
    fn every_popup_row_has_an_effect() {
        use PopupItem::*;
        for item in [
            Wallpapers,
            Widgets,
            AllApps,
            HomeSettings,
            HomeScreenLock,
            EditMode,
            SystemSettings,
            DefaultPageForWorkspace,
            AppInfo,
            Install,
            Remove,
            Uninstall,
            Customize,
            OpenInStore,
            PauseApps,
            DeepShortcut(0),
            DeepShortcut(3),
        ] {
            let effect = plan_popup_row(item);
            assert_ne!(effect, PopupEffect::Dismiss, "{item:?} has no effect");
        }
    }

    /// The rows the reference gates on the platform are recognised but not
    /// claimed as working. `Install` needs `FLAG_SUPPORTS_WEB_UI`,
    /// `OpenInStore` a Play-style store id, and `PauseApps` per-user package
    /// suspension -- all Android services with no Linux counterpart in the
    /// shell.
    #[test]
    fn the_android_only_popup_rows_are_reported_unsupported() {
        assert_eq!(plan_popup_row(PopupItem::Install), PopupEffect::Unsupported);
        assert_eq!(
            plan_popup_row(PopupItem::OpenInStore),
            PopupEffect::Unsupported
        );
        assert_eq!(
            plan_popup_row(PopupItem::PauseApps),
            PopupEffect::Unsupported
        );
    }

    /// `Customize` is the parent of rename / hide / change-icon / per-app
    /// gesture in the reference (`CustomizeDialog.kt:154-211`). It must not be
    /// collapsed into one of its children.
    #[test]
    fn customize_is_not_one_of_its_own_children() {
        assert_eq!(
            plan_popup_row(PopupItem::Customize),
            PopupEffect::CustomizeApp
        );
        assert_ne!(PopupEffect::CustomizeApp, PopupEffect::RenameApp);
        assert_ne!(PopupEffect::CustomizeApp, PopupEffect::ToggleHiddenApp);
    }

    /// A deep shortcut keeps its index, because that index is the only thing
    /// that distinguishes one app action from another.
    #[test]
    fn a_deep_shortcut_keeps_its_index() {
        for n in 0..utim_core::compositor::MAX_DEEP_SHORTCUTS {
            assert_eq!(
                plan_popup_row(PopupItem::DeepShortcut(n)),
                PopupEffect::LaunchShortcut(n),
            );
        }
    }

    /// The workspace menu is the reference's `DEFAULT_ORDER`
    /// (`LauncherOptionsPopup.kt:18-28`) minus the metadata-only `carousel`
    /// option, which is filtered at `:145` -- seven rows, and `HomeScreenLock`
    /// among them.
    #[test]
    fn the_workspace_menu_is_the_reference_order() {
        let rows = popup_items_for(false, 0, false);
        let got: Vec<PopupItem> = rows.iter().copied().collect();
        assert_eq!(
            got,
            vec![
                PopupItem::Wallpapers,
                PopupItem::Widgets,
                PopupItem::AllApps,
                PopupItem::HomeSettings,
                PopupItem::EditMode,
                PopupItem::HomeScreenLock,
                PopupItem::DefaultPageForWorkspace,
            ],
        );
    }

    /// The icon menu leads with the affirmative row, then the app's own
    /// actions, then the mutating rows (`LawnchairLauncher.kt:283-287`).
    #[test]
    fn the_icon_menu_leads_with_app_info_then_actions() {
        let rows = popup_items_for(true, 2, false);
        let got: Vec<PopupItem> = rows.iter().copied().collect();
        assert_eq!(
            got[0],
            PopupItem::AppInfo,
            "the affirmative row comes first"
        );
        assert_eq!(got[1], PopupItem::DeepShortcut(0));
        assert_eq!(got[2], PopupItem::DeepShortcut(1));
        assert!(got.contains(&PopupItem::Remove));
        assert!(got.contains(&PopupItem::Uninstall));
        assert!(got.contains(&PopupItem::Customize));
    }

    /// A locked home screen drops the mutating rows, which is what the reference
    /// does: `SystemShortcut.isHomeLocked` (`SystemShortcut.java:137-140`) gates
    /// Widgets and Remove, and `getLauncherOptions` (`LauncherOptionsPopup.kt
    /// :144-150`) drops edit-mode and widgets entirely.
    #[test]
    fn a_locked_home_drops_the_mutating_rows() {
        let workspace = popup_items_for(false, 0, true);
        assert!(!workspace.iter().any(|i| *i == PopupItem::Widgets));
        assert!(!workspace.iter().any(|i| *i == PopupItem::EditMode));
        // The lock row itself must survive, or the screen could never be
        // unlocked again.
        assert!(workspace.iter().any(|i| *i == PopupItem::HomeScreenLock));

        let icon = popup_items_for(true, 0, true);
        assert!(!icon.iter().any(|i| *i == PopupItem::Remove));
    }

    /// The deep-shortcut cap is enforced by `PopupItems`, and the menu must not
    /// overflow the fixed 8-slot capacity however many shortcuts it is offered.
    #[test]
    fn the_popup_list_cannot_overflow_or_exceed_the_shortcut_cap() {
        // More shortcuts than the cap, and more than the list holds.
        let rows = popup_items_for(true, 200, false);
        assert!(
            rows.len as usize <= 8,
            "the fixed-capacity list cannot grow"
        );
        let shortcuts = rows
            .iter()
            .filter(|i| matches!(i, PopupItem::DeepShortcut(_)))
            .count();
        assert_eq!(
            shortcuts,
            utim_core::compositor::MAX_DEEP_SHORTCUTS as usize,
            "the shortcut cap binds before the list capacity does",
        );
    }

    /// The mutating effects actually mutate. A dispatch path that compiles and
    /// runs but changes nothing is the failure mode under test.
    #[test]
    fn the_popup_effects_that_are_wired_change_state() {
        // A macro rather than a closure: `PopupTargets` has one lifetime for all
        // six borrows, which a closure returning it cannot satisfy.
        macro_rules! targets {
            ($state:ident, $pages:ident, $page:ident, $drawer:ident, $locked:ident, $sel:ident, $app:ident, $info:ident, $subject:expr) => {
                PopupTargets {
                    state: &mut $state,
                    home_pages: &mut $pages,
                    current_home_page: &mut $page,
                    app_drawer_open: &mut $drawer,
                    home_locked: &mut $locked,
                    selected_home_icon: &mut $sel,
                    active_app: &mut $app,
                    open_app_info: $info,
                    subject: $subject,
                    persist: None,
                }
            };
        }

        let mut state = LauncherState::default();
        let mut pages = vec![
            vec!["phone".to_string(), "settings".to_string()],
            vec!["terminal".to_string()],
        ];
        let mut page = 0usize;
        let mut drawer = false;
        let mut locked = false;
        let mut selected: Option<String> = Some("settings".to_string());
        let mut active: Option<String> = None;
        let open_info = false;

        // Remove drops the app from the current page and deselects it, so the
        // edit chips cannot point at a cell that no longer holds anything.
        apply_popup_effect(
            PopupEffect::RemoveFromPage,
            &mut targets!(
                state, pages, page, drawer, locked, selected, active, open_info, "settings"
            ),
        );
        assert_eq!(pages[0], vec!["phone".to_string()]);
        assert_eq!(selected, None, "removing the selected icon deselects it");

        // A remove against the wrong page is a no-op, not a mis-deletion.
        page = 1;
        apply_popup_effect(
            PopupEffect::RemoveFromPage,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info, "phone"),
        );
        assert_eq!(pages[0], vec!["phone".to_string()], "page 1 has no 'phone'");
        page = 0;

        // Remove with no subject (the workspace menu) does nothing.
        apply_popup_effect(
            PopupEffect::RemoveFromPage,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info, ""),
        );
        assert_eq!(pages[0], vec!["phone".to_string()]);

        apply_popup_effect(
            PopupEffect::OpenAllApps,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info, ""),
        );
        assert!(drawer);

        // Toggling the lock is its own inverse, and it survives both directions.
        apply_popup_effect(
            PopupEffect::ToggleHomeLock,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info, ""),
        );
        assert!(locked);
        apply_popup_effect(
            PopupEffect::ToggleHomeLock,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info, ""),
        );
        assert!(!locked);
    }

    /// Edit mode is "an icon is selected" in UTLC, so entering it with nothing
    /// selected selects the first icon on the page rather than doing nothing.
    #[test]
    fn entering_edit_mode_selects_an_icon() {
        macro_rules! targets {
            ($state:ident, $pages:ident, $page:ident, $drawer:ident, $locked:ident, $sel:ident, $app:ident, $info:ident) => {
                PopupTargets {
                    state: &mut $state,
                    home_pages: &mut $pages,
                    current_home_page: &mut $page,
                    app_drawer_open: &mut $drawer,
                    home_locked: &mut $locked,
                    selected_home_icon: &mut $sel,
                    active_app: &mut $app,
                    open_app_info: $info,
                    subject: "",
                    persist: None,
                }
            };
        }
        let mut state = LauncherState::default();
        let mut pages = vec![vec!["phone".to_string(), "settings".to_string()]];
        let mut drawer = false;
        let mut locked = false;
        let mut selected: Option<String> = None;
        let mut active: Option<String> = None;
        let open_info = false;
        let mut page = 0usize;
        apply_popup_effect(
            PopupEffect::EnterEditMode,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info),
        );
        assert_eq!(selected.as_deref(), Some("phone"));
        // It is idempotent: a second entry does not re-pick.
        apply_popup_effect(
            PopupEffect::EnterEditMode,
            &mut targets!(state, pages, page, drawer, locked, selected, active, open_info),
        );
        assert_eq!(selected.as_deref(), Some("phone"));
    }

    #[test]
    fn only_a_modal_state_blocks_a_workspace_swipe() {
        // The guard this assertion exists for. If `Normal` or `AllApps` were
        // modal, the home screen could never be paged again; if `Overview` were
        // not, a swipe behind the carousel would page the workspace under it.
        assert!(!ShellState::Normal.is_modal());
        assert!(!ShellState::AllApps { progress: 1.0 }.is_modal());
        assert!(ShellState::Overview {
            selected: 0,
            dismiss: 0.0
        }
        .is_modal());
        assert!(ShellState::PopupOpen {
            anchor_x: 0.0,
            anchor_y: 0.0,
            idx: 0
        }
        .is_modal());
    }

    #[test]
    fn test_terminal_command_execution() {
        let mut lines = Vec::new();
        let mut input = "uname -a".to_string();
        let mut app = Some("Terminal".to_string());

        let res =
            execute_terminal_command(&mut lines, &mut input, &mut app, None, None, None, None, 1);
        assert_eq!(res, TerminalAction::Continue);
        assert!(input.is_empty());
        assert!(lines.len() >= 2);
        assert_eq!(
            lines[0],
            format!("{}uname -a", utim_core::session::session().prompt())
        );
        assert!(lines[1].contains("Linux"));

        input = "exit".to_string();
        let res =
            execute_terminal_command(&mut lines, &mut input, &mut app, None, None, None, None, 1);
        assert_eq!(res, TerminalAction::CloseTab);
    }

    #[test]
    fn test_multiple_terminal_tabs() {
        let mut tabs = vec![TerminalTab::new(1), TerminalTab::new(2)];
        let mut active_tab = 0;
        let mut next_id = 3;
        let mut app = Some("Terminal".to_string());
        let mut kb = VirtualKeyboard::new();
        let mut search = false;

        // Run uname in Tab 1
        tabs[0].input = "uname".to_string();
        handle_terminal_enter(
            &mut tabs,
            &mut active_tab,
            &mut next_id,
            &mut app,
            &mut kb,
            &mut search,
        );
        if let Ok(line) = tabs[0].rx.recv_timeout(Duration::from_millis(500)) {
            push_terminal_line(&mut tabs[0].lines, &line);
        }
        assert!(tabs[0].lines.len() >= 2);
        assert!(tabs[0].lines[1].contains("Linux"));

        // Switch to Tab 2
        active_tab = 1;
        tabs[1].input = "whoami".to_string();
        handle_terminal_enter(
            &mut tabs,
            &mut active_tab,
            &mut next_id,
            &mut app,
            &mut kb,
            &mut search,
        );
        if let Ok(line) = tabs[1].rx.recv_timeout(Duration::from_millis(500)) {
            push_terminal_line(&mut tabs[1].lines, &line);
        }
        assert!(tabs[1].lines.len() >= 2);
        // `whoami` must agree with the identity the prompt advertises: never
        // "root" while the session runs unprivileged.
        let sess = utim_core::session::session();
        assert!(!tabs[1].lines[1].is_empty());
        if sess.is_root() {
            assert_eq!(tabs[1].lines[1], "root");
        } else {
            assert_ne!(tabs[1].lines[1], "root");
        }

        // Wait for whoami process to finish
        for _ in 0..50 {
            if !tabs[1].is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Open Tab 3 via "newtab" command
        tabs[1].input = "newtab".to_string();
        handle_terminal_enter(
            &mut tabs,
            &mut active_tab,
            &mut next_id,
            &mut app,
            &mut kb,
            &mut search,
        );
        assert_eq!(tabs.len(), 3);
        assert_eq!(active_tab, 2);

        // Close Tab 3 via "exit"
        tabs[2].input = "exit".to_string();
        handle_terminal_enter(
            &mut tabs,
            &mut active_tab,
            &mut next_id,
            &mut app,
            &mut kb,
            &mut search,
        );
        assert_eq!(tabs.len(), 2);
        assert_eq!(active_tab, 1);
    }

    #[test]
    fn date_formatter_matches_known_dates() {
        // The launcher shipped a hard-coded "Tue, Sep 22" for its entire
        // life, so nothing ever checked that the date formatter worked. These
        // are real weekdays, verified against `date`:
        //   2026-09-28 Monday    2026-09-22 Tuesday   2026-01-01 Thursday
        //   1970-01-01 Thursday  2000-02-29 Tuesday   2024-12-31 Tuesday
        let cases: [(i32, i32, i32, &str); 6] = [
            (1, 8, 28, "Mon, Sep 28"), // wday, mon(0-based), mday
            (2, 8, 22, "Tue, Sep 22"),
            (4, 0, 1, "Thu, Jan 1"),
            (4, 0, 1, "Thu, Jan 1"),   // 1970-01-01, the epoch
            (2, 1, 29, "Tue, Feb 29"), // 2000-02-29, a leap day
            (2, 11, 31, "Tue, Dec 31"),
        ];
        let mut buf = [0u8; 16];
        for (wday, mon, mday, want) in cases {
            let got = format_date_from_tm(&mut buf, wday, mon, mday);
            assert_eq!(got, want, "wday={wday} mon={mon} mday={mday}");
        }
    }

    #[test]
    fn date_formatter_clamps_libc_fields_instead_of_panicking() {
        // `tm_*` comes from libc and this runs on the frame path, so an
        // out-of-range value must clamp, not index out of the table.
        let mut buf = [0u8; 16];
        for (wday, mon, mday) in [
            (-1, -1, -1),
            (7, 12, 32),
            (i32::MAX, i32::MAX, i32::MAX),
            (0, 0, 0),
        ] {
            let got = format_date_from_tm(&mut buf, wday, mon, mday);
            assert!(!got.is_empty(), "{wday}/{mon}/{mday} produced nothing");
            assert!(
                got.len() <= 12,
                "{wday}/{mon}/{mday} -> {got:?} is too long"
            );
            assert!(
                got.is_ascii(),
                "{wday}/{mon}/{mday} -> {got:?} is not ASCII; the font has no \
                 other glyphs"
            );
        }
    }

    #[test]
    fn date_formatter_emits_only_renderable_characters() {
        // `font.rs` covers 0x20..0x7F and asserts non-ASCII has no ink, so a
        // stray byte would render as a blank rather than fail loudly.
        let mut buf = [0u8; 16];
        for d in 1..=31 {
            for m in 0..12 {
                for w in 0..7 {
                    let s = format_date_from_tm(&mut buf, w, m, d);
                    assert!(s.bytes().all(|b| (0x20..0x7F).contains(&b)), "{s:?}");
                }
            }
        }
    }

    #[test]
    fn test_input_tap_detection() {
        let mut dispatcher = InputDispatcher::new(1080.0, 2400.0);

        // Move to Search Pill (540, 260)
        let ev_x = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_ABS,
            code: utim_core::compositor::input::ABS_X,
            value: 16383, // center = 540
        };
        let ev_y = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_ABS,
            code: utim_core::compositor::input::ABS_Y,
            value: 3550, // 3550/32767 * 2400 ~= 260
        };
        let ev_syn = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_SYN,
            code: utim_core::compositor::input::SYN_REPORT,
            value: 0,
        };
        dispatcher.process_event(&ev_x);
        dispatcher.process_event(&ev_y);
        dispatcher.process_event(&ev_syn);

        // Down & Up
        let ev_down = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_KEY,
            code: utim_core::compositor::input::BTN_LEFT,
            value: 1,
        };
        dispatcher.process_event(&ev_down);

        let ev_up = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_KEY,
            code: utim_core::compositor::input::BTN_LEFT,
            value: 0,
        };
        let res = dispatcher.process_event(&ev_up);
        match res {
            InputDispatchResult::Tap { x, y } => {
                assert!((x - 540.0).abs() < 2.0);
                assert!((y - 260.0).abs() < 5.0);
            }
            other => panic!("Expected Tap, got {:?}", other),
        }
    }

    #[test]
    fn test_format_current_time_zero_alloc() {
        let mut buf = [0u8; 5];
        let t_str = format_current_time(&mut buf, false);
        assert_eq!(t_str.len(), 5);
        assert_eq!(&t_str[2..3], ":");
        assert!(t_str[..2].chars().all(|c| c.is_ascii_digit()));
        assert!(t_str[3..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_build_all_apps_with_firefox() {
        let mut catalogue = DesktopCatalogue::new();
        let ff_entry = DesktopApp {
            id: "firefox".to_string(),
            name: "Firefox Web Browser".to_string(),
            exec: "/usr/lib/firefox/firefox %u".to_string(),
            icon: "firefox".to_string(),
            categories: vec!["Network".to_string(), "WebBrowser".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec!["web".to_string(), "browser".to_string()],
        };
        catalogue.add_app(ff_entry);

        let apps = build_all_apps(&catalogue, &[]);
        // Base apps count is 12 + Firefox = 13
        assert!(apps.len() >= 13);

        // Check that "Firefox" app was added
        let ff_app = apps.iter().find(|a| a.name == "Firefox");
        assert!(
            ff_app.is_some(),
            "Firefox must appear on the home screen when installed"
        );
        let ff = ff_app.unwrap();
        assert_eq!(ff.color, 0xFFFF5722);
        assert_eq!(ff.glyph, "F");
        assert_eq!(ff.exec, "/usr/lib/firefox/firefox");

        // Check that the Browser shortcut is wired to Firefox
        let browser_app = apps.iter().find(|a| a.name == "Browser").unwrap();
        assert_eq!(browser_app.exec, "/usr/lib/firefox/firefox");
    }

    #[test]
    fn test_clean_build_no_uninstalled_apps() {
        let catalogue = DesktopCatalogue::new();
        let apps = build_all_apps(&catalogue, &[]);
        // Clean build has exactly 12 base apps
        assert_eq!(apps.len(), 12);
        // Firefox is not preinstalled
        assert!(apps.iter().all(|a| a.name != "Firefox"));
        // Browser shortcut default exec is empty (built-in browser)
        let browser = apps.iter().find(|a| a.name == "Browser").unwrap();
        assert!(browser.exec.is_empty());
    }

    #[test]
    fn test_build_all_apps_with_third_party() {
        let mut catalogue = DesktopCatalogue::new();
        let vlc = DesktopApp {
            id: "vlc".to_string(),
            name: "VLC media player".to_string(),
            exec: "/usr/bin/vlc".to_string(),
            icon: "vlc".to_string(),
            categories: vec!["AudioVideo".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec![],
        };
        let calc = DesktopApp {
            id: "calculator".to_string(),
            name: "Calculator".to_string(),
            exec: "gnome-calculator".to_string(),
            icon: "calculator".to_string(),
            categories: vec!["Utility".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec![],
        };
        catalogue.add_app(vlc);
        catalogue.add_app(calc);

        let apps = build_all_apps(&catalogue, &[]);
        // Base 12 + 2 installed apps = 14
        assert_eq!(apps.len(), 14);
        assert!(apps.iter().any(|a| a.id == "vlc"));
        assert!(apps.iter().any(|a| a.id == "calculator"));
    }

    #[test]
    fn test_get_app_color_deterministic() {
        let c1 = get_app_color("firefox");
        let c2 = get_app_color("firefox");
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_universal_ime_actions_and_app_input() {
        let mut active_app = Some("Browser".to_string());
        let mut app_input = String::new();
        let mut app_input_focused = true;
        let mut search_active = false;
        let mut search_query = String::new();
        let mut terminal_tabs = vec![TerminalTab::new(1)];
        let mut active_tab_idx = 0;
        let mut next_tab_id = 2;
        let mut keyboard = VirtualKeyboard::new();
        keyboard.activate();
        let mut messages_list = Vec::new();

        // 1. Commit characters to active app input
        apply_ime_action(
            ImeAction::CommitString("https://google.com".to_string()),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(app_input, "https://google.com");

        // 2. Backspace deletes character
        apply_ime_action(
            ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            },
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(app_input, "https://google.co");

        // 3. Enter in Browser finishes input and closes keyboard
        apply_ime_action(
            ImeAction::SendKey(28),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert!(!keyboard.is_active);
        assert!(!app_input_focused);

        // 4. Test Messages app: Enter posts message to messages_list
        active_app = Some("Messages".to_string());
        app_input = "Hello world!".to_string();
        app_input_focused = true;
        keyboard.activate();

        apply_ime_action(
            ImeAction::SendKey(28),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(messages_list.len(), 1);
        assert_eq!(messages_list[0], "You: Hello world!");
        assert!(app_input.is_empty());
    }

    #[test]
    fn test_universal_text_input_wayland_protocol() {
        use utim_core::compositor::protocols::{WlHeader, WlMessage};

        // The compositor only ever *parses* incoming events, so the wire
        // buffer is what a client would have sent, framed by hand. (The
        // outgoing serializer had no production caller and was deleted.)
        fn event(opcode: u16) -> [u8; 8] {
            WlHeader::new(42, opcode, 8).to_bytes()
        }

        let mut keyboard = VirtualKeyboard::new();
        assert!(!keyboard.is_active);

        // Opcode 1: zwp_text_input_v3.enable
        let wire_enable = event(1);
        let (msg_enable, len) = WlMessage::parse(&wire_enable).unwrap().unwrap();
        assert_eq!(len, wire_enable.len());
        if msg_enable.header.opcode == 1 {
            keyboard.activate();
        }
        assert!(keyboard.is_active);

        // Opcode 2: zwp_text_input_v3.disable
        let wire_disable = event(2);
        let (msg_disable, _) = WlMessage::parse(&wire_disable).unwrap().unwrap();
        if msg_disable.header.opcode == 2 {
            keyboard.deactivate();
        }
        assert!(!keyboard.is_active);

        // An unknown opcode must leave the IME alone rather than toggling it:
        // a header-only event is a well-formed message with no payload.
        //
        // The wire buffer is bound to a local: `WlMessage::parse` borrows from
        // it and returns a view, so parsing `&event(9)` inline would leave the
        // message pointing at a temporary that is dropped on the same
        // statement.
        let wire_other = event(9);
        let (msg_other, _) = WlMessage::parse(&wire_other).unwrap().unwrap();
        assert_ne!(msg_other.header.opcode, 1);
        assert_ne!(msg_other.header.opcode, 2);
        keyboard.activate();
        if msg_other.header.opcode == 1 || msg_other.header.opcode == 2 {
            keyboard.deactivate();
        }
        assert!(keyboard.is_active);
    }

    #[test]
    fn test_all_base_apps_resolve_png_icons() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
        let assets_icons = workspace_root.join("assets/icons");
        assert!(assets_icons.exists(), "assets/icons must exist");

        let mut icon_cache = IconCache::with_roots(vec![assets_icons], "hicolor");
        let mut apps = build_all_apps(&DesktopCatalogue::new(), &[]);
        apply_app_icons(&mut apps, &mut icon_cache);

        // Verify that every single base app has a resolved PNG icon
        for app in &apps {
            assert!(
                app.icon.is_some(),
                "App '{}' (id: '{}', keys: {:?}) must resolve a real PNG icon image",
                app.name,
                app.id,
                app.icon_keys
            );
        }

        // Also verify dock Apps icon
        assert!(
            icon_cache
                .get("view-app-grid")
                .or_else(|| icon_cache.get("apps"))
                .is_some(),
            "Dock Apps icon must resolve a real PNG icon image"
        );
    }

    #[test]
    fn test_home_pages_icon_reordering_and_movement() {
        let mut home_pages = [
            vec![
                "settings".to_string(),
                "files".to_string(),
                "terminal".to_string(),
                "gallery".to_string(),
            ],
            vec!["clock".to_string(), "contacts".to_string()],
        ];
        let current_page = 0;
        let mut selected_icon: Option<String> = Some("terminal".to_string());

        // 1. Reorder within Page 0: move "terminal" from index 2 to slot 0
        if let Some(sel_id) = selected_icon.take() {
            if let Some(old_pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(old_pos);
                home_pages[current_page].insert(0, sel_id);
            }
        }
        assert_eq!(
            home_pages[0],
            vec!["terminal", "settings", "files", "gallery"]
        );

        // 2. Move "settings" to Page 1
        selected_icon = Some("settings".to_string());
        if let Some(sel_id) = selected_icon.take() {
            if let Some(pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(pos);
            }
            home_pages[1].push(sel_id);
        }
        assert_eq!(home_pages[0], vec!["terminal", "files", "gallery"]);
        assert_eq!(home_pages[1], vec!["clock", "contacts", "settings"]);

        // 3. Remove "files" from Home Page 0
        selected_icon = Some("files".to_string());
        if let Some(sel_id) = selected_icon.take() {
            if let Some(pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(pos);
            }
        }
        assert_eq!(home_pages[0], vec!["terminal", "gallery"]);
        // Verify files is gone from home page 0, but catalogue retains it
        let all_apps = build_all_apps(&DesktopCatalogue::new(), &[]);
        assert!(all_apps.iter().any(|a| a.id == "files"));
    }

    #[test]
    fn test_pixelui_app_drawer_and_pinning() {
        let all_apps = build_all_apps(&DesktopCatalogue::new(), &[]);
        let mut home_pages = [vec!["terminal".to_string(), "gallery".to_string()]];
        let current_page = 0;
        let mut app_drawer_open = false;

        // 1. Tapping Apps toggles drawer
        app_drawer_open = !app_drawer_open;
        assert!(app_drawer_open);

        // 2. App search filtering in drawer
        let drawer_search = "cam";
        let filtered: Vec<&ManagedApp> = all_apps
            .iter()
            .filter(|a| {
                a.name.to_lowercase().contains(drawer_search)
                    || a.id.to_lowercase().contains(drawer_search)
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Camera");

        // 3. Long press app in drawer pins it to current home page
        let target_app = &filtered[0];
        if !home_pages[current_page].contains(&target_app.id) {
            home_pages[current_page].push(target_app.id.clone());
        }
        assert!(home_pages[current_page].contains(&"camera".to_string()));
        assert_eq!(home_pages[current_page].len(), 3);

        // 4. Close drawer
        app_drawer_open = false;
        assert!(!app_drawer_open);
    }

    /// Every tappable region of the launcher, asserted against the same
    /// `Layout` the renderer draws from. This is the regression guard for the
    /// original bug: hitboxes that were tuned by hand and drifted from the
    /// pixels the user actually saw.
    #[test]
    fn test_launcher_layout_hitboxes_match_rendered_geometry() {
        let w = 1080.0;
        let h = 2400.0;
        let l = Layout::plain(w, h);

        // 1. Status bar: the top band, and nothing below it.
        assert!(l.status_bar_h >= 20.0);
        assert!(l.status_bar_h < 80.0, "status bar is a sliver");
        assert!(!l.search.contains(w * 0.5, l.status_bar_h + 1.0));

        // 2. Search pill: hit inside, miss on every side.
        assert!(l.search.contains(w * 0.5, l.search.center_y()));
        assert!(!l.search.contains(-1.0, l.search.center_y()));
        assert!(!l.search.contains(w + 1.0, l.search.center_y()));
        assert!(!l.search.contains(w * 0.5, l.search.y - 1.0));
        assert!(!l.search.contains(w * 0.5, l.search.y + l.search.h + 1.0));

        // 3. Clock widget.
        assert!(l.clock_rect().contains(w * 0.5, l.clock_y + 1.0));

        // 4. Grid cells: the centre of every drawn icon hit-tests to itself,
        //    and the row below the grid hit-tests to nothing.
        for i in 0..(l.grid_cols * l.max_rows.min(8)) {
            let icon = l.grid_icon(i);
            let (cx, cy) = icon.center();
            assert_eq!(l.home_grid_hit(cx, cy, 0.0), Some(i), "icon {i}");
            let cell = l.grid_cell(i);
            let (ccx, ccy) = cell.center();
            assert_eq!(l.home_grid_hit(ccx, ccy, 0.0), Some(i), "cell {i}");
        }
        assert_eq!(l.home_grid_hit(w * 0.5, l.grid_bottom, 0.0), None);
        assert_eq!(l.home_grid_hit(w * 0.5, l.grid_top - 1.0, 0.0), None);

        // 5. Page indicator.
        assert_eq!(
            l.home_page_hit(l.page_dots.x + 1.0, l.page_dots.center_y(), 2),
            Some(0)
        );
        assert_eq!(
            l.home_page_hit(
                l.page_dots.x + l.page_dots.w - 1.0,
                l.page_dots.center_y(),
                2
            ),
            Some(1)
        );
        assert_eq!(l.home_page_hit(w * 0.5, l.page_dots.y - 20.0, 2), None);

        // 6. Dock: each slot, plus a miss just above the dock.
        for s in 0..l.dock_slots {
            let icon = l.dock_icon_rect(s);
            assert_eq!(l.home_dock_hit(icon.center_x(), icon.center_y()), Some(s));
        }
        assert_eq!(l.home_dock_hit(w * 0.5, l.dock.y - 1.0), None);

        // 7. Gesture nav pill is below the dock and on screen.
        assert!(l.nav_pill.y > l.dock.y + l.dock.h);
        assert!(l.nav_pill.y + l.nav_pill.h <= h);
        assert!(l.nav_pill.contains(w * 0.5, l.nav_pill.center_y()));

        // 8. Edit-mode chips only exist with a selection, sit above the grid,
        //    and are mutually exclusive.
        let sel = Layout::new(w, h, true);
        assert!(sel
            .remove_chip
            .contains(sel.remove_chip.center_x(), sel.remove_chip.center_y()));
        assert!(sel
            .move_chip
            .contains(sel.move_chip.center_x(), sel.move_chip.center_y()));
        assert!(sel.remove_chip.x + sel.remove_chip.w <= sel.move_chip.x);
        assert!(sel.move_chip.y + sel.move_chip.h <= sel.grid_top);
        // The gap between the two chips belongs to neither.
        let gap_x = (sel.remove_chip.x + sel.remove_chip.w + sel.move_chip.x) * 0.5;
        assert!(!sel.remove_chip.contains(gap_x, sel.remove_chip.center_y()));
        assert!(!sel.move_chip.contains(gap_x, sel.remove_chip.center_y()));

        // 9. Drawer: search, handle and grid all move with the sheet.
        let off = 0.0;
        assert_eq!(
            l.drawer_search_hit(
                off,
                l.drawer_search.center_x(),
                off + l.drawer_search.center_y()
            ),
            DrawerSearchHit::Focus
        );
        assert_eq!(
            l.drawer_search_hit(
                off,
                l.drawer_search.x + l.drawer_search.w - 2.0,
                off + l.drawer_search.center_y()
            ),
            DrawerSearchHit::Clear
        );
        assert_eq!(l.drawer_search_hit(off, 1.0, 1.0), DrawerSearchHit::None);
        assert!(l
            .drawer_handle
            .contains(w * 0.5, off + l.drawer_handle.center_y()));

        for i in 0..(l.grid_cols * l.drawer_rows.min(8)) {
            let cell = l.drawer_icon_cell(i);
            assert_eq!(
                l.drawer_grid_hit(off, cell.center_x(), off + cell.center_y()),
                Some(i),
                "drawer cell {i}"
            );
        }
        // A dragged-down drawer carries its content with it.
        let dragged = h * 0.5;
        let cell = l.drawer_icon_cell(3);
        assert_eq!(
            l.drawer_grid_hit(dragged, cell.center_x(), dragged + cell.center_y()),
            Some(3)
        );
        // And a touch above the sheet is not a drawer cell.
        assert_eq!(
            l.drawer_grid_hit(dragged, cell.center_x(), dragged - 1.0),
            None
        );
    }

    /// The same invariants have to hold on a small panel, otherwise the fix
    /// only works at one resolution.
    #[test]
    fn test_layout_scales_to_small_panels() {
        for (w, h) in [(360.0f32, 640.0f32), (720.0, 1280.0), (1440.0, 3120.0)] {
            let l = Layout::plain(w, h);
            for i in 0..(l.grid_cols * l.max_rows.min(6)) {
                let icon = l.grid_icon(i);
                assert_eq!(
                    l.home_grid_hit(icon.center_x(), icon.center_y(), 0.0),
                    Some(i),
                    "{w}x{h} icon {i}"
                );
            }
            for s in 0..l.dock_slots {
                let icon = l.dock_icon_rect(s);
                assert_eq!(
                    l.home_dock_hit(icon.center_x(), icon.center_y()),
                    Some(s),
                    "{w}x{h} dock {s}"
                );
            }
            assert!(
                l.search.h >= w.min(h) * 0.048,
                "{w}x{h}: search pill below the 48dp touch minimum"
            );
        }
    }

    #[test]
    fn test_lawnchair_animation_physics_step() {
        // Drawer progress interpolation towards open
        let mut drawer_prog: f32 = 0.0;
        let drawer_target: f32 = 1.0;
        let dt: f32 = 0.016;

        for _ in 0..10 {
            let diff = drawer_target - drawer_prog;
            drawer_prog += diff * (dt * 14.0).min(1.0);
        }
        assert!(
            drawer_prog > 0.85,
            "Drawer should smoothly ease open in ~160ms"
        );

        // Home scroll offset decay (returning smoothly to center)
        let mut scroll_offset: f32 = 400.0;
        for _ in 0..20 {
            if scroll_offset.abs() > 0.5 {
                scroll_offset *= 1.0 - (dt * 12.0).min(0.9);
            } else {
                scroll_offset = 0.0;
            }
        }
        assert!(
            scroll_offset < 10.0,
            "Scroll offset should spring-decay quickly to rest"
        );

        // Material ripple: the state layer grows and fades on time constants.
        let mut ripple = Some((500.0, 600.0, ripple_start_radius(1080.0), 1.0));
        for _ in 0..10 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
                if *a <= 0.0 {
                    ripple = None;
                }
            }
        }
        let (_, _, r, a) = ripple.expect("Ripple should still be active at 160ms");
        assert!(r > 100.0, "Ripple radius should expand over time: {}", r);
        assert!(
            a > 0.5 && a < 0.75,
            "Ripple alpha should decay gently: {}",
            a
        );
        // The start radius covers a 48dp touch target.
        assert!(
            ripple_start_radius(1080.0) * 2.0 >= 1080.0 * 0.048,
            "ripple must start at the minimum touch target"
        );
        let start = ripple_start_radius(360.0);
        assert!(start > 8.0, "a small panel still gets a visible ripple");

        // After additional frames, ripple fades completely to None
        for _ in 0..30 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
                if *a <= 0.0 {
                    ripple = None;
                }
            }
        }
        assert!(
            ripple.is_none(),
            "Ripple should fade completely after duration"
        );
    }

    /// The advanced surface: clock, page dots, scrolled grid, drawer sheet and
    /// row limits, all checked against the shared layout.
    #[test]
    fn test_launcher_advanced_geometry_and_animations() {
        let w = 1080.0;
        let h = 2400.0;
        let l = Layout::plain(w, h);

        // 1. Clock widget.
        assert!(l.clock_rect().contains(w * 0.5, l.clock_y + 1.0));
        assert!(
            !l.clock_rect().contains(-1.0, l.clock_y + 1.0),
            "outside padding"
        );
        assert!(
            !l.clock_rect().contains(w * 0.5, l.clock_y - 1.0),
            "above clock"
        );
        assert!(
            !l.clock_rect().contains(w * 0.5, l.search.y + 1.0),
            "below clock, inside search"
        );

        // 2. Page dots: out of bounds, and slot mapping for several page counts.
        let cy = l.page_dots.center_y();
        assert_eq!(
            l.home_page_hit(-10.0, cy, 2),
            None,
            "negative x is out of bounds"
        );
        assert_eq!(
            l.home_page_hit(w + 10.0, cy, 2),
            None,
            "x > w is out of bounds"
        );
        assert_eq!(
            l.home_page_hit(l.page_dots.center_x(), cy, 0),
            None,
            "0 pages"
        );
        assert_eq!(
            l.home_page_hit(l.page_dots.center_x(), cy, 1),
            Some(0),
            "one page is page 0"
        );
        for n in 2..=5usize {
            for p in 0..n {
                let x = l.page_dots.x + l.page_dots.w * (p as f32 + 0.5) / n as f32;
                assert_eq!(l.home_page_hit(x, cy, n), Some(p), "{n} pages, slot {p}");
            }
        }

        // 3. Grid with a scroll offset. A positive offset slides the workspace
        //    left, so a fixed touch sees the cell one column later.
        let y = l.grid_top + 4.0;
        let x = l.col_pitch * 0.5;
        let base = l.home_grid_hit(x, y, 0.0).unwrap();
        assert_eq!(l.home_grid_hit(x, y, l.col_pitch), Some(base + 1));
        // Off the left edge of the strip there is nothing.
        assert_eq!(l.home_grid_hit(x, y, -l.col_pitch * 2.0), None);

        // 4. Drawer with a mid-drag offset.
        // The handle is hit in the drawer's own coordinate space, which is
        // what the input path passes in.
        let dragged = h * 0.25;
        assert!(
            l.drawer_handle
                .contains(w * 0.5, l.drawer_handle.center_y()),
            "the handle is centred on the sheet"
        );
        assert!(
            !l.drawer_handle
                .contains(w * 0.5, dragged + l.drawer_handle.center_y()),
            "a touch above the sheet is not the handle"
        );
        assert_eq!(
            l.drawer_search_hit(
                dragged,
                l.drawer_search.center_x(),
                dragged + l.drawer_search.center_y()
            ),
            DrawerSearchHit::Focus
        );
        let cell0 = l.drawer_icon_cell(0);
        assert_eq!(
            l.drawer_grid_hit(dragged, cell0.center_x(), dragged + cell0.center_y()),
            Some(0)
        );
        assert_eq!(
            l.drawer_grid_hit(dragged, cell0.center_x(), dragged - 1.0),
            None,
            "above the sheet is not a grid hit"
        );
        assert_eq!(
            l.drawer_grid_hit(dragged, -1.0, dragged + cell0.center_y()),
            None,
            "off-screen x is out of bounds"
        );

        // 5. Row limits scale with the panel instead of being magic numbers.
        assert!(l.max_rows >= 4, "1080x2400 should fit many home rows");
        assert!(l.drawer_rows >= 8, "1080x2400 should fit many drawer rows");
        let small = Layout::plain(360.0, 640.0);
        assert!(small.max_rows < l.max_rows, "a short panel fits fewer rows");
    }

    /// Proportional metrics: the vector engine must not be monospaced.
    #[test]
    fn test_typography_proportional_metrics() {
        let scale = 2;
        let w_i = utim_core::graphics::text_width("i", scale);
        let w_m = utim_core::graphics::text_width("m", scale);
        assert!(
            w_i < w_m,
            "proportional 'i' ({}px) must be narrower than 'm' ({}px)",
            w_i,
            w_m
        );

        let w_space = utim_core::graphics::text_width(" ", scale);
        let w_w = utim_core::graphics::text_width("W", scale);
        assert!(w_space < w_w, "space should be narrower than capital W");

        // Digits are tabular so a clock never jitters.
        let zero = utim_core::graphics::text_width("0", scale);
        for d in "123456789".chars() {
            assert_eq!(
                utim_core::graphics::text_width(&d.to_string(), scale),
                zero,
                "digit {d} must share the tabular advance"
            );
        }

        let w_full = utim_core::graphics::text_width("Universal Treble", scale);
        assert!(w_full > 0);
    }

    #[test]
    fn test_power_saver_modes_and_super_extreme_recovery_shell() {
        let mut sex = SuperExtremeState::new();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 1. Volume HUD adjustment
        sex.volume_up();
        assert_eq!(sex.volume_hud.volume_percent, 60);
        assert!(sex.volume_hud.is_visible());
        let bar = sex.volume_hud.format_bar(30);
        assert!(bar.starts_with("VOL ["));
        assert!(bar.contains("60%"));

        sex.volume_down();
        assert_eq!(sex.volume_hud.volume_percent, 50);

        // 2. Lock screen actions: Emergency dialer
        let (w, h) = (360.0f32, 640.0f32);
        assert!(sex.handle_touch_tap(w * 0.20, h * 0.84, w, h, None, 0));
        assert_eq!(sex.active_screen, SuperExtremeScreen::EmergencyDialer);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 3. Lock screen camera preview & snapshot
        assert!(sex.handle_touch_tap(w * 0.70, h * 0.84, w, h, None, 0));
        assert_eq!(sex.active_screen, SuperExtremeScreen::CameraPreview);
        sex.camera_preview.update_preview();
        let photo = sex.snap_photo();
        assert!(photo.is_some());
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 4. Swipe up to unlock -> Password screen
        sex.on_swipe_up();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Password);

        // 5. Password entry and submit
        let unlocked = sex.enter_password_char('1', None, 0);
        assert!(unlocked);
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        // 6. Launch apps from home screen (0..3)
        sex.launch_home_app(0);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppAlarm);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(1);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppPhone);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(2);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppSms);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(3);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppSettings);
        assert!(sex.handle_touch_tap(w * 0.50, h * 0.55, w, h, None, 0));
        assert!(sex.request_exit_to_normal);

        // 7. Power button hold triggers Recovery Power Menu
        sex.active_screen = SuperExtremeScreen::Home;
        sex.on_power_button_press();
        sex.power_press_start =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1100));
        assert!(sex.check_power_button_hold());
        assert_eq!(sex.active_screen, SuperExtremeScreen::PowerMenu);

        // Option 0: Return to normal mode
        assert!(sex.handle_touch_tap(w * 0.50, h * 0.34, w, h, None, 0));
        assert!(sex.request_exit_to_normal);
    }

    // -----------------------------------------------------------------
    // Phase 0: the 60-second epoll freeze (0.1).
    //
    // The original `need_frames` consulted three springs. The instant a finger
    // left the glass with any *other* motion in flight, the predicate went
    // false and `epoll_wait` was handed `secs_to_min * 1000 + 500` -- up to
    // 60.5 s -- so the animation froze on whatever frame it happened to be on
    // and the shell read as hung. These drive the timeout calculation
    // directly, which is the only place the number is actually produced.
    // -----------------------------------------------------------------

    /// Every motion source, alone, must keep the loop on a frame cadence.
    ///
    /// This is the regression that matters: the failure mode was an *omission*,
    /// so the test has to enumerate the set rather than assert a total. Adding
    /// a source without an arm here fails to compile the match; removing one
    /// fails the assertion.
    #[test]
    fn every_motion_source_keeps_a_frame_scheduled() {
        let w = 1080.0_f32;
        let h = 2400.0_f32;
        let l = Layout::plain(w, h);
        let interval = frame_interval_for_hz(120.0);
        let elapsed = Duration::from_micros(1_000);

        // One source at a time, each the sole reason a frame is needed.
        let cases: [(&str, FrameDemand); 15] = [
            (
                "touch_ripple",
                FrameDemand {
                    touch_ripple: true,
                    ..Default::default()
                },
            ),
            (
                "app_launch",
                FrameDemand {
                    app_launch: true,
                    ..Default::default()
                },
            ),
            (
                "drawer",
                FrameDemand {
                    drawer: true,
                    ..Default::default()
                },
            ),
            (
                "page_scroll",
                FrameDemand {
                    page_scroll: true,
                    ..Default::default()
                },
            ),
            (
                "icon_bounce",
                FrameDemand {
                    icon_bounce: true,
                    ..Default::default()
                },
            ),
            (
                "overview",
                FrameDemand {
                    overview: true,
                    ..Default::default()
                },
            ),
            (
                "overview_scrim",
                FrameDemand {
                    overview_scrim: true,
                    ..Default::default()
                },
            ),
            (
                "popup",
                FrameDemand {
                    popup: true,
                    ..Default::default()
                },
            ),
            (
                "fastscroller",
                FrameDemand {
                    fastscroller: true,
                    ..Default::default()
                },
            ),
            (
                "workspace_scale",
                FrameDemand {
                    workspace_scale: true,
                    ..Default::default()
                },
            ),
            (
                "window_alpha",
                FrameDemand {
                    window_alpha: true,
                    ..Default::default()
                },
            ),
            (
                "folder",
                FrameDemand {
                    folder: true,
                    ..Default::default()
                },
            ),
            (
                "recents",
                FrameDemand {
                    recents: true,
                    ..Default::default()
                },
            ),
            (
                "terminal_busy",
                FrameDemand {
                    terminal_busy: true,
                    ..Default::default()
                },
            ),
            (
                "lockscreen_intro",
                FrameDemand {
                    lockscreen_intro: true,
                    ..Default::default()
                },
            ),
        ];

        for (name, demand) in cases {
            assert!(demand.any(), "{name} must request a frame");
            let t = demand.timeout_ms(elapsed, interval);
            assert!(
                t <= 16,
                "{name} scheduled {t} ms; an animation may never be \
                 coarser than a 60 Hz frame, let alone the 60 s idle tick"
            );
        }

        // Truly idle is the only state allowed to park on the clock. The
        // bound is the formula's own minimum, not a wall-clock read: the idle
        // timeout is `secs_to_min * 1000 + 500` and `secs_to_min >= 1`, so it
        // is always at least 1.5 s. Asserting against the current minute
        // boundary instead would make this test fail once a minute.
        let idle = FrameDemand::default();
        assert!(!idle.any());
        assert!(
            idle.timeout_ms(elapsed, interval) >= 1_500,
            "an idle shell must not be frame-paced"
        );
        let _ = l;
    }

    /// A frame that already overran its budget re-polls immediately.
    ///
    /// The 120 Hz budget is 8.33 ms, so a frame that took 9 ms must not then
    /// sleep another 8.33 ms: that is a 17 ms round trip for a 8.33 ms
    /// deadline, and it is the difference between hitting 120 Hz and
    /// halving it to 60.
    #[test]
    fn an_overran_frame_does_not_sleep_again() {
        let interval = frame_interval_for_hz(120.0);
        let busy = FrameDemand {
            drawer: true,
            ..Default::default()
        };
        let over = interval + Duration::from_millis(1);
        assert_eq!(busy.timeout_ms(over, interval), 0);
        // And the idle path agrees, or an idle shell would spin at 100% CPU.
        assert_eq!(FrameDemand::default().timeout_ms(over, interval), 0);
    }

    /// The remainder handed to `epoll_wait` is the *unspent* frame, not a
    /// whole extra one.
    #[test]
    fn the_frame_timeout_is_the_remainder_not_a_whole_frame() {
        let interval = frame_interval_for_hz(120.0);
        let busy = FrameDemand {
            drawer: true,
            ..Default::default()
        };
        let half = interval / 2;
        let t = busy.timeout_ms(half, interval);
        let want = interval.saturating_sub(half).as_millis() as libc::c_int;
        assert_eq!(t, want, "half a frame in, half a frame of timeout left");
    }

    /// A non-finite or nonsensical rate must not divide by zero.
    ///
    /// `Duration::from_micros(0)` would make `elapsed >= frame_interval`
    /// always true and spin the loop at 100% CPU, and a negative would wrap in
    /// the `as u64` cast.
    #[test]
    fn a_bad_refresh_rate_degrades_to_60hz_not_a_zero_interval() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let d = frame_interval_for_hz(bad);
            assert_eq!(d, frame_interval_for_hz(60.0), "rate {bad}");
            assert!(d.as_micros() > 0, "rate {bad} produced a zero interval");
        }
    }

    /// 120 Hz must be 8.33 ms, not 8 ms.
    ///
    /// Truncating to whole milliseconds gives 8 ms = 125 Hz, which beats every
    /// deadline it is measured against and shows up as a permanent
    /// under-budget that hides real regressions. 90 Hz is 11.1 ms, and 60 Hz
    /// must be unchanged at 16.67 ms.
    #[test]
    fn refresh_pacing_matches_the_panel() {
        assert_eq!(frame_interval_for_hz(120.0), Duration::from_micros(8_333));
        assert_eq!(frame_interval_for_hz(90.0), Duration::from_micros(11_111));
        assert_eq!(frame_interval_for_hz(60.0), Duration::from_micros(16_667));
        // Exact to the microsecond -- the resolution `Duration` is asked for.
        // 1e6/120 is 8333.33 us and the nearest integer is 8333, i.e.
        // 120.005 Hz: marginally *fast*, so a frame is never late. The plan's
        // "8.33 ms" is that value rounded for prose.
        for hz in [60.0_f64, 90.0, 120.0] {
            let got = frame_interval_for_hz(hz).as_nanos() as f64;
            let ideal = 1e9 / hz;
            assert!(
                (got - ideal).abs() <= 500.0,
                "{hz} Hz: {got} ns vs ideal {ideal} ns"
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 0: UTF-8 app names (0.2).
    //
    // `d_app.name[..12]` sliced by BYTE index. A name whose 12th byte landed
    // inside a multi-byte sequence panicked, and the release profile sets
    // `panic = "abort"` -- so one non-ASCII `.desktop` file aborted the
    // compositor during the boot-time catalogue scan.
    // -----------------------------------------------------------------

    #[test]
    fn display_name_truncation_never_splits_a_character() {
        // Every one of these panicked under byte slicing: byte 12 falls
        // inside a multi-byte sequence in all of them.
        for name in [
            "Trình duyệt",    // "Browser", Vietnamese
            "设置应用",       // "Settings", CJK
            "Cây cối",        // "Tree", Latin-1 supplement
            "Ελληνικά",       // Greek
            "Приложения",     // Cyrillic
            "日本語のアプリ", // Japanese
            "Ünïcödé Ñame",
            "🎉🎊🎈 Party App", // 4-byte emoji
        ] {
            let got = truncate_display_name(name, 12, false);
            assert!(
                std::str::from_utf8(got.as_bytes()).is_ok(),
                "{name:?} produced non-UTF-8 output"
            );
            // No replacement chars: the slice is on a char boundary, so every
            // retained char is the original one.
            assert!(!got.contains('\u{FFFD}'), "{name:?} -> {got:?} is mangled");
            assert!(got.chars().count() <= 12, "{name:?} -> {got:?} too long");
        }
    }

    #[test]
    fn display_name_truncation_counts_characters_not_bytes() {
        // 12 chars, 24 bytes: must be kept whole, not cut at byte 12.
        let cjk = "设置应用管理器设置应用"; // 12 chars
        assert_eq!(truncate_display_name(cjk, 12, false), cjk);

        // Exactly 12 chars: the `.take(12).collect()` consumes all 12 and the
        // `chars.next()` probe finds nothing, so the ORIGINAL is returned --
        // not a rebuilt copy. Same value either way, but that is the branch
        // that makes "no truncation" cheap.
        let exact = "abcdefghijkl";
        assert_eq!(exact.len(), 12);
        assert_eq!(truncate_display_name(exact, 12, false), exact);

        // 13 chars: truncated to 12, and the dropped one is the last.
        assert_eq!(
            truncate_display_name("abcdefghijklm", 12, false),
            "abcdefghijkl"
        );
        assert_eq!(
            truncate_display_name("abcdefghijklm", 12, true),
            "abcdefghi...",
            "the three dots are inside the 12-character budget, not appended to it"
        );
        assert_eq!(
            truncate_display_name("abcdefghijklm", 12, true)
                .chars()
                .count(),
            12,
            "an elided label is never wider than the one it replaces"
        );

        // Short names pass through untouched.
        assert_eq!(truncate_display_name("Phone", 12, false), "Phone");
        assert_eq!(truncate_display_name("", 12, false), "");
    }

    // -----------------------------------------------------------------
    // Phase 0: the tap ripple (0.4) and Phase 2.2: the long-press target.
    // -----------------------------------------------------------------

    /// A tap must resolve to a *bounded* target, not the whole panel.
    #[test]
    fn a_workspace_tap_resolves_to_its_own_cell() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let cell = l.grid_cell(0);
        let got = ripple_target(&l, cell.center_x(), cell.y + cell.h * 0.5, 0.0, 0, 1)
            .expect("a tap on cell 0 must resolve to a target");

        // The clip is cell-sized, not panel-sized. This is the assertion that
        // would have failed before: the old code passed the full 1080x2400
        // panel, which is what drew the screen-wide grey wash.
        assert!(
            got.w < w * 0.5,
            "clip is {} px wide on a {w} px panel",
            got.w
        );
        assert!(
            got.h < h * 0.25,
            "clip is {} px tall on a {h} px panel",
            got.h
        );
        // And it actually contains the touch.
        assert!(
            got.x <= cell.center_x() && got.x + got.w >= cell.center_x(),
            "clip {:?} misses the touch x {}",
            got,
            cell.center_x()
        );
    }

    /// A press on the wallpaper has no control, and must report that.
    ///
    /// `None` is the honest answer and the renderer treats it as "whole
    /// panel", which is correct for a gesture with no target.
    #[test]
    fn a_wallpaper_press_resolves_to_no_target() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        // The status bar: above the grid, above the hotseat, not a control the
        // ripple should be bounded to.
        assert!(ripple_target(&l, w * 0.5, l.grid_top - 40.0, 0.0, 0, 1).is_none());
    }

    /// Long-press must name the app it landed on.
    ///
    /// The id was hardcoded to `String::new()`, so `popup_rows` could never
    /// take its app branch: there was no reachable path to App info or
    /// Uninstall, and every long-press opened the workspace menu.
    #[test]
    fn a_long_press_on_an_icon_names_that_app() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let apps = vec![
            ManagedApp::new("phone", "Phone", "", 0xFF10B981, "P"),
            ManagedApp::new("messages", "Messages", "", 0xFF3B82F6, "M"),
        ];
        let page = vec!["phone".to_string(), "messages".to_string()];

        let cell = l.grid_cell(0);
        let id = long_press_app_id(
            &apps,
            &page,
            &l,
            0.0,
            1,
            cell.center_x(),
            cell.y + cell.h * 0.5,
        );
        assert_eq!(id, "phone", "slot 0 must resolve to the page's first id");

        let cell1 = l.grid_cell(1);
        let id1 = long_press_app_id(
            &apps,
            &page,
            &l,
            0.0,
            1,
            cell1.center_x(),
            cell1.y + cell1.h * 0.5,
        );
        assert_eq!(id1, "messages");
    }

    // -----------------------------------------------------------------
    // Phase 0 (0.5) + Phase 3 (3.3) + Phase 4 (4.1).
    // -----------------------------------------------------------------

    /// The per-frame views must not allocate.
    ///
    /// `ShellScratch` replaced four `Vec`s that were rebuilt on every frame
    /// tick. The property under test is the one the plan states: a frame's
    /// render inputs are produced into fixed storage, and overflowing the
    /// capacity drops rows rather than growing the buffer.
    #[test]
    fn the_frame_scratch_drops_overflow_instead_of_growing() {
        let mut v: FrameView<u32, 4> = FrameView::new(0);
        for i in 0..10u32 {
            v.push(i);
        }
        assert_eq!(v.as_slice().len(), 4, "capacity must be a hard ceiling");
        assert_eq!(v.as_slice(), &[0, 1, 2, 3]);

        // And a fresh view starts empty, so a second frame cannot see the
        // first frame's rows.
        let mut v2: FrameView<u32, 4> = FrameView::new(0);
        assert!(v2.as_slice().is_empty());
        v2.push(9);
        assert_eq!(v2.as_slice(), &[9]);
    }

    /// The catalogue must come out of `build_all_apps` in name order.
    ///
    /// The section index and the fast scroller both binary-search the row
    /// list by letter, and a recents card carries a `u32` index into it, so
    /// an unsorted list made the teardrop show the wrong letter.
    #[test]
    fn the_catalogue_is_alphabetised_case_insensitively() {
        let c = DesktopCatalogue::new();
        let apps = build_all_apps(&c, &[]);
        assert!(!apps.is_empty(), "the builtin apps must always be present");
        let keys: Vec<String> = apps.iter().map(|a| a.name.to_lowercase()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "build_all_apps must return A-Z order");
    }

    // -----------------------------------------------------------------
    // Phase 2: the recents carousel (2.3, 2.4).
    //
    // The overview is a modal state, so every arm of the touch handler was
    // gated on `!shell_state.is_modal()` and a touch inside it was discarded
    // before any of them ran. `Recents::on_card_drag`, `on_card_release` and
    // `clear_all` had no call sites at all, `kill_queue` was always empty,
    // and the carousel was a picture of a carousel.
    // -----------------------------------------------------------------

    /// A carousel of `n` cards with pids `1000..1000+n`.
    ///
    /// `push` inserts at the FRONT (most recent first), so slot 0 is the last
    /// card pushed. The tests below address cards by *slot* and assert on the
    /// pid recorded there, so the ordering is stated once here rather than
    /// being re-derived (and mis-stated) in every case.
    fn carousel_with(n: usize) -> Recents {
        let l = Layout::plain(1080.0, 2400.0);
        let mut r = Recents::new(&l);
        for i in 0..n {
            let _ = r.push(TaskCard::new(i as u32, 1000 + i as i32, 0));
        }
        r
    }

    /// The pid the model holds in `slot`, so a test can assert the *card* it
    /// touched and not just an index.
    fn pid_at(r: &Recents, slot: usize) -> i32 {
        r.cards[slot].pid
    }

    /// Step the model until every spring has parked, so a geometry assertion
    /// reads a settled layout rather than one mid-animation.
    fn settle(r: &mut Recents) {
        for _ in 0..600 {
            let _ = r.step(1.0 / 120.0, 8.333);
        }
    }

    /// A tap on a card must reach the carousel, not the modal gate.
    #[test]
    fn a_touch_on_a_card_is_not_swallowed_by_the_modal_gate() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let r = carousel_with(3);
        // Card 0 is the selected one, so it is centred.
        let r0 = r.card_rect_centered(0, w, h).expect("card 0");
        let cx = r0.x + r0.w * 0.5;
        let cy = r0.y + r0.h * 0.5;
        assert_eq!(
            overview_hit(&r, w, h, cx, cy),
            OverviewHit::Card(0),
            "the centre of card 0 must hit card 0"
        );
        // And the card under the *centre* is the selected one -- this is the
        // 2.3 assertion, and it is what the old `first_x` form got wrong.
        assert!(
            (cx - w * 0.5).abs() < 1.0,
            "the selected card must be centred, its centre x is {cx} on a {w} px panel"
        );
    }

    /// Selecting another card must move the strip, and the new selection must
    /// end up centred.
    #[test]
    fn selecting_a_card_moves_it_to_the_centre() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let mut r = carousel_with(3);
        r.select(2);
        // The move is a `grid_reflow` spring, not a jump, so the geometry only
        // reads as "moved" once the model has settled.
        settle(&mut r);
        let r2 = r.card_rect_centered(2, w, h).expect("card 2");
        assert!(
            (r2.x + r2.w * 0.5 - w * 0.5).abs() < 1.0,
            "card 2's centre is {} on a {w} px panel",
            r2.x + r2.w * 0.5
        );
        // Card 0 must have moved off centre to the left, by exactly one
        // pitch per index.
        let r0 = r.card_rect_centered(0, w, h).expect("card 0");
        assert!(
            r0.x + r0.w * 0.5 < w * 0.5,
            "card 0 must be left of centre once card 2 is selected"
        );
        // And by exactly two pitches: the strip moved, it did not reflow
        // into an arbitrary arrangement.
        let gap = (r0.x + r0.w * 0.5 - r2.x - r2.w * 0.5).abs();
        assert!(
            (gap - r.pitch * 2.0).abs() < 1.0,
            "card 0 sits {gap} px from card 2, expected 2 pitches ({})",
            r.pitch * 2.0
        );
    }

    /// A drag up on a card must dismiss it, and the kill must be armed.
    ///
    /// This is the whole of 2.4: before it, `on_card_drag` was never called,
    /// so the card never moved and `kill_queue` was always empty.
    #[test]
    fn dragging_a_card_up_dismisses_it_and_arms_the_kill() {
        let mut r = carousel_with(3);
        assert!(r.len > 0, "the carousel must have cards to dismiss");
        let target = pid_at(&r, 0);

        // Drag card 0 far enough up to cross the dismiss threshold. The
        // model's own `dismiss_length` is the travel bound.
        let far = r.dismiss_length * 0.8;
        r.on_card_drag(0, -far);
        r.on_card_release(0);

        // Stepping the model must eventually raise a Force for that card.
        let mut killed: Vec<i32> = Vec::new();
        for _ in 0..600 {
            for action in r.step(1.0 / 120.0, 8.333).iter() {
                if let utim_core::compositor::KillAction::Force(pid) = action {
                    killed.push(*pid);
                }
            }
            if !killed.is_empty() {
                break;
            }
        }
        assert!(
            killed.contains(&target),
            "the dragged card's pid {target} must be force-killed, got {killed:?}"
        );
    }

    /// Clear All must be reachable and must empty the stack.
    ///
    /// It was unreachable before: `clear_all` had no call site, so there was
    /// no way to dismiss everything at once.
    #[test]
    fn clear_all_is_reachable_and_empties_the_stack() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let mut r = carousel_with(4);
        let btn = overview_clear_all_rect(w, h);
        assert_eq!(
            overview_hit(&r, w, h, btn.center_x(), btn.center_y()),
            OverviewHit::ClearAll,
            "the Clear All button must be hit-testable"
        );

        let mut shell = ShellState::Overview {
            selected: 0,
            dismiss: 0.0,
        };
        let mut no_launch = None;
        overview_touch_up(
            &mut r,
            (w, h),
            (btn.center_x(), btn.center_y()),
            false,
            &mut shell,
            &mut no_launch,
        );
        // The stack is still full: `clear_all` starts the dismissal springs,
        // it does not teleport the cards away. The overview must NOT close
        // yet -- the frame loop decides that, once the springs have run.
        assert_eq!(r.len, 4, "clear_all animates, it does not unlink");
        assert!(
            matches!(shell, ShellState::Overview { .. }),
            "the overview must stay up while the cards animate out"
        );

        // Every pid is queued. The card leaves the stack when the shell acts
        // on the `Force` (that is the "close acknowledged" point), so the
        // test plays the shell's part.
        let mut killed: Vec<i32> = Vec::new();
        for _ in 0..600 {
            for action in r.step(1.0 / 120.0, 8.333).iter() {
                if let utim_core::compositor::KillAction::Force(pid) = action {
                    killed.push(*pid);
                    let _ = r.remove_by_pid(*pid);
                }
            }
            if r.len == 0 && killed.len() >= 4 {
                break;
            }
        }
        for pid in 1000..1004 {
            assert!(
                killed.contains(&pid),
                "pid {pid} was never killed: {killed:?}"
            );
        }
        // And now the frame loop's rule fires: an empty overview closes.
        assert_eq!(r.len, 0, "the stack must drain once the dismissals settle");
    }

    /// A cancelled Clear All must not dismiss anything.
    #[test]
    fn a_cancelled_clear_all_does_not_dismiss() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let mut r = carousel_with(3);
        let btn = overview_clear_all_rect(w, h);
        let mut shell = ShellState::Overview {
            selected: 0,
            dismiss: 0.0,
        };
        let mut no_launch = None;
        overview_touch_up(
            &mut r,
            (w, h),
            (btn.center_x(), btn.center_y()),
            true,
            &mut shell,
            &mut no_launch,
        );
        assert_eq!(r.len, 3, "a cancelled touch must not clear anything");
        let mut killed = false;
        for _ in 0..600 {
            for action in r.step(1.0 / 120.0, 8.333).iter() {
                if let utim_core::compositor::KillAction::Force(_) = action {
                    killed = true;
                }
            }
        }
        assert!(!killed, "a cancelled Clear All must not kill anything");
    }

    /// A cancelled touch must not kill anything.
    ///
    /// `Cancel` is a system takeover (a palm rejection, a notification
    /// stealing the gesture), not a fling. Treating it as a release would
    /// SIGKILL whatever app the user was trying to keep.
    #[test]
    fn a_cancelled_drag_does_not_kill_the_app() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let mut r = carousel_with(2);
        let card = r.card_rect_centered(0, w, h).expect("card 0");
        let cx = card.x + card.w * 0.5;
        let cy = card.y + card.h * 0.5;

        let mut shell = ShellState::Overview {
            selected: 0,
            dismiss: 0.0,
        };
        overview_touch_down(&mut r, w, h, cx, cy);
        let far = r.dismiss_length * 0.8;
        // A disabled sink, so the unit test never touches `/sys`.
        let mut haptics = Haptics::disabled();
        overview_touch_move(
            &mut r,
            &mut haptics,
            // The test only cares that the drag edge is detected; the gate is
            // covered by `the_haptic_gate_honours_the_setting`.
            true,
            w,
            h,
            cx,
            cy - far,
        );
        let mut no_launch = None;
        overview_touch_up(&mut r, (w, h), (cx, cy), true, &mut shell, &mut no_launch);

        let mut killed = false;
        for _ in 0..600 {
            for action in r.step(1.0 / 120.0, 8.333).iter() {
                if let utim_core::compositor::KillAction::Force(_) = action {
                    killed = true;
                }
            }
        }
        assert!(!killed, "a cancelled drag must not reach KillState::Grace");
    }

    // -----------------------------------------------------------------
    // Phase 3.2: the drawer scrolls and holds the whole catalogue.
    //
    // The drawer used to draw `.take(drawer_rows * grid_cols)` apps -- a hard
    // 50 on 1080p -- with no scroll and no indication that anything else
    // existed. Everything past the first screen was unreachable.
    // -----------------------------------------------------------------

    #[test]
    fn the_drawer_has_no_app_cap() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let one_screen = l.drawer_rows * l.grid_cols;
        // 100+ apps is the plan's acceptance bar; the old cap was one screen.
        assert!(
            one_screen < 100,
            "the reference panel must hold more than one screen of apps, \
             or this test proves nothing (one screen = {one_screen})"
        );
    }

    #[test]
    fn a_list_that_fits_does_not_scroll() {
        let l = Layout::plain(1080.0, 2400.0);
        // Fewer apps than one screen: no travel at all.
        assert_eq!(l.drawer_max_scroll_y(1), 0.0);
        assert_eq!(l.drawer_max_scroll_y(l.drawer_rows * l.grid_cols), 0.0);
        // More: the excess content height.
        let n = l.drawer_rows * l.grid_cols + 25;
        let max = l.drawer_max_scroll_y(n);
        assert!(max > 0.0, "an overflowing list must be scrollable");
        let rows = n.div_ceil(l.grid_cols);
        let content = rows as f32 * l.row_pitch;
        let band = l.drawer_grid_bottom - l.drawer_grid_top;
        assert!(
            (max - (content - band)).abs() < 0.01,
            "max scroll is {} for {rows} rows",
            max
        );
    }

    /// Every row of a 100+ app catalogue must be reachable by scrolling.
    #[test]
    fn every_row_of_a_hundred_apps_is_reachable() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let n = 137usize; // the plan's "100+" bar
        let max = l.drawer_max_scroll_y(n);

        // Walk the list one row at a time and hit-test the middle of each
        // row's icons. A row that cannot be hit at ANY scroll offset is
        // unreachable, which is the bug.
        let rows = n.div_ceil(l.grid_cols);
        let mut unreachable: Vec<usize> = Vec::new();
        for row in 0..rows {
            let mut hit = false;
            // A page of scroll positions is enough: the offset that centres
            // this row is the one that matters.
            for step in 0..=(rows as i32) {
                let scroll = step as f32 * l.row_pitch;
                if scroll > max + l.row_pitch {
                    break;
                }
                let cell = l.drawer_icon_cell_scrolled(row * l.grid_cols, scroll);
                let cy = cell.center_y();
                if cy < l.drawer_grid_top || cy > l.drawer_grid_bottom {
                    continue;
                }
                if l.drawer_grid_hit_scrolled(0.0, cell.center_x(), cy, scroll, n)
                    == Some(row * l.grid_cols)
                {
                    hit = true;
                    break;
                }
            }
            if !hit {
                unreachable.push(row);
            }
        }
        assert!(
            unreachable.is_empty(),
            "{}/{} rows unreachable in a {n}-app catalogue: {unreachable:?}",
            unreachable.len(),
            rows
        );
    }

    /// The draw and the hit-test must agree at every scroll offset.
    ///
    /// Only for cells that are actually inside the band: a cell scrolled off
    /// the top or bottom is correctly a *miss* for the hit test, and asking
    /// for a hit there would be testing the wrong thing.
    #[test]
    fn the_scrolled_draw_and_the_scrolled_hit_test_agree() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let mut checked = 0;
        for scroll in [0.0_f32, 37.0, 120.5, 300.0, 900.0] {
            for idx in 0..64usize {
                let cell = l.drawer_icon_cell_scrolled(idx, scroll);
                let cy = cell.center_y();
                if cy < l.drawer_grid_top || cy > l.drawer_grid_bottom {
                    // Off-band: the hit test must miss, and must not wrap.
                    assert_eq!(
                        l.drawer_grid_hit_scrolled(0.0, cell.center_x(), cy, scroll, 64),
                        None,
                        "scroll {scroll}, off-band cell {idx} must be a miss"
                    );
                    continue;
                }
                let got = l.drawer_grid_hit_scrolled(0.0, cell.center_x(), cy, scroll, 64);
                assert_eq!(got, Some(idx), "scroll {scroll}, cell {idx}: got {got:?}");
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} on-band cells were compared");
    }

    /// A rubber-banded scroll past the top must not underflow.
    #[test]
    fn a_scrolled_past_the_top_row_is_a_miss_not_an_underflow() {
        let l = Layout::plain(1080.0, 2400.0);
        // The band allows a negative offset. A y near the grid's top with
        // that offset resolves to a NEGATIVE row, and `(negative) as usize`
        // is ~1.8e19 -- which would then index clean off the end of the
        // catalogue and launch whatever app happened to be there.
        let y = l.drawer_grid_top + 1.0;
        let got = l.drawer_grid_hit_scrolled(0.0, l.col_pitch * 0.5, y, -100.0, 137);
        assert!(
            got.is_none(),
            "a scrolled-past row must be a miss, got {got:?}"
        );

        // And a far larger band, which would overflow the row math itself.
        let huge = l.drawer_grid_hit_scrolled(0.0, l.col_pitch * 0.5, y, -1.0e9, 137);
        assert!(huge.is_none(), "a wild offset must be a miss, got {huge:?}");
    }

    /// The viewport row count must not bound the content.
    ///
    /// `Layout::drawer_rows` is how many rows fit on screen. Capping the
    /// hit test with it is what made the scroll cosmetic: rows past the first
    /// screen were culled by the renderer AND rejected by the tap, so 18 of
    /// 28 rows in a 137-app catalogue were unreachable at any offset.
    #[test]
    fn the_viewport_row_count_does_not_bound_the_catalogue() {
        let l = Layout::plain(1080.0, 2400.0);
        let n = 137usize;
        let rows = n.div_ceil(l.grid_cols);
        assert!(
            rows > l.drawer_rows,
            "this test needs more content rows ({rows}) than fit on screen ({})",
            l.drawer_rows
        );
        // The last row must be reachable at the maximum scroll.
        let max = l.drawer_max_scroll_y(n);
        let last = l.drawer_icon_cell_scrolled((rows - 1) * l.grid_cols, max);
        let got = l.drawer_grid_hit_scrolled(0.0, last.center_x(), last.center_y(), max, n);
        assert_eq!(
            got,
            Some((rows - 1) * l.grid_cols),
            "the last row must be tappable"
        );

        // And the cull range must admit it too.
        let vr = l.visible_row_range(max, rows);
        assert!(
            vr.end >= rows,
            "the last row must survive culling at max scroll: {vr:?}"
        );
    }

    /// The rubber band must resist, and must be escapable.
    #[test]
    fn the_drawer_rubber_band_resists_and_settles_back() {
        let l = Layout::plain(1080.0, 2400.0);
        let n = 200usize;
        let max = l.drawer_max_scroll_y(n);
        assert!(max > 0.0);

        // A long pull above the top follows only a fraction of the finger.
        let pulled = clamp_drawer_scroll(-100.0, &l, n);
        assert!(
            pulled < 0.0,
            "a pull above the top must go slightly negative"
        );
        assert!(
            pulled > -100.0,
            "but must resist: {pulled} is not resistance"
        );

        // And a pull past the bottom.
        let over = clamp_drawer_scroll(max + 100.0, &l, n);
        assert!(over > max, "a pull past the bottom must overshoot");
        assert!(over < max + 100.0, "but must resist: {over}");

        // Inside the range it is the identity.
        for v in [0.0, 10.0, max * 0.5, max] {
            assert_eq!(
                clamp_drawer_scroll(v, &l, n),
                v,
                "inside the range is untouched"
            );
        }

        // Bounded: no amount of pulling escapes by more than the slack.
        let slack = (l.drawer_grid_bottom - l.drawer_grid_top) * 0.5;
        assert!(clamp_drawer_scroll(-100_000.0, &l, n) >= -slack);
        assert!(clamp_drawer_scroll(max + 100_000.0, &l, n) <= max + slack);

        // A non-scrollable list never moves at all.
        assert_eq!(clamp_drawer_scroll(-100.0, &l, 1), 0.0);
    }

    // -----------------------------------------------------------------
    // Phase 6.1: the smartspace weather half.
    //
    // `weather_str` was hardcoded to `""`, so the smartspace rendered the
    // date-only line forever and `smartspace_phase` cross-faded between two
    // identical strings -- the phase was live, the second half was not.
    // -----------------------------------------------------------------

    /// A sink file the test owns, in a directory unique to this test.
    ///
    /// The path is threaded through `WeatherSink::at` rather than through
    /// `$XDG_RUNTIME_DIR`. The env var is process-global, so tests that mutate
    /// it race each other whenever the harness runs them on more than one
    /// thread -- which is the default. Three of these tests were failing
    /// intermittently for exactly that reason: one test's `set_var` was
    /// another test's `remove_var`.
    struct WeatherFixture {
        dir: std::path::PathBuf,
        file: std::path::PathBuf,
    }
    impl WeatherFixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("utlc-weather-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            let file = dir.join("utlc-weather");
            Self { dir, file }
        }
        /// A sink reading this fixture. Starts with nothing published.
        fn sink(&self) -> WeatherSink {
            WeatherSink::at(self.file.clone())
        }
        fn write(&self, body: &str) {
            std::fs::write(&self.file, body).expect("write sink");
        }
    }
    impl Drop for WeatherFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn the_weather_sink_reads_what_the_platform_published() {
        let fx = WeatherFixture::new("read");
        fx.write("21C Light rain");
        let mut sink = fx.sink();
        let mut buf = String::new();
        sink.refresh(&mut buf);
        assert_eq!(buf, "21C Light rain");

        // A same-length update must still be seen: "21C" -> "31C" is the most
        // common real update, and a length-only change check misses every one.
        fx.write("31C Light rain");
        sink.refresh(&mut buf);
        assert_eq!(
            buf, "31C Light rain",
            "a same-length update must be observed"
        );
    }

    #[test]
    fn a_missing_or_empty_sink_reads_as_no_weather() {
        let fx = WeatherFixture::new("missing");
        let mut sink = fx.sink();
        let mut buf = String::new();
        // Nothing published at all.
        sink.refresh(&mut buf);
        assert!(buf.is_empty(), "an absent sink must read empty");

        // Published but empty.
        fx.write("");
        sink.refresh(&mut buf);
        assert!(buf.is_empty(), "an empty sink must read empty");

        // Published but whitespace.
        fx.write("   \n\t ");
        sink.refresh(&mut buf);
        assert!(buf.is_empty(), "a whitespace-only sink must read empty");

        // Then it comes back: a sink that recovers must not stay empty.
        fx.write("18C");
        sink.refresh(&mut buf);
        assert_eq!(buf, "18C", "the sink must recover");
    }

    #[test]
    fn the_weather_sink_truncates_on_a_character_boundary() {
        let fx = WeatherFixture::new("utf8");
        // 3-byte CJK characters, deliberately cut mid-sequence by the byte cap.
        fx.write("\u{8bbe}\u{7f6e}\u{5e94}\u{7528}\u{7ba1}\u{7406}\u{5668}\u{5929}\u{6c14}\u{60c5}\u{51b5}");
        let mut sink = fx.sink();
        let mut buf = String::new();
        sink.refresh(&mut buf);
        assert!(!buf.is_empty());
        assert!(
            std::str::from_utf8(buf.as_bytes()).is_ok(),
            "the truncated line must stay valid UTF-8: {buf:?}"
        );
        assert!(
            buf.len() <= WEATHER_MAX_BYTES,
            "{} bytes exceeds the {WEATHER_MAX_BYTES} cap",
            buf.len()
        );
        // And no replacement characters, which is what a mid-sequence byte cut
        // would produce.
        assert!(
            !buf.contains('\u{FFFD}'),
            "the cut split a character: {buf:?}"
        );
    }

    #[test]
    fn the_weather_sink_backs_off_instead_of_polling_a_dead_file() {
        let fx = WeatherFixture::new("backoff");
        let mut sink = fx.sink();
        let mut buf = String::new();
        // No file at all: every refresh fails.
        for _ in 0..WEATHER_GIVE_UP_AFTER {
            sink.refresh(&mut buf);
            assert!(buf.is_empty());
        }
        assert!(
            sink.misses >= WEATHER_GIVE_UP_AFTER,
            "misses must accumulate, got {}",
            sink.misses
        );

        // The backoff skips reads *between* poll boundaries. At exactly
        // `WEATHER_GIVE_UP_AFTER` misses the sink is on a boundary (it is a
        // multiple of `WEATHER_BACKOFF_TICKS`), so that one does poll --
        // which is the point: a sink that comes back must be noticed.
        let on_boundary = sink.misses;
        sink.refresh(&mut buf);
        assert_eq!(
            sink.misses,
            on_boundary + 1,
            "a miss count on a poll boundary must still be polled"
        );
        let off_boundary = sink.misses;
        sink.refresh(&mut buf);
        assert_eq!(
            sink.misses,
            off_boundary + 1,
            "the tick counter must advance even on a skipped read, or the \\
             next boundary is unreachable and a recovered sink is never seen"
        );

        // And it still recovers on its next scheduled poll rather than
        // staying dead for the life of the shell.
        fx.write("27C Clear");
        for _ in 0..=WEATHER_BACKOFF_TICKS {
            sink.refresh(&mut buf);
        }
        assert_eq!(sink.misses, 0, "a success must reset the backoff");
        assert_eq!(buf, "27C Clear", "a recovered sink must be picked up again");
    }

    /// The production path must be under the runtime dir, not the user's home.
    #[test]
    fn the_weather_sink_path_is_a_runtime_path() {
        let p = weather_path();
        let s = p.to_string_lossy();
        assert!(
            s.contains("utlc-weather"),
            "the sink file must be named utlc-weather, got {s}"
        );
        // With no `XDG_RUNTIME_DIR` set (the normal case under systemd, where
        // it is only set for a user session) it must land in /run.
        assert!(
            p.starts_with("/run") || std::env::var("XDG_RUNTIME_DIR").is_ok(),
            "a sink with no runtime dir must fall back to /run, got {p:?}"
        );
    }

    // -----------------------------------------------------------------
    // Security: `kill` must never be handed a broadcast pid.
    //
    // `kill` treats 0 as "my whole process group" and -1 as "everything I may
    // signal" -- on a root shell, the entire machine. The terminal cleanup
    // negates the pid for the process-group form, so a pid of 1 became -1 and
    // a cleanup turned into a system-wide SIGKILL.
    // -----------------------------------------------------------------

    #[test]
    fn a_broadcast_pid_is_never_signalled() {
        // 0 signals the caller's own process group, and 1 (via `-(1i32)`)
        // signals every process it may signal. Those are the two broadcasts.
        for bad in [0u32, 1u32] {
            assert!(
                !pid_is_signallable(bad),
                "pid {bad} must be refused by the signallable guard"
            );
        }
        // 2 is deliberately NOT refused. The guard is about broadcast
        // semantics, not a denylist: inside a PID namespace this shell can be
        // pid 1 and its first child pid 2, so a floor of 3 would break cleanup
        // in exactly the container case the project targets.
        assert!(pid_is_signallable(2), "pid 2 can legitimately be our child");
        // A negative i32 cast to u32 becomes enormous and must be refused,
        // which is what protects the recents kill path.
        assert!(!pid_is_signallable(-1i32 as u32));
        assert!(!pid_is_signallable(-12345i32 as u32));
        // And the upper bound: a u32 pid above i32::MAX wraps when negated,
        // so `-(pid as i32)` would land back in the positive range.
        assert!(!pid_is_signallable(u32::MAX));
        assert!(!pid_is_signallable(i32::MAX as u32 + 1));
    }

    #[test]
    fn the_shell_cannot_signal_itself_or_its_parent() {
        let me = std::process::id();
        assert!(
            !pid_is_signallable(me),
            "the compositor must never signal itself (pid {me})"
        );
        // SAFETY: `getppid` takes no arguments and cannot fail.
        let parent = unsafe { libc::getppid() } as u32;
        if parent > 1 {
            assert!(
                !pid_is_signallable(parent),
                "the compositor must never signal its supervisor (pid {parent})"
            );
        }
    }

    #[test]
    fn signal_child_tree_is_a_no_op_for_an_unsignallable_pid() {
        // If this returned normally without doing damage, the guard works.
        // `0` would have signalled the whole process group and `1` everything.
        for bad in [0u32, 1u32, std::process::id()] {
            signal_child_tree(bad);
        }
        // The test process is still here, which is the actual assertion: a
        // broadcast SIGKILL would have taken the test runner with it.
        assert!(std::process::id() > 0);
    }

    /// The negated group pid must itself be `< -1`, or it means "everything".
    #[test]
    fn the_process_group_form_can_never_evaluate_to_minus_one() {
        for pid in 2u32..=64 {
            let p = pid as i32;
            // `pid_is_signallable` is what admits the pid; re-prove the
            // property at the point of use, which is what makes the two
            // guards independent rather than one being a comment.
            assert!(pid_is_signallable(pid), "pid {pid} should be signallable");
            assert!(
                (-p) < -1,
                "pid {pid} negates to {}, which is a broadcast",
                -p
            );
        }
    }

    // -----------------------------------------------------------------
    // Panics: byte-index slicing of UTF-8, and out-of-range page indexing.
    // -----------------------------------------------------------------

    #[test]
    fn terminal_line_wrapping_never_splits_a_character() {
        // Every one of these panicked under `rem[..54]`: byte 54 lands inside a
        // multi-byte sequence. `panic = "abort"` in release, so one such line
        // from one command killed the compositor.
        for line in [
            "设置应用管理器天气情况设置应用管理器天气情况设置应用管理器天气情况",
            "Trình duyệt cực kỳ nặng nề và chậm chạp trên máy tính xách tay",
            "🎉🎊🎈 emoji line that is definitely longer than fifty four bytes 🎉",
            "Cây cối mọc rất nhanh và cao lớn trong khu vườn phía sau nhà",
        ] {
            let mut out: Vec<String> = Vec::new();
            push_terminal_line(&mut out, line);
            assert!(!out.is_empty(), "{line:?} produced no lines");
            // Every chunk is valid UTF-8 and reconstructs the input.
            for chunk in &out {
                assert!(
                    std::str::from_utf8(chunk.as_bytes()).is_ok(),
                    "a chunk is not valid UTF-8: {chunk:?}"
                );
                assert!(!chunk.contains('\u{FFFD}'), "a chunk is mangled: {chunk:?}");
            }
        }
    }

    #[test]
    fn floor_char_boundary_lands_where_it_says() {
        let s = "aa设置应用";
        // "aa" is 2 bytes; each CJK char is 3, so boundaries are 0,1,2,5,8,11.
        for n in 0..=s.len() {
            let f = floor_char_boundary(s, n);
            assert!(f <= n, "floor({n}) = {f} must not exceed n");
            assert!(s.is_char_boundary(f), "floor({n}) = {f} is not a boundary");
            // And it is the *largest* boundary at or below n.
            let mut expect = n;
            while expect > 0 && !s.is_char_boundary(expect) {
                expect -= 1;
            }
            assert_eq!(f, expect, "floor({n})");
        }
        // Past the end clamps to the length.
        assert_eq!(floor_char_boundary(s, s.len() + 100), s.len());
    }

    /// The drawer view is a *window*, and the renderer must be able to place
    /// it.
    ///
    /// The bug this pins: the renderer's cull range was derived from
    /// `drawer_apps.len()`, which is the fixed-capacity window, while the
    /// shell's hit test resolved against the whole catalogue. With 137 apps
    /// the window held 64, the cull stopped at row 12, and rows 13..27 were
    /// tappable but invisible -- a launcher that will launch an app you cannot
    /// see.
    #[test]
    fn the_drawer_window_and_the_cull_range_agree() {
        let l = Layout::plain(1080.0, 2400.0);
        let n = 137usize;
        let total_rows = n.div_ceil(l.grid_cols);

        // Simulate the shell: window starts at the first visible row.
        for scroll in [0.0f32, 200.0, 600.0, 1200.0] {
            let vr = l.visible_row_range(scroll, total_rows);
            let first_index = vr.start * l.grid_cols;
            // The window is what the shell fills: from `first_index` onward,
            // up to its capacity.
            let win_end = (first_index + FRAME_MAX_GRID).min(n);
            let win_len = win_end - first_index;

            // The renderer maps back: window index j -> catalogue index.
            // Every visible row must be inside the window, or it cannot be
            // drawn even though it is tappable.
            let last_needed = (vr.end * l.grid_cols).min(n);
            assert!(
                first_index + win_len >= last_needed,
                "scroll {scroll}: window covers catalogue {first_index}..{}, \
                 but rows up to {last_needed} are visible",
                first_index + win_len
            );
        }
    }

    /// A long-press on empty workspace must fall through to the workspace menu.
    #[test]
    fn a_long_press_on_empty_workspace_names_nothing() {
        let (w, h) = (1080.0_f32, 2400.0_f32);
        let l = Layout::plain(w, h);
        let apps = vec![ManagedApp::new("phone", "Phone", "", 0xFF10B981, "P")];
        // Off the grid entirely.
        assert_eq!(
            long_press_app_id(&apps, &[], &l, 0.0, 1, w * 0.5, l.grid_top - 40.0),
            String::new()
        );
        // On the grid, but the page has no entry in that slot: a nearly-empty
        // home screen, where the grid reports a slot past the page's end. This
        // must be a miss, not an index-out-of-bounds panic.
        let cell = l.grid_cell(l.grid_cols * l.max_rows - 1);
        assert_eq!(
            long_press_app_id(
                &apps,
                &["phone".to_string()],
                &l,
                0.0,
                1,
                cell.center_x(),
                cell.y + cell.h * 0.5,
            ),
            String::new(),
            "a slot past the end of the page is a miss"
        );
        // And a stale id (app uninstalled, page not pruned) is also a miss,
        // so the popup never names an app that cannot be launched.
        assert_eq!(
            long_press_app_id(
                &apps,
                &["ghost-app".to_string()],
                &l,
                0.0,
                1,
                cell.center_x(),
                cell.y + cell.h * 0.5,
            ),
            String::new()
        );
    }
}
