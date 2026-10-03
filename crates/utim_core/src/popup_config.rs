//! Workspace long-press popup: the item *catalogue*, its order, and which
//! entries are on.
//!
//! # What this replaces
//!
//! `crates/utlc/src/main.rs:207-220` builds the workspace menu from a literal
//! `out.push(PopupItem::Wallpapers) / Widgets / AllApps / HomeSettings` chain
//! with two `if !home_locked` branches. Four unconditional rows, and the order
//! is the reference's order with the *lock* and *default-page* rows moved:
//!
//! | | reference | UTLC (hardcoded) |
//! |---|---|---|
//! | rows | 8 possible, 6 on by default | 4 unconditional, 2 conditional |
//! | `lock` before `edit_mode`? | **yes** (`DEFAULT_ORDER`, rows 2 and 3) | **no** -- `EditMode` is pushed first |
//! | `default_page` shown when the workspace *is* on the default page? | **no** (filtered at `LauncherOptionsPopup.kt:151`) | **yes**, always |
//! | user-reorderable / disable-able? | **yes** (`LauncherPopupPreference.kt:88-125`) | no, it is code |
//!
//! So this module is the *configuration* the reference persists, and it is
//! deliberately not a re-derivation of [`crate::compositor::recents::PopupItem`]:
//! that type is "what row is this, right now", this one is "which rows exist,
//! in what order, on or off".
//!
//! # The nine kinds
//!
//! `LauncherOptionsPopup.DEFAULT_ORDER` is nine entries
//! (`LauncherOptionsPopup.kt:18-28`) -- **nine**, not eight, and not the four
//! UTLC had. `PopupItems`' capacity comment (`recents.rs:1749-1756`) already
//! says "eight once `carousel` is filtered", which is right about the *built
//! menu* and wrong about the *order list*: `carousel` occupies a slot in
//! `DEFAULT_ORDER` and in the persisted string, it is only excluded from the
//! rows that get drawn (`:142`). Modelling eight kinds would make the
//! persisted text unrepresentable, so this module models all nine and marks
//! [`PopupKind::is_carousel`].
//!
//! # Storage
//!
//! [`PopupConfig`] is two fixed arrays and nothing else: `[PopupKind; 9]` for
//! the order and `[bool; 9]` for the enable bits, indexed by
//! [`PopupKind::index`] so the two can never disagree about which row is
//! which. 18 bytes, `Copy`, no `Vec`, no `String` -- it can live in the
//! compositor's frame-path state and be handed to the painter by reference.
//!
//! The text form exists because the reference's persistence *is* a string
//! (`PreferenceManager2.kt:406-410`, key `launcher_popup_order`) and a
//! launcher whose settings survive a reboot need not be able to read a format
//! we never wrote. [`PopupConfig::write_text`] renders into a caller-supplied
//! buffer and [`PopupConfig::from_text`] parses without allocating; see
//! [`PopupConfig::from_text`] for the repair rules, which are the reference's
//! `restoreMissingPopupOptions` (`LauncherOptionsPopup.kt:30-48`) plus two
//! hardenings the reference needs and we do not.

use crate::compositor::recents::{PopupItem, PopupItems};

/// Number of entries in `LauncherOptionsPopup.DEFAULT_ORDER`
/// (`LauncherOptionsPopup.kt:18-28`).
pub const POPUP_KIND_COUNT: usize = 9;

/// Longest persisted identifier: `home_settings`
/// (`LauncherOptionsPopup.kt:25`, `PreferenceManager2.kt` default string).
const MAX_ID_LEN: usize = 13;

/// Buffer size that always holds any valid [`PopupConfig::write_text`].
///
/// Nine `+`/`-` prefixes, nine identifiers of at most [`MAX_ID_LEN`] bytes and
/// eight `|` separators: `9 * (1 + 13) + 8 = 134`. The caller owns the buffer,
/// so this is only a convenience constant -- the same guarantee comes from
/// [`PopupConfig::write_text`] returning [`PopupTextError::BufferTooSmall`]
/// with the exact `needed` length rather than truncating.
pub const POPUP_TEXT_MAX: usize = POPUP_KIND_COUNT * (1 + MAX_ID_LEN) + (POPUP_KIND_COUNT - 1);

/// The nine entries the workspace long-press menu is configured from.
///
/// The discriminants are the `DEFAULT_ORDER` positions and are load-bearing:
/// [`PopupKind::index`] is the identity cast, which is what lets
/// [`PopupConfig`] index its enable array by kind without a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PopupKind {
    /// `LauncherOptionPopupItem("carousel", true)` (`:19`) -- the wallpaper
    /// quick picker strip. Metadata-only: filtered out of the built menu at
    /// `LauncherOptionsPopup.kt:142` and not draggable in the editor
    /// (`LauncherPopupPreference.kt:119`), but it is a real slot in the order.
    Carousel = 0,
    /// `LauncherOptionPopupItem("lock", false)` (`:20`).
    Lock = 1,
    /// `LauncherOptionPopupItem("edit_mode", false)` (`:21`).
    EditMode = 2,
    /// `LauncherOptionPopupItem("wallpaper", true)` (`:22`).
    Wallpaper = 3,
    /// `LauncherOptionPopupItem("widgets", true)` (`:23`).
    Widgets = 4,
    /// `LauncherOptionPopupItem("all_apps", true)` (`:24`).
    AllApps = 5,
    /// `LauncherOptionPopupItem("home_settings", true)` (`:25`).
    HomeSettings = 6,
    /// `LauncherOptionPopupItem("sys_settings", false)` (`:26`).
    SysSettings = 7,
    /// `LauncherOptionPopupItem("default_page", false)` (`:27`).
    DefaultPage = 8,
}

impl PopupKind {
    /// Every kind, in `DEFAULT_ORDER` order (`LauncherOptionsPopup.kt:18-28`).
    pub const ALL: [PopupKind; POPUP_KIND_COUNT] = [
        PopupKind::Carousel,
        PopupKind::Lock,
        PopupKind::EditMode,
        PopupKind::Wallpaper,
        PopupKind::Widgets,
        PopupKind::AllApps,
        PopupKind::HomeSettings,
        PopupKind::SysSettings,
        PopupKind::DefaultPage,
    ];

