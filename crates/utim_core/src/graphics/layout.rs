//! Resolution-independent launcher layout.
//!
//! # Why this module exists
//!
//! The shell used to carry a wall of absolute pixel constants
//! (`SEARCH_Y = 235.0`, `GRID_ROW_H = 115.0`, `ICON_SIZE = 64.0`, ...) that
//! were hand-tuned for one 1080x2400 panel. Render code and hit-test code
//! then each spelled those numbers out again, so a touch landed on a
//! neighbouring cell, the action chips overlapped row 0, and nothing scaled
//! to a different display.
//!
//! Every piece of launcher geometry now derives from a single [`Layout`]
//! value built from the panel size. The renderer and the hit-tester both read
//! the same struct, so a rectangle drawn is by construction the rectangle that
//! is tested. The proportions follow Launcher3/Lawnchair's `CellLayout`:
//!
//! * grid columns and icon size are a fraction of the panel width,
//! * cell height is icon + label + padding (never a magic number),
//! * the hotseat is pinned to the bottom with a fixed inner margin,
//! * the workspace is what is left over between the search pill and the dock.
//!
//! All arithmetic is `f32`; hit-tests are inclusive of the exact same
//! floating-point edges the renderer uses, so there is no rounding drift.

/// Horizontal page gutter, as a fraction of panel width.
pub const PAGE_MARGIN: f32 = 0.030;
/// Grid icon edge length, as a fraction of the grid column pitch.
pub const ICON_FRACTION: f32 = 0.56;
/// Icon corner radius, as a fraction of the icon size (Material 3 style).
pub const ICON_RADIUS: f32 = 0.30;
/// Vertical gap between an icon's bottom and its label's ascender line.
pub const LABEL_GAP: f32 = 0.22;
/// Extra space below a label before the next cell starts.
pub const CELL_BOTTOM_GAP: f32 = 0.20;
/// Height of the system status bar, as a fraction of panel height.
pub const STATUS_BAR_FRACTION: f32 = 0.018;
/// Minimum touch target edge, as a fraction of panel width (Material 3 says
/// 48dp; on a 1080px panel that is 4.4% of the width).
pub const TOUCH_TARGET_FRACTION: f32 = 0.048;
/// Fraction of the panel width used as horizontal padding for panels.
pub const PANEL_PAD_FRACTION: f32 = 0.030;
/// Corner radius of a search pill, as a fraction of its own height.
pub const PILL_RADIUS: f32 = 0.5;

/// Which vertical band a touch belongs to on the home screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeZone {
    StatusBar,
    Clock,
    Search,
    ActionChips,
    Grid,
    PageIndicator,
    Dock,
    NavPill,
    Empty,
}

/// Which vertical band a touch belongs to inside the app drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerZone {
    StatusBar,
    Handle,
    Search,
    Header,
    Grid,
    NavPill,
    Empty,
}

/// Geometry for a single icon cell, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Cell {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
    #[inline]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
    /// Expand to a minimum touch target edge, keeping the center fixed.
    #[inline]
    pub fn with_min_touch(&self, min_edge: f32) -> Cell {
        let w = self.w.max(min_edge);
        let h = self.h.max(min_edge);
        let cx = self.x + self.w * 0.5;
        let cy = self.y + self.h * 0.5;
        Cell {
            x: cx - w * 0.5,
            y: cy - h * 0.5,
            w,
            h,
        }
    }
}

/// Horizontal button geometry (Material 3 filled/tonal buttons).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Button {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Button {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
    #[inline]
    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }
    #[inline]
    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }
    #[inline]
    pub fn center(&self) -> (f32, f32) {
        (self.center_x(), self.center_y())
    }
}

/// Rounded-rectangle geometry shared by pills, cards and icon tiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
}

impl Rect {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
    #[inline]
    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }
    #[inline]
    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }
    #[inline]
    pub fn center(&self) -> (f32, f32) {
        (self.center_x(), self.center_y())
    }
}

/// Complete launcher geometry for one panel size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub w: f32,
    pub h: f32,

    // System UI
    pub status_bar_h: f32,

    // Clock widget
    pub clock_h: f32,
    pub clock_y: f32,

    // Search pill
    pub search: Rect,
    pub search_glyph_w: f32,

    // Edit-mode action chips
    pub chips_y: f32,
    pub chip_h: f32,
    pub chip_gap: f32,
    pub remove_chip: Button,
    pub move_chip: Button,

    // Workspace grid
    pub grid_cols: usize,
    pub grid_top: f32,
    pub grid_bottom: f32,
    pub col_pitch: f32,
    pub row_pitch: f32,
    pub icon_size: f32,
    pub icon_radius: f32,
    pub label_scale: usize,
    pub max_rows: usize,

    // Page indicator
    pub page_dots: Rect,

    // Hotseat
    pub dock: Rect,
    pub dock_slots: usize,
    pub dock_icon: f32,

    // Gesture nav
    pub nav_pill: Rect,

    // App drawer
    pub drawer_status_h: f32,
    pub drawer_handle: Rect,
    pub drawer_search: Rect,
    pub drawer_header_y: f32,
    pub drawer_grid_top: f32,
    pub drawer_grid_bottom: f32,
    pub drawer_rows: usize,
    pub drawer_icon: f32,
    pub drawer_label_scale: usize,
}

/// Number of grid columns: 4 on phones, 5 or 6 on wide panels, matching
/// Launcher3's `numColumns` heuristics.
#[inline]
pub fn grid_cols_for(w: f32) -> usize {
    if w >= 1400.0 {
        6
    } else if w >= 1000.0 {
        5
    } else {
        4
    }
}

