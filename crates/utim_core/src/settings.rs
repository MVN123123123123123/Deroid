//! The settings surface: what the Settings screen shows and what tapping a row does.
//!
//! # Why this exists
//!
//! `LauncherState` persists about thirty settings -- theme, icon shape, grid
//! dimensions, haptics, screen timeout, wallpaper path, folder size -- and each
//! one is honoured somewhere in the shell. Until now **nothing could change any
//! of them.** The in-shell Settings app painted seven hardcoded section cards
//! describing the device (`"Network & Internet"`, `"Battery"`, `"About Phone"`),
//! and tapping one did nothing at all.
//!
//! So the store was write-only from the user's point of view: correct, tested,
//! persisted, and unreachable. That is the same failure this project has been
//! bitten by three separate times, and it is worth naming why it keeps recurring.
//! A field that is *read* by the renderer looks alive. A field that is *written*
//! by nothing is indistinguishable from a correct one until someone tries to
//! change it.
//!
//! # What this module is
//!
//! A projection, not a store. [`build`] turns a [`LauncherState`] into a flat
//! list of rows the renderer draws, and [`apply`] turns a tap on one of those rows
//! into a mutation of the state. It owns no settings of its own and persists
//! nothing; the shell already has the `touch()`-and-save convention and this
//! plugs into it.
//!
//! # Shape of a row
//!
//! Every label and every value string is `&'static str`. That is a deliberate
//! constraint and it is what makes the projection free to hold: a value like
//! "115%" or "Rounded rect" is a choice from a closed set, so it is a static
//! selected by matching, not a `format!` of a live number. The rows therefore
//! borrow nothing from the state, so a `Vec<SettingRow<'static>>` built when the
//! panel opens survives the catalogue rescan that reassigns everything the shell
//! borrows elsewhere.
//!
//! The cost is that this cannot express an unbounded value -- a free-text
//! wallpaper path, say. Those rows are [`SettingKind::Text`]: they show their
//! current value as a static "Set..." label and their action is a no-op until
//! something can type into them. The reference has the same split, between
//! `PreferenceScreen` toggles and its text-entry dialogs.
//!
//! # The one exception: a position, not a string
//!
//! The wallpaper row is [`SettingKind::Picker`], and it does not break the
//! `&'static str` rule. What it needs to display is not a *path* but a
//! position -- "3 of 7" -- and a position is two `Option<u32>`s
//! ([`SettingRow::at`], [`SettingRow::of`]) rather than a string that would have
//! to be `format!`ed into a buffer the panel does not own. The candidate list
//! itself is unbounded and lives behind `&mut` in [`picker::WallpaperPicker`],
//! which is why that is a separate module and not a field here: a `Vec<String>`
//! in a `Copy` row would undo everything the shape of a row is for.
//!
//! [`build`] leaves both position fields `None`, which the renderer reads as
//! "not a picker yet"; [`build_paged`] is what fills them.

pub mod picker;

use crate::launcher_state::{AccentSource, IconShape, LauncherState};

/// The type scales offered, as `(percent, multiplier)`.
///
/// The reference exposes font scale as a slider over discrete stops
/// (`FontSizePreference`, `ui/preferences`), and UTLC's `Layout::new_scaled`
/// already threads a multiplier end to end -- it just had no way to be handed
/// one. Three stops rather than a continuous slider because the multiplier is
/// read per frame into cell heights and row pitch, and a continuous value would
/// want a live drag the panel has no affordance for.
const FONT_SCALES: [(u16, f32); 3] = [(100, 1.0), (115, 1.15), (130, 1.30)];

/// Screen timeouts offered, in seconds. Zero is "never", and is first because it
/// is the setting a user on battery actually wants.
const TIMEOUTS: [u32; 5] = [0, 15, 30, 60, 300];

/// How a row behaves when tapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// Flips a boolean. The value is "On"/"Off".
    Toggle,
    /// Advances a closed set by one, wrapping. The reference's radio groups and
    /// `ListPreference` both wrap; `PreferenceScreen`'s spinners do not, but a
    /// wrapping list is one tap from either end.
    Choice {
        /// How many values the set has. Kept so the renderer can draw the
        /// current position as `n / total` without knowing the set.
        n: usize,
    },
    /// Adjusts an integer by a step, clamped. For grid dimensions, where the
    /// bounds are the reference's own slider ranges rather than a closed set.
    Step { min: usize, max: usize, step: usize },
    /// Shows a value but has no action yet. Honest rather than a dead tap: the
    /// row says it is not editable instead of accepting a touch that does nothing.
    Text,
    /// A chooser over a list the row does not hold: the wallpaper.
    ///
    /// Deliberately carries no list. `SettingRow` is `Copy` and holds only
    /// `&'static str`, and a candidate list is a `Vec<String>` of paths
    /// unbounded by anything this crate knows. The state machine is
    /// [`picker::WallpaperPicker`], owned by the shell for the lifetime of the
    /// panel, and the row carries only where the cursor is -- [`Self::at`] and
    /// [`Self::of`] -- so the renderer can label the row without holding the
    /// thing being labelled.
    ///
    /// The reference's equivalent is a carousel whose selection and commit are
    /// separate: a touch on any card but the current one only moves the tick
    /// (`WallpaperCarouselView.kt:108-115`), and a touch on the ticked card is
    /// what sets the wallpaper. Hence a `Picker` row is *not* handled by
    /// [`apply`] -- it has no fixed value set to advance -- and the shell
    /// intercepts its key, moves the picker, and commits on the second tap.
    Picker,
}

