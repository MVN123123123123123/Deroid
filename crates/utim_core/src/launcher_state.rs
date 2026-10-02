//! Persistent launcher state: the layer UTLC did not have at all.
//!
//! # Why this module exists
//!
//! Every other subsystem in this crate is downstream of one fact: until this
//! file existed, the launcher kept *everything* in stack locals inside
//! `utlc`'s `run_daemon`. `home_pages` was a `vec![…]` literal, the hotseat was
//! a five-name array, and the only `fs::write` in the whole binary was a
//! terminal log. A reboot returned the device to the factory layout, and there
//! was no way to express a user preference, because there was no store to put
//! one in.
//!
//! The reference keeps the same information in four places: the `Favorites`
//! SQLite table for icon positions (`DatabaseHelper.java:493`), a DataStore
//! named `"preferences"` for ~110 typed keys
//! (`PreferenceManager2.kt:975-978`), `FolderDao` for folder membership, and
//! `WallpaperDao` for wallpaper history. All four are reproduced here by one
//! flat key/value file, because a launcher on a 2 GB phone does not need a
//! database and AGENTS.md forbids the dependencies one would pull in.
//!
//! # Format
//!
//! Line-oriented `key=value`, `#` for comments, and a handful of conventions
//! that keep it unambiguous without a nesting syntax:
//!
//! * repeated keys are lists -- two `page.0=` lines would be a parse error, but
//!   `hidden=` may appear any number of times and keeps its order;
//! * `page.<n>=a|b|c` is the page at index `n`. Each entry is a **cell**:
//!   a bare app id, or `FOLDER:/<id>` for a folder, so a page line written by
//!   a build that predates folders still parses and no cell is lost;
//! * `folder.<id>=<title>|<rank>|<drawer>|<item>|…` is one folder, matching the
//!   reference's own split between a folder's identity and its membership
//!   (`data/folder/FolderEntity.kt:11,38`);
//! * `name.<app-id>=<label>` and `icon.<app-id>=<icon-key>` are per-app maps,
//!   keyed by app id so a renamed app keeps its settings across a rescan;
//! * `|` and `=` are escaped as `%7C` and `%3D` in any value, so an app label
//!   containing either character round-trips.
//!
//! Unknown keys are ignored and unparsable lines are skipped with the line
//! number reported. A corrupt or truncated file must not stop the phone
//! booting -- a launcher that panics because its preferences file has a stray
//! byte is a bricked device, and the reference degrades the same way
//! (`SharedPreferencesMigration` falls back to defaults on any read failure).
//!
//! # When it is written
//!
//! [`LauncherState::save_atomic`] is called from the shell's mutation sites and
//! on shutdown, never per frame. Writing on every frame would be an fsync per
//! vsync; the [`Self::dirty`] flag exists so the common case of "nothing
//! changed" costs one bool test.

use crate::graphics::layout::{FOLDER_GRID_MAX, FOLDER_GRID_MIN};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum dock slots. Matches `FRAME_MAX_DOCK` in the shell and the
/// reference's `numShownHotseatIcons` (`DeviceProfile.java:222`), which is
/// 4 on a 360 dp phone and 5 above it.
pub const DOCK_SLOTS: usize = 5;

/// Maximum workspace pages the reference will keep bound. Its model is an
/// "extra empty screen" that is added on drag and stripped when empty
/// (`Workspace.addExtraEmptyScreens:819`, `stripEmptyScreens:1090`); this is a
/// flat bound on the same idea.
pub const MAX_PAGES: usize = 16;

/// Icons per workspace page. `DeviceProfile`'s `numColumns` x `numRows` grid
/// (`InvariantDeviceProfile.java:455-456`); UTLC's phone profile is 4 x 6.
pub const PAGE_CAPACITY: usize = 24;

/// Folders the state file may hold, across the workspace and the drawer.
///
/// The reference has no such limit: `FolderDao.insertFolder` is a bare
/// `OnConflictStrategy.REPLACE` with no `LIMIT`
/// (`data/folder/service/FolderDao.kt:18-19`) and membership is a separate
/// row per item. That is fine when a real database enforces referential
/// integrity and UTLC does not have one, so a flat file needs a bound it can
/// apply at the point of allocation. 32 is two screens of folders on the
/// reference profile (24 cells per page) plus headroom for the drawer, which
/// is a number a user will not reach by accident.
pub const MAX_FOLDERS: usize = 32;

/// Items in one folder.
///
/// Also ours, for the same reason: `FolderPagedView` pages a folder without a
/// cap (`mOrganizer.getMaxItemsPerPage()`, `FolderGridOrganizer.java:92`, is
/// `cols * rows` and `getPageCount()` follows from the item count), so the
/// reference's only real limit is the user's patience. 64 is 2.6 pages at the
/// largest grid the reference will configure -- 5 x 5
/// (`FolderPreferences.kt:84,90`) -- and past it the folder is unusable on a
/// phone-sized panel regardless of what the file allows.
pub const MAX_FOLDER_ITEMS: usize = 64;

/// Bytes in one folder title.
///
/// The reference stores the title as a plain `String`
/// (`data/folder/FolderEntity.kt:13`) with no length constraint and renders it
/// through a single-line `EditText` that ellipsises at the footer width
/// (`Folder.java:706-712`). 64 bytes is about 5 lines of the footer at the
/// reference's 13 sp label size, so the tail is unreachable in the UI anyway;
/// bounding it here is what stops a corrupt or hostile file putting a
/// megabyte of text into a state file the shell writes on every mutation.
pub const MAX_FOLDER_TITLE_BYTES: usize = 64;

/// Prefix that marks a cell as a folder rather than an app.
///
/// The reference writes a folder's identity in two columns -- a container and
/// an item type -- and the type stringifies to `"FOLDER"`
/// (`LauncherSettings.java:218`, `ITEM_TYPE_FOLDER` at `:111`), with desktop
/// favourites anchored at `CONTAINER_DESKTOP = -100` (`:182`). Two columns
/// cannot be carried by one token, so this is the single-column spelling: the
/// type string, then `/`, then the id. The `/` is what makes it unambiguous
/// rather than merely prefixed -- an app id may legally contain a colon
/// (component keys are `pkg/class`), so a bare `FOLDER:7` could in principle
/// be an app id, while `FOLDER:/7` is not a legal Java identifier.
pub const FOLDER_CELL_PREFIX: &str = "FOLDER:/";

/// Icon shape, mirroring the reachable part of the reference's
/// `IconShape` presets (`icons/shape/IconShape.kt:312-420`). The reference
/// ships ~28 plus a custom four-corner editor; UTLC's `compositor::icons`
/// implements three, and only one of those is ever reached, so the
/// configuration surface exposes exactly the ones that exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconShape {
    /// Superellipse. The reference's default, and the only one `mask_tile`
    /// currently applies (`icons.rs:299`).
    Squircle = 0,
    Circle = 1,
    RoundedRect = 2,
}

impl IconShape {
    /// Parse a stored token, falling back to the default.
    pub fn from_token(s: &str) -> Self {
        match s {
            "circle" => Self::Circle,
            "rounded-rect" => Self::RoundedRect,
            _ => Self::Squircle,
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            Self::Squircle => "squircle",
            Self::Circle => "circle",
            Self::RoundedRect => "rounded-rect",
        }
    }

    /// Every shape, in menu order, for the settings surface.
    pub fn all() -> [Self; 3] {
        [Self::Squircle, Self::Circle, Self::RoundedRect]
    }
}

/// Where the accent colour comes from.
///
/// The reference resolves one accent from
/// `ColorOption` (`theme/color/ColorOption.kt:20-100`): the platform accent, the
/// wallpaper's primary, a custom colour, or the default. UTLC was hard-wired to
/// "average of the first wallpaper found", which is not one of these -- it is
/// an implementation detail that read as a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccentSource {
    /// Sample the wallpaper. UTLC's current behaviour, now named.
    Wallpaper = 0,
    /// A fixed colour the user chose.
    Custom = 1,
    /// The reference's default Material You palette, unseeded.
    Default = 2,
}

impl AccentSource {
    pub fn from_token(s: &str) -> Self {
        match s {
            "custom" => Self::Custom,
            "default" => Self::Default,
            _ => Self::Wallpaper,
        }
    }
    pub fn token(self) -> &'static str {
        match self {
            Self::Wallpaper => "wallpaper",
            Self::Custom => "custom",
            Self::Default => "default",
        }
    }
}

/// One occupied workspace cell.
///
/// `home_pages: Vec<Vec<String>>` cannot *express* a folder in its type: there
/// is no string that is both "one cell" and "several apps". What it can do is
/// carry the cell's token, so this enum is the typed view of it and
/// [`LauncherState::pages_as_cells`] / [`LauncherState::cells_as_pages`] are
/// the conversions. `home_pages` keeps its type, and so every existing reader
/// of it keeps compiling while the shell is migrated a call site at a time.
///
/// A reader that has not been migrated still behaves sanely: a folder token
/// resolves to no catalogue entry, so it draws as an empty slot rather than
/// as a wrong app. That is the property that makes the migration incremental
/// rather than all-or-nothing.
///
/// `Folder(u32)` holds a [`FolderRecord`] id, not a nested list, for two
/// reasons that both come from the reference: membership is a *separate row*
/// (`data/folder/FolderEntity.kt:32-38`, `FolderItems` with a `CASCADE`
/// foreign key, not a column of the desktop row), and the id is what the
/// reference's own model hands out (`FolderEntry.id`,
/// `data/folder/FolderEntity.kt:43`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// An app, by catalogue id. Matches an entry of `home_pages` one for one.
    App(String),
    /// A folder, by [`FolderRecord::id`].
    Folder(u32),
}

impl Cell {
    /// Parse one cell token: a bare app id, or `FOLDER:/<id>`.
    ///
    /// Anything after the prefix that is not a `u32` is an **app id**, not an
    /// error. A newer build may write a richer token, and a line-oriented
    /// format's promise is that an older build ignores what it cannot read
    /// rather than refusing the file -- the same rule the `page.` /
    /// `name.` / `icon.` prefixes follow below.
    pub fn from_token(token: &str) -> Self {
        if let Some(rest) = token.strip_prefix(FOLDER_CELL_PREFIX) {
            if let Ok(id) = rest.parse::<u32>() {
                return Self::Folder(id);
            }
        }
        Self::App(token.to_string())
    }

    /// The token [`Self::from_token`] reads.
    ///
    /// Allocates, so it belongs at save time and in tests, never on the frame
    /// path -- the same split the rest of this module keeps between
    /// [`Self::to_text`] and the geometry that runs per frame.
    pub fn token(&self) -> String {
        match self {
            Self::App(id) => id.clone(),
            Self::Folder(id) => format!("{FOLDER_CELL_PREFIX}{id}"),
        }
    }

    /// The app id, if this cell holds an app.
    #[inline]
    pub fn app_id(&self) -> Option<&str> {
        match self {
            Self::App(id) => Some(id.as_str()),
            Self::Folder(_) => None,
        }
    }

    /// The folder id, if this cell holds a folder.
    #[inline]
    pub fn folder_id(&self) -> Option<u32> {
        match self {
            Self::App(_) => None,
            Self::Folder(id) => Some(*id),
        }
    }
}

/// One folder: its title, its order, where it lives, and what is in it.
///
/// The four attributes are the reference's `FolderInfoEntity`
/// (`data/folder/FolderEntity.kt:11-17`) minus the two that have no UTLC
/// equivalent: `hide`, which is a *drawer* concept the reference models as a
/// folder attribute and this launcher has no drawer-fold to apply it to, and
/// `timestamp`, which exists only to break Room's change-detection ties.
///
/// Membership is inlined rather than being a second record type because the
/// file is flat: `FolderItemEntity` (`data/folder/FolderEntity.kt:32-38`) is a
/// separate table purely so the database can index `folderId`, and in a
/// `key=value` file the *key* already carries the folder id. `rank` on the
/// item is its index in `items`, exactly as `FolderService.updateFolderWithItems`
/// builds it (`data/folder/service/FolderService.kt:39-43`, `mapIndexed`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderRecord {
    /// Stable handle, referenced by [`Cell::Folder`]. Lowest free non-zero id.
    pub id: u32,
    /// User-set name. Empty means "show the reference's hint instead"
    /// (`Folder.java:706-712`, `mFolderName.setHint`).
    pub title: String,
    /// Drawer order. The reference reads folders `ORDER BY rank ASC`
    /// (`data/folder/service/FolderDao.kt:24-29`).
    pub rank: u32,
    /// App ids, in rank order. Rank `n` is `items[n]`.
    pub items: Vec<String>,
    /// `true` for a drawer folder, `false` for a workspace folder.
    ///
    /// UTLC has no drawer fold yet, so this is recorded and not yet consulted;
    /// it exists because a folder that later moves surfaces must not have to
    /// be re-created to learn where it came from.
    pub is_drawer: bool,
}