impl Layout {
    /// Build the layout for a panel of `w` x `h` pixels.
    ///
    /// `has_selection` reserves the action-chip band above the grid, which is
    /// how the workspace makes room for the edit-mode actions.
    pub fn new(w: f32, h: f32, has_selection: bool) -> Self {
        let w = w.max(1.0);
        let h = h.max(1.0);
        let pad = w * PANEL_PAD_FRACTION;
        // Touch targets must be at least 48dp on *either* axis, so they scale
        // with the short edge: on a 2000x1000 panel a width-derived minimum
        // would be 96px and eat the whole workspace.
        let min_touch = w.min(h) * TOUCH_TARGET_FRACTION;

        let status_bar_h = (h * STATUS_BAR_FRACTION).max(min_touch * 0.9);

        // Clock: cap height of the display clock, then the date line under it.
        let clock_h = (h * 0.052).max(28.0);
        let clock_y = status_bar_h + h * 0.022;
        let date_bottom = clock_y + clock_h + h * 0.010;

        // Search pill spans the panel minus symmetric gutters.
        let search_h = (h * 0.036).max(min_touch);
        let search_y = date_bottom + h * 0.016;
        let search = Rect {
            x: pad,
            y: search_y,
            w: w - pad * 2.0,
            h: search_h,
            radius: search_h * PILL_RADIUS,
        };

        // Edit-mode chip band, only when something is selected.
        let chip_h = (h * 0.028).max(min_touch);
        let chips_y = search.y + search.h + h * 0.014;
        let chip_gap = w * 0.018;
        let chip_w = ((w - pad * 2.0) - chip_gap) * 0.5;
        let remove_chip = Button {
            x: pad,
            y: chips_y,
            w: chip_w,
            h: chip_h,
        };
        let move_chip = Button {
            x: pad + chip_w + chip_gap,
            y: chips_y,
            w: chip_w,
            h: chip_h,
        };

        // Grid: columns from the panel width, then everything else follows.
        let grid_cols = grid_cols_for(w);
        let col_pitch = w / grid_cols as f32;
        let icon_size = col_pitch * ICON_FRACTION;
        let label_scale = if h >= 1600.0 { 2 } else { 1 };
        // Label line box: em height plus descender, plus the gaps around it.
        let label_h = if label_scale == 2 { 30.0 } else { 15.0 } * 1.2;
        let row_pitch = icon_size + icon_size * LABEL_GAP + label_h + icon_size * CELL_BOTTOM_GAP;

        // Hotseat and gesture nav are anchored to the bottom edge.
        let nav_h = (h * 0.006).max(3.0);
        let nav_margin = h * 0.008;
        let nav_pill = Rect {
            x: w * 0.5 - w * 0.065,
            y: h - nav_margin - nav_h,
            w: w * 0.130,
            h: nav_h,
            radius: nav_h * 0.5,
        };
        let dock_h = (h * 0.062).max(min_touch);
        let dock = Rect {
            x: pad,
            y: nav_pill.y - h * 0.014 - dock_h,
            w: w - pad * 2.0,
            h: dock_h,
            radius: dock_h * 0.36,
        };
        let dock_slots = if w >= 1000.0 { 5 } else { 4 };
        let dock_icon = (dock.h * 0.56).min(dock_w_for(dock.w, dock_slots) * 0.58);

        // Workspace band: between the chips/search and the page dots.
        let grid_top = if has_selection {
            chips_y + chip_h + h * 0.012
        } else {
            search.y + search.h + h * 0.016
        };
        let dots_h = (h * 0.010).max(min_touch * 0.4);
        let page_dots = Rect {
            x: w * 0.5 - w * 0.10,
            y: dock.y - h * 0.020 - dots_h,
            w: w * 0.20,
            h: dots_h,
            radius: dots_h * 0.5,
        };
        let grid_bottom = page_dots.y;
        let max_rows = ((grid_bottom - grid_top) / row_pitch).floor().max(0.0) as usize;

        // App drawer overlay: handle, search, header, then a full-bleed grid.
        let drawer_status_h = status_bar_h;
        let handle_w = w * 0.075;
        let handle_h = (h * 0.0035).max(3.0);
        let drawer_handle = Rect {
            x: w * 0.5 - handle_w * 0.5,
            y: drawer_status_h + h * 0.008,
            w: handle_w,
            h: handle_h,
            radius: handle_h * 0.5,
        };
        let drawer_search_h = (h * 0.034).max(min_touch);
        let drawer_search = Rect {
            x: pad,
            y: drawer_handle.y + drawer_handle.h + h * 0.012,
            w: w - pad * 2.0,
            h: drawer_search_h,
            radius: drawer_search_h * PILL_RADIUS,
        };
        let drawer_header_y = drawer_search.y + drawer_search.h + h * 0.016;
        let drawer_grid_top = drawer_header_y + (h * 0.018);
        let drawer_grid_bottom = h - nav_margin * 2.0 - nav_h;
        let drawer_rows = ((drawer_grid_bottom - drawer_grid_top) / row_pitch)
            .floor()
            .max(0.0) as usize;

        Self {
            w,
            h,
            status_bar_h,
            clock_h,
            clock_y,
            search,
            search_glyph_w: search_h * 0.52,
            chips_y,
            chip_h,
            chip_gap,
            remove_chip,
            move_chip,
            grid_cols,
            grid_top,
            grid_bottom,
            col_pitch,
            row_pitch,
            icon_size,
            icon_radius: icon_size * ICON_RADIUS,
            label_scale,
            max_rows,
            page_dots,
            dock,
            dock_slots,
            dock_icon,
            nav_pill,
            drawer_status_h,
            drawer_handle,
            drawer_search,
            drawer_header_y,
            drawer_grid_top,
            drawer_grid_bottom,
            drawer_rows,
            drawer_icon: icon_size * 1.06,
            drawer_label_scale: label_scale,
        }
    }

    /// Convenience constructor with no edit-mode band.
    #[inline]
    pub fn plain(w: f32, h: f32) -> Self {
        Self::new(w, h, false)
    }

    // ---------------------------------------------------------------- cells

    /// Geometry of workspace cell `index` (row-major), page-local.
    ///
    /// `index` is unbounded: callers clamp with [`Layout::max_rows`].
    #[inline]
    pub fn grid_cell(&self, index: usize) -> Cell {
        let row = index / self.grid_cols;
        let col = index % self.grid_cols;
        Cell {
            x: col as f32 * self.col_pitch,
            y: self.grid_top + row as f32 * self.row_pitch,
            w: self.col_pitch,
            h: self.row_pitch,
        }
    }

    /// Geometry of the workspace icon tile inside cell `index`.
    #[inline]
    pub fn grid_icon(&self, index: usize) -> Rect {
        let cell = self.grid_cell(index);
        Rect {
            x: cell.x + (cell.w - self.icon_size) * 0.5,
            y: cell.y,
            w: self.icon_size,
            h: self.icon_size,
            radius: self.icon_radius,
        }
    }

    /// Geometry of the icon tile for drawer row-major `index`.
    #[inline]
    pub fn drawer_icon_cell(&self, index: usize) -> Rect {
        let row = index / self.grid_cols;
        let col = index % self.grid_cols;
        Rect {
            x: col as f32 * self.col_pitch,
            y: self.drawer_grid_top + row as f32 * self.row_pitch,
            w: self.col_pitch,
            h: self.icon_size * 1.06,
            radius: self.icon_size * ICON_RADIUS * 1.06,
        }
    }

    /// Geometry of the tappable clock / date widget.
    #[inline]
    pub fn clock_rect(&self) -> Rect {
        Rect {
            x: self.w * PANEL_PAD_FRACTION,
            y: self.clock_y,
            w: self.w - self.w * PANEL_PAD_FRACTION * 2.0,
            h: self.clock_h + self.h * 0.020,
            radius: 0.0,
        }
    }