/// One settings row, as the renderer draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingRow {
    /// Stable identifier. This is the persistence key's counterpart -- it is what
    /// [`apply`] matches on, and what a test asserts against, so it must not be a
    /// label string that could be reworded.
    pub key: &'static str,
    /// Human label.
    pub label: &'static str,
    /// Current value, already rendered as text.
    pub value: &'static str,
    /// Behaviour on tap.
    pub kind: SettingKind,
    /// Grouping header, or `None` for a row that continues the previous group.
    ///
    /// The reference's `PreferenceCategory` (`res/xml/launcher_preferences.xml`),
    /// which is why it is a per-row field rather than a separate list: it keeps a
    /// row self-contained for both drawing and hit-testing, so neither can be
    /// built without seeing the grouping.
    pub section: Option<&'static str>,
    /// 1-based position in a [`SettingKind::Picker`]'s list: `Some(3)` for the
    /// third of seven. `None` on every other kind, and `None` on a picker whose
    /// list is not known yet -- which is what [`build`] produces and what the
    /// renderer reads as "not a picker".
    ///
    /// Two numbers rather than a formatted string, and that is the whole reason
    /// this struct could stay `&'static str`-only: `format!("{at} of {of}")`
    /// allocates, and a panel that rebuilds its rows on every change cannot
    /// afford one per row per change. It is a pair of integers, so the renderer
    /// puts them wherever it likes.
    pub at: Option<u32>,
    /// How many candidates the picker has, or `0`/`None` for "unknown".
    ///
    /// Never zero for a live picker -- `WallpaperPicker::page_count` is 0 only
    /// for an empty list, which [`build_paged`] reports as "unknown" rather than
    /// as an empty picker, because a row reading "0 of 0" invites the user to
    /// tap something that does not exist.
    pub of: Option<u32>,
}

impl SettingRow {
    /// The empty row, for a fixed-capacity view's filler.
    pub const EMPTY: Self = Self {
        key: "",
        label: "",
        value: "",
        kind: SettingKind::Text,
        section: None,
        at: None,
        of: None,
    };
}

/// Project `s` into the rows the Settings screen draws.
///
/// Allocates a `Vec`, and that is deliberate: it is called when the panel opens
/// and after every change, not per frame. The rows hold only `&'static str`, so
/// the returned vector does not borrow the state and can outlive any mutation of
/// it -- which is what makes it safe to keep across the catalogue rescan.
///
/// Ordering is appearance, home screen, then system: the reference's own
/// `launcher_preferences.xml` order, and roughly how often each is reached.
///
/// # The picker row
///
/// This is [`build_paged`] with the position left unknown, and it is that
/// function rather than a second copy of the row list so the two cannot drift:
/// the wallpaper row is [`SettingKind::Picker`] here too, with `at`/`of` `None`.
/// A caller that has enumerated wallpapers should call [`build_paged`] with
/// [`picker::WallpaperPicker::position`] as `at` and
/// [`picker::WallpaperPicker::len`] as `of`; a caller that has not gets the
/// honest "nothing to choose yet" it should draw.
pub fn build(s: &LauncherState) -> Vec<SettingRow> {
    // Zero means unknown, which `build_paged` turns into `None`/`None`.
    build_paged(s, 0, 0)
}

/// [`build`], plus the picker's position on the one row that has one.
///
/// `at` is 1-based, as [`picker::WallpaperPicker::position`] returns it; `of` is
/// how many candidates there are. **`of == 0` means "unknown"**, not "empty":
/// both position fields come back `None`, which is exactly what [`build`]
/// produces and exactly what the renderer reads as "not a picker yet". A row
/// labelled "0 of 0" would invite a tap on something that cannot be drawn.
///
/// `at` is clamped into `1..=of` rather than trusted. Both numbers come from
/// call sites that do not exist yet, and the same reason
/// [`grid_col_stop`] clamps is the reason this does: a row that says "0 of 7" or
/// "9 of 7" is a lie the panel cannot detect, and the two numbers describing one
/// selection must not be able to contradict each other.
pub fn build_paged(s: &LauncherState, at: u32, of: u32) -> Vec<SettingRow> {
    let mut rows = build_rows(s);
    if of != 0 {
        let at = at.clamp(1, of);
        if let Some(r) = rows.iter_mut().find(|r| r.key == WALLPAPER_KEY) {
            r.at = Some(at);
            r.of = Some(of);
        }
    }
    rows
}