// ---------------------------------------------------------------------------
// Editing a folder's contents.
//
// `LauncherState` has the cross-folder operations (`folder_create`,
// `folder_add_item`, `folder_remove_item`, `folder_delete`) and the renderer has
// `FolderOpen`, so a folder could be *created* and *drawn* and never *changed*.
// These five are the missing middle: they take no id and look up no folder, so
// the shell holds a `&mut FolderRecord` for the folder it has open and calls them
// directly.
//
// Two properties are the reason they are an API rather than four `pub items`
// mutations the shell writes inline:
//
// * **total.** Every one of them answers `false` for an index or id it cannot
//   honour. The inputs are drag-handler arithmetic -- a cell index, a drop
//   target computed from a finger -- and the reference has the same problem in
//   the same place, which is why it *clamps* rather than throws when it inserts
//   (`Folder.java:1715`, `rank = Utilities.boundToRange(rank, 0,
//   mInfo.getContents().size())`, and `Utilities.java:671-673` is literally
//   `Math.max(lowerBound, Math.min(value, upperBound))`).
// * **truthful.** `false` means "nothing changed", not "an error". That is what
//   lets the shell write `if rec.move_item(i, j) { state.touch(); state.save(); }`
//   without marking the state dirty and doing an fsync for a tap that moved no
//   icon. It is the same convention `settings::apply` uses, and for the same
//   reason.
//
// None of these allocate on a frame path: they are gesture handlers. `reorder`
// does allocate one `Vec`, bounded by [`MAX_FOLDER_ITEMS`], and does so only on
// a successful permutation.
// ---------------------------------------------------------------------------

impl FolderRecord {
    /// Move the item at `from` to index `to`. `false` for an out-of-range
    /// `from`, an empty folder, or a move that changes nothing.
    ///
    /// `to` is clamped into `0..len`, so the last legal index is `len - 1` and a
    /// drag that reports a target past the end lands on the last slot rather
    /// than on a hole. The reference clamps the same way for the same reason
    /// (`Folder.java:1715`), and with the same upper bound expressed against
    /// the list it is about to insert into.
    ///
    /// Remove-then-insert, which is what makes `to` mean *the index the item
    /// ends at* rather than "how far to walk": moving 0 -> 3 in `[a b c d]`
    /// yields `[b c d a]`, not `[b c a d]`. The reference reaches the same
    /// result by a different route -- it inserts into a `GridLayout` at a rank
    /// and then re-ranks the whole contents list from position 0
    /// (`Folder.java:1379-1392`, `updateItemLocationsInDatabaseBatch` loops
    /// `for i in 0..total` calling `verifier.updateRankAndPos(itemInfo, i)`),
    /// which is the statement that *order is the list* on this side of the wire
    /// too.
    ///
    /// Returns `false` for `from == to` (including after `to` clamps onto
    /// `from`): the folder is byte-identical, so there is nothing to persist.
    pub fn move_item(&mut self, from: usize, to: usize) -> bool {
        if from >= self.items.len() {
            // Covers the empty folder too, and is the reason there is no separate
            // `is_empty` check: `0 >= 0`.
            return false;
        }
        // `len >= 1` here, so `len - 1` cannot underflow.
        let to = to.min(self.items.len() - 1);
        if from == to {
            return false;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
        true
    }

    /// Remove the item at `at`, shifting the rest down. `false` if out of range.
    ///
    /// The caller already knows which app it asked for -- `at` is a cell it drew
    /// -- which is why the removed id is dropped rather than returned. What the
    /// caller has to do with it is put it somewhere else, and that is the shell's
    /// choice: the reference hands the item back to the caller's completion path
    /// (`Folder.java:1740-1741`, `removeFolderContent` hands its `ItemInfo[]` to
    /// `mContent.removeItem` and to `rearrangeChildren`, and the drop target
    /// decides where it lands), and inventing a destination here would put the
    /// app somewhere the user did not drop it.
    pub fn remove_item(&mut self, at: usize) -> bool {
        if at >= self.items.len() {
            return false;
        }
        self.items.remove(at);
        true
    }

    /// Append `item`. `false` if it is already present, if it is empty, or if the
    /// folder is at [`MAX_FOLDER_ITEMS`].
    ///
    /// Appended, not inserted at a position: the reference's one-argument
    /// `addFolderContent` appends at `mInfo.getContents().size()`
    /// (`Folder.java:1705-1708`), and an append is what makes a drop land at the
    /// end of the visible grid rather than at the tap point.
    ///
    /// The duplicate refusal is **ours to enforce and the reference's to
    /// guarantee structurally**. It has no folder-level check because an item in
    /// its database *is* its row, with a `container` column
    /// (`LauncherSettings.java:169-173`), and `ModelWriter.addOrMoveItemInDatabase`
    /// (`:110-122`) adds or *moves* it rather than copying: "Adds an item to the
    /// DB if it was not created previously, or move it to a new
    /// `<container, screen, cellX, cellY>`". Two containers for one app is not
    /// expressible there. Here `items` is a plain list, so the same app twice
    /// draws the same icon in two cells and both launch the same thing -- so it
    /// is refused rather than silently duplicated. `LauncherState::sanitise`
    /// cleans up after any record that was built by some other route.
    ///
    /// This is also the one place [`Self::items`]' emptiness rule lives;
    /// `LauncherState::folder_add_item` delegates here so the bound and the
    /// duplicate rule have a single copy.
    pub fn insert_item(&mut self, item: &str) -> bool {
        if item.is_empty() || self.items.iter().any(|i| i == item) {
            return false;
        }
        if self.items.len() >= MAX_FOLDER_ITEMS {
            return false;
        }
        self.items.push(item.to_string());
        true
    }

    /// Remove `item` by value. `false` if it is not there.
    ///
    /// The by-value counterpart to [`Self::remove_item`], for a caller holding an
    /// app id rather than a cell: a drag-out that was resolved by id rather than
    /// by tap, or an app that vanished from the catalogue.
    ///
    /// Removes *one* occurrence. [`LauncherState::folder_remove_item`] uses
    /// `retain` and so also clears a duplicate that only a hand-built record
    /// could hold -- `sanitise` and [`Self::insert_item`] both prevent one, so the
    /// two agree on every state this crate can produce.
    pub fn remove(&mut self, item: &str) -> bool {
        let Some(at) = self.items.iter().position(|i| i == item) else {
            return false;
        };
        self.remove_item(at)
    }

    /// Reorder to match `order`, which must be a permutation of the current
    /// items. `false` and *nothing changed* otherwise.
    ///
    /// This is the guard against a drag handler that drops an index, and it is
    /// borrowed almost verbatim from the reference's own reorder guard,
    /// `FolderDao.updateFolderRanks` (`data/folder/service/FolderDao.kt:66-75`):
    ///
    /// ```kotlin
    /// val currentIds = getAllFolderIds().toSet()
    /// if (orderedIds.size != currentIds.size || orderedIds.toSet() != currentIds) {
    ///     return
    /// }
    /// ```
    ///
    /// A length check alone is not enough and a set comparison alone is not
    /// either, which is why the port needs both plus a distinctness check: with
    /// duplicated items -- possible in a record built by hand, before `sanitise`
    /// -- a set comparison passes while a duplicate silently *replaces* a real
    /// app. So each entry of `order` is resolved to its own unused index in
    /// `items` and must find one, and the lengths must match. A `reorder` that
    /// accepted a partial list would delete apps from a folder, which is worse
    /// than having no `reorder` at all.
    ///
    /// The new list is built first and assigned only once the whole of `order`
    /// has validated, so a rejection leaves `self` untouched without needing a
    /// rollback.
    ///
    /// Returns `false` for an order that is already in force as well, for the
    /// same reason [`Self::move_item`] does: an identical folder is not worth an
    /// fsync.
    pub fn reorder(&mut self, order: &[String]) -> bool {
        if order.len() != self.items.len() {
            return false;
        }
        let mut taken = vec![false; self.items.len()];
        let mut next: Vec<String> = Vec::with_capacity(order.len());
        for want in order {
            // The *first not-yet-taken* match, not the first match. With a
            // duplicate in `items` -- only a hand-built record can hold one,
            // since `sanitise` and `insert_item` both refuse to create it --
            // always taking index 0 would make every second entry of the same id
            // look like a repeat and reject a legitimate permutation. Consuming
            // the index is what makes this a permutation of the *multiset*.
            let Some(at) = self
                .items
                .iter()
                .enumerate()
                .position(|(at, cur)| !taken[at] && cur == want)
            else {
                // An id that is not in the folder, or one that has already been
                // used up by an earlier entry of `order`. The reference returns
                // here too (`FolderDao.kt:69`), and for the same reason: `set`
                // membership at a matching length is only a permutation if every
                // entry is really one of the current ids.
                return false;
            };
            taken[at] = true;
            next.push(want.clone());
        }
        if next == self.items {
            return false;
        }
        self.items = next;
        true
    }
}

/// The persisted launcher state.
///
/// Every field has a default that reproduces the behaviour UTLC had before this
/// module existed, so an absent or empty file is a valid configuration rather
/// than an error path. That is deliberate: the shell must boot identically
/// whether or not a state file has been written yet.
#[derive(Debug, Clone, PartialEq)]
pub struct LauncherState {
    /// App ids per workspace page, in cell order. Bounded by [`MAX_PAGES`] x
    /// [`PAGE_CAPACITY`]; the shell truncates on load rather than rejecting,
    /// because a state file written by a future version must not brick an older
    /// shell.
    pub home_pages: Vec<Vec<String>>,

    /// Which page a Home gesture returns to *right now*. Session state, but it
    /// is written because the reference persists the page order too, and losing
    /// it is the most visible single field to lose.
    pub current_page: usize,

    /// Which page Home returns to, as the user set it. The reference's
    /// `defaultHomePage` (`PreferenceManager2.kt:379`), reachable from the
    /// workspace popup's "Set default page" row
    /// (`Workspace.setDefaultPage:3918`).
    pub default_page: usize,

    /// Dock contents as app ids. Ids, not display names: the shell's old dock
    /// was a `["Phone", "Messages", …]` name match, which silently re-resolved
    /// to a different app the moment a catalogue entry was renamed.
    pub dock: Vec<String>,

    /// App ids the user hid. The reference's `hiddenApps`
    /// (`PreferenceManager2.kt:319`), filtered in
    /// `LawnchairAlphabeticalAppsList.updateItemFilter:79-86`.
    pub hidden_apps: Vec<String>,

    /// Per-app display-name overrides, id -> label. The reference's
    /// `customAppName` (`PreferenceManager.kt:133`). UTLC truncated every
    /// catalogue name to 12 characters at scan time (`main.rs:1236`), so a long
    /// name was mangled in *every* surface at once; the truncation is a
    /// rendering concern and belongs at the draw site, not in the catalogue.
    pub custom_names: Vec<(String, String)>,

    /// Per-app icon overrides, id -> icon key. The reference's `IconOverride`
    /// Room entity (`data/iconoverride/IconOverride.kt:10`), consulted *first*
    /// in `LawnchairIconProvider.resolveIconEntry:95-101`.
    pub icon_overrides: Vec<(String, String)>,

    /// Folders, ordered by [`FolderRecord::rank`].
    ///
    /// Held separately from [`Self::home_pages`] because the reference holds
    /// it separately too: a desktop row points at a folder, and the folder's
    /// contents live in their own table
    /// (`data/folder/FolderEntity.kt:19-38`). A [`Cell::Folder`] in
    /// `pages_as_cells()` is the only thing that joins the two, and it joins
    /// them by id, so renaming or emptying a folder never rewrites a page.
    ///
    /// Off the frame path: it is read at load and written at mutation, and
    /// the frame path reads the resolved `AppGridItem` slice the shell already
    /// builds (`drm_kms::DrmInteractiveState::folder_apps`).
    pub folders: Vec<FolderRecord>,

    /// Icon mask. The reference offers ~28 presets
    /// (`IconShapePreference.iconShapeEntries:77-110`); UTLC's renderer
    /// implements three, and the shape used to be a compile-time constant at
    /// `icons.rs:299`.
    pub icon_shape: IconShape,

    /// Tint every icon to a single colour. The reference's
    /// `forceIconMonochrome` (`PreferenceManager.kt:206`) driving
    /// `MonoIconThemeController` (`LawnchairThemeManager.kt:123`). UTLC's
    /// `icons::make_monochrome` is correct and has never been called.
    pub monochrome_icons: bool,