    /// Geometry of hotseat slot `index`.
    #[inline]
    pub fn dock_slot(&self, index: usize) -> Cell {
        let pitch = self.dock.w / self.dock_slots as f32;
        Cell {
            x: self.dock.x + index as f32 * pitch,
            y: self.dock.y,
            w: pitch,
            h: self.dock.h,
        }
    }

    #[inline]
    pub fn dock_icon_rect(&self, index: usize) -> Rect {
        let slot = self.dock_slot(index);
        Rect {
            x: slot.x + (slot.w - self.dock_icon) * 0.5,
            y: slot.y + (slot.h - self.dock_icon) * 0.5,
            w: self.dock_icon,
            h: self.dock_icon,
            radius: self.dock_icon * ICON_RADIUS,
        }
    }

    // ------------------------------------------------------------ hit tests

    /// Which home-screen band contains `(x, y)`, ignoring app identity.
    pub fn home_zone(&self, x: f32, y: f32) -> HomeZone {
        if y <= self.status_bar_h {
            return HomeZone::StatusBar;
        }
        if self.nav_pill.contains(x, y) {
            return HomeZone::NavPill;
        }
        if self.dock.contains(x, y) {
            return HomeZone::Dock;
        }
        if self.page_dots.contains(x, y) {
            return HomeZone::PageIndicator;
        }
        if self.search.contains(x, y) {
            return HomeZone::Search;
        }
        if self.remove_chip.contains(x, y) || self.move_chip.contains(x, y) {
            return HomeZone::ActionChips;
        }
        if y >= self.grid_top && y < self.grid_bottom {
            return HomeZone::Grid;
        }
        if y < self.search.y {
            return HomeZone::Clock;
        }
        HomeZone::Empty
    }

    /// Workspace cell under `(x, y)`, accounting for a horizontal scroll
    /// offset in pixels (positive = scrolled toward the next page).
    ///
    /// Returns `(page, index)` where `index` is the row-major cell index
    /// within `page`. Mid-swipe the pages in flight legitimately extend past
    /// the panel edges, so the horizontal window spans the whole paginated
    /// strip and the page is derived from the column rather than assumed.
    pub fn home_grid_hit_paged(
        &self,
        x: f32,
        y: f32,
        scroll: f32,
        total_pages: usize,
    ) -> Option<(usize, usize)> {
        if y < self.grid_top || y >= self.grid_bottom || self.max_rows == 0 {
            return None;
        }
        let rel_x = x + scroll;
        if rel_x < 0.0 {
            return None;
        }
        let page_pitch = self.w;
        let page = (rel_x / page_pitch) as usize;
        if total_pages > 0 && page >= total_pages {
            return None;
        }
        let col = ((rel_x - page as f32 * page_pitch) / self.col_pitch) as usize;
        if col >= self.grid_cols {
            // Trailing gutter of a page: no cell there.
            return None;
        }
        let row = ((y - self.grid_top) / self.row_pitch) as usize;
        if row >= self.max_rows {
            return None;
        }
        Some((page, row * self.grid_cols + col))
    }

    /// Workspace cell index under `(x, y)`, restricted to the page the panel
    /// is currently showing. Mid-swipe the caller should use
    /// [`Layout::home_grid_hit_paged`] with the real page count.
    #[inline]
    pub fn home_grid_hit(&self, x: f32, y: f32, scroll: f32) -> Option<usize> {
        self.home_grid_hit_paged(x, y, scroll, 1).map(|(_, i)| i)
    }

    /// Hotseat slot index under `(x, y)`.
    pub fn home_dock_hit(&self, x: f32, y: f32) -> Option<usize> {
        if !self.dock.contains(x, y) {
            return None;
        }
        let pitch = self.dock.w / self.dock_slots as f32;
        let idx = ((x - self.dock.x) / pitch).min(self.dock_slots as f32 - 1.0);
        Some(idx as usize)
    }

    /// Page index under the page-indicator dots.
    pub fn home_page_hit(&self, x: f32, y: f32, total_pages: usize) -> Option<usize> {
        if total_pages == 0 || !self.page_dots.contains(x, y) {
            return None;
        }
        let t = (x - self.page_dots.x) / self.page_dots.w;
        Some((t * total_pages as f32).min(total_pages as f32 - 1.0) as usize)
    }

    /// Which drawer band contains `(x, y)`, where `drawer_y_offset` is the
    /// drawer's on-screen top edge in pixels.
    pub fn drawer_zone(&self, drawer_y_offset: f32, x: f32, y: f32) -> DrawerZone {
        if y < drawer_y_offset || self.nav_pill.contains(x, y) {
            return DrawerZone::Empty;
        }
        if y - drawer_y_offset <= self.drawer_status_h {
            return DrawerZone::StatusBar;
        }
        if self.drawer_handle.contains(x, y - drawer_y_offset) {
            return DrawerZone::Handle;
        }
        if self.drawer_search.contains(x, y - drawer_y_offset) {
            return DrawerZone::Search;
        }
        let local_y = y - drawer_y_offset;
        if local_y < self.drawer_grid_top {
            return DrawerZone::Header;
        }
        DrawerZone::Grid
    }

    /// Which part of the drawer search field a touch landed on.
    #[inline]
    pub fn drawer_search_hit(&self, drawer_y_offset: f32, x: f32, y: f32) -> DrawerSearchHit {
        if !self.drawer_search.contains(x, y - drawer_y_offset) {
            return DrawerSearchHit::None;
        }
        // The clear affordance occupies the trailing 48dp of the field.
        let clear_w = self.w.min(self.h) * TOUCH_TARGET_FRACTION;
        if x >= self.drawer_search.x + self.drawer_search.w - clear_w {
            DrawerSearchHit::Clear
        } else {
            DrawerSearchHit::Focus
        }
    }

    /// Drawer cell index under `(x, y)`.
    pub fn drawer_grid_hit(&self, drawer_y_offset: f32, x: f32, y: f32) -> Option<usize> {
        if x < 0.0 || x >= self.w || self.drawer_rows == 0 {
            return None;
        }
        let local_y = y - drawer_y_offset;
        if local_y < self.drawer_grid_top || local_y >= self.drawer_grid_bottom {
            return None;
        }
        let col = (x / self.col_pitch).min(self.grid_cols as f32 - 1.0) as usize;
        let row = ((local_y - self.drawer_grid_top) / self.row_pitch) as usize;
        if row >= self.drawer_rows {
            return None;
        }
        Some(row * self.grid_cols + col)
    }
}