/// The rows themselves, with no position filled in.
///
/// The private half of [`build_paged`], so [`build`] and [`build_paged`] are one
/// list rather than two: twenty rows that drift apart is a bug waiting for
/// whichever one somebody forgets to update.
fn build_rows(s: &LauncherState) -> Vec<SettingRow> {
    let font_stop = FONT_SCALES
        .iter()
        .position(|(_, m)| (*m - s.font_scale).abs() < 0.001)
        .unwrap_or(0);
    let shape_at = icon_shape_index(s.icon_shape);
    let accent_at = match s.accent_source {
        AccentSource::Wallpaper => 0,
        AccentSource::Custom => 1,
        AccentSource::Default => 2,
    };
    let timeout_at = TIMEOUTS
        .iter()
        .position(|t| *t == s.screen_timeout_s)
        .unwrap_or(1);

    vec![
        // --- Appearance -------------------------------------------------
        row(
            "dark-theme",
            "Dark theme",
            on_off(s.dark_theme),
            SettingKind::Toggle,
            Some("Appearance"),
        ),
        row(
            "follow-system-theme",
            "Follow system theme",
            on_off(s.follow_system_theme),
            SettingKind::Toggle,
            None,
        ),
        row(
            "accent-source",
            "Accent colour",
            ACCENTS[accent_at],
            SettingKind::Choice { n: ACCENTS.len() },
            None,
        ),
        row(
            "icon-shape",
            "Icon shape",
            SHAPES[shape_at],
            SettingKind::Choice { n: SHAPES.len() },
            None,
        ),
        row(
            "monochrome-icons",
            "Monochrome icons",
            on_off(s.monochrome_icons),
            SettingKind::Toggle,
            None,
        ),
        row(
            "font-scale",
            "Font size",
            FONT_LABELS[font_stop],
            SettingKind::Choice {
                n: FONT_SCALES.len(),
            },
            None,
        ),
        row(
            "clock-24h",
            "24-hour clock",
            on_off(s.clock_24h),
            SettingKind::Toggle,
            None,
        ),
        row(
            "show-search-bar",
            "Search bar",
            on_off(s.show_search_bar),
            SettingKind::Toggle,
            None,
        ),
        row(
            WALLPAPER_KEY,
            "Wallpaper",
            if s.wallpaper.is_empty() {
                "From the system"
            } else {
                "Custom"
            },
            // A real control now. Picking a file needs a candidate list, and the
            // candidate list is not something a `&'static str` row can hold --
            // so the row names the control and `picker::WallpaperPicker` holds
            // the list, with this row's `at`/`of` describing where its cursor
            // is. The reference's shape is the same split: its picker is a
            // carousel fed from outside (`WallpaperViewModel.kt:44`,
            // `getTopWallpapers()`), and the row's own job is only to say which
            // wallpaper is current.
            //
            // Deliberately *not* handled by `apply`: a picker has no fixed value
            // set to advance, so there is nothing for a key-to-mutation match to
            // return. The shell intercepts this key, calls
            // `picker.move_by(delta)`, and commits
            // `state.wallpaper = picker.selected()` on the second tap.
            SettingKind::Picker,
            None,
        ),
        // --- Home screen ------------------------------------------------
        row(
            "grid-cols",
            "Workspace columns",
            COL_LABELS[grid_col_stop(s.grid_cols)],
            SettingKind::Step {
                min: 0,
                max: 12,
                step: 1,
            },
            Some("Home screen"),
        ),
        row(
            "grid-rows",
            "Workspace rows",
            COL_LABELS[grid_col_stop(s.grid_rows)],
            SettingKind::Step {
                min: 0,
                max: 12,
                step: 1,
            },
            None,
        ),
        row(
            "folder-cols",
            "Folder columns",
            folder_label(s.folder_cols),
            SettingKind::Step {
                min: 0,
                max: crate::graphics::layout::FOLDER_GRID_MAX,
                step: 1,
            },
            None,
        ),
        row(
            "folder-rows",
            "Folder rows",
            folder_label(s.folder_rows),
            SettingKind::Step {
                min: 0,
                max: crate::graphics::layout::FOLDER_GRID_MAX,
                step: 1,
            },
            None,
        ),
        row(
            "home-locked",
            "Lock home screen",
            on_off(s.home_locked),
            SettingKind::Toggle,
            None,
        ),
        row(
            "default-page",
            "Default page",
            "Page 1",
            // Cycleable but not yet: paging by more than one is a
            // `default_workspace_page` pref the reference stores as an index
            // (`PreferenceManager2.kt`), and the shell currently has no "go to a
            // different page on launch" path to point it at. Left as `Text`
            // rather than a toggle that sets a value nothing reads.
            SettingKind::Text,
            None,
        ),
        // --- System -----------------------------------------------------
        row(
            "haptics",
            "Haptic feedback",
            on_off(s.haptics),
            SettingKind::Toggle,
            Some("System"),
        ),
        row(
            "auto-rotate",
            "Auto-rotate",
            // Not a `Toggle`. `LauncherState::auto_rotate` is persisted and read
            // by nothing: honouring it means a D-Bus subscription to
            // `net.hadess.SensorProxy` plus an output-mode change on the KMS
            // device, which is a feature rather than a wiring pass.
            //
            // Presenting it as a switch is the failure this module's own
            // `every_row_is_reachable_and_no_row_is_a_dead_tap` test exists to
            // catch: a control a user can move with no result, and no way to tell
            // that apart from a broken one. Demoted to `Text` so the row admits
            // it is not editable, which is at least the same information the user
            // would get from a toggle that does nothing -- except honest.
            if s.auto_rotate {
                "On (not applied)"
            } else {
                // Qualified like the true branch. A bare "Off" is byte-for-byte
                // what a `Toggle` renders, so the row would be indistinguishable
                // from a switch -- which is the exact failure
                // `every_row_is_reachable_and_no_row_is_a_dead_tap` exists to
                // catch, and it rejects it below.
                "Off (not applied)"
            },
            SettingKind::Text,
            None,
        ),
        row(
            "screen-timeout",
            "Screen timeout",
            TIMEOUT_LABELS[timeout_at],
            SettingKind::Choice { n: TIMEOUTS.len() },
            None,
        ),
    ]
}

const ACCENTS: [&str; 3] = ["From wallpaper", "Custom", "Default"];
const SHAPES: [&str; 3] = ["Squircle", "Circle", "Rounded rect"];
const FONT_LABELS: [&str; 3] = ["100%", "115%", "130%"];
const TIMEOUT_LABELS: [&str; 5] = ["Never", "15 s", "30 s", "1 min", "5 min"];

/// The one row whose position [`build_paged`] fills.
///
/// A constant because three places now name it: the row literal, the patch in
/// `build_paged`, and `apply`'s doc, which says the shell intercepts this key
/// before `apply` is called. A string literal typed twice is a string literal
/// that will be reworded in one place only.
pub const WALLPAPER_KEY: &str = "wallpaper";
const COL_LABELS: [&str; 13] = [
    "Auto", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12",
];