    /// User type scale, as a multiplier on the panel's base scale.
    ///
    /// `Layout::new_scaled` / `plain_scaled` (`layout.rs:284`, `:536`) already
    /// implement this end to end, threading it into `DeviceProfile::grid_metrics`
    /// and so into cell height and row pitch. `Layout::new` passed a literal
    /// `1.0` (`layout.rs:271`), so the whole mechanism was reachable only from
    /// four unit tests.
    pub font_scale: f32,

    /// Wallpaper, as a path. Empty means "the first one found", which is what
    /// UTLC probed unconditionally.
    pub wallpaper: String,

    /// The palette's colour seed as `0xAARRGGBB`. Zero means "derive it from
    /// the wallpaper", which is what UTLC did with no way to override.
    pub accent_color: u32,

    /// Where [`Self::accent_color`] comes from when it is non-zero.
    pub accent_source: AccentSource,

    /// Whether the launcher is dark. `false` selects
    /// `MaterialYouPalette::from_seed_light`, which exists and is fully tested
    /// (`palette.rs:477`) and which nothing in the shell has ever called.
    pub dark_theme: bool,

    /// Follow the system's light/dark setting rather than [`Self::dark_theme`].
    /// The reference's `ThemeChoice.SYSTEM`
    /// (`ui/preferences/components/ThemePreference.kt:12-30`).
    pub follow_system_theme: bool,

    /// 12-hour clock. UTLC's `format_current_time` writes `HH:MM`
    /// unconditionally (`main.rs:4691-4695`).
    pub clock_24h: bool,

    /// Show the home-screen search pill. The reference's
    /// `isHotseatEnabled` (`PreferenceManager2.kt:292`); the pill was
    /// unconditionally built and drawn.
    pub show_search_bar: bool,

    /// Rotate the panel with the accelerometer. The reference's `allowRotation`
    /// (`res/xml/launcher_preferences.xml:44`). UTLC has a complete
    /// orientation model (`sensors/sensor_proxy.rs:33-96`) and no path to
    /// reach it.
    pub auto_rotate: bool,

    /// Seconds of no input before the display blanks. Zero disables the
    /// timeout. The reference delegates to the system keyguard; UTLC has no
    /// blanking path at all, which is why a device running it never sleeps.
    pub screen_timeout_s: u32,

    /// Refuse home-screen edits. The reference's `lockHomeScreen`
    /// (`PreferenceManager2.kt:374`), which makes `Workspace.startDrag` return
    /// `null` before the drag view is built (`Workspace.java:1963-1973`).
    pub home_locked: bool,

    /// Workspace columns. Zero means "derive from panel width", which is what
    /// `grid_cols_for` (`layout.rs:250-258`) does today. The reference exposes
    /// `workspaceColumns` / `workspaceRows`
    /// (`PreferenceManager.kt:111-112`) with a live preview.
    pub grid_cols: usize,
    /// Workspace rows. Zero means "derive from the band height".
    pub grid_rows: usize,

    /// Folder grid size, `(cols, rows)`. Zero means "use the profile's 3x3".
    ///
    /// The reference exposes `folderColumns` and `folderRows` as 2..5 sliders
    /// (`FolderPreferences.kt:84,90`), feeding
    /// `PreferenceManager2.kt:706-711` -> `DeviceProfileOverrides.kt:122` ->
    /// `DeviceProfile.java:462`. UTLC hard-coded 3x3, so the renderer had the
    /// geometry (`DeviceProfile::with_folder_grid`) and nothing could reach it.
    ///
    /// Zero rather than 3 so an absent key means "the profile default", which is
    /// what a device profile is for.
    pub folder_cols: usize,
    /// Folder grid rows. See [`Self::folder_cols`].
    pub folder_rows: usize,

    /// Enable the launcher's own haptics. The reference's drawer haptic toggle
    /// (`PreferenceManager2.kt:436`) and its global feedback mode.
    pub haptics: bool,

    /// Set when a field has changed since the last save. Never persisted, and
    /// never compared: it exists so the shell can write on change rather than
    /// working out which mutation implies a write.
    dirty: bool,
}

impl Default for LauncherState {
    /// The factory configuration: exactly what UTLC produced before this module
    /// existed, so a fresh install and a wiped state file behave identically.
    fn default() -> Self {
        Self {
            home_pages: vec![
                vec![
                    "phone".into(),
                    "messages".into(),
                    "contacts".into(),
                    "clock".into(),
                    "files".into(),
                    "gallery".into(),
                    "music".into(),
                    "settings".into(),
                    "treble".into(),
                    "browser".into(),
                    "terminal".into(),
                    "camera".into(),
                ],
                vec![
                    "terminal".into(),
                    "treble".into(),
                    "contacts".into(),
                    "clock".into(),
                ],
            ],
            current_page: 0,
            default_page: 0,
            dock: vec![
                "phone".into(),
                "messages".into(),
                "settings".into(),
                "browser".into(),
                "camera".into(),
            ],
            hidden_apps: Vec::new(),
            custom_names: Vec::new(),
            icon_overrides: Vec::new(),
            // No folders, because no page holds a `Cell::Folder` either: the
            // factory layout predates the feature entirely, so a fresh install
            // is a launcher with zero folders rather than one with a folder
            // whose cell the workspace cannot draw.
            folders: Vec::new(),
            icon_shape: IconShape::Squircle,
            monochrome_icons: false,
            font_scale: 1.0,
            wallpaper: String::new(),
            accent_color: 0,
            accent_source: AccentSource::Wallpaper,
            dark_theme: true,
            follow_system_theme: false,
            clock_24h: true,
            show_search_bar: true,
            auto_rotate: false,
            screen_timeout_s: 0,
            home_locked: false,
            grid_cols: 0,
            grid_rows: 0,
            folder_cols: 0,
            folder_rows: 0,
            haptics: true,
            dirty: false,
        }
    }
}

impl LauncherState {
    /// The dock padded to [`DOCK_SLOTS`] entries, for a fixed-size index into
    /// the draw and hit-test paths. Empty slots are empty strings.
    pub fn dock_slots(&self) -> [&str; DOCK_SLOTS] {
        let mut out: [&str; DOCK_SLOTS] = [""; DOCK_SLOTS];
        for (i, slot) in out.iter_mut().enumerate() {
            if let Some(id) = self.dock.get(i) {
                *slot = id.as_str();
            }
        }
        out
    }

    /// `true` if the app is user-hidden.
    pub fn is_hidden(&self, id: &str) -> bool {
        self.hidden_apps.iter().any(|h| h == id)
    }

    /// The user's label for an app, if they set one.
    pub fn custom_name(&self, id: &str) -> Option<&str> {
        self.custom_names
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    }

    /// The user's icon for an app, if they set one.
    pub fn icon_override(&self, id: &str) -> Option<&str> {
        self.icon_overrides
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    }

    // ---------------------------------------------------------------- cells
    //
    // `pages_as_cells` / `cells_as_pages` are called from exactly one place
    // today: their own tests. The first production call site is `main.rs`, in
    // the workspace draw loop, replacing `state.home_pages[page]` with
    // `state.pages_as_cells()[page]` so a cell can be a folder; and in the save
    // path, `cells_as_pages(&cells)` writes `home_pages` back before
    // `save_to`. Both are one-line swaps because the round trip is exact.

    /// The workspace as cells: one [`Cell`] per occupied cell, in order.
    ///
    /// `home_pages` entries are either an app id or a folder token
    /// ([`Cell::from_token`] tells them apart), so this is a re-tag of the
    /// page rather than a re-read of a second structure: the cell *position*
    /// of a folder is stored in the page, and the folder's *contents* are
    /// stored in [`Self::folders`]. That split is the reference's -- a
    /// desktop row carries a folder's position and nothing about its
    /// contents (`LauncherSettings.java:236-243`, the `screen` / `cellX` /
    /// `cellY` columns; membership is `FolderItems`,
    /// `data/folder/FolderEntity.kt:32-38`).
    ///
    /// Allocates a fresh tree, so it is a load/edit-time call and not a
    /// frame-path one -- the shell builds the `AppGridItem` slice the renderer
    /// reads and calls this once per page change. That is what buys the
    /// one-line-at-a-time migration the type of [`Self::home_pages`] cannot
    /// allow on its own: the draw and hit paths keep reading `home_pages`
    /// until they are ready to read cells.
    pub fn pages_as_cells(&self) -> Vec<Vec<Cell>> {
        self.home_pages
            .iter()
            .map(|page| page.iter().map(|c| Cell::from_token(c)).collect())
            .collect()
    }

    /// Cells back to the `home_pages` spelling, ready for
    /// [`Self::to_text`].
    ///
    /// Exact round trip, with no caveats and no normalisation:
    /// `cells_as_pages(&s.pages_as_cells()) == s.home_pages` for every state.
    /// That is the property that makes the migration one line at a time -- a
    /// call site can convert, use cells, convert back, and write the file
    /// byte-for-byte as before.
    pub fn cells_as_pages(cells: &[Vec<Cell>]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|page| page.iter().map(Cell::token).collect())
            .collect()
    }

    // -------------------------------------------------------------- folders
    //
    // Uncalled outside their tests in this pass. The call sites are all in
    // `main.rs`: `folder_create` in the drag-to-create gesture and in the
    // folder long-press "ungroup" row, `folder_add_item` /
    // `folder_remove_item` on drop-into and drag-out, `folder_set_title` on the
    // footer editor's commit, `folder_delete` on ungroup and on
    // `FolderCollapse::IntoApp`, and `folder_of` as the one-container guard
    // before a drag starts. Each is one line followed by `state.touch()`.

    /// One folder by id.
    pub fn folder(&self, id: u32) -> Option<&FolderRecord> {
        self.folders.iter().find(|f| f.id == id)
    }

    /// Which folder holds `app_id`, if any.
    ///
    /// The lookup drag-to-create-folder needs and folder auto-fill needs:
    /// without it, "is this app already somewhere it can't be" is a scan over
    /// every folder at every drop. The reference reaches the same question
    /// through its model, where an item's container is a column
    /// (`LauncherSettings.java:169-173`), so the answer is one indexed read
    /// rather than a walk; here the scan is bounded by [`MAX_FOLDERS`] x
    /// [`MAX_FOLDER_ITEMS`] and happens once per drop, which is not a frame.
    ///
    /// The first folder in rank order wins, which is deterministic because
    /// [`Self::sanitise`] sorts by rank and [`Self::folder_create`] refuses to
    /// add an app that is already in one.
    pub fn folder_of(&self, app_id: &str) -> Option<u32> {
        self.folders
            .iter()
            .find(|f| f.items.iter().any(|i| i == app_id))
            .map(|f| f.id)
    }

    /// A new folder holding `items`, with the lowest free non-zero id.
    ///
    /// `rank` is `max(rank) + 1`, the reference's own append rule
    /// (`data/folder/service/FolderService.kt:49-52`). Ids are reused after a
    /// delete, unlike Room's `autoGenerate`; the reference never has to cope
    /// with that because its ids live in a database column, whereas here an
    /// id is a handle that only ever appears in a `Cell::Folder` in this same
    /// file, and the file is written whole and renamed into place
    /// ([`Self::save_to`]). A stale handle therefore cannot survive a save.
    ///
    /// `None` when [`MAX_FOLDERS`] folders already exist, or when `items`
    /// names an app that is already in another folder. Adding is *not*
    /// rejected because the app is still on a workspace page: the shell drops
    /// a cell into the folder in the same gesture, and if it forgets,
    /// [`Self::sanitise`] resolves it on the next load.
    pub fn folder_create(&mut self, title: &str, items: &[String], is_drawer: bool) -> Option<u32> {
        if self.folders.len() >= MAX_FOLDERS {
            return None;
        }
        let mut taken: Vec<&str> = Vec::new();
        let items: Vec<String> = items
            .iter()
            .take(MAX_FOLDER_ITEMS)
            .map(String::as_str)
            .filter(|id| !id.is_empty() && self.folder_of(id).is_none())
            .filter(|id| {
                if taken.contains(id) {
                    false
                } else {
                    taken.push(id);
                    true
                }
            })
            .map(|id| id.to_string())
            .collect();
        let id = self.next_free_folder_id();
        let rank = self
            .folders
            .iter()
            .map(|f| f.rank)
            .max()
            .map_or(0, |r| r + 1);
        self.folders.push(FolderRecord {
            id,
            title: clip_title(title),
            rank,
            items,
            is_drawer,
        });
        Some(id)
    }