/// Width of one hotseat slot, given the dock width and slot count.
#[inline]
fn dock_w_for(dock_w: f32, slots: usize) -> f32 {
    dock_w / slots.max(1) as f32
}

// ===========================================================================
// Keyboard
// ===========================================================================

/// A virtual keyboard key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Shift,
    Backspace,
    Hide,
    Space,
    Enter,
}

/// Which app surface is on screen, for the panel-level geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPanel {
    None,
    Browser,
    Terminal,
    Messages,
    Settings,
    Phone,
    Other,
}

/// Geometry of the quick-settings grid in the notification shade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileGrid {
    pub origin: (f32, f32),
    pub tile: (f32, f32),
    pub gap: (f32, f32),
    pub cols: usize,
    pub rows: usize,
}

impl TileGrid {
    #[inline]
    pub fn cell(&self, index: usize) -> Rect {
        let col = index % self.cols;
        let row = index / self.cols;
        Rect {
            x: self.origin.0 + col as f32 * (self.tile.0 + self.gap.0),
            y: self.origin.1 + row as f32 * (self.tile.1 + self.gap.1),
            w: self.tile.0,
            h: self.tile.1,
            radius: self.tile.1 * 0.22,
        }
    }
    /// Tile index under `(x, y)`, if any.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        (0..(self.cols * self.rows)).find(|&i| self.cell(i).contains(x, y))
    }
}

/// Keyboard row key counts, matching the render order.
pub const KB_ROW1: usize = 10;
pub const KB_ROW2: usize = 9;
pub const KB_ROW3_MID: usize = 7;

/// Full virtual keyboard geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyboard {
    pub frame: Rect,
    /// Frame rows: number, top, height.
    pub rows: [f32; 4],
    pub row1: Rect,
    pub row2: Rect,
    pub row3_shift: Rect,
    pub row3_mid: [Rect; KB_ROW3_MID],
    pub row3_backspace: Rect,
    pub row4_hide: Rect,
    pub row4_space: Rect,
    pub row4_enter: Rect,
}

impl Keyboard {
    /// Build the keyboard for a panel, sized as a fraction of the display.
    pub fn new(w: f32, h: f32) -> Self {
        let pad = w * 0.012;
        // The frame must actually contain four rows of keys, so derive the
        // key height from the available height rather than the other way
        // round: on a square panel a height-derived key would overflow.
        let avail = (h * 0.175).max(w.min(h) * 0.16);
        let key_h = (avail * 0.175).max(w.min(h) * 0.040);
        let key_gap = key_h * 0.18;
        let top_pad = key_h * 0.38;
        let inset = key_h * 0.23;
        let frame_h = top_pad + 4.0 * key_h + 3.0 * key_gap + key_h * 0.38;
        let frame = Rect {
            x: pad,
            y: h - frame_h - h * 0.008,
            w: w - pad * 2.0,
            h: frame_h,
            radius: w * 0.024,
        };
        let inner_x = frame.x + inset;
        let inner_w = frame.w - inset * 2.0;
        let rows = [
            frame.y + top_pad,
            frame.y + top_pad + (key_h + key_gap),
            frame.y + top_pad + 2.0 * (key_h + key_gap),
            frame.y + top_pad + 3.0 * (key_h + key_gap),
        ];

        let mk = |x: f32, w: f32, row: usize| Rect {
            x,
            y: rows[row],
            w,
            h: key_h,
            radius: key_h * 0.20,
        };

        // Row 1 and row 2 span the full inner width; `row1_at` / `row2_at`
        // divide them into keys, so the strip and the keys can never disagree.
        let row1 = mk(inner_x, inner_w, 0);
        let row2 = mk(inner_x, inner_w, 1);
        let shift_w = inner_w * 0.19;
        let mid_w = (inner_w - shift_w * 2.0) / KB_ROW3_MID as f32;
        let mut row3_mid = [row3_dummy(); KB_ROW3_MID];
        for (i, slot) in row3_mid.iter_mut().enumerate() {
            *slot = mk(inner_x + shift_w + mid_w * i as f32, mid_w, 2);
        }
        let hide_w = inner_w * 0.18;
        let enter_w = inner_w * 0.24;
        let space_w = inner_w - hide_w - enter_w;

        Self {
            frame,
            rows,
            row1,
            row2,
            row3_shift: mk(inner_x, shift_w, 2),
            row3_mid,
            row3_backspace: mk(inner_x + inner_w - shift_w, shift_w, 2),
            row4_hide: mk(inner_x, hide_w, 3),
            row4_space: mk(inner_x + hide_w, space_w, 3),
            row4_enter: mk(inner_x + inner_w - enter_w, enter_w, 3),
        }
    }

    /// Key at index `i` of row 1.
    #[inline]
    pub fn row1_at(&self, i: usize) -> Rect {
        Rect {
            x: self.row1.x + self.row1.w / KB_ROW1 as f32 * i as f32,
            y: self.row1.y,
            w: self.row1.w / KB_ROW1 as f32,
            h: self.row1.h,
            radius: self.row1.radius,
        }
    }

    /// Key at index `i` of row 2.
    #[inline]
    pub fn row2_at(&self, i: usize) -> Rect {
        Rect {
            x: self.row2.x + self.row2.w / KB_ROW2 as f32 * i as f32,
            y: self.row2.y,
            w: self.row2.w / KB_ROW2 as f32,
            h: self.row2.h,
            radius: self.row2.radius,
        }
    }

    /// Key under `(x, y)`, if the touch landed on the keyboard.
    pub fn hit(&self, x: f32, y: f32) -> Option<Key> {
        if !self.frame.contains(x, y) {
            return None;
        }
        if self.row3_shift.contains(x, y) {
            return Some(Key::Shift);
        }
        if self.row3_backspace.contains(x, y) {
            return Some(Key::Backspace);
        }
        for r in self.row3_mid.iter() {
            if r.contains(x, y) {
                let i = row3_index(self, x, y)?;
                return Some(Key::Char(ROW3[i]));
            }
        }
        if self.row4_hide.contains(x, y) {
            return Some(Key::Hide);
        }
        if self.row4_enter.contains(x, y) {
            return Some(Key::Enter);
        }
        if self.row4_space.contains(x, y) {
            return Some(Key::Space);
        }
        for (i, key) in ROW1.iter().enumerate() {
            if self.row1_at(i).contains(x, y) {
                return Some(Key::Char(*key));
            }
        }
        for (i, key) in ROW2.iter().enumerate() {
            if self.row2_at(i).contains(x, y) {
                return Some(Key::Char(*key));
            }
        }
        None
    }
}

const fn row3_dummy() -> Rect {
    Rect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    }
}