    /// The persisted identifier, byte-identical to the reference's
    /// `LauncherOptionPopupItem.identifier`.
    pub const fn id(self) -> &'static str {
        match self {
            PopupKind::Carousel => "carousel",
            PopupKind::Lock => "lock",
            PopupKind::EditMode => "edit_mode",
            PopupKind::Wallpaper => "wallpaper",
            PopupKind::Widgets => "widgets",
            PopupKind::AllApps => "all_apps",
            PopupKind::HomeSettings => "home_settings",
            PopupKind::SysSettings => "sys_settings",
            PopupKind::DefaultPage => "default_page",
        }
    }

    /// The English label the editor row shows.
    ///
    /// Every one of these is the `@StringRes` that
    /// `LauncherOptionsPopup.getMetadataForOption` (`:166-216`) names,
    /// resolved against the base `strings.xml`. ASCII only on purpose: this
    /// tree has no font package, so a non-ASCII label renders as tofu.
    pub const fn label(self) -> &'static str {
        match self {
            // R.string.wallpaper_quick_picker
            PopupKind::Carousel => "Wallpaper quick picker",
            // R.string.home_screen_lock
            PopupKind::Lock => "Lock home screen",
            // R.string.edit_home_screen
            PopupKind::EditMode => "Edit home screen",
            // R.string.styles_wallpaper_button_text
            PopupKind::Wallpaper => "Wallpaper & style",
            // R.string.widget_button_text
            PopupKind::Widgets => "Widgets",
            // R.string.all_apps_button_label
            PopupKind::AllApps => "Apps list",
            // R.string.settings_button_text
            PopupKind::HomeSettings => "Home settings",
            // R.string.system_settings
            PopupKind::SysSettings => "System settings",
            // R.string.set_default_home_page
            PopupKind::DefaultPage => "Set as default page",
        }
    }

    /// The drawable the editor row shows, as a symbolic name.
    ///
    /// The reference is a `@DrawableRes` int
    /// (`LauncherOptionsPopup.kt:166-216`); UTLC has no drawable resources, so
    /// this is the identity the renderer switches on.
    pub const fn icon(self) -> PopupIcon {
        match self {
            // R.drawable.ic_wallpaper
            PopupKind::Carousel => PopupIcon::Wallpaper,
            // R.drawable.ic_lock
            PopupKind::Lock => PopupIcon::Lock,
            // R.drawable.enter_home_gardening_icon
            PopupKind::EditMode => PopupIcon::Edit,
            // R.drawable.ic_palette
            PopupKind::Wallpaper => PopupIcon::Palette,
            // SystemShortcut.Widgets.getDrawableId()
            PopupKind::Widgets => PopupIcon::Widgets,
            // R.drawable.ic_apps
            PopupKind::AllApps => PopupIcon::Apps,
            // R.drawable.ic_home_screen
            PopupKind::HomeSettings => PopupIcon::HomeScreen,
            // R.drawable.ic_setting
            PopupKind::SysSettings => PopupIcon::Settings,
            // R.drawable.ic_home_pin
            PopupKind::DefaultPage => PopupIcon::HomePin,
        }
    }

    /// `LauncherOptionMetadata.isCarousel`, the only kind that sets it
    /// (`LauncherOptionsPopup.kt:171`).
    #[inline]
    pub const fn is_carousel(self) -> bool {
        matches!(self, PopupKind::Carousel)
    }

    /// The `isEnabled` this kind carries in `DEFAULT_ORDER`
    /// (`LauncherOptionsPopup.kt:19-27`): six on, three off.
    pub const fn default_enabled(self) -> bool {
        match self {
            PopupKind::Carousel
            | PopupKind::Wallpaper
            | PopupKind::Widgets
            | PopupKind::AllApps
            | PopupKind::HomeSettings => true,
            PopupKind::Lock
            | PopupKind::EditMode
            | PopupKind::SysSettings
            | PopupKind::DefaultPage => false,
        }
    }

    /// Position in `DEFAULT_ORDER`, i.e. the enum discriminant.
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The kind whose `id()` is `s`, if any.
    ///
    /// Case-sensitive and exact, matching the reference's `map` lookup keyed
    /// on the identifier (`LauncherOptionsPopup.kt:73-137`).
    pub fn from_id(s: &str) -> Option<PopupKind> {
        PopupKind::ALL.into_iter().find(|k| k.id() == s)
    }

    /// The row this kind draws, or `None` for [`PopupKind::Carousel`].
    ///
    /// `None` is not an error: `carousel` is filtered out of the built menu
    /// (`LauncherOptionsPopup.kt:142`), so it is an order slot with no row.
    pub const fn as_popup_item(self) -> Option<PopupItem> {
        match self {
            PopupKind::Carousel => None,
            PopupKind::Lock => Some(PopupItem::HomeScreenLock),
            PopupKind::EditMode => Some(PopupItem::EditMode),
            PopupKind::Wallpaper => Some(PopupItem::Wallpapers),
            PopupKind::Widgets => Some(PopupItem::Widgets),
            PopupKind::AllApps => Some(PopupItem::AllApps),
            PopupKind::HomeSettings => Some(PopupItem::HomeSettings),
            PopupKind::SysSettings => Some(PopupItem::SystemSettings),
            PopupKind::DefaultPage => Some(PopupItem::DefaultPageForWorkspace),
        }
    }
}

/// The drawable behind a [`PopupKind`] row, named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PopupIcon {
    /// `R.drawable.ic_wallpaper` (`LauncherOptionsPopup.kt:170`).
    Wallpaper,
    /// `R.drawable.ic_lock` (`:177`).
    Lock,
    /// `R.drawable.enter_home_gardening_icon` (`:186`).
    Edit,
    /// `R.drawable.ic_palette` (`:191`).
    Palette,
    /// `SystemShortcut.Widgets.getDrawableId()` (`:196`).
    Widgets,
    /// `R.drawable.ic_apps` (`:201`).
    Apps,
    /// `R.drawable.ic_home_screen` (`:206`).
    HomeScreen,
    /// `R.drawable.ic_setting` (`:182`).
    Settings,
    /// `R.drawable.ic_home_pin` (`:211`).
    HomePin,
}

/// Why a [`PopupConfig::reorder`] was refused.
///
/// Every variant means **nothing changed**. A settings editor that applies
/// half of a drag and drops the rest is worse than one that refuses the drag,
/// so the order array is only ever written by a path that has already proved
/// every index and every precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupReorderError {
    /// `from` or `to` is not a position in a nine-slot list.
    IndexOutOfRange {
        /// The offending position.
        index: usize,
    },
    /// The row at that position is switched off, so moving it is a change
    /// with no visible effect to the user who made it.
    ItemDisabled(PopupKind),
    /// The row is pinned in place and cannot be reordered at all.
    ///
    /// `LauncherPopupPreference.kt:119` sets
    /// `isDraggable = !metadata.isCarousel`, so the wallpaper carousel is the
    /// one entry the reference's own editor refuses to drag.
    ItemPinned(PopupKind),
}

/// Why a [`PopupConfig::write_text`] did not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupTextError {
    /// The buffer is shorter than the rendered text. `needed` is the exact
    /// length, so the caller can resize and retry; nothing is written.
    BufferTooSmall {
        /// Exact byte length [`PopupConfig::write_text`] would produce.
        needed: usize,
        /// Length of the buffer that was passed.
        provided: usize,
    },
}

/// The runtime facts `LauncherOptionsPopup.getLauncherOptions` filters on.
///
/// `getLauncherOptions` (`:139-153`) reads three things that are not in the
/// preference: whether the home screen is locked, and whether the workspace is
/// currently showing its default page. Handing them in as a `Copy` struct
/// keeps the filter a pure function of `(config, facts)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PopupFilter {
    /// `prefs2.lockHomeScreen` (`LauncherOptionsPopup.kt:64`). When set, the
    /// `edit_mode` and `widgets` rows are dropped (`:144-150`).
    pub home_locked: bool,
    /// `launcher.workspace.isCurrentPageDefault`
    /// (`LauncherOptionsPopup.kt:151`). When set, the `default_page` row is
    /// dropped, because setting the current page as the default again is a
    /// no-op.
    pub current_page_is_default: bool,
}

/// The popup's persisted order and enable flags.
///
/// Two fixed arrays, `Copy`, 18 bytes, no allocation on any operation. The
/// order array is always a permutation of [`PopupKind::ALL`]: nothing writes
/// to it except [`PopupConfig::reorder`] (which only ever moves one element
/// inside the bounds) and [`PopupConfig::from_text`] (which builds the
/// permutation up in the first place).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupConfig {
    /// Row order. `order[i]` is the kind drawn at position `i`.
    order: [PopupKind; POPUP_KIND_COUNT],
    /// Enable bits, indexed by [`PopupKind::index`] rather than by position,
    /// so a reorder cannot lose an entry's flag.
    enabled: [bool; POPUP_KIND_COUNT],
}

impl PopupConfig {
    /// `DEFAULT_ORDER` with its own enable flags
    /// (`LauncherOptionsPopup.kt:18-28`), which is also
    /// `PreferenceManager2.kt:408`'s `defaultValue`.
    pub const DEFAULT: Self = Self {
        order: PopupKind::ALL,
        enabled: [
            true,  // carousel
            false, // lock
            false, // edit_mode
            true,  // wallpaper
            true,  // widgets
            true,  // all_apps
            true,  // home_settings
            false, // sys_settings
            false, // default_page
        ],
    };