    /// Drop a folder. The membership dies with it, which is the reference's
    /// `ON DELETE CASCADE` (`data/folder/FolderEntity.kt:26`) expressed as a
    /// field delete rather than a foreign key.
    ///
    /// `false` if there was no such folder. The caller still has to clear the
    /// [`Cell::Folder`] that pointed at it -- this module owns the record, the
    /// shell owns the cell.
    pub fn folder_delete(&mut self, id: u32) -> bool {
        let before = self.folders.len();
        self.folders.retain(|f| f.id != id);
        self.folders.len() != before
    }

    /// Append `app_id` to folder `id`. `false` if the folder is unknown, full,
    /// or already holds the app.
    ///
    /// Appended at the end, which is `rank = index` in
    /// `FolderService.updateFolderWithItems` (`data/folder/service/FolderService.kt:39-43`)
    /// and is what makes a drop land at the end of the visible grid rather
    /// than at the tap point.
    ///
    /// The one-container rule spans folders, so it cannot live on
    /// [`FolderRecord::insert_item`] -- this layer asks [`Self::folder_of`]
    /// first, which also catches the app already being in *this* folder. What is
    /// left -- the empty-id check, the [`MAX_FOLDER_ITEMS`] bound and the push --
    /// is delegated rather than restated, so the bound has one copy in the crate.
    /// The two agree on every input: a folder id that does not exist and an app
    /// already held both return `false` either way.
    pub fn folder_add_item(&mut self, id: u32, app_id: &str) -> bool {
        if app_id.is_empty() {
            return false;
        }
        // One container per app, the invariant the reference gets from its
        // `container` column and the pager's own move code. Refused rather
        // than silently duplicated: the same app twice in one folder draws
        // the same icon in two cells and both launch the same thing.
        if self.folder_of(app_id).is_some() {
            return false;
        }
        let Some(f) = self.folders.iter_mut().find(|f| f.id == id) else {
            return false;
        };
        f.insert_item(app_id)
    }

    /// Remove `app_id` from folder `id`. `false` if either is unknown.
    ///
    /// The app goes nowhere: the reference's `Folder.removeItem`
    /// (`Folder.java:1745-1760`) hands the item back to the caller's
    /// completion path, which puts it in whatever container the drop was on.
    /// Here that is the shell's choice of cell, and inventing one would put
    /// the app somewhere the user did not drop it.
    pub fn folder_remove_item(&mut self, id: u32, app_id: &str) -> bool {
        let Some(f) = self.folders.iter_mut().find(|f| f.id == id) else {
            return false;
        };
        let before = f.items.len();
        f.items.retain(|i| i != app_id);
        before != f.items.len()
    }

    /// Rename a folder. `false` if the folder is unknown.
    ///
    /// An empty title is legal and is not the same as clearing it: the
    /// reference shows `R.string.folder_hint_text` as the hint for an empty
    /// name (`Folder.java:706-712`), so the empty title is the untitled state
    /// and the renderer is what supplies the placeholder.
    pub fn folder_set_title(&mut self, id: u32, title: &str) -> bool {
        let Some(f) = self.folders.iter_mut().find(|f| f.id == id) else {
            return false;
        };
        let clipped = clip_title(title);
        f.title = clipped;
        true
    }

    /// The next id to hand out: the lowest non-zero integer not already in
    /// use. Skips 0 so a `Cell::Folder(0)` can never be confused with
    /// "unset", which matters because 0 is the default of a `u32` field the
    /// shell may build without one.
    fn next_free_folder_id(&self) -> u32 {
        let mut next = 1u32;
        loop {
            if self.folders.iter().all(|f| f.id != next) {
                return next;
            }
            next = next.saturating_add(1);
            // `MAX_FOLDERS` records cannot fill more than `MAX_FOLDERS` ids,
            // so this is unreachable; the clamp keeps it total.
            if next > MAX_FOLDERS as u32 {
                return MAX_FOLDERS as u32;
            }
        }
    }

    /// Add or replace a per-app label. `true` if it was added, `false` if an
    /// existing entry was replaced.
    pub fn set_custom_name(&mut self, id: &str, label: &str) -> bool {
        set_map_entry(&mut self.custom_names, id, label)
    }

    /// Add or replace a per-app icon key.
    pub fn set_icon_override(&mut self, id: &str, key: &str) -> bool {
        set_map_entry(&mut self.icon_overrides, id, key)
    }

    /// Mark the state as needing a write. Cheap, and it means the shell never
    /// has to reason about *which* mutation implies a save.
    pub fn touch(&mut self) {
        self.dirty = true;
    }

    /// Whether a field has changed since the last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Acknowledge that the state has been written.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Clamp every field into its legal range and return the state.
    ///
    /// The consuming half of [`Self::sanitise`], for the callers that want the
    /// bounds applied and have nothing to do afterwards. `sanitise` itself stays
    /// `&mut self` because the load path has the state in hand already and
    /// should not have to move it.
    pub fn sanitised(mut self) -> Self {
        self.sanitise();
        self
    }

    /// Clamp every field into its legal range.
    ///
    /// A state file is untrusted input: it is a text file on a writable path, it
    /// may have been written by a different build, and the shell must boot with
    /// it. Every bound is applied here, once, rather than at each use site.
    pub fn sanitise(&mut self) {
        self.home_pages.truncate(MAX_PAGES);
        for page in &mut self.home_pages {
            page.truncate(PAGE_CAPACITY);
        }
        // An empty workspace is unrecoverable -- there would be nothing to draw
        // and no way back to a page. Guarantee one page.
        if self.home_pages.is_empty() {
            self.home_pages.push(Vec::new());
        }
        let last = self.home_pages.len() - 1;
        self.current_page = self.current_page.min(last);
        self.default_page = self.default_page.min(last);

        self.dock.truncate(DOCK_SLOTS);
        self.hidden_apps.truncate(4096);
        // Duplicate ids would make the hidden set's meaning depend on iteration
        // order somewhere downstream.
        dedup_in_place(&mut self.hidden_apps);
        self.custom_names.truncate(4096);
        self.icon_overrides.truncate(4096);
        self.sanitise_folders();

        // A zero or non-finite scale would collapse the grid to nothing or make
        // every measurement NaN. `Layout::new_scaled` also guards, but the state
        // file should not be able to reach it with a bad value.
        if !self.font_scale.is_finite() || self.font_scale < 0.5 || self.font_scale > 3.0 {
            self.font_scale = 1.0;
        }
        self.grid_cols = self.grid_cols.min(12);
        self.grid_rows = self.grid_rows.min(12);
        // 0 is "the profile default", so it must survive sanitise; anything else
        // is clamped to the reference's slider range rather than to a bound of
        // convenience. A 3-column folder on a 2-column panel is not a state a
        // user can reach through the settings, and letting the file express it
        // would put geometry on screen that the settings cannot describe.
        self.folder_cols = if self.folder_cols == 0 {
            0
        } else {
            self.folder_cols.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX)
        };
        self.folder_rows = if self.folder_rows == 0 {
            0
        } else {
            self.folder_rows.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX)
        };
        self.screen_timeout_s = self.screen_timeout_s.min(86_400);
    }

    /// Apply the folder bounds, in the order the reference's model imposes
    /// them: rank order first, then truncation, then the one-container rule.
    ///
    /// Split out of [`Self::sanitise`] only because it is long and this way
    /// the invariant order is visible as the function's own order -- and the
    /// order matters. Truncating *after* sorting is what makes the
    /// [`MAX_FOLDERS`] that survive a deterministic subset (the lowest ranks)
    /// rather than whichever happened to parse first.
    fn sanitise_folders(&mut self) {
        // `FolderDao.getAllFoldersWithItems` reads `ORDER BY rank ASC`
        // (`data/folder/service/FolderDao.kt:24-29`), and `sort_by_key` is
        // stable, so equal ranks keep file order and the result is a
        // total order -- `folder_of`'s "first wins" is only deterministic
        // because of this line.
        self.folders.sort_by_key(|f| f.rank);
        self.folders.truncate(MAX_FOLDERS);

        let mut seen_ids: Vec<u32> = Vec::with_capacity(self.folders.len());
        self.folders.retain(|f| {
            // A duplicate id is not a second folder, it is an unreadable
            // line, and keeping both would make every `Cell::Folder(n)`
            // ambiguous. First in rank order wins.
            if f.id == 0 || seen_ids.contains(&f.id) {
                return false;
            }
            seen_ids.push(f.id);
            true
        });

        let mut taken: Vec<String> = Vec::new();
        for f in &mut self.folders {
            f.items.truncate(MAX_FOLDER_ITEMS);
            // Built into a fresh list rather than `retain`ed, because
            // `retain`'s predicate would read `taken` before the loop had
            // extended it -- so a repeat *within one folder* would survive
            // and the folder would draw the same app in two cells.
            let mut kept: Vec<String> = Vec::with_capacity(f.items.len());
            for id in &f.items {
                // An empty id is a gap in the dock's sense, not an app, and a
                // repeat is one app in two containers.
                if !id.is_empty() && !taken.contains(id) {
                    taken.push(id.clone());
                    kept.push(id.clone());
                }
            }
            f.items = kept;
            // Clipped, not rejected: the title is the one field with no
            // length the UI enforces, and a truncated label is a cosmetic
            // loss where a rejected folder is an app the user cannot open.
            clip_title_in_place(&mut f.title);
        }

        // One container per app. The reference gets this from the `container`
        // column (`LauncherSettings.java:169-173`), where an item *is* its
        // row and cannot be in two places; here it is a join between
        // `home_pages` and `folders` and has to be enforced.
        //
        // The page entry loses, not the folder entry: a folder's membership
        // is what the user built deliberately (a drag into it), and the page
        // entry is the residue of that gesture. This is also the self-heal
        // for a shell that added to a folder and forgot to blank the cell --
        // the next load removes the ghost rather than showing the app twice.
        if !taken.is_empty() {
            for page in &mut self.home_pages {
                page.retain(|id| !taken.contains(id));
            }
        }
    }

    /// Serialise to the on-disk text form.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str("# utlc launcher state v1\n");
        // Two free functions rather than nested closures: a closure capturing
        // `out` mutably cannot coexist with a second closure capturing the same
        // `out`, and the borrow checker is right to object -- two writers to one
        // buffer through two handles is a genuine aliasing hazard, not a
        // limitation worth working around.
        fn line(out: &mut String, k: &str, v: &str) {
            out.push_str(k);
            out.push('=');
            out.push_str(&escape_value(v));
            out.push('\n');
        }
        fn flag(out: &mut String, k: &str, v: bool) {
            line(out, k, if v { "1" } else { "0" });
        }

        for (i, page) in self.home_pages.iter().enumerate() {
            line(&mut out, &format!("page.{i}"), &page.join("|"));
        }
        // One `folder.<id>` line per folder, holding the identity the
        // reference keeps in `Folders` (title, rank) and the membership it
        // keeps in `FolderItems`, in the one line this format affords.
        for f in &self.folders {
            // The title and each member are escaped a *second* time, inside
            // the line, before `line` escapes the whole value.
            //
            // The second pass is what makes the positional split sound. A
            // title containing `|` -- "Tools | Games" is a title a real user
            // would type -- would otherwise escape to `%7C`, come back as a
            // real `|` from the outer unescape, and be read as a field
            // boundary. Escaping the fields before joining keeps a `|` in the
            // *text* distinct from the `|` that *separates* fields, using the
            // one escape function this module already has.
            let mut v = escape_value(&f.title);
            v.push('|');
            v.push_str(&f.rank.to_string());
            v.push('|');
            v.push(if f.is_drawer { '1' } else { '0' });
            for item in &f.items {
                v.push('|');
                v.push_str(&escape_value(item));
            }
            line(&mut out, &format!("folder.{}", f.id), &v);
        }
        line(&mut out, "current-page", &self.current_page.to_string());
        line(&mut out, "default-page", &self.default_page.to_string());
        line(&mut out, "dock", &self.dock.join("|"));
        for id in &self.hidden_apps {
            line(&mut out, "hidden", id);
        }
        for (id, label) in &self.custom_names {
            line(&mut out, &format!("name.{id}"), label);
        }
        for (id, key) in &self.icon_overrides {
            line(&mut out, &format!("icon.{id}"), key);
        }
        line(&mut out, "icon-shape", self.icon_shape.token());
        flag(&mut out, "monochrome-icons", self.monochrome_icons);
        line(&mut out, "font-scale", &fmt_f32(self.font_scale));
        line(&mut out, "wallpaper", &self.wallpaper);
        line(
            &mut out,
            "accent-color",
            &format!("{:#010x}", self.accent_color),
        );
        line(&mut out, "accent-source", self.accent_source.token());
        flag(&mut out, "dark-theme", self.dark_theme);
        flag(&mut out, "follow-system-theme", self.follow_system_theme);
        flag(&mut out, "clock-24h", self.clock_24h);
        flag(&mut out, "show-search-bar", self.show_search_bar);
        flag(&mut out, "auto-rotate", self.auto_rotate);
        line(
            &mut out,
            "screen-timeout",
            &self.screen_timeout_s.to_string(),
        );
        flag(&mut out, "home-locked", self.home_locked);
        line(&mut out, "grid-cols", &self.grid_cols.to_string());
        line(&mut out, "grid-rows", &self.grid_rows.to_string());
        line(&mut out, "folder-cols", &self.folder_cols.to_string());
        line(&mut out, "folder-rows", &self.folder_rows.to_string());
        flag(&mut out, "haptics", self.haptics);
        out
    }

    /// Parse the on-disk text form, starting from [`Self::default`].
    ///
    /// Unknown keys are ignored and malformed lines are skipped. Returns the
    /// number of lines that could not be understood, so a caller can log it --
    /// this is the signal that a file was truncated or hand-edited.
    pub fn from_text(text: &str) -> (Self, usize) {
        let mut s = Self::default();
        let mut bad = 0usize;
        // The first `page.<n>` key replaces the whole default workspace rather
        // than merging into it. Merging would mean a file that stores only
        // `page.0` -- a legal, minimal file -- silently kept the factory page 1
        // as well, so the user's "one page" workspace came back with two.
        let mut saw_page = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bad += 1;
                continue;
            };
            let (key, value) = (key.trim(), unescape_value(value));
            match key {
                "current-page" => s.current_page = value.parse().unwrap_or(s.current_page),
                "default-page" => s.default_page = value.parse().unwrap_or(s.default_page),
                "dock" => s.dock = split_list(&value),
                "hidden" => s.hidden_apps.push(value),
                "icon-shape" => s.icon_shape = IconShape::from_token(&value),
                "monochrome-icons" => s.monochrome_icons = value == "1",
                "font-scale" => s.font_scale = value.parse().unwrap_or(s.font_scale),
                "wallpaper" => s.wallpaper = value,
                "accent-color" => s.accent_color = parse_hex(&value),
                "accent-source" => s.accent_source = AccentSource::from_token(&value),
                "dark-theme" => s.dark_theme = value == "1",
                "follow-system-theme" => s.follow_system_theme = value == "1",
                "clock-24h" => s.clock_24h = value == "1",
                "show-search-bar" => s.show_search_bar = value == "1",
                "auto-rotate" => s.auto_rotate = value == "1",
                "screen-timeout" => {
                    s.screen_timeout_s = value.parse().unwrap_or(s.screen_timeout_s)
                }
                "home-locked" => s.home_locked = value == "1",
                "grid-cols" => s.grid_cols = value.parse().unwrap_or(s.grid_cols),
                "grid-rows" => s.grid_rows = value.parse().unwrap_or(s.grid_rows),
                "folder-cols" => s.folder_cols = value.parse().unwrap_or(s.folder_cols),
                "folder-rows" => s.folder_rows = value.parse().unwrap_or(s.folder_rows),
                "haptics" => s.haptics = value == "1",
                _ => {
                    if let Some(rest) = key.strip_prefix("page.") {
                        match rest.parse::<usize>() {
                            Ok(i) if i < MAX_PAGES => {
                                if !saw_page {
                                    s.home_pages.clear();
                                    saw_page = true;
                                }
                                // Pages may arrive out of order, so grow the
                                // vector to fit rather than rejecting a gap.
                                if i >= s.home_pages.len() {
                                    s.home_pages.resize(i + 1, Vec::new());
                                }
                                // A cell entry may be an app id or a
                                // folder token; `Cell::from_token` tells them
                                // apart, and both spellings live in the one
                                // `Vec<Vec<String>>` so the field's type --
                                // which other code reads -- is untouched.
                                s.home_pages[i] = split_list(&value);
                            }
                            // An index at or past the bound is refused, not
                            // truncated: a `page.99999999` in a corrupt file
                            // would otherwise resize the vector to that length
                            // before `sanitise` cut it back. The cap is applied
                            // here, at the point of allocation.
                            Ok(_) => bad += 1,
                            // A key that merely *starts* with "page." but is not
                            // one is a future version's format, not garbage.
                            Err(_) => {}
                        }
                    } else if let Some(id) = key.strip_prefix("name.") {
                        s.custom_names.push((id.to_string(), value));
                    } else if let Some(id) = key.strip_prefix("icon.") {
                        s.icon_overrides.push((id.to_string(), value));
                    } else if let Some(rest) = key.strip_prefix("folder.") {
                        match rest.parse::<u32>() {
                            Ok(id) if id != 0 && id <= MAX_FOLDERS as u32 => {
                                // The cap is applied here, at the point of
                                // allocation, for the reason `page.99999999`
                                // is: a file full of `folder.<n>` lines would
                                // otherwise build the whole vector before
                                // `sanitise` cut it back, and the id bound is
                                // what makes that impossible -- ids above
                                // `MAX_FOLDERS` cannot exist after sanitise.
                                match parse_folder_value(&value) {
                                    Some(mut f) => {
                                        f.id = id;
                                        s.folders.push(f);
                                    }
                                    None => bad += 1,
                                }
                            }
                            // Id 0 is reserved as "unset" by
                            // `next_free_folder_id`, and an id past the cap
                            // is a folder this build cannot represent.
                            _ => bad += 1,
                        }
                    }
                    // Anything else is a key from a newer build. Ignoring it is
                    // the whole point of a line-oriented format.
                }
            }
        }
        s.sanitise();
        (s, bad)
    }

    /// The state file path: `$XDG_STATE_HOME/utlc/launcher.state`, falling back
    /// to `/var/lib/utlc` when running as root, which is the convention
    /// `utim_core::session` already uses for the runtime directory.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/var/lib"));
        base.join("utlc").join("launcher.state")
    }

    /// Load from [`Self::default_path`]. A missing file is not an error: it
    /// yields the defaults with `dirty` false.
    pub fn load() -> Self {
        Self::load_from(&Self::default_path()).unwrap_or_default()
    }

    /// Load from an explicit path.
    pub fn load_from(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let (mut s, bad) = Self::from_text(&text);
        if bad > 0 {
            eprintln!(
                "[UTLC] {}: skipped {bad} unreadable line(s) in the launcher state",
                path.display()
            );
        }
        s.dirty = false;
        Some(s)
    }

    /// Write to [`Self::default_path`], atomically.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Write to an explicit path, atomically.
    ///
    /// Write to a sibling `.tmp`, flush, then `rename(2)`. A launcher that is
    /// power-cycled mid-write must find either the old file or the new one, never
    /// a half-written one -- the failure mode of a plain `File::create` here is a
    /// device that boots into a default layout and quietly loses the user's dock
    /// every time the battery dies during a save.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(self.to_text().as_bytes())?;
            // Durability matters here: without the sync the rename can land
            // before the data and the "atomic" write is a lie.
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