#[inline]
fn row3_index(k: &Keyboard, x: f32, _y: f32) -> Option<usize> {
    for (i, r) in k.row3_mid.iter().enumerate() {
        if x >= r.x && x <= r.x + r.w {
            return Some(i);
        }
    }
    None
}

/// Row 1 key labels.
pub const ROW1: [char; KB_ROW1] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
/// Row 2 key labels.
pub const ROW2: [char; KB_ROW2] = ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o'];
/// Row 3 middle key labels.
pub const ROW3: [char; KB_ROW3_MID] = ['z', 'x', 'c', 'v', 'b', 'n', 'm'];

// ===========================================================================
// App surfaces
// ===========================================================================

/// Geometry of a single app window: top app bar, terminal tabs, message box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppLayout {
    pub bar: Rect,
    pub back: Button,
    pub close: Button,
    pub tabs: Rect,
    pub tab_count: usize,
    pub input: Rect,
    pub send: Button,
    pub scroll_top: f32,
    pub scroll_bottom: f32,
}

impl AppLayout {
    /// Build app-window geometry for `panel`.
    pub fn new(w: f32, h: f32, panel: AppPanel, tab_count: usize) -> Self {
        let pad = w * PANEL_PAD_FRACTION;
        let min_touch = w.min(h) * TOUCH_TARGET_FRACTION;
        let bar_h = min_touch.max(h * 0.038);
        let bar_y = h * 0.020 + h * STATUS_BAR_FRACTION;
        let bar = Rect {
            x: pad,
            y: bar_y,
            w: w - pad * 2.0,
            h: bar_h,
            radius: bar_h * 0.28,
        };
        let btn_w = (bar.w * 0.30).min(bar_h * 2.4);
        let btn_h = bar_h * 0.72;
        let btn_y = bar.y + (bar.h - btn_h) * 0.5;
        let back = Button {
            x: bar.x + bar.h * 0.18,
            y: btn_y,
            w: btn_w,
            h: btn_h,
        };
        let close = Button {
            x: bar.x + bar.w - bar.h * 0.18 - btn_w,
            y: btn_y,
            w: btn_w,
            h: btn_h,
        };

        let tabs_y = bar.y + bar.h + h * 0.010;
        let tab_h = min_touch * 0.72;
        let tabs = Rect {
            x: pad,
            y: tabs_y,
            w: w - pad * 2.0,
            h: tab_h,
            radius: tab_h * 0.24,
        };

        let input_h = min_touch * 0.95;
        let input = Rect {
            x: pad,
            y: h - h * 0.055 - input_h,
            w: w - pad * 2.0 - min_touch * 1.3,
            h: input_h,
            radius: input_h * 0.28,
        };
        let send = Button {
            x: input.x + input.w + min_touch * 0.2,
            y: input.y,
            w: min_touch * 1.1,
            h: input_h,
        };

        let scroll_top = match panel {
            AppPanel::Terminal => tabs.y + tabs.h + h * 0.008,
            _ => bar.y + bar.h + h * 0.008,
        };
        let scroll_bottom = h * 0.86;

        Self {
            bar,
            back,
            close,
            tabs,
            tab_count,
            input,
            send,
            scroll_top,
            scroll_bottom,
        }
    }

    /// Terminal tab strip geometry for `index`.
    pub fn tab_rect(&self, index: usize) -> Rect {
        let pitch = self.tabs.w / (self.tab_count.max(1) + self.add_slot()) as f32;
        Rect {
            x: self.tabs.x + index as f32 * pitch,
            y: self.tabs.y,
            w: pitch * 0.94,
            h: self.tabs.h,
            radius: self.tabs.h * 0.24,
        }
    }

    /// Trailing hit area of the active tab that closes it.
    pub fn tab_close_zone(&self, index: usize, active: usize) -> Rect {
        let r = self.tab_rect(index);
        let w = (r.h * 0.55).min(r.w * 0.5);
        let _ = active;
        Rect {
            x: r.x + r.w - w,
            y: r.y,
            w,
            h: r.h,
            radius: r.h * 0.24,
        }
    }

    /// The "+" tab appended after the real tabs, when there is room.
    pub fn add_tab_rect(&self) -> Option<Rect> {
        if self.add_slot() == 0 {
            return None;
        }
        let pitch = self.tabs.w / (self.tab_count + 1) as f32;
        Some(Rect {
            x: self.tabs.x + self.tab_count as f32 * pitch,
            y: self.tabs.y,
            w: pitch * 0.94,
            h: self.tabs.h,
            radius: self.tabs.h * 0.24,
        })
    }

    #[inline]
    fn add_slot(&self) -> usize {
        if self.tab_count < 4 {
            1
        } else {
            0
        }
    }

    /// Terminal tab hit test.
    pub fn hit_tab(&self, x: f32, y: f32) -> Option<TabHit> {
        if !self.tabs.contains(x, y) && self.add_tab_rect().map(|r| r.contains(x, y)) != Some(true) {
            return None;
        }
        for i in 0..self.tab_count {
            if self.tab_rect(i).contains(x, y) {
                return Some(TabHit::Select(i));
            }
        }
        self.add_tab_rect()
            .filter(|r| r.contains(x, y))
            .map(|_| TabHit::Add)
    }

    /// Terminal tab hit test that resolves the close affordance on the active
    /// tab, mirroring how the strip is drawn.
    pub fn hit_tab_active(&self, x: f32, y: f32, active: usize) -> Option<TabHit> {
        if self.tab_count > 1 && self.tab_close_zone(active, active).contains(x, y) {
            return Some(TabHit::Close(active));
        }
        self.hit_tab(x, y)
    }
}

/// Result of a terminal tab strip hit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    Select(usize),
    Close(usize),
    Add,
}

/// Which part of a drawer search field a touch landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerSearchHit {
    None,
    Focus,
    Clear,
}

/// Which quick-settings tile a touch landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadeZone {
    Header,
    Tiles(usize),
    Brightness,
    Notifications,
    Handle,
    Empty,
}

/// Geometry of the quick-settings / notification shade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadeLayout {
    pub top: f32,
    pub clock_size: f32,
    pub date_y: f32,
    pub tiles: TileGrid,
    pub brightness: Rect,
    pub notif_title_y: f32,
    pub notifs: [Rect; 3],
    pub handle: Rect,
}