/// Apply a tap on the row keyed `key`.
///
/// `delta` is `+1` for a plain tap and lets a future long-press step backwards;
/// every setting here is reachable forwards, so nothing passes anything but `1`
/// today and the parameter exists so that decision is not baked into eight call
/// sites.
///
/// Returns `true` when the state changed. The caller persists on change, which is
/// the existing `state.touch(); state.save()` convention -- and returning `false`
/// for an untouched row matters, because it is what stops a tap that changes
/// nothing from marking the state dirty and writing the file.
///
/// # Panics
///
/// Never. A key that no longer exists returns `false` rather than panicking,
/// because the key comes from a row built earlier and a settings rename must not
/// be able to crash the shell.
pub fn apply(s: &mut LauncherState, key: &str, delta: i32) -> bool {
    match key {
        "dark-theme" => flip(&mut s.dark_theme),
        "follow-system-theme" => flip(&mut s.follow_system_theme),
        "monochrome-icons" => flip(&mut s.monochrome_icons),
        "clock-24h" => flip(&mut s.clock_24h),
        "show-search-bar" => flip(&mut s.show_search_bar),
        "home-locked" => flip(&mut s.home_locked),
        "haptics" => flip(&mut s.haptics),
        // No `auto-rotate` arm. The row was demoted to `Text` because
        // `LauncherState::auto_rotate` is read by nothing, and a `Text` row that
        // still had a mutating arm is the exact contradiction this module exists
        // to prevent: the row says "not editable" while the key silently changes
        // persisted state. Either the row is a control or the key is inert, and
        // here the key is inert.
        //
        // Removing the arm also means `apply` returns `false` for it, so the
        // shell skips the `touch()`/`save()` and does not mark the state dirty for
        // a change that never happened.
        "accent-source" => {
            let next = match s.accent_source {
                AccentSource::Wallpaper => AccentSource::Custom,
                AccentSource::Custom => AccentSource::Default,
                AccentSource::Default => AccentSource::Wallpaper,
            };
            if next == s.accent_source {
                return false;
            }
            s.accent_source = next;
            // A custom colour only means something with a custom source, and
            // leaving the old seed behind would make the two disagree about what
            // the accent is.
            s.accent_color = if next == AccentSource::Custom {
                s.accent_color | 0xFF00_0000
            } else {
                0
            };
            true
        }
        "icon-shape" => {
            let next = match (s.icon_shape, delta >= 0) {
                (IconShape::Squircle, _) => IconShape::Circle,
                (IconShape::Circle, true) => IconShape::RoundedRect,
                (IconShape::Circle, false) => IconShape::Squircle,
                (IconShape::RoundedRect, true) => IconShape::Squircle,
                (IconShape::RoundedRect, false) => IconShape::Circle,
            };
            if next == s.icon_shape {
                return false;
            }
            s.icon_shape = next;
            true
        }
        "font-scale" => {
            let at = FONT_SCALES
                .iter()
                .position(|(_, m)| (*m - s.font_scale).abs() < 0.001)
                .unwrap_or(0) as i32;
            let next = wrap(at + delta.signum(), FONT_SCALES.len() as i32) as usize;
            let scale = FONT_SCALES[next].1;
            if (scale - s.font_scale).abs() < f32::EPSILON {
                return false;
            }
            s.font_scale = scale;
            true
        }
        "screen-timeout" => {
            let at = TIMEOUTS
                .iter()
                .position(|t| *t == s.screen_timeout_s)
                .unwrap_or(1) as i32;
            let next = wrap(at + delta.signum(), TIMEOUTS.len() as i32) as usize;
            if TIMEOUTS[next] == s.screen_timeout_s {
                return false;
            }
            s.screen_timeout_s = TIMEOUTS[next];
            true
        }
        "grid-cols" => step_usize(&mut s.grid_cols, delta, 12),
        "grid-rows" => step_usize(&mut s.grid_rows, delta, 12),
        "folder-cols" => step_usize(
            &mut s.folder_cols,
            delta,
            crate::graphics::layout::FOLDER_GRID_MAX,
        ),
        "folder-rows" => step_usize(
            &mut s.folder_rows,
            delta,
            crate::graphics::layout::FOLDER_GRID_MAX,
        ),
        // `Text` rows and any key from a newer build.
        //
        // `SettingKind::Picker` rows land here too, and that is deliberate: a
        // picker has no closed value set for this match to advance, because the
        // values *are* the enumerated wallpapers and this function cannot see
        // them -- `apply` takes a `LauncherState` and a key, and a `Vec<String>`
        // of paths is not either. The shell intercepts `WALLPAPER_KEY` before
        // calling here, moves the picker, and commits
        // `state.wallpaper = picker.selected()`. If this ever grows a
        // `WALLPAPER_KEY` arm it will silently become a second, wrong way to set
        // the wallpaper, because the shell would still own the picker.
        _ => false,
    }
}

/// Wrap `v` into `0..n`, for `n > 0`.
#[inline]
fn wrap(v: i32, n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    v.rem_euclid(n)
}

#[inline]
fn flip(b: &mut bool) -> bool {
    *b = !*b;
    true
}

fn step_usize(v: &mut usize, delta: i32, max: usize) -> bool {
    let before = *v;
    // Zero means "derive from the panel", which is a real choice for the
    // workspace rows but not for the folder's -- `FOLDER_GRID_MIN` is 2, so
    // stepping from 0 has to land on the minimum rather than on 1.
    let next = if delta >= 0 {
        before.saturating_add(1)
    } else {
        before.saturating_sub(1)
    };
    let next = if next > max { max } else { next };
    if next == before {
        return false;
    }
    *v = next;
    true
}

const fn row(
    key: &'static str,
    label: &'static str,
    value: &'static str,
    kind: SettingKind,
    section: Option<&'static str>,
) -> SettingRow {
    SettingRow {
        key,
        label,
        value,
        kind,
        section,
        // No position. A row built by [`row`] is one of the twenty that show a
        // value from a closed set, and `None` is what "not a picker" means to
        // the renderer. The picker row gets its position from [`build_paged`].
        at: None,
        of: None,
    }
}