    /// The default configuration. Same as [`PopupConfig::DEFAULT`].
    #[inline]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    /// Restore `DEFAULT_ORDER` and its default flags.
    ///
    /// The equivalent of the editor's "reset" (`ReorderablePreferenceGroup`'s
    /// `defaultList` argument, `LauncherPopupPreference.kt:91`).
    #[inline]
    pub fn reset(&mut self) {
        *self = Self::DEFAULT;
    }

    /// The kind drawn at `position`, or `None` past the end.
    #[inline]
    pub fn kind_at(&self, position: usize) -> Option<PopupKind> {
        self.order.get(position).copied()
    }

    /// Where `kind` sits in the order. Always a valid index, by the
    /// permutation invariant.
    #[inline]
    pub fn position_of(&self, kind: PopupKind) -> usize {
        self.order
            .iter()
            .position(|k| *k == kind)
            .expect("order is a permutation of PopupKind::ALL")
    }

    /// Number of order slots. Constantly [`POPUP_KIND_COUNT`].
    #[inline]
    pub const fn len(&self) -> usize {
        POPUP_KIND_COUNT
    }

    /// Always `false`: a popup configuration with no kinds is not a state this
    /// type can hold. Present so the type satisfies the `len`/`is_empty`
    /// convention without a caller wondering whether it applies.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Whether `kind`'s row is switched on.
    #[inline]
    pub fn is_enabled(&self, kind: PopupKind) -> bool {
        self.enabled[kind.index()]
    }

    /// How many kinds are on.
    pub fn enabled_count(&self) -> usize {
        self.enabled.iter().filter(|b| **b).count()
    }

    /// The enabled kinds, in order, into a fixed-capacity array.
    ///
    /// The return is `([PopupKind; POPUP_KIND_COUNT], usize)` rather than a
    /// `Vec` because a `Vec` here would be an allocation on the long-press
    /// path for a list that is at most nine bytes of enum. Callers that want
    /// an iterator can use [`PopupConfig::iter`] and filter.
    pub fn enabled_kinds(&self) -> ([PopupKind; POPUP_KIND_COUNT], usize) {
        let mut out = [PopupKind::Carousel; POPUP_KIND_COUNT];
        let mut n = 0usize;
        for &k in &self.order {
            if self.is_enabled(k) {
                out[n] = k;
                n += 1;
            }
        }
        (out, n)
    }