impl ShadeLayout {
    /// Build the shade layout for a panel.
    pub fn new(w: f32, h: f32) -> Self {
        let pad = w * PANEL_PAD_FRACTION;
        let min_touch = w.min(h) * TOUCH_TARGET_FRACTION;
        let top = h * STATUS_BAR_FRACTION;
        let clock_size = h * 0.040;
        let date_y = top + h * 0.040;
        let tile_w = (w - pad * 2.0 - w * 0.018) * 0.5;
        let tile_h = min_touch.max(h * 0.046);
        let tiles = TileGrid {
            origin: (pad, date_y + h * 0.030),
            tile: (tile_w, tile_h),
            gap: (w * 0.018, h * 0.008),
            cols: 2,
            rows: 4,
        };
        let slider_y = tiles.origin.1 + 4.0 * (tile_h + tiles.gap.1) + h * 0.006;
        let brightness = Rect {
            x: pad,
            y: slider_y,
            w: w - pad * 2.0,
            h: tile_h,
            radius: tile_h * 0.5,
        };
        let notif_title_y = brightness.y + brightness.h + h * 0.012;
        let card_h = (h * 0.056).max(min_touch);
        let card_gap = h * 0.008;
        let notifs = [
            Rect {
                x: pad,
                y: notif_title_y + h * 0.016,
                w: w - pad * 2.0,
                h: card_h,
                radius: card_h * 0.22,
            },
            Rect {
                x: pad,
                y: notif_title_y + h * 0.016 + card_h + card_gap,
                w: w - pad * 2.0,
                h: card_h,
                radius: card_h * 0.22,
            },
            Rect {
                x: pad,
                y: notif_title_y + h * 0.016 + 2.0 * (card_h + card_gap),
                w: w - pad * 2.0,
                h: card_h,
                radius: card_h * 0.22,
            },
        ];
        let handle_w = w * 0.13;
        let handle = Rect {
            x: w * 0.5 - handle_w * 0.5,
            y: h - h * 0.025,
            w: handle_w,
            h: (h * 0.003).max(3.0),
            radius: 2.0,
        };
        Self {
            top,
            clock_size,
            date_y,
            tiles,
            brightness,
            notif_title_y,
            notifs,
            handle,
        }
    }

    /// Which shade band contains `(x, y)`.
    pub fn zone(&self, x: f32, y: f32) -> ShadeZone {
        if y < self.tiles.origin.1 {
            return ShadeZone::Header;
        }
        if let Some(i) = self.tiles.hit(x, y) {
            return ShadeZone::Tiles(i);
        }
        if self.brightness.contains(x, y) {
            return ShadeZone::Brightness;
        }
        if y >= self.notifs[0].y {
            return ShadeZone::Notifications;
        }
        if self.handle.contains(x, y) {
            return ShadeZone::Handle;
        }
        ShadeZone::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panel sizes the shell is expected to run on, plus awkward ones.
    const PANELS: &[(f32, f32)] = &[
        (1080.0, 2400.0),
        (1440.0, 3120.0),
        (720.0, 1280.0),
        (360.0, 640.0),
        (600.0, 600.0),
        (2000.0, 1000.0),
    ];

    #[test]
    fn bands_never_overlap_in_stacking_order() {
        for &(w, h) in PANELS {
            for sel in [false, true] {
                let l = Layout::new(w, h, sel);
                let name = format!("{w}x{h} sel={sel}");
                assert!(l.status_bar_h < l.clock_y, "{name}: status bar overlaps clock");
                assert!(l.clock_y < l.search.y, "{name}: clock overlaps search");
                assert!(l.search.y + l.search.h <= l.grid_top + 0.001, "{name}: search overlaps grid");
                if sel {
                    // The chip band only exists in edit mode, and it is what
                    // pushes the grid down: row 0 must start below it.
                    assert!(
                        l.chips_y + l.chip_h <= l.grid_top + 0.001,
                        "{name}: chips overlap grid"
                    );
                } else {
                    assert_eq!(l.grid_top, l.search.y + l.search.h + h * 0.016, "{name}");
                }
                assert!(l.grid_top < l.grid_bottom, "{name}: grid band is empty");
                assert!(l.grid_bottom <= l.page_dots.y + 0.001, "{name}: grid overlaps dots");
                assert!(l.page_dots.y + l.page_dots.h <= l.dock.y + 0.001, "{name}: dots overlap dock");
                assert!(l.dock.y + l.dock.h <= l.nav_pill.y + 0.001, "{name}: dock overlaps nav");
                assert!(l.nav_pill.y + l.nav_pill.h <= h + 0.001, "{name}: nav off screen");
            }
        }
    }

    #[test]
    fn chips_do_not_overlap_row_zero() {
        for &(w, h) in PANELS {
            let plain = Layout::new(w, h, false);
            let sel = Layout::new(w, h, true);
            let chip_bottom = sel.remove_chip.y + sel.remove_chip.h;
            assert!(
                chip_bottom <= sel.grid_top,
                "{w}x{h}: chip row overlaps grid row 0 ({} vs {})",
                chip_bottom,
                sel.grid_top
            );
            // The reserved band must not eat the whole workspace.
            assert!(sel.max_rows > 0, "{w}x{h} sel: no room for icons");
            assert!(plain.max_rows > 0, "{w}x{h}: no room for icons");
        }
    }

    #[test]
    fn rendered_cells_are_exactly_the_tested_cells() {
        for &(w, h) in PANELS {
            let l = Layout::new(w, h, false);
            for i in 0..(l.grid_cols * l.max_rows.min(6)) {
                let icon = l.grid_icon(i);
                // The cell centre must hit-test back to the same cell, even
                // when the touch is inside the cell but outside the icon.
                let cell = l.grid_cell(i);
                let (cx, cy) = cell.center();
                assert_eq!(l.home_grid_hit(cx, cy, 0.0), Some(i), "{w}x{h} cell {i}");
                let (ix, iy) = icon.center();
                assert_eq!(l.home_grid_hit(ix, iy, 0.0), Some(i), "{w}x{h} icon {i}");
            }
        }
    }

    #[test]
    fn hit_tests_are_exclusive_beyond_the_last_row() {
        for &(w, h) in PANELS {
            let l = Layout::new(w, h, false);
            let below = l.grid_bottom + 1.0;
            assert_eq!(l.home_grid_hit(w * 0.5, below, 0.0), None, "{w}x{h}");
            assert_eq!(l.home_grid_hit(-1.0, l.grid_top, 0.0), None, "{w}x{h}");
            assert_eq!(l.home_grid_hit(l.w, l.grid_top, 0.0), None, "{w}x{h}");
        }
    }

    #[test]
    fn scroll_offset_shifts_columns_consistently() {
        let l = Layout::plain(1080.0, 2400.0);
        // A positive scroll slides the workspace left, so the content under a
        // fixed touch moves one column later in page order.
        for row in 0..l.max_rows.min(4) {
            let y = l.grid_top + row as f32 * l.row_pitch + 4.0;
            let x = l.col_pitch * 2.0 + 1.0;
            let base = l.home_grid_hit(x, y, 0.0).unwrap();
            let scrolled = l.home_grid_hit(x, y, l.col_pitch).unwrap();
            assert_eq!(scrolled, base + 1, "row {row}");
        }
        // Scrolling a full page width brings the next page under the touch.
        let y = l.grid_top + 4.0;
        assert_eq!(l.home_grid_hit_paged(10.0, y, 0.0, 3), Some((0, 0)));
        assert_eq!(l.home_grid_hit_paged(10.0, y, l.w, 3), Some((1, 0)));
        assert_eq!(l.home_grid_hit_paged(10.0, y, 2.0 * l.w, 3), Some((2, 0)));
        // One column pitch into the next page is still that page's column 0.
        assert_eq!(
            l.home_grid_hit_paged(10.0, y, l.w + l.col_pitch, 3),
            Some((1, 1))
        );
        // Past the last page there is nothing under the touch.
        assert_eq!(l.home_grid_hit_paged(10.0, y, 3.0 * l.w, 3), None);
        // And the trailing gutter of a page belongs to no cell.
        assert_eq!(l.home_grid_hit_paged(l.col_pitch * 3.0 + 2.0, y, 0.0, 3), Some((0, 3)));
    }

    #[test]
    fn dock_and_drawer_hit_tests_match_their_geometry() {
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            for s in 0..l.dock_slots {
                let r = l.dock_icon_rect(s);
                let (cx, cy) = (r.center_x(), r.center_y());
                assert_eq!(l.home_dock_hit(cx, cy), Some(s), "{w}x{h} dock {s}");
            }
            // One pixel outside the dock must not hit.
            assert_eq!(l.home_dock_hit(l.dock.center_x(), l.dock.y - 1.0), None);

            for i in 0..(l.grid_cols * l.drawer_rows.min(6)) {
                let r = l.drawer_icon_cell(i);
                assert_eq!(l.drawer_grid_hit(0.0, r.center_x(), r.center_y()), Some(i));
            }
        }
    }