const fn on_off(v: bool) -> &'static str {
    if v {
        "On"
    } else {
        "Off"
    }
}

fn icon_shape_index(s: IconShape) -> usize {
    match s {
        IconShape::Squircle => 0,
        IconShape::Circle => 1,
        IconShape::RoundedRect => 2,
    }
}

/// Which entry of [`COL_LABELS`] a stored column count is.
///
/// `0` is "Auto" and is the first stop for every dimension: the reference exposes
/// it as the "auto-size" end of the workspace slider
/// (`PreferenceManager.kt:111-112`), and UTLC's `grid_cols_for`
/// (`layout.rs:250-258`) already derives a value from the panel width when it is
/// zero.
fn grid_col_stop(v: usize) -> usize {
    v.clamp(0, COL_LABELS.len() - 1)
}

/// Folder dimensions never say "Auto" -- the reference's slider is 2..5
/// (`FolderPreferences.kt:84,90`) -- so zero renders as the minimum rather than
/// as a lie.
fn folder_label(v: usize) -> &'static str {
    match v {
        0 => "2 (minimum)",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        _ => "5",
    }
}

/// Which row a touch at `(x, y)` lands on, or `None`.
///
/// `geom` is what the renderer published
/// ([`crate::graphics::drm_kms::settings_geometry`]) -- the row height and pitch it
/// actually drew, not numbers this function recomputes. That is the whole contract
/// and it is worth being pedantic about: the first version of this took the list
/// rect and derived the pitch, and the test written to catch paint/hit drift passed
/// while the drift was present, because the test derived the pitch the same way.
/// Now there is no derivation on this side at all, so "painted" and "tappable" are
/// the same numbers by construction.
///
/// Returns the row's key without allocating: `&'static str`, not `String`.
pub fn hit(
    x: f32,
    y: f32,
    geom: &crate::graphics::drm_kms::SettingsGeometry,
    rows: &[SettingRow],
) -> Option<&'static str> {
    // Nothing has been painted yet, or the panel had no rows.
    if rows.is_empty() || geom.step <= 0.0 || geom.list_h <= 0.0 || y < geom.list_top {
        return None;
    }
    let (card_x, card_w) = card_bounds_from(geom);
    if x < card_x || x > card_x + card_w {
        return None;
    }
    let offset = y - geom.list_top;
    let idx = (offset / geom.step).floor();
    if idx < 0.0 {
        return None;
    }
    // The pitch exceeds the drawn height, so the space between two rows belongs to
    // neither. A tap in it is a miss rather than the nearest row: silently
    // rounding to a neighbour is what makes a control feel unreliable.
    if offset - idx * geom.step > geom.card_h {
        return None;
    }
    rows.get(idx as usize).map(|r| r.key)
}

/// The content card's x and width, derived from the painted bar rect.
///
/// A function of the published geometry rather than of an `AppLayout`, so the hit
/// test cannot be handed a *different* layout than the one the panel was painted
/// with -- which is the same class of bug as recomputing the pitch.
#[inline]
pub fn card_bounds_from_pub(geom: &crate::graphics::drm_kms::SettingsGeometry) -> (f32, f32) {
    card_bounds_from(geom)
}

#[inline]
fn card_bounds_from(geom: &crate::graphics::drm_kms::SettingsGeometry) -> (f32, f32) {
    (
        geom.bar_x + geom.bar_h * 0.25,
        geom.bar_w - geom.bar_h * 0.5,
    )
}