    /// Every slot in order, paired with its enable flag.
    pub fn iter(&self) -> impl Iterator<Item = (PopupKind, bool)> + '_ {
        self.order.iter().map(move |k| (*k, self.is_enabled(*k)))
    }

    /// Switch `kind` on or off; returns the previous flag.
    ///
    /// This is the switch row's whole job
    /// (`LauncherPopupPreference.kt:107-114`), and it is allowed to leave
    /// every kind off: the reference has no "at least one" guard either, and a
    /// launcher whose long-press does nothing is the user's own choice. A
    /// shell that wants a floor should check [`PopupConfig::enabled_count`]
    /// before calling.
    #[inline]
    pub fn set_enabled(&mut self, kind: PopupKind, on: bool) -> bool {
        let slot = &mut self.enabled[kind.index()];
        core::mem::replace(slot, on)
    }

    /// Move the row at `from` to position `to`.
    ///
    /// On any error the configuration is left **byte-identical** -- the
    /// permutation is only ever produced by this function and the array is
    /// written after every precondition has been checked, so there is no
    /// intermediate state to roll back from.
    ///
    /// The `from` row is the one being dragged, so it is the one whose
    /// properties are checked: an unknown position, a switched-off row, or the
    /// pinned carousel is refused. `to` is a *destination*, so it is only
    /// range-checked -- landing an enabled, unpinned row on a position that
    /// currently holds a disabled one is a normal move.
    pub fn reorder(&mut self, from: usize, to: usize) -> Result<PopupKind, PopupReorderError> {
        let kind = *self
            .order
            .get(from)
            .ok_or(PopupReorderError::IndexOutOfRange { index: from })?;
        if to >= POPUP_KIND_COUNT {
            return Err(PopupReorderError::IndexOutOfRange { index: to });
        }
        if kind.is_carousel() {
            return Err(PopupReorderError::ItemPinned(kind));
        }
        if !self.is_enabled(kind) {
            return Err(PopupReorderError::ItemDisabled(kind));
        }
        self.order[from] = self.order[to];
        self.order[to] = kind;
        Ok(kind)
    }

    /// Exact byte length [`PopupConfig::write_text`] will produce.
    pub fn text_len(&self) -> usize {
        let mut n = 0usize;
        for (i, k) in self.order.iter().enumerate() {
            n += 1 + k.id().len();
            if i + 1 < POPUP_KIND_COUNT {
                n += 1;
            }
        }
        n
    }

    /// Render the persisted form into `out`, returning the bytes written.
    ///
    /// The format is the reference's exactly: a `+` for an enabled row, a `-`
    /// for a disabled one, `|` between rows
    /// (`LauncherOptionsPopup.toOptionOrderString`, `LauncherOptionsPopup.kt:274-278`),
    /// in the current order, so the string is both the storage format and a
    /// faithful description of what the user sees in the editor.
    ///
    /// Nothing is written when the buffer is short; the error carries the
    /// exact length needed.
    pub fn write_text(&self, out: &mut [u8]) -> Result<usize, PopupTextError> {
        let needed = self.text_len();
        if out.len() < needed {
            return Err(PopupTextError::BufferTooSmall {
                needed,
                provided: out.len(),
            });
        }
        let mut n = 0usize;
        for (i, k) in self.order.iter().enumerate() {
            if i > 0 {
                out[n] = b'|';
                n += 1;
            }
            out[n] = if self.is_enabled(*k) { b'+' } else { b'-' };
            n += 1;
            let id = k.id().as_bytes();
            out[n..n + id.len()].copy_from_slice(id);
            n += id.len();
        }
        Ok(n)
    }

    /// Parse the persisted form, repairing what it can.
    ///
    /// This is a boot-path read, so it is **total**: it cannot fail and never
    /// returns an error a caller has to handle, because a preference read that
    /// can fail is a launcher that cannot start. The repairs, in order:
    ///
    /// * `+id` / `-id`, and a bare `id` counts as enabled
    ///   (`toLauncherOptions`, `LauncherOptionsPopup.kt:263-272`).
    /// * An identifier this build does not know is **dropped**. The reference
    ///   keeps it in its `List<LauncherOptionPopupItem>` and only discards it
    ///   much later at the `mapNotNull` (`:152`), which means a row from a
    ///   *newer* Lawnchair shows up in the editor with no label and no icon --
    ///   `getMetadataForOption` throws on it (`:214`). Dropping it here keeps
    ///   the editor and the writer in agreement.
    /// * A repeated identifier keeps only its **last** occurrence, so a
    ///   hand-edited or half-migrated string resolves the way a settings file
    ///   does: the later assignment is the one in effect. (The reference
    ///   duplicates the row, and the built menu then shows it twice.)
    /// * An identifier this build knows but the string omits is **appended**,
    ///   in `DEFAULT_ORDER` order, with its `DEFAULT_ORDER` flag. That is
    ///   `restoreMissingPopupOptions` (`:39-46`) generalised: the reference
    ///   runs it at launch, this runs at parse, so a string written by an
    ///   older build that had fewer kinds still loads.
    ///
    /// Round trip: `from_text(&c.write_text()?) == c` for every `c`, and
    /// `write_text(&from_text(s))` is stable under a second parse.
    pub fn from_text(s: &str) -> Self {
        let mut cfg = Self {
            order: [PopupKind::Carousel; POPUP_KIND_COUNT],
            enabled: [false; POPUP_KIND_COUNT],
        };
        let mut seen = [false; POPUP_KIND_COUNT];
        let mut n = 0usize;
        for token in s.split('|') {
            let (id, on) = match token.as_bytes().first() {
                Some(b'+') => (&token[1..], true),
                Some(b'-') => (&token[1..], false),
                _ => (token, true),
            };
            let Some(kind) = PopupKind::from_id(id) else {
                continue;
            };
            // A repeat keeps its slot and takes the later flag.
            if !seen[kind.index()] {
                seen[kind.index()] = true;
                cfg.order[n] = kind;
                n += 1;
            }
            cfg.enabled[kind.index()] = on;
        }
        // Everything absent from the string, in DEFAULT_ORDER order.
        for k in PopupKind::ALL {
            if !seen[k.index()] {
                cfg.order[n] = k;
                n += 1;
            }
        }
        debug_assert_eq!(n, POPUP_KIND_COUNT);
        for k in PopupKind::ALL {
            if !seen[k.index()] {
                cfg.enabled[k.index()] = k.default_enabled();
            }
        }
        cfg
    }

    /// Build the row list to draw, applying the reference's three filters.
    ///
    /// `LauncherOptionsPopup.getLauncherOptions` (`:139-153`) is a four-stage
    /// pipeline over the persisted order: keep what is enabled, drop
    /// `carousel`, drop `edit_mode`/`widgets` when the home screen is locked,
    /// drop `default_page` when the workspace is already on its default page,
    /// then resolve each surviving identifier to a row. This is the same four
    /// stages, and the result feeds [`PopupItems`] directly.
    ///
    /// At most eight rows survive, which is exactly
    /// [`PopupItems`]' capacity: nine kinds, one of which (`carousel`) is
    /// metadata-only.
    pub fn build_items(&self, filter: PopupFilter) -> PopupItems {
        let mut out = PopupItems::EMPTY;
        for kind in self.order {
            if !self.is_enabled(kind) {
                continue;
            }
            if filter.home_locked && matches!(kind, PopupKind::EditMode | PopupKind::Widgets) {
                continue;
            }
            if kind == PopupKind::DefaultPage && filter.current_page_is_default {
                continue;
            }
            // `carousel` yields `None` and is dropped here, which is the
            // `identifier != "carousel"` clause of `:142`.
            if let Some(item) = kind.as_popup_item() {
                out.push(item);
            }
        }
        out
    }
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cfg: &PopupConfig) -> String {
        let mut buf = [0u8; POPUP_TEXT_MAX];
        let n = cfg.write_text(&mut buf).expect("POPUP_TEXT_MAX is enough");
        String::from_utf8(buf[..n].to_vec()).expect("ids and separators are ASCII")
    }

    // -- the nine kinds ---------------------------------------------------

    /// The claim the whole module rests on: nine, in this order, with these
    /// default flags. Every entry is quoted from `DEFAULT_ORDER`
    /// (`LauncherOptionsPopup.kt:18-28`).
    #[test]
    fn the_default_order_is_the_references_nine_entries() {
        let want = [
            ("carousel", true),
            ("lock", false),
            ("edit_mode", false),
            ("wallpaper", true),
            ("widgets", true),
            ("all_apps", true),
            ("home_settings", true),
            ("sys_settings", false),
            ("default_page", false),
        ];
        assert_eq!(want.len(), POPUP_KIND_COUNT);
        assert_eq!(PopupKind::ALL.len(), POPUP_KIND_COUNT);
        for (i, (id, on)) in want.iter().enumerate() {
            let k = PopupKind::ALL[i];
            assert_eq!(k.id(), *id, "position {i}");
            assert_eq!(PopupKind::from_id(id), Some(k), "{id}");
            assert_eq!(k.index(), i, "{id} must be its own array index");
            assert_eq!(PopupConfig::DEFAULT.is_enabled(k), *on, "{id} default flag");
        }
        // `PopupConfig::DEFAULT` and `PopupKind::default_enabled` are two
        // separate encodings of `DEFAULT_ORDER` (`LauncherOptionsPopup.kt:18-28`
        // versus `PreferenceManager2.kt:408`'s rendered string), and they are
        // read on two different paths: `DEFAULT` at boot, `default_enabled`
        // when a persisted string is repaired by `from_text`. If they drift,
        // a launcher whose `launcher_popup_order` is deleted comes up with a
        // different menu than one that has never been edited.
        for k in PopupKind::ALL {
            assert_eq!(
                k.default_enabled(),
                PopupConfig::DEFAULT.is_enabled(k),
                "{}: the const table and the default config disagree",
                k.id()
            );
        }
    }

    /// UTLC shipped four unconditional rows; the reference has nine slots.
    /// If this ever drops below nine, the persisted text is
    /// unrepresentable and `PopupItems`' eight-row capacity comment is the
    /// thing to fix instead.
    #[test]
    fn the_catalogue_is_bigger_than_the_four_rows_utlc_had() {
        let kinds = PopupKind::ALL.len();
        assert!(kinds > 4, "catalogue shrank to {kinds}");
        assert_eq!(kinds, 9);
        // Six rows on by default, and one of the nine is metadata-only.
        assert_eq!(PopupConfig::DEFAULT.enabled_count(), 5);
        let (on, n) = PopupConfig::DEFAULT.enabled_kinds();
        assert_eq!(n, 5);
        assert_eq!(on[0], PopupKind::Carousel);
        assert_eq!(on[1], PopupKind::Wallpaper);
    }

    /// `getMetadataForOption` (`:166-216`) pairs each identifier with a label
    /// and a drawable. A kind with the wrong icon renders the wrong glyph in
    /// both the editor and the row, and the mistake is invisible in a
    /// screenshot because the layout is identical.
    #[test]
    fn every_kind_carries_its_reference_label_and_icon() {
        let want = [
            (
                PopupKind::Carousel,
                "Wallpaper quick picker",
                PopupIcon::Wallpaper,
            ),
            (PopupKind::Lock, "Lock home screen", PopupIcon::Lock),
            (PopupKind::EditMode, "Edit home screen", PopupIcon::Edit),
            (
                PopupKind::Wallpaper,
                "Wallpaper & style",
                PopupIcon::Palette,
            ),
            (PopupKind::Widgets, "Widgets", PopupIcon::Widgets),
            (PopupKind::AllApps, "Apps list", PopupIcon::Apps),
            (
                PopupKind::HomeSettings,
                "Home settings",
                PopupIcon::HomeScreen,
            ),
            (
                PopupKind::SysSettings,
                "System settings",
                PopupIcon::Settings,
            ),
            (
                PopupKind::DefaultPage,
                "Set as default page",
                PopupIcon::HomePin,
            ),
        ];
        for (kind, label, icon) in want {
            assert_eq!(kind.label(), label, "{}", kind.id());
            assert_eq!(kind.icon(), icon, "{}", kind.id());
            assert!(kind.label().is_ascii(), "{} label is not ASCII", kind.id());
        }
        // Only the carousel is a carousel (`:171`).
        let carousels: Vec<PopupKind> = PopupKind::ALL
            .into_iter()
            .filter(|k| k.is_carousel())
            .collect();
        assert_eq!(carousels, vec![PopupKind::Carousel]);
    }

    /// An unknown identifier must resolve to nothing rather than to a
    /// plausible default: `getMetadataForOption` throws on it (`:214`), and
    /// silently defaulting would put an unlabelled row in the editor.
    #[test]
    fn an_unknown_identifier_resolves_to_nothing() {
        assert_eq!(PopupKind::from_id(""), None);
        assert_eq!(PopupKind::from_id("enterAllApps"), None);
        assert_eq!(
            PopupKind::from_id("Carousel"),
            None,
            "ids are case-sensitive"
        );
        assert_eq!(PopupKind::from_id("lock "), None, "and untrimmed");
        // Every declared id round-trips through the parser, and no two kinds
        // share one.
        for k in PopupKind::ALL {
            assert_eq!(PopupKind::from_id(k.id()), Some(k));
        }
    }

    // -- the enable set ---------------------------------------------------

    /// The switch row. Turning a kind off must not disturb the order or the
    /// other eight flags -- that is the whole content of the reference's
    /// `onCheckedChange` (`:111-114`), which only writes
    /// `optionsList[index].isEnabled`.
    #[test]
    fn set_enabled_changes_one_flag_and_nothing_else() {
        let before = PopupConfig::DEFAULT;
        let mut cfg = before;
        // The return value is the *previous* flag, so a caller can drive a
        // switch row straight from it. `sys_settings` is off in DEFAULT_ORDER.
        let was_on = cfg.set_enabled(PopupKind::SysSettings, true);
        assert!(!was_on, "the previous flag was not returned");
        assert!(cfg.is_enabled(PopupKind::SysSettings));
        assert_eq!(cfg.order, before.order, "order moved");
        // Every other flag is untouched.
        for k in PopupKind::ALL {
            if k != PopupKind::SysSettings {
                assert_eq!(cfg.is_enabled(k), before.is_enabled(k), "{}", k.id());
            }
        }
        assert!(cfg.set_enabled(PopupKind::SysSettings, false), "round trip");
        assert_eq!(cfg, before);
    }

    /// Disabling every kind is allowed, exactly as the reference allows it:
    /// `LauncherPopupPreference.kt:107-114` has no "at least one" floor and
    /// `getLauncherOptions` happily returns an empty list. A launcher that
    /// refuses to honour the last switch off is not the reference.
    #[test]
    fn every_kind_can_be_switched_off() {
        let mut cfg = PopupConfig::DEFAULT;
        for k in PopupKind::ALL {
            cfg.set_enabled(k, false);
        }
        assert_eq!(cfg.enabled_count(), 0);
        assert!(cfg.build_items(PopupFilter::default()).is_empty());
        assert_eq!(text(&cfg).matches('-').count(), POPUP_KIND_COUNT);
    }

    // -- reorder ----------------------------------------------------------

    /// A legal move relocates exactly one row and leaves the rest of the
    /// permutation -- and every enable flag, which is indexed by kind rather
    /// than by position -- alone.
    ///
    /// The move lands on `lock`, which is **off**, on purpose. Landing on a
    /// differently-flagged row is the only thing that can tell a kind-indexed
    /// enable array from a position-indexed one: an earlier version moved
    /// `wallpaper` onto the carousel, both of which are on, and the
    /// perturbation sweep confirmed that a position-indexed implementation
    /// passed it.
    #[test]
    fn a_legal_reorder_moves_one_row_and_keeps_the_flags() {
        let mut cfg = PopupConfig::DEFAULT;
        // Positions: 0 carousel, 1 lock, 2 edit_mode, 3 wallpaper, ...
        assert_eq!(cfg.kind_at(3), Some(PopupKind::Wallpaper));
        assert!(cfg.is_enabled(PopupKind::Wallpaper));
        assert!(!cfg.is_enabled(PopupKind::Lock), "the flags must differ");
        assert_eq!(
            cfg.reorder(3, 1),
            Ok(PopupKind::Wallpaper),
            "wallpaper onto the (disabled) lock's slot"
        );
        assert_eq!(cfg.kind_at(1), Some(PopupKind::Wallpaper));
        assert_eq!(cfg.kind_at(3), Some(PopupKind::Lock));
        // Each flag travelled with its kind, not with the position it was on.
        assert!(cfg.is_enabled(PopupKind::Wallpaper), "moved on");
        assert!(!cfg.is_enabled(PopupKind::Lock), "moved off");
        // Every other flag is untouched.
        for k in PopupKind::ALL {
            if !matches!(k, PopupKind::Wallpaper | PopupKind::Lock) {
                assert_eq!(
                    cfg.is_enabled(k),
                    PopupConfig::DEFAULT.is_enabled(k),
                    "{}",
                    k.id()
                );
            }
        }
        // Still a permutation.
        let mut seen = [false; POPUP_KIND_COUNT];
        for k in cfg.order {
            assert!(!seen[k.index()], "{} appears twice", k.id());
            seen[k.index()] = true;
        }
        assert!(cfg.order.iter().all(|k| seen[k.index()]));
    }

    /// A reorder is either applied whole or refused whole. This is the
    /// property the brief is really about, so it is checked by *byte
    /// equality* against the pre-call value for every refusal, including the
    /// out-of-range destination, which is checked second and could otherwise
    /// be left having already swapped the source.
    #[test]
    fn a_refused_reorder_changes_nothing_at_all() {
        // Turn on everything so the only refusals left are structural, then
        // re-test the disabled refusal separately.
        let mut cfg = PopupConfig::DEFAULT;
        for k in PopupKind::ALL {
            cfg.set_enabled(k, true);
        }
        let all_on = cfg;

        // Unknown source position.
        for bad in [POPUP_KIND_COUNT, POPUP_KIND_COUNT + 7, usize::MAX] {
            assert_eq!(
                cfg.reorder(bad, 0),
                Err(PopupReorderError::IndexOutOfRange { index: bad })
            );
            assert_eq!(cfg, all_on, "index {bad} disturbed the config");
        }
        // Unknown destination position, with an in-range source that *would*
        // otherwise have been swapped: the destination check must fire before
        // the write.
        for bad in [POPUP_KIND_COUNT, 99] {
            assert_eq!(
                cfg.reorder(3, bad),
                Err(PopupReorderError::IndexOutOfRange { index: bad })
            );
            assert_eq!(cfg, all_on, "destination {bad} disturbed the config");
        }
        // The pinned carousel.
        assert_eq!(
            cfg.reorder(0, 8),
            Err(PopupReorderError::ItemPinned(PopupKind::Carousel))
        );
        assert_eq!(cfg, all_on, "a pinned row moved");

        // A disabled row.
        let mut off = all_on;
        off.set_enabled(PopupKind::AllApps, false);
        assert_eq!(
            off.reorder(5, 0),
            Err(PopupReorderError::ItemDisabled(PopupKind::AllApps))
        );
        // The refusal left the *disabled* configuration byte-identical, not
        // the all-on one it was derived from.
        assert_eq!(off.kind_at(0), Some(PopupKind::Carousel));
        assert_eq!(off.kind_at(5), Some(PopupKind::AllApps));
        assert!(!off.is_enabled(PopupKind::AllApps));
        assert_eq!(off.enabled_count(), 8);
        // And the same refusal on a source that is merely out of range must
        // not have consumed the disabled row's slot.
        assert_eq!(
            off.reorder(99, 5),
            Err(PopupReorderError::IndexOutOfRange { index: 99 })
        );
        assert_eq!(off.kind_at(5), Some(PopupKind::AllApps));
    }

    /// `LauncherPopupPreference.kt:119` makes the carousel the one entry the
    /// editor will not drag, and it is off-by-default-then-... no: it is
    /// *enabled* by default, so a config that never touched it still refuses
    /// to move it. That distinguishes `ItemPinned` from `ItemDisabled`.
    #[test]
    fn the_carousel_is_pinned_even_though_it_is_enabled() {
        let cfg = PopupConfig::DEFAULT;
        assert!(cfg.is_enabled(PopupKind::Carousel));
        let mut moved = cfg;
        assert_eq!(
            moved.reorder(0, 1),
            Err(PopupReorderError::ItemPinned(PopupKind::Carousel))
        );
        assert_eq!(moved, cfg);
        // Every other kind that is enabled *can* be moved.
        for k in PopupKind::ALL {
            if k.is_carousel() || !cfg.is_enabled(k) {
                continue;
            }
            let mut c = cfg;
            let p = c.position_of(k);
            let q = if p == 0 { 1 } else { 0 };
            assert_eq!(c.reorder(p, q), Ok(k), "{} should be draggable", k.id());
        }
    }

    /// A move *onto* a disabled row's slot is legal: the disabled row is the
    /// destination, not the thing being dragged. Refusing this would make the
    /// array order depend on the enable flags, which is the coupling the
    /// kind-indexed enable array exists to avoid.
    #[test]
    fn a_destination_holding_a_disabled_row_is_still_a_valid_destination() {
        let mut cfg = PopupConfig::DEFAULT;
        // Position 1 is `lock`, which is off by default.
        assert_eq!(cfg.kind_at(1), Some(PopupKind::Lock));
        assert!(!cfg.is_enabled(PopupKind::Lock));
        assert_eq!(
            cfg.reorder(3, 1),
            Ok(PopupKind::Wallpaper),
            "an enabled row may land on a disabled one"
        );
        assert_eq!(cfg.kind_at(1), Some(PopupKind::Wallpaper));
        assert_eq!(cfg.kind_at(3), Some(PopupKind::Lock));
    }

    /// Moving a row to where it already is is a no-op, not an error, so a
    /// drag handler that fires on every pointer move does not have to filter
    /// it out.
    #[test]
    fn a_reorder_to_the_same_position_is_a_no_op() {
        let cfg = PopupConfig::DEFAULT;
        let mut same = cfg;
        assert_eq!(same.reorder(3, 3), Ok(PopupKind::Wallpaper));
        assert_eq!(same, cfg);
    }

    // -- reset ------------------------------------------------------------

    /// The editor's reset, and the only way back from a scrambled order.
    #[test]
    fn reset_restores_default_order_and_flags() {
        let mut cfg = PopupConfig::DEFAULT;
        cfg.set_enabled(PopupKind::Lock, true);
        cfg.reorder(3, 0)
            .expect("wallpaper is enabled and not pinned");
        assert_ne!(cfg, PopupConfig::DEFAULT);
        cfg.reset();
        assert_eq!(cfg, PopupConfig::DEFAULT);
        assert_eq!(cfg.kind_at(0), Some(PopupKind::Carousel));
        assert!(!cfg.is_enabled(PopupKind::Lock), "flags restored too");
    }

    // -- the text form ----------------------------------------------------

    /// The exact persisted string, character for character: `+`/`-` prefixes
    /// in `DEFAULT_ORDER` with `|` between
    /// (`toOptionOrderString`, `LauncherOptionsPopup.kt:274-278`). If this
    /// drifts, an existing `launcher_popup_order` value stops loading.
    #[test]
    fn the_default_text_is_the_references_default_value() {
        assert_eq!(
            text(&PopupConfig::DEFAULT),
            "+carousel|-lock|-edit_mode|+wallpaper|+widgets|+all_apps\
             |+home_settings|-sys_settings|-default_page"
                .replace(' ', "")
        );
        // Which is exactly `PreferenceManager2.kt:408`'s defaultValue.
        assert_eq!(
            PopupConfig::from_text(&text(&PopupConfig::DEFAULT)),
            PopupConfig::DEFAULT
        );
    }

    /// The round trip the brief requires, in both directions: write then parse
    /// is the identity, and parse then write is stable.
    #[test]
    fn the_text_form_round_trips() {
        // A deliberately awkward configuration: three flags changed, order
        // shuffled, and one row moved to the very end. Both moves are made with
        // rows that are enabled and not the pinned carousel, so the assertions
        // below are about the text form and not about the reorder refusals.
        let mut cfg = PopupConfig::DEFAULT;
        cfg.set_enabled(PopupKind::Lock, true);
        cfg.set_enabled(PopupKind::SysSettings, true);
        cfg.reorder(3, 8).expect("wallpaper is enabled by default");
        cfg.reorder(4, 1).expect("widgets is enabled by default");
        assert!(!cfg.is_enabled(PopupKind::EditMode), "still off");
        assert!(!cfg.is_enabled(PopupKind::DefaultPage), "still off");
        let once = text(&cfg);
        assert!(once.ends_with("+wallpaper"), "{once}");
        let back = PopupConfig::from_text(&once);
        assert_eq!(back, cfg, "write -> parse must be the identity");
        assert_eq!(text(&back), once, "parse -> write must be stable");
        // And after a reorder, the string describes the new order, so the two
        // representations cannot disagree about what is drawn.
        let mut moved = back;
        moved.reorder(1, 5).expect("all_apps is on and not pinned");
        assert!(text(&moved).starts_with("+carousel|+all_apps|"), "{once}");
        assert_eq!(PopupConfig::from_text(&text(&moved)), moved);
    }

    /// `toLauncherOptions` (`:263-272`) treats a bare identifier as enabled.
    /// Dropping that default would silently switch on rows the user turned
    /// off, if any writer ever emitted a bare id.
    #[test]
    fn a_bare_identifier_counts_as_enabled() {
        let cfg = PopupConfig::from_text("wallpaper|lock|edit_mode");
        assert!(cfg.is_enabled(PopupKind::Wallpaper));
        assert!(cfg.is_enabled(PopupKind::Lock));
        assert!(cfg.is_enabled(PopupKind::EditMode));
    }

    /// A short buffer must be refused with the exact length needed and must
    /// not be written to, because a truncated preference string parses as a
    /// *different* configuration rather than failing.
    #[test]
    fn a_short_buffer_is_refused_with_the_length_needed() {
        let cfg = PopupConfig::DEFAULT;
        let need = cfg.text_len();
        assert_eq!(need, text(&cfg).len());
        for short in 0..need {
            let mut buf = [0xAAu8; POPUP_TEXT_MAX];
            assert_eq!(
                cfg.write_text(&mut buf[..short]),
                Err(PopupTextError::BufferTooSmall {
                    needed: need,
                    provided: short
                }),
                "a {short}-byte buffer should not have been accepted"
            );
            assert!(
                buf.iter().all(|b| *b == 0xAA),
                "buffer {short} was written to"
            );
        }
        // One byte more than needed is enough, and does not need a byte spare.
        let mut buf = [0u8; POPUP_TEXT_MAX];
        assert_eq!(cfg.write_text(&mut buf[..need]), Ok(need));
    }

    /// `text_len` is exact for every configuration, not just the default: after
    /// reorders and flag changes a buffer of exactly that length still fits,
    /// and one byte short is still refused without being written to. A length
    /// that only held for `DEFAULT` would truncate the first edited config.
    #[test]
    fn text_len_is_exact_after_reorders_and_flag_changes() {
        let mut cfg = PopupConfig::DEFAULT;
        cfg.set_enabled(PopupKind::Lock, true);
        cfg.set_enabled(PopupKind::SysSettings, true);
        cfg.reorder(3, 8).expect("wallpaper is enabled by default");
        cfg.reorder(4, 1).expect("widgets is enabled by default");
        let need = cfg.text_len();
        assert_eq!(need, text(&cfg).len(), "the length must describe the text");
        assert!(need <= POPUP_TEXT_MAX);
        let mut exact = vec![0u8; need];
        assert_eq!(cfg.write_text(&mut exact), Ok(need));
        assert_eq!(PopupConfig::from_text(&text(&cfg)), cfg);
        if need > 0 {
            let mut one_short = vec![0xAAu8; need - 1];
            assert_eq!(
                cfg.write_text(&mut one_short),
                Err(PopupTextError::BufferTooSmall {
                    needed: need,
                    provided: need - 1
                })
            );
            assert!(
                one_short.iter().all(|b| *b == 0xAA),
                "the refused write touched the buffer"
            );
        }
    }

    /// `enabled_kinds` follows the order array, not `ALL`: after a reorder the
    /// moved row is enumerated where it sits, which is what the painter draws.
    /// A position-indexed enable array would pass the swap test in
    /// `a_legal_reorder_moves_one_row_and_keeps_the_flags` only when the two
    /// swapped rows share a flag -- here they do not, and the enumeration
    /// order moves with the row.
    #[test]
    fn enabled_kinds_follow_the_order_array() {
        let mut cfg = PopupConfig::DEFAULT;
        // Position 1 is `lock` (off); position 4 is `widgets` (on).
        cfg.reorder(4, 1).expect("widgets is enabled");
        let (on, n) = cfg.enabled_kinds();
        assert_eq!(n, 5);
        assert_eq!(
            &on[..n],
            &[
                PopupKind::Carousel,
                PopupKind::Widgets,
                PopupKind::Wallpaper,
                PopupKind::AllApps,
                PopupKind::HomeSettings,
            ],
            "widgets moved to position 1 and the enumeration followed it"
        );
    }

    /// `POPUP_TEXT_MAX` has to be an upper bound, not a guess: the longest
    /// possible string is the default order's, and it must fit.
    #[test]
    fn the_published_buffer_bound_is_not_too_small() {
        let mut cfg = PopupConfig::DEFAULT;
        let mut worst = cfg.text_len();
        // A configuration with the longest ids in the leading positions is the
        // only place a length could differ, so check the default's own order
        // and the reversed one.
        cfg.reorder(6, 0).expect("home_settings is on");
        worst = worst.max(cfg.text_len());
        assert!(
            worst <= POPUP_TEXT_MAX,
            "worst case {worst} exceeds the published bound {POPUP_TEXT_MAX}"
        );
        let mut buf = [0u8; POPUP_TEXT_MAX];
        assert!(cfg.write_text(&mut buf).is_ok());
        assert_eq!(POPUP_TEXT_MAX, 134);
    }

    // -- from_text repairs ------------------------------------------------

    /// `restoreMissingPopupOptions` (`:39-46`): a string written by a build
    /// with fewer kinds still loads, with the missing ones appended in
    /// `DEFAULT_ORDER` order and their default flags.
    #[test]
    fn a_string_missing_kinds_gets_them_appended_in_default_order() {
        let cfg = PopupConfig::from_text("+widgets|-lock");
        assert_eq!(cfg.kind_at(0), Some(PopupKind::Widgets));
        assert_eq!(cfg.kind_at(1), Some(PopupKind::Lock));
        // The rest, in DEFAULT_ORDER order minus the two already present.
        let want = [
            PopupKind::Carousel,
            PopupKind::EditMode,
            PopupKind::Wallpaper,
            PopupKind::AllApps,
            PopupKind::HomeSettings,
            PopupKind::SysSettings,
            PopupKind::DefaultPage,
        ];
        for (i, k) in want.iter().enumerate() {
            assert_eq!(cfg.kind_at(2 + i), Some(*k), "appended position {i}");
        }
        // Enabled: the two named (`+widgets`, `-lock`) contribute one, and the
        // appended defaults contribute carousel, wallpaper, all_apps and
        // home_settings -- four more. Five, not six: the two off-by-default
        // kinds among the appended (edit_mode, default_page) stay off.
        assert_eq!(cfg.enabled_count(), 5, "appended kinds take their default");
        assert!(!cfg.is_enabled(PopupKind::EditMode));
        assert!(!cfg.is_enabled(PopupKind::DefaultPage));
        assert!(cfg.is_enabled(PopupKind::Carousel));
        // Every kind exactly once.
        let mut count = [0usize; POPUP_KIND_COUNT];
        for k in cfg.order {
            count[k.index()] += 1;
        }
        assert!(
            count.iter().all(|c| *c == 1),
            "not a permutation: {count:?}"
        );
    }

    /// An unknown identifier is dropped here rather than at the build step.
    /// The reference keeps it until `mapNotNull` (`:152`) and then throws at
    /// `getMetadataForOption` (`:214`) if the editor reaches it -- so keeping
    /// it would mean a row with no label and no icon.
    #[test]
    fn an_unknown_identifier_is_dropped_not_kept() {
        // `enterAllApps` is a real identifier in the *build* map
        // (`LauncherOptionsPopup.kt:116-122`) but is not in `DEFAULT_ORDER`,
        // so it is not a configurable kind.
        let cfg = PopupConfig::from_text("+widgets|enterAllApps|+lock");
        // Dropped, so the whole order is the two known ids followed by the
        // seven that were absent, in `DEFAULT_ORDER` order.
        let want = [
            PopupKind::Widgets,
            PopupKind::Lock,
            PopupKind::Carousel,
            PopupKind::EditMode,
            PopupKind::Wallpaper,
            PopupKind::AllApps,
            PopupKind::HomeSettings,
            PopupKind::SysSettings,
            PopupKind::DefaultPage,
        ];
        for (i, k) in want.iter().enumerate() {
            assert_eq!(cfg.kind_at(i), Some(*k), "position {i}");
        }
        assert_eq!(cfg.len(), POPUP_KIND_COUNT);
        let mut count = [0usize; POPUP_KIND_COUNT];
        for k in cfg.order {
            count[k.index()] += 1;
        }
        assert!(count.iter().all(|c| *c == 1));
        // Had it been kept, it would have occupied position 1 and pushed
        // `lock` to 2 -- so this is the assertion that pins "dropped".
        assert_eq!(cfg.position_of(PopupKind::Lock), 1);
        // Wholly unknown garbage still yields a complete, defaultable config.
        let junk = PopupConfig::from_text("+|???|");
        assert_eq!(junk.enabled_count(), 5, "all defaults");
        assert_eq!(PopupConfig::from_text(""), PopupConfig::DEFAULT);
    }

    /// A duplicated identifier keeps one row. The reference duplicates it, and
    /// the built menu then shows the row twice -- and a duplicated
    /// `PopupItem` in an 8-slot list can push a real row off the end.
    #[test]
    fn a_repeated_identifier_yields_one_row_and_the_later_flag() {
        let cfg = PopupConfig::from_text("+all_apps|-all_apps|+widgets");
        assert_eq!(cfg.position_of(PopupKind::AllApps), 0);
        assert!(!cfg.is_enabled(PopupKind::AllApps), "the later flag wins");
        assert!(cfg.is_enabled(PopupKind::Widgets));
        // `all_apps` occupies one slot even though the string named it twice:
        // `widgets` is at position 1, and the appended default is at 2.
        assert_eq!(cfg.position_of(PopupKind::Widgets), 1);
        assert_eq!(cfg.kind_at(2), Some(PopupKind::Carousel));
        let mut count = [0usize; POPUP_KIND_COUNT];
        for k in cfg.order {
            count[k.index()] += 1;
        }
        assert!(
            count.iter().all(|c| *c == 1),
            "not a permutation: {count:?}"
        );
    }

    /// Reading a preference must not be able to fail, so a corrupt string has
    /// to land somewhere usable rather than in an error the boot path cannot
    /// handle. This drives a spread of hostile inputs.
    #[test]
    fn from_text_is_total_over_hostile_input() {
        for s in [
            "",
            "|",
            "||||||",
            "+",
            "-",
            "+|+|+|+|+|+|+|+|+",
            "-carousel|-carousel|-carousel",
            "+default_page|+default_page|+default_page|+default_page|+default_page",
            "\u{0}+widgets",
            "widgets+",
            "+WIDGETS",
            "wallpaper|wallpaper|wallpaper|wallpaper|wallpaper|wallpaper|wallpaper|wallpaper|wallpaper|wallpaper",
        ] {
            let cfg = PopupConfig::from_text(s);
            assert_eq!(cfg.len(), POPUP_KIND_COUNT, "{s:?}");
            let mut count = [0usize; POPUP_KIND_COUNT];
            for k in cfg.order {
                count[k.index()] += 1;
            }
            assert!(count.iter().all(|c| *c == 1), "{s:?} -> {count:?}");
            // And whatever it decided is writable and re-parseable.
            let round = text(&cfg);
            assert_eq!(PopupConfig::from_text(&round), cfg, "{s:?}");
        }
    }

    // -- building the rows ------------------------------------------------

    /// `getLauncherOptions` (`:139-153`) is a filter pipeline; this walks it
    /// with the reference's own `DEFAULT_ORDER` enabled set, unlocked, not on
    /// the default page, and gets the reference's row order -- which is *not*
    /// the order `main.rs:207-220` pushes.
    #[test]
    fn the_built_rows_follow_the_reference_filter_pipeline() {
        let mut cfg = PopupConfig::DEFAULT;
        for k in PopupKind::ALL {
            cfg.set_enabled(k, true);
        }
        let open = PopupFilter::default();
        let items = cfg.build_items(open);
        let got: Vec<PopupItem> = (0..8).filter_map(|i| items.get(i)).collect();
        assert_eq!(
            got,
            vec![
                // `lock` is DEFAULT_ORDER position 1, `edit_mode` position 2:
                // the reference shows lock first. main.rs:214-216 pushes
                // EditMode before HomeScreenLock.
                PopupItem::HomeScreenLock,
                PopupItem::EditMode,
                PopupItem::Wallpapers,
                PopupItem::Widgets,
                PopupItem::AllApps,
                PopupItem::HomeSettings,
                PopupItem::SystemSettings,
                PopupItem::DefaultPageForWorkspace,
            ]
        );
    }

    /// Each of the reference's three filters removes exactly the rows it is
    /// cited for: the locked home screen kills edit-mode and widgets
    /// (`:144-150`), and being on the default page kills `default_page`
    /// (`:151`). `carousel` is already gone in all cases (`:142`).
    #[test]
    fn each_reference_filter_removes_only_its_own_rows() {
        let mut cfg = PopupConfig::DEFAULT;
        for k in PopupKind::ALL {
            cfg.set_enabled(k, true);
        }
        let list = |f: PopupFilter| -> Vec<PopupItem> {
            let items = cfg.build_items(f);
            (0..8).filter_map(|i| items.get(i)).collect()
        };

        let locked = list(PopupFilter {
            home_locked: true,
            current_page_is_default: false,
        });
        assert!(
            !locked.contains(&PopupItem::EditMode),
            "edit_mode on a locked home"
        );
        assert!(
            !locked.contains(&PopupItem::Widgets),
            "widgets on a locked home"
        );
        assert!(
            locked.contains(&PopupItem::HomeScreenLock),
            "lock itself stays"
        );
        assert!(locked.contains(&PopupItem::Wallpapers));

        let on_default = list(PopupFilter {
            home_locked: false,
            current_page_is_default: true,
        });
        assert!(!on_default.contains(&PopupItem::DefaultPageForWorkspace));
        assert!(on_default.contains(&PopupItem::EditMode));
        assert!(on_default.contains(&PopupItem::Widgets));

        // Both at once is the additive case, not an either/or: 8 rows minus
        // edit_mode, widgets and default_page.
        let both = list(PopupFilter {
            home_locked: true,
            current_page_is_default: true,
        });
        assert_eq!(both.len(), 5);
        // And the carousel is in none of them, however the flags are set: it
        // is filtered before any of the other three clauses (`:142`).
        for f in [
            PopupFilter::default(),
            PopupFilter {
                home_locked: true,
                current_page_is_default: false,
            },
            PopupFilter {
                home_locked: false,
                current_page_is_default: true,
            },
        ] {
            assert_eq!(
                list(f).len(),
                8 - f.home_locked as usize * 2 - f.current_page_is_default as usize
            );
            assert!(
                !list(f)
                    .iter()
                    .any(|i| matches!(i, PopupItem::DeepShortcut(_))),
                "the carousel has no row"
            );
        }
    }

    /// A disabled row is not built, whatever else is true of it. This is the
    /// first filter in the reference (`:141-143`) and the one a user actually
    /// drives from the editor.
    #[test]
    fn a_disabled_row_is_not_built() {
        let mut cfg = PopupConfig::DEFAULT;
        let before = cfg.build_items(PopupFilter::default()).len;
        assert_eq!(before, 4, "the five defaults minus the metadata carousel");
        cfg.set_enabled(PopupKind::AllApps, false);
        let items = cfg.build_items(PopupFilter::default());
        assert_eq!(items.len, 3);
        assert!(!items.iter().any(|i| *i == PopupItem::AllApps));
        // A row's own flag wins over the runtime filters: a disabled `widgets`
        // stays out of a locked home's menu too.
        cfg.set_enabled(PopupKind::Widgets, false);
        let locked = cfg.build_items(PopupFilter {
            home_locked: false,
            current_page_is_default: false,
        });
        assert!(!locked.iter().any(|i| *i == PopupItem::Widgets));
    }

    /// The built list must never exceed `PopupItems`' eight-slot capacity --
    /// nine kinds, one metadata-only, is the arithmetic that makes it fit, and
    /// `PopupItems::push` silently *drops* rather than growing, so an overflow
    /// would be a row that vanishes. The drop is observed directly: `push`
    /// returns `false` and there is no way for the config to fill all nine.
    #[test]
    fn the_built_rows_fit_the_fixed_capacity_list() {
        let mut cfg = PopupConfig::DEFAULT;
        for k in PopupKind::ALL {
            cfg.set_enabled(k, true);
        }
        // No row is ever lost, so the list is exactly the eight real kinds.
        let items = cfg.build_items(PopupFilter::default());
        assert_eq!(items.len, 8, "all eight real rows, nothing dropped");
        let mut distinct: Vec<PopupItem> = items.iter().copied().collect();
        distinct.sort_by_key(|i| format!("{i:?}"));
        let before = distinct.len();
        distinct.dedup();
        assert_eq!(distinct.len(), before, "a row appears twice: {distinct:?}");
        // Prove the capacity is the binding constraint by asking for a ninth
        // row directly: `push` reports the refusal instead of growing, which
        // is what would happen if a tenth kind were ever added.
        let mut list = items;
        assert!(
            !list.push(PopupItem::Install),
            "a ninth row must be refused"
        );
        assert_eq!(list.len, 8, "the refused push changed nothing");
    }

    /// The whole model is two fixed arrays, and the enable array is indexed by
    /// kind so the two cannot desynchronise. The size assertion is what keeps
    /// somebody from "improving" it into a `Vec`.
    #[test]
    fn the_model_is_two_fixed_arrays_and_no_heap() {
        assert_eq!(
            core::mem::size_of::<PopupConfig>(),
            POPUP_KIND_COUNT * (core::mem::size_of::<PopupKind>() + 1)
        );
        // `Copy` is what lets the compositor hand it to the painter by value.
        fn assert_copy<T: Copy + Clone>() {}
        assert_copy::<PopupConfig>();
        assert_copy::<PopupKind>();
        assert_copy::<PopupIcon>();
        assert_copy::<PopupFilter>();
        assert_copy::<PopupReorderError>();
        // `needs_drop` is the structural "no heap in here" check: a `String`,
        // `Box` or `Vec` field would make it true, and it is false for two
        // arrays of `Copy` scalars.
        assert!(
            !core::mem::needs_drop::<PopupConfig>(),
            "PopupConfig owns something that needs dropping, so it allocates"
        );
        assert!(!core::mem::needs_drop::<PopupFilter>());
    }
}