    #[test]
    fn drawer_offset_moves_the_whole_overlay_together() {
        let l = Layout::plain(1080.0, 2400.0);
        let icon = l.drawer_icon_cell(2);
        // Fully open: the drawer is at offset 0.
        assert_eq!(l.drawer_grid_hit(0.0, icon.center_x(), icon.center_y()), Some(2));
        // Dragged half way down: the same content moved with the overlay.
        let off = l.h * 0.5;
        assert_eq!(
            l.drawer_grid_hit(off, icon.center_x(), icon.center_y() + off),
            Some(2)
        );
        // Touches above the drawer's top edge belong to the home screen.
        assert_eq!(l.drawer_grid_hit(off, icon.center_x(), off - 5.0), None);
    }

    #[test]
    fn drawer_bands_are_ordered_and_disjoint() {
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            let probe = w * 0.5;
            let mut prev_y = -1.0;
            let mut prev = DrawerZone::Empty;
            for step in 0..400 {
                let y = h * step as f32 / 400.0;
                let z = l.drawer_zone(0.0, probe, y);
                if z != prev {
                    assert!(y > prev_y, "{w}x{h}: zone changed without advancing");
                    prev_y = y;
                    prev = z;
                }
            }
            assert_eq!(l.drawer_zone(0.0, probe, -1.0), DrawerZone::Empty);
            assert_eq!(l.drawer_zone(l.h, probe, l.h - 1.0), DrawerZone::Empty);
        }
    }

    #[test]
    fn every_panel_gets_a_usable_launcher() {
        for &(w, h) in PANELS {
            for sel in [false, true] {
                let l = Layout::new(w, h, sel);
                let n = l.grid_cols * l.max_rows;
                assert!(n >= 8, "{w}x{h} sel={sel}: only {n} cells");
                assert!(l.icon_size >= 24.0, "{w}x{h}: icon too small");
                assert!(l.dock.h > 0.0 && l.dock.w > 0.0, "{w}x{h}: empty dock");
                // A square or landscape panel legitimately fits fewer drawer
                // rows; it just must not fit none.
                assert!(l.drawer_rows >= 1, "{w}x{h}: drawer too short");
                // Search pill and chips must be wide enough to be tappable.
                assert!(l.search.h >= (w * 0.02).min(w.min(h) * 0.09), "{w}x{h}: search pill too short");
                assert!(l.remove_chip.h >= (w * 0.02).min(w.min(h) * 0.048), "{w}x{h}: chip too short");
            }
        }
    }

    #[test]
    fn column_count_tracks_panel_width() {
        assert_eq!(grid_cols_for(360.0), 4);
        assert_eq!(grid_cols_for(1080.0), 5);
        assert_eq!(grid_cols_for(1600.0), 6);
    }

    // ------------------------------------------------------------- keyboard

    #[test]
    fn keyboard_rows_and_keys_do_not_overlap() {
        for &(w, h) in PANELS {
            let k = Keyboard::new(w, h);
            let n = format!("{w}x{h}");
            // Rows are ordered and disjoint.
            for i in 0..3 {
                assert!(
                    k.rows[i] + k.row1.h <= k.rows[i + 1] + 0.001,
                    "{n}: keyboard rows {i} overlap"
                );
            }
            let last = k.rows[3] + k.row1.h;
            assert!(last <= k.frame.y + k.frame.h + 0.001, "{n}: last row escapes frame");
            // Row 1 keys tile the row exactly once.
            let total: f32 = (0..KB_ROW1).map(|i| k.row1_at(i).w).sum();
            assert!((total - k.row1.w).abs() < 0.5, "{n}: row1 width {}", total);
            // Every key is a distinct, non-empty rect.
            for r in [k.row1, k.row2, k.row3_shift, k.row3_backspace, k.row4_hide, k.row4_space, k.row4_enter] {
                assert!(r.w > 0.0 && r.h > 0.0, "{n}: degenerate key");
            }
            for r in k.row3_mid.iter() {
                assert!(r.w > 0.0, "{n}: degenerate row3 key");
            }
        }
    }

    #[test]
    fn every_key_reports_itself_under_its_own_centre() {
        for &(w, h) in PANELS {
            let k = Keyboard::new(w, h);
            let n = format!("{w}x{h}");
            let probe = |r: Rect| (r.center_x(), r.center_y());
            let (x, y) = probe(k.row3_shift);
            assert_eq!(k.hit(x, y), Some(Key::Shift), "{n}: shift");
            let (x, y) = probe(k.row3_backspace);
            assert_eq!(k.hit(x, y), Some(Key::Backspace), "{n}: backspace");
            let (x, y) = probe(k.row4_hide);
            assert_eq!(k.hit(x, y), Some(Key::Hide), "{n}: hide");
            let (x, y) = probe(k.row4_enter);
            assert_eq!(k.hit(x, y), Some(Key::Enter), "{n}: enter");
            let (x, y) = probe(k.row4_space);
            assert_eq!(k.hit(x, y), Some(Key::Space), "{n}: space");
            for i in 0..KB_ROW1 {
                let (x, y) = probe(k.row1_at(i));
                assert_eq!(k.hit(x, y), Some(Key::Char(ROW1[i])), "{n}: row1 {i}");
            }
            for i in 0..KB_ROW2 {
                let (x, y) = probe(k.row2_at(i));
                assert_eq!(k.hit(x, y), Some(Key::Char(ROW2[i])), "{n}: row2 {i}");
            }
            for i in 0..KB_ROW3_MID {
                let (x, y) = probe(k.row3_mid[i]);
                assert_eq!(k.hit(x, y), Some(Key::Char(ROW3[i])), "{n}: row3 {i}");
            }
        }
    }

    #[test]
    fn keyboard_rejects_touches_outside_its_frame() {
        for &(w, h) in PANELS {
            let k = Keyboard::new(w, h);
            assert_eq!(k.hit(w * 0.5, 0.0), None, "{w}x{h}: above frame");
            assert_eq!(k.hit(w * 0.5, k.frame.y - 1.0), None, "{w}x{h}: above frame");
            assert_eq!(k.hit(-1.0, k.frame.center_y()), None, "{w}x{h}: left of frame");
            assert_eq!(k.hit(w + 1.0, k.frame.center_y()), None, "{w}x{h}: right of frame");
        }
    }

    // ---------------------------------------------------------- app surfaces

    #[test]
    fn app_bar_buttons_are_inside_the_bar_and_disjoint() {
        for &(w, h) in PANELS {
            for panel in [AppPanel::Browser, AppPanel::Terminal, AppPanel::Messages] {
                let a = AppLayout::new(w, h, panel, 2);
                let n = format!("{w}x{h} {panel:?}");
                assert!(a.bar.x <= a.back.x && a.back.x + a.back.w <= a.bar.x + a.bar.w, "{n}: back outside bar");
                assert!(a.bar.x <= a.close.x && a.close.x + a.close.w <= a.bar.x + a.bar.w, "{n}: close outside bar");
                assert!(a.back.x + a.back.w <= a.close.x, "{n}: back overlaps close");
                assert!(a.close.y + a.close.h <= a.bar.y + a.bar.h, "{n}: close below bar");
                // Content never starts above the bar.
                assert!(a.scroll_top >= a.bar.y + a.bar.h - 0.001, "{n}: content under bar");
                // Input row is on screen and above the gesture area.
                assert!(a.input.y + a.input.h <= h, "{n}: input off screen");
                assert!(a.send.x + a.send.w <= w, "{n}: send off screen");
            }
        }
    }

    #[test]
    fn terminal_tabs_tile_without_overlap_and_respect_the_cap() {
        for &(w, h) in PANELS {
            for n in 1..=4usize {
                let a = AppLayout::new(w, h, AppPanel::Terminal, n);
                let label = format!("{w}x{h} tabs={n}");
                let mut prev_right = a.tabs.x - 1.0;
                for i in 0..n {
                    let r = a.tab_rect(i);
                    assert!(r.x >= prev_right, "{label}: tab {i} overlaps previous");
                    assert!(r.x + r.w <= a.tabs.x + a.tabs.w + 0.001, "{label}: tab {i} off strip");
                    prev_right = r.x + r.w;
                }
                if let Some(add) = a.add_tab_rect() {
                    assert!(add.x >= prev_right, "{label}: + overlaps last tab");
                    assert_eq!(a.hit_tab(add.center_x(), add.center_y()), Some(TabHit::Add), "{label}");
                } else {
                    assert!(n >= 4, "{label}: missing + below the cap");
                }
            }
        }
    }

    #[test]
    fn tab_hit_distinguishes_select_close_and_add() {
        let a = AppLayout::new(1080.0, 2400.0, AppPanel::Terminal, 3);
        for i in 0..3 {
            let r = a.tab_rect(i);
            let y = r.center_y();
            // The close affordance lives on the active tab's trailing edge.
            let close = a.tab_close_zone(i, i);
            assert_eq!(
                a.hit_tab_active(close.center_x(), y, i),
                Some(TabHit::Close(i)),
                "tab {i} close"
            );
            let (x, _) = r.center();
            assert_eq!(a.hit_tab(x, y), Some(TabHit::Select(i)), "tab {i} select");
            assert_eq!(a.hit_tab_active(x, y, i), Some(TabHit::Select(i)), "tab {i} active select");
        }
        // With a single tab there is nothing to close, so the whole strip
        // selects.
        let one = AppLayout::new(1080.0, 2400.0, AppPanel::Terminal, 1);
        let r = one.tab_rect(0);
        assert_eq!(one.hit_tab_active(r.center_x(), r.center_y(), 0), Some(TabHit::Select(0)));
        assert_eq!(a.hit_tab(-5.0, a.tabs.center_y()), None);
    }

    // ------------------------------------------------------------------ shade

    #[test]
    fn shade_tiles_tile_a_clean_grid_and_hit_back() {
        for &(w, h) in PANELS {
            let s = ShadeLayout::new(w, h);
            let n = format!("{w}x{h}");
            for i in 0..8 {
                let c = s.tiles.cell(i);
                assert!(c.x >= 0.0 && c.x + c.w <= w + 0.001, "{n}: tile {i} off panel");
                assert_eq!(s.tiles.hit(c.center_x(), c.center_y()), Some(i), "{n}: tile {i}");
                // A point one pixel right of the tile is the next tile or a gap.
                let after = c.x + c.w + 1.0;
                assert_ne!(s.tiles.hit(after, c.center_y()), Some(i), "{n}: tile {i} bleed");
            }
            assert_eq!(s.zone(s.brightness.center_x(), s.brightness.center_y()), ShadeZone::Brightness, "{n}");
            assert_eq!(s.zone(s.notifs[1].center_x(), s.notifs[1].center_y()), ShadeZone::Notifications, "{n}");
            assert_eq!(s.zone(w * 0.5, 1.0), ShadeZone::Header, "{n}");
        }
    }

    #[test]
    fn notification_cards_do_not_overlap() {
        for &(w, h) in PANELS {
            let s = ShadeLayout::new(w, h);
            for i in 0..s.notifs.len() - 1 {
                assert!(
                    s.notifs[i].y + s.notifs[i].h <= s.notifs[i + 1].y + 0.001,
                    "{w}x{h}: notification {i} overlaps {i}"
                );
            }
            assert!(s.notifs[s.notifs.len() - 1].y + s.notifs[2].h <= h, "{w}x{h}: last card off screen");
        }
    }
}