/// The content card's x and width, from the app panel's top bar.
///
/// The same expression as [`card_bounds_from`], written against a `Rect` for
/// callers that hold one. Both exist because the renderer has the bar and the hit
/// test has the published copy, and neither should be reading a *third* source.
#[inline]
pub fn card_bounds(bar: &crate::graphics::layout::Rect) -> (f32, f32) {
    (bar.x + bar.h * 0.25, bar.w - bar.h * 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every value a [`SettingKind::Text`] row is allowed to show.
    ///
    /// The review gate, not a formatting list. Adding an honest `Text` row means
    /// adding its value here and, beside the row, a comment saying why it is not
    /// editable -- which is the whole point of the `Text` kind: a row that admits
    /// it is not settable is honest, and a row that presents a control is a dead
    /// tap.
    ///
    /// A `Toggle` value is deliberately *not* in this list, and the two tests
    /// that read it reject `"On"`/`"Off"` on a `Text` row before consulting it,
    /// because a `Text` row showing one of those is drawing a switch that cannot
    /// be moved.
    const TEXT_VALUES: [&str; 3] = [
        // `default-page`: cycleable, but the shell has no "go to a different
        // page on launch" path for the value to reach.
        "Page 1",
        // `auto-rotate`: persisted and read by nothing, so the row says so
        // rather than offering a switch that changes nothing. *Both* branches
        // have to be qualified, or the false branch draws a bare "Off".
        "On (not applied)",
        "Off (not applied)",
    ];

    /// A tap must activate the row it landed on, and the gaps must hit nothing.
    ///
    /// Uses a synthetic `SettingsGeometry` here; the version that pins this against
    /// the *real* painted numbers is `settings_tap_matches_the_painted_rows` in
    /// `graphics::screenshot`. Both exist because they fail for different reasons:
    /// this one catches a regression in the band logic, that one catches the paint
    /// and the hit test disagreeing.
    #[test]
    fn a_tap_hits_its_own_row_and_the_gaps_hit_nothing() {
        let s = LauncherState::default();
        let rows = build(&s);
        assert!(rows.len() > 4, "the panel is a list, not a stub");

        let g = crate::graphics::drm_kms::SettingsGeometry {
            list_top: 400.0,
            list_h: 1400.0,
            bar_x: 0.0,
            bar_h: 40.0,
            bar_w: 1080.0,
            panel_h: 2400.0,
            card_h: 52.0,
            step: 66.0,
        };
        let (card_x, card_w) = card_bounds_from(&g);
        let cx = card_x + card_w * 0.5;

        for (i, r) in rows.iter().enumerate() {
            let mid = g.list_top + g.step * i as f32 + g.card_h * 0.5;
            assert_eq!(
                hit(cx, mid, &g, &rows),
                Some(r.key),
                "a tap at the centre of row {i} ({}) missed",
                r.label
            );
            // The gap below belongs to neither row. Rounding to the nearest is what
            // makes a control feel unreliable.
            assert_eq!(
                hit(
                    cx,
                    g.list_top + g.step * i as f32 + g.card_h + (g.step - g.card_h) * 0.5,
                    &g,
                    &rows
                ),
                None,
                "the gap below row {i} hit something"
            );
        }

        // Horizontal bounds, the search field above, an unpublished rect, and no rows.
        assert_eq!(hit(card_x - 1.0, g.list_top + 1.0, &g, &rows), None);
        assert_eq!(
            hit(card_x + card_w + 1.0, g.list_top + 1.0, &g, &rows),
            None
        );
        assert_eq!(
            hit(cx, g.list_top - 4.0, &g, &rows),
            None,
            "a tap on the search field activated a settings row"
        );
        let unpublished = crate::graphics::drm_kms::SettingsGeometry {
            list_top: 0.0,
            list_h: 0.0,
            bar_x: 0.0,
            bar_h: 40.0,
            bar_w: 1080.0,
            panel_h: 2400.0,
            card_h: 0.0,
            step: 0.0,
        };
        assert_eq!(
            hit(cx, 100.0, &unpublished, &rows),
            None,
            "before the panel is painted, nothing is tappable"
        );
        assert_eq!(hit(cx, g.list_top + 1.0, &g, &[]), None);
    }

    #[test]
    fn every_row_is_reachable_and_no_row_is_a_dead_tap() {
        let s = LauncherState::default();
        let rows = build(&s);
        assert!(!rows.is_empty(), "the panel has to have something in it");

        // A `Text` row is the one kind that legitimately does nothing, and each
        // of ours must say why in its value rather than present a switch. The
        // check is that no row claims to be editable and then is not.
        for r in &rows {
            match r.kind {
                SettingKind::Text => {
                    // A `Text` row has to *admit* it is not editable, and "On" or
                    // "Off" on its own is byte-for-byte what a `Toggle` draws --
                    // so a `Text` row showing one is indistinguishable from a
                    // switch that does nothing. That is the failure this test
                    // exists to catch, so it is rejected outright rather than
                    // allowlisted.
                    assert_ne!(
                        r.value, "On",
                        "{} is Text but its value looks like a switch",
                        r.key
                    );
                    assert_ne!(
                        r.value, "Off",
                        "{} is Text but its value looks like a switch",
                        r.key
                    );
                    assert!(
                        TEXT_VALUES.contains(&r.value),
                        "{} is Text with value {:?}, which is not one of the honest \
                         not-yet-settable values. If the row genuinely is not \
                         editable, say why in a comment beside it and add the \
                         value here -- that list is the review gate.",
                        r.key,
                        r.value
                    );
                }
                SettingKind::Picker => {
                    // A picker cannot be checked with `apply` -- it has no fixed
                    // value set for `apply` to advance -- so "reachable" has to
                    // mean something else for it: there is a picker that can
                    // reach every candidate, and this row says where in that list
                    // it is. Both halves are checked, because either alone is
                    // satisfiable by a dead tap: a real picker with a row that
                    // labels nothing, or a row that labels a position no picker
                    // can be at.
                    let mut picker = picker::WallpaperPicker::new(wallpapers(7), 0);
                    let mut reached = 0usize;
                    for _ in 0..7 {
                        if picker.selected().is_some() {
                            reached += 1;
                        }
                        picker.move_by(1);
                    }
                    assert_eq!(
                        reached, 7,
                        "{} claims to be a picker over 7 candidates but the picker \
                         cannot reach all of them",
                        r.key
                    );

                    // And the row's own numbers agree with the picker.
                    let paged = build_paged(&s, 3, 7);
                    let pr = paged
                        .iter()
                        .find(|x| x.key == WALLPAPER_KEY)
                        .expect("the wallpaper row exists");
                    assert_eq!(pr.kind, SettingKind::Picker);
                    assert_eq!((pr.at, pr.of), (Some(3), Some(7)));

                    let picker = picker::WallpaperPicker::new(wallpapers(7), 2);
                    assert_eq!(
                        picker.position(),
                        pr.at,
                        "the row says \"{} of {:?}\" and the picker is at {:?}",
                        pr.at.unwrap_or(0),
                        pr.of,
                        picker.position()
                    );
                }
                _ => {
                    // Every non-Text row must actually change the state, or it is
                    // a dead tap dressed as a control.
                    let mut probe = s.clone();
                    let changed = apply(&mut probe, r.key, 1);
                    assert!(
                        changed,
                        "{} is {:?} but a tap changed nothing",
                        r.key, r.kind
                    );
                }
            }
        }
    }

    /// Seven candidate wallpaper paths, as the shell would enumerate them.
    fn wallpapers(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("/data/wallpapers/{i:02}.png"))
            .collect()
    }

    /// The row keyed `k`, or a panic naming it. A free function rather than a
    /// closure because a closure returning a borrow of its own argument needs a
    /// lifetime the inference cannot pick.
    fn find_row<'a>(rows: &'a [SettingRow], k: &str) -> &'a SettingRow {
        rows.iter()
            .find(|r| r.key == k)
            .unwrap_or_else(|| panic!("row {k}"))
    }

    /// `build` leaves the picker's position unknown; `build_paged` fills it.
    #[test]
    fn build_paged_fills_the_pickers_position_and_build_leaves_it_unknown() {
        let s = LauncherState::default();
        let plain = build(&s);
        let paged = build_paged(&s, 3, 7);

        // `build` is `build_paged(_, 0, 0)`, so both must agree on everything
        // except the two position fields -- otherwise the two row lists have
        // drifted and the renderer will disagree with the panel depending on
        // which one the caller happened to use.
        assert_eq!(plain.len(), paged.len());
        for (a, b) in plain.iter().zip(&paged) {
            assert_eq!(
                (a.key, a.label, a.value, a.kind, a.section),
                (b.key, b.label, b.value, b.kind, b.section),
                "build and build_paged disagree about {}",
                a.key
            );
        }

        let w = find_row(&plain, WALLPAPER_KEY);
        assert_eq!(w.kind, SettingKind::Picker);
        assert_eq!(w.at, None, "`build` does not know the list");
        assert_eq!(w.of, None);

        let w = find_row(&paged, WALLPAPER_KEY);
        assert_eq!((w.at, w.of), (Some(3), Some(7)));

        // Only the picker row has a position. Every other row is `None`/`None`,
        // including the other `Text` row.
        for r in &paged {
            if r.key != WALLPAPER_KEY {
                assert_eq!(
                    (r.at, r.of),
                    (None, None),
                    "{} is {:?} and must not claim a picker position",
                    r.key,
                    r.kind
                );
            }
        }
        assert_eq!(
            find_row(&paged, "default-page").kind,
            SettingKind::Text,
            "the other Text row is still Text"
        );
    }

    /// `of == 0` is "unknown", not "empty", and `at` cannot escape `1..=of`.
    #[test]
    fn an_unknown_or_out_of_range_position_is_reported_as_unknown_or_clamped() {
        let s = LauncherState::default();
        let pos = |at: u32, of: u32| {
            let rows = build_paged(&s, at, of);
            let w = rows.iter().find(|r| r.key == WALLPAPER_KEY).expect("row");
            (w.at, w.of)
        };

        assert_eq!(
            pos(0, 0),
            (None, None),
            "`of == 0` means unknown, not \"0 of 0\""
        );
        assert_eq!(
            pos(3, 0),
            (None, None),
            "a position with no total is still unknown"
        );
        assert_eq!(pos(0, 7), (Some(1), Some(7)), "positions are 1-based");
        assert_eq!(
            pos(99, 7),
            (Some(7), Some(7)),
            "a position past the end clamps rather than rendering \"99 of 7\""
        );
        assert_eq!(pos(1, 1), (Some(1), Some(1)));
        assert_eq!(pos(4, 4), (Some(4), Some(4)));
    }

    /// The wallpaper row is a `Picker` and `apply` still declines it.
    ///
    /// This is a contract, not an accident: `apply` matches keys to mutations and
    /// a picker has no closed value set, so the shell intercepts this key and
    /// owns the picker itself. A `WALLPAPER_KEY` arm appearing in `apply` would
    /// be a second way to set the wallpaper that the picker does not know about.
    #[test]
    fn the_wallpaper_row_is_a_picker_and_apply_has_no_handler_for_it() {
        let s = LauncherState::default();
        let rows = build(&s);
        let w = rows.iter().find(|r| r.key == WALLPAPER_KEY).expect("row");
        assert_eq!(
            w.kind,
            SettingKind::Picker,
            "a wallpaper row that is not a picker is the dead tap this change \
             exists to remove"
        );
        let mut probe = s.clone();
        assert!(
            !apply(&mut probe, WALLPAPER_KEY, 1),
            "apply must decline the picker: the shell owns the candidate list"
        );
        assert_eq!(
            probe.wallpaper, s.wallpaper,
            "and must not have written anything"
        );

        // The commit the shell does instead, which is what makes the row live.
        let mut picker = picker::WallpaperPicker::new(wallpapers(7), 3);
        assert!(picker.move_by(1));
        probe.wallpaper = picker.selected().unwrap_or_default().to_string();
        probe.touch();
        assert_eq!(probe.wallpaper, "/data/wallpapers/04.png");
        assert!(probe.is_dirty(), "a commit marks the state for save");
        let rows = build(&probe);
        assert_eq!(
            rows.iter()
                .find(|r| r.key == WALLPAPER_KEY)
                .expect("row")
                .value,
            "Custom",
            "a committed path reads as Custom, not as the system's"
        );
    }

    #[test]
    fn a_text_row_never_draws_something_that_looks_like_a_switch() {
        // The rule `every_row_is_reachable_and_no_row_is_a_dead_tap` enforces,
        // asserted directly so the failure is legible without reading that loop.
        // "On" and "Off" are exactly what `on_off` renders for a `Toggle`.
        let s = LauncherState::default();
        for r in build(&s) {
            if r.kind == SettingKind::Text {
                assert_ne!(r.value, "On", "{} is Text and reads as a switch", r.key);
                assert_ne!(r.value, "Off", "{} is Text and reads as a switch", r.key);
                assert!(
                    TEXT_VALUES.contains(&r.value),
                    "{} is Text with unlisted value {:?}",
                    r.key,
                    r.value
                );
            }
        }

        // And the list is not vacuous: every entry is something a row actually
        // shows, so it cannot quietly become a place to hide a new value. Both
        // auto-rotate branches have to be reachable, which is what pins the
        // qualifier on the *false* one -- the bare "Off" this test exists to
        // reject.
        let mut on = LauncherState::default();
        on.auto_rotate = true;
        let shown: Vec<&str> = build(&s)
            .iter()
            .chain(build(&on).iter())
            .map(|r| r.value)
            .collect();
        for value in TEXT_VALUES {
            assert!(
                shown.contains(&value),
                "TEXT_VALUES holds {value:?}, which no row ever shows"
            );
        }
    }

    /// A toggle flips, and a second tap puts it back.
    #[test]
    fn a_toggle_flips_and_flips_back() {
        let mut s = LauncherState::default();
        let before = s.dark_theme;
        assert!(apply(&mut s, "dark-theme", 1));
        assert_ne!(s.dark_theme, before);
        assert!(apply(&mut s, "dark-theme", 1));
        assert_eq!(s.dark_theme, before, "a second tap restores the value");
    }

    /// Choices cycle and wrap. A choice that stopped at its last value would make
    /// the last entry the only way to reach the first.
    #[test]
    fn choices_wrap_rather_than_sticking() {
        let mut s = LauncherState::default();
        let seen: Vec<IconShape> = {
            let mut v = Vec::new();
            for _ in 0..4 {
                v.push(s.icon_shape);
                assert!(apply(&mut s, "icon-shape", 1));
            }
            v
        };
        assert_eq!(
            seen,
            vec![
                IconShape::Squircle,
                IconShape::Circle,
                IconShape::RoundedRect,
                IconShape::Squircle
            ],
            "three shapes, and the fourth tap returns to the first"
        );
    }

    /// Font scale only ever holds a value the panel can display.
    ///
    /// `build` looks the label up by matching, with `unwrap_or(0)`, so an
    /// off-list scale would silently render as 100% while the layout used
    /// something else. That divergence is invisible, so it is asserted.
    #[test]
    fn the_font_scale_label_always_matches_the_stored_value() {
        let mut s = LauncherState::default();
        for _ in 0..5 {
            let rows = build(&s);
            let label = rows
                .iter()
                .find(|r| r.key == "font-scale")
                .expect("the row exists")
                .value;
            let expected = FONT_LABELS
                .iter()
                .find(|l| **l == label)
                .expect("a known label");
            let pct: u16 = expected.trim_end_matches('%').parse().expect("a number");
            assert!(
                (s.font_scale - pct as f32 / 100.0).abs() < 0.001,
                "label {label:?} does not describe font_scale {}",
                s.font_scale
            );
            assert!(apply(&mut s, "font-scale", 1));
        }
    }

    /// Steps clamp at both ends instead of wrapping.
    ///
    /// Wrapping would be wrong here: a column count cycling from 12 back to
    /// "Auto" would silently resize the whole workspace on an over-tap, and the
    /// reference's slider stops at its bound (`PreferenceManager.kt:111`).
    #[test]
    fn steps_clamp_and_do_not_wrap() {
        let mut s = LauncherState::default();
        s.grid_cols = 12;
        assert!(!apply(&mut s, "grid-cols", 1), "already at the maximum");
        assert_eq!(s.grid_cols, 12, "an over-tap leaves the value alone");
        // Down from the maximum is a real change, so it succeeds -- the clamp is
        // a bound, not a latch.
        assert!(apply(&mut s, "grid-cols", -1));
        assert_eq!(s.grid_cols, 11);
        s.grid_cols = 0;
        assert!(!apply(&mut s, "grid-cols", -1), "already at the minimum");
        assert_eq!(s.grid_cols, 0, "`0` is Auto, a real stop, not a hole");
    }

    /// A renamed or removed setting must not be able to crash the shell.
    #[test]
    fn an_unknown_key_is_a_no_op_not_a_panic() {
        let mut s = LauncherState::default();
        assert!(!apply(&mut s, "no-such-setting", 1));
        assert!(!apply(&mut s, "", 1));
    }

    /// The accent colour follows its source.
    ///
    /// Leaving a custom seed behind a "from wallpaper" setting would make the
    /// two disagree about what the accent is, and the palette would keep using a
    /// colour the settings claim is not in use.
    #[test]
    fn the_accent_colour_is_cleared_when_its_source_stops_being_custom() {
        let mut s = LauncherState::default();
        assert_eq!(s.accent_source, AccentSource::Wallpaper);
        assert_eq!(s.accent_color, 0);
        assert!(apply(&mut s, "accent-source", 1));
        assert_eq!(s.accent_source, AccentSource::Custom);
        assert_ne!(s.accent_color, 0, "a custom source needs a colour to use");
        assert!(apply(&mut s, "accent-source", 1));
        assert_eq!(s.accent_source, AccentSource::Default);
        assert_eq!(
            s.accent_color, 0,
            "a non-custom source must not keep a stale seed"
        );
    }

    /// Every row carries the key `apply` matches on, and the keys are unique.
    ///
    /// Two rows sharing a key would mean a tap changing the wrong setting, and
    /// neither the renderer nor a test would notice.
    #[test]
    fn keys_are_unique() {
        let s = LauncherState::default();
        let rows = build(&s);
        for (i, a) in rows.iter().enumerate() {
            for b in rows.iter().skip(i + 1) {
                assert_ne!(a.key, b.key, "duplicate settings key {}", a.key);
            }
        }
    }

    /// The rows' own labels must agree with the state they were built from.
    #[test]
    fn a_rows_value_agrees_with_the_state() {
        let mut s = LauncherState::default();
        s.monochrome_icons = true;
        s.show_search_bar = false;
        let rows = build(&s);
        let find = |k: &str| rows.iter().find(|r| r.key == k).expect("row").value;
        assert_eq!(find("monochrome-icons"), "On");
        assert_eq!(find("show-search-bar"), "Off");
    }

    /// Every toggle's value string is one of the two the panel knows how to draw.
    #[test]
    fn toggle_values_are_only_on_or_off() {
        let s = LauncherState::default();
        for r in build(&s) {
            if r.kind == SettingKind::Toggle {
                assert!(
                    r.value == "On" || r.value == "Off",
                    "{} has toggle value {:?}",
                    r.key,
                    r.value
                );
            }
        }
    }
}