// ---------------------------------------------------------------------------
// Small helpers. No dependencies: AGENTS.md forbids a crate for string escapes.
// ---------------------------------------------------------------------------

/// Add or replace `key` in an ordered map, preserving position on replace.
/// `true` if added, `false` if replaced.
fn set_map_entry(map: &mut Vec<(String, String)>, key: &str, value: &str) -> bool {
    if let Some(slot) = map.iter_mut().find(|(k, _)| k == key) {
        slot.1.clear();
        slot.1.push_str(value);
        false
    } else {
        map.push((key.to_string(), value.to_string()));
        true
    }
}

fn dedup_in_place(items: &mut Vec<String>) {
    let mut seen: Vec<String> = Vec::with_capacity(items.len());
    items.retain(|item| {
        if seen.iter().any(|s| s == item) {
            false
        } else {
            seen.push(item.clone());
            true
        }
    });
}

/// `|` is the list separator, so it is the one character that must be escaped in
/// a value. `=` and `%` are escaped too so a value can never be mis-split by a
/// future format that treats them specially.
fn escape_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '|' => out.push_str("%7C"),
            '=' => out.push_str("%3D"),
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            _ => out.push(c),
        }
    }
    out
}

fn unescape_value(v: &str) -> String {
    if !v.contains('%') {
        return v.to_string();
    }
    let bytes = v.as_bytes();
    let mut out = String::with_capacity(v.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &v[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                // Only the four escapes this module writes are accepted; a
                // stray `%zz` is left verbatim rather than becoming a byte.
                match b {
                    0x7C | 0x3D | 0x25 | 0x0A => {
                        out.push(b as char);
                        i += 3;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        // Copy one whole character, so a multi-byte one is not split.
        let ch = v[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn split_list(v: &str) -> Vec<String> {
    if v.is_empty() {
        return Vec::new();
    }
    v.split('|').map(|s| s.to_string()).collect()
}

/// One `folder.<id>` value: `<title>|<rank>|<drawer>|<item>|…`.
///
/// `None` when the fixed part is missing or unparsable, which the caller
/// counts as an unreadable line. A folder with an empty title and no items is
/// the legal degenerate case and parses: `|0|0` is a title, a rank and a flag,
/// with nothing after it.
///
/// The split is positional, not by count, because the tail is the only
/// variable-length part: title first because it is the only field that may
/// contain arbitrary text, and the reference reads it back the same way
/// (`FolderDao.updateFolderTitle`, `data/folder/service/FolderDao.kt:37-38`).
///
/// `value` has already been through the line-level [`unescape_value`], so
/// every field is unescaped again here -- see the second `escape_value` in
/// [`LauncherState::to_text`] for why the text fields need it and the two
/// numeric fields do not.
fn parse_folder_value(value: &str) -> Option<FolderRecord> {
    let mut parts = value.splitn(4, '|');
    let title = unescape_value(parts.next()?);
    let rank = parts.next()?.parse::<u32>().ok()?;
    let drawer = parts.next()?;
    let items: Vec<String> = match parts.next() {
        Some(rest) if !rest.is_empty() => {
            split_list(rest).iter().map(|s| unescape_value(s)).collect()
        }
        _ => Vec::new(),
    };
    Some(FolderRecord {
        id: 0,
        title,
        rank,
        // Only `"1"` is true, matching every other flag in this format: a
        // garbled token must not silently turn a workspace folder into a
        // drawer one.
        is_drawer: drawer == "1",
        items,
    })
}

/// Truncate a folder title to [`MAX_FOLDER_TITLE_BYTES`] on a char boundary.
fn clip_title(title: &str) -> String {
    if title.len() <= MAX_FOLDER_TITLE_BYTES {
        return title.to_string();
    }
    let mut end = MAX_FOLDER_TITLE_BYTES;
    while end > 0 && !title.is_char_boundary(end) {
        end -= 1;
    }
    title[..end].to_string()
}

/// [`clip_title`] in place, so `sanitise` does not reallocate every title in
/// the file on a load that changed none of them.
fn clip_title_in_place(title: &mut String) {
    if title.len() <= MAX_FOLDER_TITLE_BYTES {
        return;
    }
    let clipped = clip_title(title);
    title.clear();
    title.push_str(&clipped);
}

/// Shortest representation that round-trips, so the file does not accumulate
/// float noise on every write.
fn fmt_f32(v: f32) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `0xAARRGGBB`, `AARRGGBB`, or a bare decimal. Anything unparsable is 0, which
/// means "derive it", not "black".
fn parse_hex(v: &str) -> u32 {
    let t = v.trim();
    let r = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => t.parse::<u32>(),
    };
    r.ok().filter(|v| *v != u32::MAX).unwrap_or(0)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_state_file_survives_a_real_write_read_cycle() {
        let dir = std::env::temp_dir().join(format!("utlc-rt-{}", std::process::id()));
        let p = dir.join("utlc").join("launcher.state");
        let mut s = LauncherState {
            home_pages: vec![vec!["alpha".into()], vec!["beta".into(), "gamma".into()]],
            current_page: 1,
            // A gap in the dock: an empty slot is a legal state, and it has to
            // survive the write as a gap rather than collapsing the list.
            dock: vec!["alpha".into(), String::new(), "gamma".into()],
            hidden_apps: vec!["secret".into()],
            font_scale: 1.5,
            dark_theme: false,
            clock_24h: false,
            home_locked: true,
            grid_cols: 5,
            ..Default::default()
        };
        // Every character the format has to escape.
        s.set_custom_name("com.x", "A = Label | With % Chars");
        s.save_to(&p).expect("save");
        let back = LauncherState::load_from(&p).expect("load");
        assert_eq!(back, s);
        // And the parent directory is created, not assumed.
        assert!(p.exists());
        println!("{}", std::fs::read_to_string(&p).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn the_default_state_is_the_pre_module_layout() {
        // If this changes, a fresh install and a wiped state file have silently
        // diverged, which is the one behaviour that must not drift.
        let s = LauncherState::default();
        assert_eq!(s.home_pages.len(), 2);
        assert_eq!(s.home_pages[0].len(), 12);
        assert_eq!(s.dock.len(), DOCK_SLOTS);
        assert_eq!(s.font_scale, 1.0);
        assert!(s.dark_theme);
        assert!(!s.home_locked);
        assert_eq!(s.icon_shape, IconShape::Squircle);
    }

    #[test]
    fn state_round_trips_through_text() {
        let mut s = LauncherState::default();
        s.home_pages[0][0] = "custom.app".into();
        s.home_pages.push(vec!["a".into(), "b".into(), "c".into()]);
        s.current_page = 2;
        s.default_page = 1;
        s.dock = vec!["x".into(), "y".into()];
        s.hidden_apps = vec!["secret.one".into(), "secret.two".into()];
        s.set_custom_name("com.a", "Renamed");
        s.set_icon_override("com.a", "my-icon");
        s.icon_shape = IconShape::Circle;
        s.monochrome_icons = true;
        s.font_scale = 1.25;
        s.wallpaper = "/usr/share/backgrounds/a.png".into();
        s.accent_color = 0xFF3B82F6;
        s.accent_source = AccentSource::Custom;
        s.dark_theme = false;
        s.follow_system_theme = true;
        s.clock_24h = false;
        s.show_search_bar = false;
        s.auto_rotate = true;
        s.screen_timeout_s = 30;
        s.home_locked = true;
        s.grid_cols = 5;
        s.grid_rows = 6;
        s.folder_cols = 4;
        s.folder_rows = 2;
        s.haptics = false;

        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0, "a file we wrote must parse cleanly");
        assert_eq!(back, s, "round trip must be lossless");
    }

    /// The separators are escaped, so an app label containing them survives.
    /// Without this a label with a `|` would silently split into two list
    /// entries and truncate the visible name.
    #[test]
    fn separators_inside_values_survive() {
        let mut s = LauncherState::default();
        s.set_custom_name("com.a", "Pipe | Equals = Percent %");
        s.set_custom_name("com.b", "Multi\nline");
        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0);
        assert_eq!(back.custom_name("com.a"), Some("Pipe | Equals = Percent %"));
        assert_eq!(back.custom_name("com.b"), Some("Multi\nline"));
    }

    /// A multi-byte label must not be cut in half by the unescaper.
    #[test]
    fn non_ascii_values_survive() {
        let mut s = LauncherState::default();
        s.set_custom_name("com.a", "Ünïcødé 日本語 🎉");
        let (back, _) = LauncherState::from_text(&s.to_text());
        assert_eq!(back.custom_name("com.a"), Some("Ünïcødé 日本語 🎉"));
    }

    /// A truncated or hand-mangled file must still yield a bootable state. The
    /// reference degrades the same way; a launcher that panics on a stray byte
    /// is a bricked device.
    #[test]
    fn a_corrupt_file_yields_defaults_not_a_panic() {
        let junk = "\u{0} not a key value\npage.0=a|b\npage.x=nope\n\
                    page.99999999=must not allocate\nfont-scale=nan\ndock\n\
                    grid-cols=notanumber\n#comment\n\n=\nhidden=keep\n";
        let (s, bad) = LauncherState::from_text(junk);
        // A line with no `=`, a non-numeric page index and an out-of-range one
        // are all unreadable. None of them may panic, hang, or allocate.
        assert!(bad >= 3, "expected the garbage to be counted, got {bad}");
        assert!(!s.home_pages.is_empty(), "one page always exists");
        assert_eq!(s.home_pages[0], vec!["a".to_string(), "b".to_string()]);
        assert!(
            s.home_pages.len() <= MAX_PAGES,
            "an index past the bound is refused, not allocated"
        );
        assert_eq!(s.hidden_apps, vec!["keep".to_string()]);
        // Out-of-range and unparsable numerics fall back rather than sticking.
        assert_eq!(s.font_scale, 1.0, "a non-finite scale is not a scale");
        assert_eq!(s.grid_cols, 0);
    }

    /// A minimal file -- one key, one page -- is a complete configuration, not
    /// a partial one. Merging it into the defaults would give the user back the
    /// factory page 1 they had just deleted.
    #[test]
    fn a_single_page_file_replaces_the_default_workspace() {
        let (s, bad) = LauncherState::from_text("page.0=solo\n");
        assert_eq!(bad, 0);
        assert_eq!(s.home_pages.len(), 1, "the default's second page is gone");
        assert_eq!(s.home_pages[0], vec!["solo".to_string()]);
    }

    /// Every bound is applied once, at load, so no use site has to check.
    #[test]
    fn sanitise_bounds_every_field() {
        let mut s = LauncherState {
            font_scale: 99.0,
            grid_cols: 400,
            grid_rows: 400,
            screen_timeout_s: u32::MAX,
            dock: vec!["a".into(); 50],
            hidden_apps: vec!["dup".into(), "dup".into(), "other".into()],
            // A page index from a wider workspace than this build keeps.
            current_page: 900,
            default_page: 900,
            ..Default::default()
        };
        for _ in 0..40 {
            s.home_pages.push(vec!["x".into(); 80]);
        }
        s.sanitise();

        assert_eq!(
            s.font_scale, 1.0,
            "a 99x type scale would be a white screen"
        );
        assert!(s.grid_cols <= 12 && s.grid_rows <= 12);
        assert_eq!(s.screen_timeout_s, 86_400);
        assert_eq!(s.dock.len(), DOCK_SLOTS);
        assert_eq!(s.hidden_apps, vec!["dup".to_string(), "other".to_string()]);
        assert_eq!(s.home_pages.len(), MAX_PAGES);
        assert!(s.home_pages.iter().all(|p| p.len() <= PAGE_CAPACITY));
        assert_eq!(s.current_page, MAX_PAGES - 1, "clamped to the last page");
        assert_eq!(s.default_page, MAX_PAGES - 1, "clamped to the last page");
    }

    /// A non-finite scale must not survive, whatever it came from: `NaN` would
    /// propagate into every cell measurement and turn the grid into a blank
    /// screen rather than a legible one.
    #[test]
    fn a_non_finite_scale_is_rejected() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            let s = LauncherState {
                font_scale: bad,
                ..Default::default()
            }
            .sanitised();
            assert!(s.font_scale.is_finite(), "{bad} must not survive");
            assert_eq!(s.font_scale, 1.0, "{bad} must fall back to the default");
        }
    }

    /// An empty workspace is unrecoverable: nothing to draw and no way back.
    #[test]
    fn an_empty_workspace_is_never_produced() {
        let (mut s, _) = LauncherState::from_text("page.0=\n");
        s.home_pages.clear();
        s.sanitise();
        assert_eq!(s.home_pages.len(), 1);
        assert!(s.home_pages[0].is_empty());
    }

    /// Pages may be written out of order or with gaps; the loader must not
    /// reject a later page just because an earlier one was skipped.
    #[test]
    fn pages_may_arrive_out_of_order() {
        let (s, bad) = LauncherState::from_text("page.2=c\npage.0=a\n");
        assert_eq!(bad, 0);
        assert_eq!(s.home_pages.len(), 3);
        assert_eq!(s.home_pages[0], vec!["a".to_string()]);
        assert!(s.home_pages[1].is_empty(), "the gap is an empty page");
        assert_eq!(s.home_pages[2], vec!["c".to_string()]);
    }

    #[test]
    fn the_dock_is_a_fixed_size_index() {
        let s = LauncherState {
            dock: vec!["only".into()],
            ..Default::default()
        };
        let slots = s.dock_slots();
        assert_eq!(slots[0], "only");
        assert_eq!(slots[1], "", "unused slots are empty, not absent");
        assert_eq!(slots.len(), DOCK_SLOTS);
    }

    #[test]
    fn per_app_maps_replace_in_place() {
        let mut s = LauncherState::default();
        assert!(s.set_custom_name("com.a", "First"), "a new key is added");
        assert!(
            !s.set_custom_name("com.a", "Second"),
            "an existing key is replaced"
        );
        assert_eq!(s.custom_names.len(), 1, "no duplicate entry is left behind");
        assert_eq!(s.custom_name("com.a"), Some("Second"));
        assert_eq!(s.custom_name("com.missing"), None);
    }

    #[test]
    fn the_dirty_flag_is_never_persisted() {
        let mut s = LauncherState::default();
        s.touch();
        assert!(s.is_dirty());
        let text = s.to_text();
        assert!(!text.contains("dirty"), "{text}");
        let (back, _) = LauncherState::from_text(&text);
        assert!(
            !back.is_dirty(),
            "a freshly parsed state has nothing to save"
        );
    }

    #[test]
    fn icon_shapes_round_trip_and_have_a_stable_order() {
        for shape in IconShape::all() {
            assert_eq!(IconShape::from_token(shape.token()), shape);
        }
        // An unknown token is a newer build's shape, not a hard error.
        assert_eq!(IconShape::from_token("droplet"), IconShape::Squircle);
        assert_eq!(
            IconShape::all().len(),
            3,
            "the three the renderer implements"
        );
    }

    #[test]
    fn accent_sources_round_trip() {
        for src in [
            AccentSource::Wallpaper,
            AccentSource::Custom,
            AccentSource::Default,
        ] {
            assert_eq!(AccentSource::from_token(src.token()), src);
        }
        assert_eq!(
            AccentSource::from_token("nonsense"),
            AccentSource::Wallpaper
        );
    }

    /// The atomic write must never leave a partial file behind, and a save over
    /// an existing file must replace it rather than append.
    #[test]
    fn saving_is_atomic_and_replaces() {
        let dir = std::env::temp_dir().join(format!("utlc-state-{}", std::process::id()));
        let path = dir.join("launcher.state");
        let mut s = LauncherState {
            dock: vec!["first".into()],
            ..Default::default()
        };
        s.save_to(&path).expect("first save");
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("dock=first"));

        s.dock = vec!["second".into()];
        s.save_to(&path).expect("second save");
        let second = fs::read_to_string(&path).unwrap();
        assert!(second.contains("dock=second"));
        assert!(!second.contains("first"), "the old contents must be gone");

        // The temporary file is renamed away, not left next to the target.
        assert!(!path.with_extension("tmp").exists());

        assert_eq!(
            LauncherState::load_from(&path).unwrap().dock,
            vec!["second".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing file is the first-run case, not a failure.
    #[test]
    fn a_missing_file_is_not_an_error() {
        assert!(LauncherState::load_from(Path::new("/nonexistent/utlc/launcher.state")).is_none());
    }

    // ---------------------------------------------------------------- cells

    /// The two page spellings must be one format. A device whose state file
    /// was written before folders existed has to come back with every cell it
    /// had, and a device whose file has folders must not lose them.
    #[test]
    fn the_old_page_spelling_still_loads_and_gains_nothing() {
        let (s, bad) = LauncherState::from_text("page.0=a|b|c\n");
        assert_eq!(bad, 0);
        assert_eq!(
            s.home_pages[0],
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        let cells = s.pages_as_cells();
        assert_eq!(
            cells[0],
            vec![
                Cell::App("a".into()),
                Cell::App("b".into()),
                Cell::App("c".into())
            ],
            "every legacy entry is an app cell, in order"
        );
        assert!(
            s.folders.is_empty(),
            "a legacy file has no folders to invent"
        );
    }

    /// A file written by the current shell round-trips through the cell form
    /// unchanged. This is the whole backward-compatibility claim: the format
    /// is extended, not replaced, so old readers and old files both work.
    #[test]
    fn the_cell_form_round_trips_exactly() {
        let s = LauncherState {
            home_pages: vec![vec!["FOLDER:/1".into(), "b".into(), "FOLDER:/2".into()]],
            ..Default::default()
        };
        let cells = s.pages_as_cells();
        assert_eq!(cells[0][0], Cell::Folder(1));
        assert_eq!(cells[0][1], Cell::App("b".into()));
        assert_eq!(cells[0][2], Cell::Folder(2));
        assert_eq!(cells[0][0].folder_id(), Some(1));
        assert_eq!(cells[0][0].app_id(), None, "a folder cell holds no app id");
        assert_eq!(
            LauncherState::cells_as_pages(&cells),
            s.home_pages,
            "cells must convert back byte-for-byte or the file churns on save"
        );
        // And through text, with the escape machinery in the middle.
        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0);
        assert_eq!(back.pages_as_cells(), cells);
    }

    /// A token that starts like a folder but is not one is an app id, not a
    /// parse error -- the rule every other prefix in this format follows.
    #[test]
    fn a_malformed_folder_token_degrades_to_an_app() {
        assert_eq!(
            Cell::from_token("FOLDER:/nope"),
            Cell::App("FOLDER:/nope".into())
        );
        assert_eq!(Cell::from_token("FOLDER:/"), Cell::App("FOLDER:/".into()));
        assert_eq!(Cell::from_token("FOLDER:7"), Cell::App("FOLDER:7".into()));
        // An id with leading zeros is still a folder; `u32::from_str` accepts
        // it and `token()` normalises it, which is a semantic no-op.
        assert_eq!(Cell::from_token("FOLDER:/007"), Cell::Folder(7));
        for c in [Cell::App(String::new()), Cell::Folder(u32::MAX)] {
            assert_eq!(Cell::from_token(&c.token()), c);
        }
    }

    /// The prefix must be distinguishable from anything an app id can be.
    /// `FOLDER:7` is not safe -- component keys are `pkg/class` and a package
    /// name is a Java identifier, so a colon cannot appear in one, but the
    /// `/` is what makes the guarantee independent of that reasoning.
    #[test]
    fn the_folder_prefix_cannot_collide_with_a_legal_component_key() {
        assert!(Cell::from_token("FOLDER:/3") != Cell::App("FOLDER:/3".into()));
        // A component key that merely mentions the prefix is still an app.
        assert_eq!(
            Cell::from_token("com.example.FOLDER:/3"),
            Cell::App("com.example.FOLDER:/3".into())
        );
    }

    // -------------------------------------------------------------- folders

    /// The reference stores four attributes and membership; all of them must
    /// survive a write, including a title containing every escaped character.
    #[test]
    fn a_folder_survives_a_write_read_cycle() {
        let mut s = LauncherState::default();
        // Members that are not on the default pages: an app in a folder leaves
        // the page (see `an_app_in_a_folder_leaves_the_page_on_load`), so
        // borrowing the factory ids would make the assertion about that rule
        // instead of about the file format.
        let id = s
            .folder_create(
                "Tools | = % \n",
                &["term.one".to_string(), "clock.one".into()],
                false,
            )
            .expect("first folder");
        s.home_pages[0] = vec![format!("{FOLDER_CELL_PREFIX}{id}"), "phone".into()];
        assert!(s.folder_add_item(id, "files.one"), "a free app is accepted");

        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0, "a file we wrote must parse cleanly");
        assert_eq!(back, s);
        let f = back.folder(id).expect("folder survives");
        assert_eq!(f.title, "Tools | = % \n");
        assert_eq!(f.rank, 0, "the first folder ranks zero");
        assert!(!f.is_drawer);
        assert_eq!(f.items, vec!["term.one", "clock.one", "files.one"]);
        assert_eq!(
            back.folder_of("clock.one"),
            Some(id),
            "membership is found by id"
        );
    }

    /// Every escape in the format, exercised through a folder title *and* a
    /// member id, because a folder line is the only value with three
    /// positional fields in front of the list.
    #[test]
    fn folder_values_survive_the_escapes() {
        let mut s = LauncherState::default();
        let id = s
            .folder_create(
                "A = B | C % D\nE",
                &["pipe|id".into(), "eq=id".into()],
                true,
            )
            .unwrap();
        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0);
        let f = back.folder(id).unwrap();
        assert_eq!(f.title, "A = B | C % D\nE");
        assert_eq!(f.items, vec!["pipe|id", "eq=id"]);
        assert!(f.is_drawer, "the drawer flag round-trips");
    }

    /// A degenerate line is still a folder: empty title, no items, zero rank.
    /// Refusing it would lose a folder the user made and did not fill yet.
    #[test]
    fn an_empty_folder_parses() {
        let (s, bad) = LauncherState::from_text("folder.7=|0|0\n");
        assert_eq!(bad, 0);
        let f = s.folder(7).expect("an untitled empty folder is legal");
        assert_eq!(f.title, "");
        assert!(f.items.is_empty());
        assert!(!f.is_drawer);
    }

    /// The reference reads folders `ORDER BY rank ASC` and appends at
    /// `max(rank) + 1`; both must be true here or folder order would depend
    /// on which line the parser happened to read first.
    #[test]
    fn folders_are_ordered_by_rank_not_by_file_order() {
        let (mut s, bad) = LauncherState::from_text(
            "folder.5=|9|0|late\nfolder.2=|1|0|early\nfolder.8=|4|0|middle\n",
        );
        assert_eq!(bad, 0);
        let ids: Vec<u32> = s.folders.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![2, 8, 5], "sorted by rank, ties keep file order");

        let n = s
            .folder_create("new", &[], false)
            .expect("appends after the highest rank");
        assert_eq!(s.folder(n).unwrap().rank, 10, "max(rank) + 1");
        // And a re-sort keeps it last.
        s.sanitise();
        assert_eq!(s.folders.last().unwrap().id, n);
    }

    /// Every bound the module declares, applied at load, so no use site has to
    /// check. A folder that exceeds one of them must be *cut*, not refused:
    /// the user still has to be able to open the apps that survived.
    #[test]
    fn folder_bounds_are_applied_at_load() {
        let many: Vec<String> = (0..MAX_FOLDER_ITEMS + 10)
            .map(|i| format!("app.{i}"))
            .collect();
        let long_title = "x".repeat(MAX_FOLDER_TITLE_BYTES + 40);
        let mut text = String::from("page.0=page.app|FOLDER:/1\n");
        // One folder per id, three of them past the bound so the extras are
        // refused at the point of allocation and reported. Every folder owns a
        // distinct member, so nothing is lost to the one-container rule and
        // the bounds below are what the assertions actually measure. Folder 2
        // carries the over-bound title and item list.
        for id in 1..=(MAX_FOLDERS as u32 + 3) {
            if id == 2 {
                text.push_str(&format!("folder.2={long_title}|0|0|{}\n", many.join("|")));
            } else {
                text.push_str(&format!("folder.{id}=|0|0|f{id}\n"));
            }
        }

        let (s, bad) = LauncherState::from_text(&text);
        assert!(bad >= 3, "folders past the id bound are counted, got {bad}");
        assert!(
            s.folders.len() <= MAX_FOLDERS,
            "the folder count is bounded, got {}",
            s.folders.len()
        );
        assert!(s.folder(1).is_some(), "the lowest id survives");
        let f = s.folder(2).expect("the over-bound folder survived");
        assert_eq!(
            f.items.len(),
            MAX_FOLDER_ITEMS,
            "items are cut, not refused"
        );
        assert_eq!(f.items[0], "app.0", "the kept items are the leading ones");
        assert!(
            f.title.len() <= MAX_FOLDER_TITLE_BYTES,
            "the title is clipped to {} bytes, got {}",
            MAX_FOLDER_TITLE_BYTES,
            f.title.len()
        );
        assert!(
            s.home_pages.iter().all(|p| p.len() <= PAGE_CAPACITY),
            "a folder still occupies one cell of the page bound"
        );
    }

    /// A title longer than the bound must be clipped, not refused, and the
    /// clip must land on a character boundary.
    #[test]
    fn an_overlong_title_is_clipped_on_a_char_boundary() {
        let mut s = LauncherState::default();
        let id = s.folder_create(&"é".repeat(200), &[], false).unwrap();
        let n = s.folder(id).unwrap().title.len();
        assert!(n <= MAX_FOLDER_TITLE_BYTES, "clipped to {n} bytes");
        assert_eq!(n % 2, 0, "2-byte chars, so an even count means no split");
        let (back, bad) = LauncherState::from_text(&s.to_text());
        assert_eq!(bad, 0);
        assert_eq!(back.folder(id).unwrap().title, s.folder(id).unwrap().title);
    }

    /// The reference gives an item exactly one container. Here the page and
    /// the folder are separate lists joined only by the app id, so the join
    /// has to be enforced -- and the *page* loses, because the folder
    /// membership is the part the user built deliberately.
    #[test]
    fn an_app_in_a_folder_leaves_the_page_on_load() {
        let (s, bad) = LauncherState::from_text("page.0=a|b|c\nfolder.1=|0|0|b|d\n");
        assert_eq!(bad, 0);
        assert_eq!(
            s.home_pages[0],
            vec!["a".to_string(), "c".to_string()],
            "b is in the folder, so the page must not also claim it"
        );
        assert_eq!(s.folder_of("b"), Some(1));
        // The self-heal is idempotent, so a save/reload loop is stable.
        let (again, _) = LauncherState::from_text(&s.to_text());
        assert_eq!(again, s, "one load is enough; the next changes nothing");
    }

    /// Adding is refused for an app that already has a container, which is
    /// what keeps the invariant above from needing to be repaired constantly.
    #[test]
    fn an_app_can_be_in_exactly_one_folder() {
        let mut s = LauncherState::default();
        let a = s.folder_create("A", &["phone".into()], false).unwrap();
        let b = s.folder_create("B", &[], false).unwrap();
        assert!(!s.folder_add_item(b, "phone"), "already in folder a");
        assert!(!s.folder_add_item(a, "phone"), "already in folder a, again");
        assert_eq!(s.folder(a).unwrap().items, vec!["phone"]);
        assert!(s.folder_add_item(b, "camera"), "a free app is accepted");
        assert_eq!(s.folder_of("camera"), Some(b));
        assert_eq!(s.folder_of("nothing"), None);
    }

    /// Duplicate and empty ids inside one folder line are garbage in a
    /// corrupt file and must not become two cells of the same app.
    #[test]
    fn duplicate_members_are_dropped_in_rank_order() {
        let (s, bad) = LauncherState::from_text("folder.1=|0|0|a||a|b|c|a\n");
        assert_eq!(bad, 0);
        assert_eq!(s.folder(1).unwrap().items, vec!["a", "b", "c"]);
    }

    /// Two records claiming one id would make every `Cell::Folder(n)`
    /// ambiguous, so the loser is dropped rather than renamed.
    #[test]
    fn a_duplicate_folder_id_is_dropped_not_renamed() {
        let (s, bad) = LauncherState::from_text("folder.1=first|0|0|a\nfolder.1=second|1|0|b\n");
        assert_eq!(bad, 0, "a repeated id is a readable line with one meaning");
        assert_eq!(s.folders.len(), 1);
        assert_eq!(s.folder(1).unwrap().title, "first");
    }

    /// The full mutation surface, in the order a drag performs it: create,
    /// fill, rename, remove, delete -- and each refusal reported separately so
    /// the shell can distinguish "no such folder" from "no room".
    #[test]
    fn folder_mutations_report_what_they_did() {
        let mut s = LauncherState::default();
        let a = s.folder_create("Alpha", &["phone".into()], false).unwrap();
        assert_eq!(a, 1, "ids start at one so 0 can mean unset");
        let b = s.folder_create("Beta", &[], false).unwrap();
        assert_ne!(a, b);

        assert!(s.folder_add_item(a, "clock"));
        assert!(s.folder_add_item(a, "camera"));
        assert!(!s.folder_add_item(a, ""), "an empty id is not an app");
        assert!(!s.folder_add_item(99, "phone"), "unknown folder");
        assert_eq!(s.folder(a).unwrap().items, vec!["phone", "clock", "camera"]);

        assert!(s.folder_set_title(a, "Renamed"));
        assert_eq!(s.folder(a).unwrap().title, "Renamed");
        assert!(!s.folder_set_title(99, "x"), "unknown folder");

        assert!(s.folder_remove_item(a, "clock"));
        assert!(!s.folder_remove_item(a, "clock"), "already gone");
        assert!(!s.folder_remove_item(99, "clock"), "unknown folder");
        assert_eq!(s.folder(a).unwrap().items, vec!["phone", "camera"]);
        assert_eq!(s.folder_of("clock"), None);

        assert!(s.folder_delete(b), "deleted");
        assert!(!s.folder_delete(b), "already gone");
        assert!(s.folder(b).is_none());
        // The surviving folder is untouched by its neighbour's deletion.
        assert_eq!(s.folder(a).unwrap().title, "Renamed");
    }

    // ------------------------------------------------------------ folder edits
    //
    // `FolderRecord`'s own mutation API. Every test here covers the *refusal*
    // as well as the happy path, because a folder editor that accepts a bad
    // index does not fail -- it silently deletes an app.

    /// A folder holding `ids`, for the edit tests.
    fn rec(ids: &[&str]) -> FolderRecord {
        FolderRecord {
            id: 7,
            title: "Tools".into(),
            rank: 0,
            items: ids.iter().map(|s| (*s).to_string()).collect(),
            is_drawer: false,
        }
    }

    fn ids(r: &FolderRecord) -> Vec<&str> {
        r.items.iter().map(String::as_str).collect()
    }

    /// The record's contents as one string, so "unchanged" can be asserted as
    /// byte-identity rather than as field-by-field equality. A rejected edit must
    /// not even reallocate into a differently-ordered but equal list.
    fn contents_of(r: &FolderRecord) -> String {
        format!("{}|{}|{:?}", r.title, r.items.join("|"), r.is_drawer)
    }

    /// `to` is the index the item *ends* at, in both directions, and an
    /// out-of-range target clamps rather than appending or panicking.
    #[test]
    fn move_item_lands_the_item_on_the_requested_index() {
        let mut f = rec(&["a", "b", "c", "d"]);
        assert!(f.move_item(0, 3));
        assert_eq!(ids(&f), vec!["b", "c", "d", "a"], "a ends at index 3");
        assert!(f.move_item(3, 0));
        assert_eq!(ids(&f), vec!["a", "b", "c", "d"], "and back again");

        // One step either way, which is where a remove-then-insert and an
        // insert-then-remove disagree.
        let mut f = rec(&["a", "b", "c"]);
        assert!(f.move_item(0, 1));
        assert_eq!(ids(&f), vec!["b", "a", "c"]);
        assert!(f.move_item(1, 2));
        assert_eq!(ids(&f), vec!["b", "c", "a"]);
    }

    /// A `to` past the end clamps to the last index, as the reference's
    /// `boundToRange` does (`Utilities.java:671-673`).
    #[test]
    fn move_item_clamps_its_target_and_refuses_a_bad_source() {
        let mut f = rec(&["a", "b", "c"]);
        assert!(f.move_item(0, 99));
        assert_eq!(ids(&f), vec!["b", "c", "a"], "clamped to the last index");
        assert!(f.move_item(1, 0));
        assert_eq!(ids(&f), vec!["c", "b", "a"]);

        assert!(!f.move_item(3, 0), "source one past the end");
        assert!(!f.move_item(usize::MAX, 0));
        assert_eq!(
            ids(&f),
            vec!["c", "b", "a"],
            "a refused move changes nothing"
        );

        // And the degenerate folders.
        let mut e = rec(&[]);
        assert!(!e.move_item(0, 0), "an empty folder has nothing to move");
        let mut one = rec(&["only"]);
        assert!(!one.move_item(0, 5), "clamped onto itself, so no move");
        assert_eq!(ids(&one), vec!["only"]);
    }

    /// A move that changes nothing reports `false`, so the shell does not write
    /// the state file for a drop that moved no icon.
    #[test]
    fn a_move_that_lands_where_it_started_is_not_a_move() {
        let mut f = rec(&["a", "b", "c"]);
        assert!(!f.move_item(1, 1), "same index");
        let before = f.clone();
        assert!(!f.move_item(1, 1));
        assert_eq!(f, before, "and it is byte-identical afterwards");
    }

    /// Removal by index shifts everything below it down, and every out-of-range
    /// index is refused rather than clamped into somebody else's app.
    #[test]
    fn remove_item_takes_exactly_that_slot() {
        let mut f = rec(&["a", "b", "c"]);
        assert!(f.remove_item(1));
        assert_eq!(
            ids(&f),
            vec!["a", "c"],
            "the rest shift down, nothing is skipped"
        );
        assert!(f.remove_item(1));
        assert_eq!(ids(&f), vec!["a"]);
        assert!(f.remove_item(0));
        assert_eq!(ids(&f), Vec::<&str>::new());
        assert!(!f.remove_item(0), "the folder is now empty");

        let mut f = rec(&["a", "b"]);
        assert!(!f.remove_item(2));
        assert!(!f.remove_item(usize::MAX));
        assert_eq!(ids(&f), vec!["a", "b"], "a refused removal takes nothing");
    }

    /// A folder must not hold the same app twice, and an empty id is not an app.
    #[test]
    fn insert_item_appends_and_refuses_a_duplicate() {
        let mut f = rec(&["a", "b"]);
        assert!(f.insert_item("c"));
        assert_eq!(ids(&f), vec!["a", "b", "c"], "appended at the end");
        assert!(!f.insert_item("c"), "already present");
        assert!(!f.insert_item("a"), "and not just the last one");
        assert_eq!(
            ids(&f),
            vec!["a", "b", "c"],
            "a refused insert adds nothing"
        );
        assert!(!f.insert_item(""), "an empty id is not an app");
        // Ids are compared exactly: no trimming, no case folding. `a` and `a `
        // are two different apps as far as the catalogue is concerned, and
        // quietly merging them would drop one of the user's icons.
        assert!(f.insert_item("a "), "a different id is a different app");
        assert_eq!(ids(&f), vec!["a", "b", "c", "a "]);
        assert!(f.insert_item("A"), "and matching is case-sensitive");
        assert_eq!(ids(&f), vec!["a", "b", "c", "a ", "A"]);
        assert!(f.remove("a "), "and both can be removed by their own id");
        assert!(f.remove("A"));
    }

    /// A folder is full at [`MAX_FOLDER_ITEMS`], and says so.
    #[test]
    fn a_full_folder_refuses_another_item() {
        let all: Vec<String> = (0..MAX_FOLDER_ITEMS).map(|i| format!("app{i}")).collect();
        let mut f = FolderRecord {
            items: all.clone(),
            ..rec(&[])
        };
        assert_eq!(f.items.len(), MAX_FOLDER_ITEMS);
        assert!(!f.insert_item("one-too-many"));
        assert_eq!(
            f.items, all,
            "and the refusal did not evict an existing app"
        );
        // One short, and it fits.
        f.items.pop();
        assert!(f.insert_item("fits"));
        assert_eq!(f.items.len(), MAX_FOLDER_ITEMS);
    }

    /// Removal by value, for a caller holding an id rather than a cell.
    #[test]
    fn remove_takes_an_item_by_value_and_reports_an_absent_one() {
        let mut f = rec(&["a", "b", "c"]);
        assert!(f.remove("b"));
        assert_eq!(ids(&f), vec!["a", "c"]);
        assert!(!f.remove("b"), "already gone");
        assert!(!f.remove("zz"), "never there");
        assert!(!f.remove(""), "an empty id matches no item");
        assert_eq!(ids(&f), vec!["a", "c"]);

        let mut e = rec(&[]);
        assert!(!e.remove("a"), "an empty folder holds nothing to remove");
    }

    /// Every permutation is accepted, and every non-permutation is refused.
    ///
    /// The refusals are the point. A `reorder` that accepted a partial list would
    /// delete apps from a folder, and one that accepted a duplicate would
    /// silently replace one with another.
    #[test]
    fn reorder_accepts_only_a_permutation() {
        let mut f = rec(&["a", "b", "c", "d"]);

        // The happy path, in both directions.
        assert!(f.reorder(&["d".into(), "c".into(), "b".into(), "a".into()]));
        assert_eq!(ids(&f), vec!["d", "c", "b", "a"]);
        assert!(f.reorder(&["b".into(), "a".into(), "d".into(), "c".into()]));
        assert_eq!(ids(&f), vec!["b", "a", "d", "c"]);
        assert!(f.reorder(&["c".into(), "b".into(), "a".into(), "d".into()]));
        assert_eq!(ids(&f), vec!["c", "b", "a", "d"]);

        let good = f.clone();
        // Too short: three entries for four items.
        assert!(!f.reorder(&["c".into(), "b".into(), "a".into()]));
        // Too long: five for four.
        assert!(!f.reorder(&["c".into(), "b".into(), "a".into(), "d".into(), "e".into()]));
        assert!(!f.reorder(&[]), "empty for a non-empty folder");
        // A duplicate and a missing app, at exactly the right length.
        assert!(!f.reorder(&["c".into(), "b".into(), "c".into(), "a".into()]));
        // An id that was never in the folder, balanced by one left out.
        assert!(!f.reorder(&["c".into(), "b".into(), "a".into(), "zz".into()]));
        // Two unknown ids and two real ones.
        assert!(!f.reorder(&["c".into(), "yy".into(), "a".into(), "zz".into()]));
        assert_eq!(f, good, "every refusal left the folder byte-identical");

        // The identity is a permutation too, but it changed nothing, so it is
        // not a change and must not report one.
        assert!(!f.reorder(&["c".into(), "b".into(), "a".into(), "d".into()]));
        assert_eq!(f, good);

        // An empty folder can only be reordered to empty, which is again a no-op.
        let mut e = rec(&[]);
        assert!(!e.reorder(&[]));
        assert!(!e.reorder(&["a".into()]));
        assert!(e.items.is_empty());
    }

    /// A rejected `reorder` must not half-apply, and that includes a rejection
    /// that is only discovered part way through the list.
    #[test]
    fn a_rejected_reorder_leaves_the_folder_byte_identical() {
        let mut f = rec(&["a", "b", "c", "d", "e"]);
        let before = contents_of(&f);
        // The bad entry is last, so a naive implementation has already pushed
        // four of the five before it finds out.
        assert!(!f.reorder(&["e".into(), "d".into(), "c".into(), "b".into(), "zz".into()]));
        assert_eq!(contents_of(&f), before, "the folder must be untouched");

        // And the same for a record with a duplicate in it, which a set
        // comparison alone would wave through.
        let mut dup = rec(&["a", "a", "b"]);
        let before = contents_of(&dup);
        assert!(!dup.reorder(&["a".into(), "b".into(), "b".into()]));
        assert_eq!(contents_of(&dup), before, "a set comparison is not enough");

        // The genuine permutation of a record with a duplicate does apply.
        assert!(dup.reorder(&["b".into(), "a".into(), "a".into()]));
        assert_eq!(ids(&dup), vec!["b", "a", "a"]);
    }

    /// The record API and the state-level API must not disagree about the
    /// empty-id, duplicate and full-folder refusals.
    ///
    /// `folder_add_item` delegates to `insert_item` for exactly this reason, but
    /// a delegation can be undone by a later edit, and the two entry points are
    /// what the shell will use from different places.
    #[test]
    fn the_state_and_the_record_agree_on_what_a_folder_will_accept() {
        let mut s = LauncherState::default();
        let id = s.folder_create("A", &[], false).unwrap();

        // `insert_item` sees only this folder, so it cannot enforce the
        // one-container-per-app rule across folders -- which is why the
        // cross-folder check stays in `folder_add_item`. Every *other* refusal
        // is shared, which is what the delegation is for.
        let mut r = FolderRecord::default();
        assert_eq!(r.insert_item("x"), s.folder_add_item(id, "x"));
        assert_eq!(r.insert_item("x"), s.folder_add_item(id, "x"), "duplicate");
        assert_eq!(r.insert_item(""), s.folder_add_item(id, ""), "empty id");
        assert_eq!(r.insert_item("y"), s.folder_add_item(id, "y"));

        // An app already in *another* folder is refused by the state and
        // accepted by the record. That is the one intended difference, and it is
        // the reason the cross-folder check cannot move down into `insert_item`.
        let other = s.folder_create("B", &[], false).unwrap();
        assert!(s.folder_add_item(other, "z"));
        let mut solo = FolderRecord::default();
        assert!(
            solo.insert_item("z"),
            "a record cannot see the other folder"
        );
        assert!(!s.folder_add_item(other, "z"), "the state can, and refuses");
        assert!(
            !s.folder_add_item(other, "y"),
            "and an app already in the first folder is refused too"
        );

        // And the removal paths agree on what is present.
        assert_eq!(r.remove("x"), s.folder_remove_item(id, "x"));
        assert_eq!(r.remove("x"), s.folder_remove_item(id, "x"), "already gone");
        assert_eq!(r.remove("nope"), s.folder_remove_item(id, "nope"));
    }

    /// Ids are handed out lowest-first and reused after a delete, so a state
    /// file does not grow an ever-larger id space.
    #[test]
    fn folder_ids_are_reused_after_a_delete() {
        let mut s = LauncherState::default();
        let a = s.folder_create("A", &[], false).unwrap();
        let b = s.folder_create("B", &[], false).unwrap();
        assert_eq!((a, b), (1, 2));
        s.folder_delete(a);
        assert_eq!(
            s.folder_create("C", &[], false),
            Some(1),
            "the gap is reused"
        );
        // Full means full, and it says so rather than overwriting one.
        for _ in 0..MAX_FOLDERS {
            s.folder_create("filler", &[], false);
        }
        assert_eq!(s.folders.len(), MAX_FOLDERS);
        assert!(s.folder_create("one too many", &[], false).is_none());
    }

    /// The default state is the pre-module layout, and that includes *no*
    /// folders: a fresh install must not claim the workspace can draw one
    /// before the shell can.
    #[test]
    fn the_default_state_has_no_folders() {
        assert!(LauncherState::default().folders.is_empty());
        assert!(LauncherState::default().pages_as_cells()[0]
            .iter()
            .all(|c| matches!(c, Cell::App(_))));
    }
}
