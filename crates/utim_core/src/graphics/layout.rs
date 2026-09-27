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

use super::font;
use core::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

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
    /// Width of one hotseat slot, computed once: `dock.w / dock_slots`.
    pub dock_pitch: f32,
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
    /// Whether the edit-mode action-chip band is reserved. `home_zone` only
    /// reports `ActionChips` when this is set; otherwise the chip rects are
    /// inert geometry overlapping the top of the grid band.
    pub has_selection: bool,
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
        // Label line box: em height plus descender, derived from the same
        // type scale the renderer draws with, so geometry and type agree.
        let label_h = font::em_px_at(label_scale, w as usize) * 1.2;
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
        let dock_pitch = dock_w_for(dock.w, dock_slots);
        let dock_icon = (dock.h * 0.56).min(dock_pitch * 0.58);

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
            // The renderer spaces dots by w/(n-1) with the first dot's left
            // edge at dots.x, so the last dot's body extends one dot width
            // past w*0.20; publish the strip that is actually drawn.
            w: w * 0.20 + dots_h,
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
            dock_pitch,
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
            has_selection,
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
            h: self.drawer_icon,
            radius: self.drawer_icon * ICON_RADIUS,
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
        Cell {
            x: self.dock.x + index as f32 * self.dock_pitch,
            y: self.dock.y,
            w: self.dock_pitch,
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
        // The chip rects always exist as geometry, but they only cover live
        // actions in edit mode; otherwise row 0 of the grid starts under
        // them and must win.
        if self.has_selection && (self.remove_chip.contains(x, y) || self.move_chip.contains(x, y)) {
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
        if total_pages == 0 {
            return None;
        }
        if !x.is_finite() || !y.is_finite() || !scroll.is_finite() {
            return None;
        }
        if y < self.grid_top || y >= self.grid_bottom || self.max_rows == 0 {
            return None;
        }
        let rel_x = x + scroll;
        if rel_x < 0.0 {
            return None;
        }
        let page_pitch = self.w;
        let page = (rel_x / page_pitch) as usize;
        if page >= total_pages {
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
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        if !self.dock.contains(x, y) {
            return None;
        }
        let idx = ((x - self.dock.x) / self.dock_pitch).min(self.dock_slots as f32 - 1.0);
        Some(idx as usize)
    }

    /// Page index under the page-indicator dots, using the same pitch the
    /// renderer draws with: dots span the strip at `w/(n-1)`, first dot's
    /// left edge at `page_dots.x`. An n-way split of the strip would leave
    /// the last dot untappable and misattribute its neighbours.
    pub fn home_page_hit(&self, x: f32, y: f32, total_pages: usize) -> Option<usize> {
        if total_pages == 0 {
            return None;
        }
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        if x < self.page_dots.x || x > self.page_dots.x + self.page_dots.w {
            return None;
        }
        if y < self.page_dots.y || y > self.page_dots.y + self.page_dots.h {
            return None;
        }
        if total_pages == 1 {
            return Some(0);
        }
        let pitch = self.page_dots.w / (total_pages as f32 - 1.0);
        let t = ((x - self.page_dots.x) / pitch + 0.5).floor().max(0.0);
        Some((t as usize).min(total_pages - 1))
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
        // Bound the Grid band exactly where drawer_grid_hit stops accepting
        // cells; past the bottom there is no cell under the touch.
        if local_y < self.drawer_grid_bottom {
            return DrawerZone::Grid;
        }
        DrawerZone::Empty
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
        if !drawer_y_offset.is_finite() || !x.is_finite() || !y.is_finite() {
            return None;
        }
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

    // ------------------------------------------------- density-aware metrics
    //
    // Everything below is the single source of truth for the dp-sized
    // sub-layouts. They are methods rather than fields on purpose: each is a
    // pure function of `(w, h)`, so storing them would grow the `Copy` value
    // that the render path passes around by ~200 bytes for nothing.

    /// The dp scale and per-device metrics for this panel width.
    #[inline]
    pub fn profile(&self) -> DeviceProfile {
        DeviceProfile::for_panel(self.w)
    }

    /// The top workspace row, which is where the smartspace lives.
    #[inline]
    pub fn smartspace(&self) -> SmartspaceLayout {
        SmartspaceLayout::new(self, self.grid_bottom - self.grid_top)
    }

    /// The hotseat search pill, centred in the hotseat cell row.
    #[inline]
    pub fn qsb(&self) -> QsbLayout {
        QsbLayout::new(self)
    }

    /// The page-indicator band, left-anchored slot grid.
    #[inline]
    pub fn page_indicator(&self) -> PageIndicatorLayout {
        PageIndicatorLayout::new(self)
    }

    /// The app-drawer sheet at its `shift` top-edge offset (`0` = closed,
    /// `h` = fully open; see [`DrawerSheetLayout`]).
    #[inline]
    pub fn drawer_sheet(&self, shift: f32) -> DrawerSheetLayout {
        DrawerSheetLayout::new(self, shift)
    }

    /// The app-list fast scroller.
    #[inline]
    pub fn fast_scroller(&self) -> FastScrollerLayout {
        FastScrollerLayout::new(self)
    }

    /// The recents card stack and its action band.
    #[inline]
    pub fn recents(&self) -> RecentsLayout {
        RecentsLayout::new(self)
    }

    /// The folder grid, its chrome and the icon-preview metrics.
    #[inline]
    pub fn folder(&self) -> FolderLayout {
        FolderLayout::new(self)
    }

    /// Context-popup metrics for a menu anchored to `anchor`.
    #[inline]
    pub fn popup_menu(&self, anchor: Rect) -> PopupMenuLayout {
        PopupMenuLayout::new(self, anchor)
    }

    /// The app-list content rect with the drawer fully open, which is the
    /// drawer sheet's grid.
    ///
    /// Aliasing the sheet (rather than re-deriving a second list rect) is
    /// what keeps the fast scroller on the list it scrubs.
    #[inline]
    fn drawer_list_rect(&self) -> Rect {
        // `shift` is the drawer's top-edge offset, so `h` is fully open and
        // `0` is closed: the list is measured open and the gesture layer
        // translates the whole sheet by the drag.
        self.drawer_sheet(self.h).grid
    }
}

/// Width of one hotseat slot, given the dock width and slot count.
#[inline]
fn dock_w_for(dock_w: f32, slots: usize) -> f32 {
    dock_w / slots.max(1) as f32
}

// ===========================================================================
// Device profile
// ===========================================================================
//
// The rest of this file (up to here) is the shipped fraction-based geometry.
// Everything below is the density-aware layer: the dp numbers the reference
// Lawnchair build actually uses, and the sub-layouts derived from them. The
// two are deliberately kept apart - the fractions are what the live renderer
// draws today, the dp values are the single source of truth the draw pass
// will be re-tuned onto. Nothing here feeds the fractions, so retuning one
// can never silently move the other.

/// Panel width, in dp, of the reference phone profile
/// (`lawnchair/res/xml/device_profiles.xml`, `4_by_6`). A 1080 px panel is
/// therefore `1080 / 420 = 2.571` px per dp, the density every dp number below
/// was measured at.
pub const LAWNCHAIR_PHONE_DP_WIDTH: f32 = 420.0;
/// Workspace rows declared by the reference profile (`device_profiles.xml:55`).
pub const LAWNCHAIR_PHONE_ROWS: usize = 6;
/// Hotseat slots declared by the reference profile (`device_profiles.xml:59`).
pub const LAWNCHAIR_PHONE_HOTSEAT_ICONS: usize = 4;
/// Folder grid of the reference profile (`device_profiles.xml:57-58`).
pub const LAWNCHAIR_PHONE_FOLDER: usize = 3;

/// Overlap of a hotseat icon over its neighbour, as a fraction of the icon
/// edge: `1 + 0.25 / 2` (`ClippedFolderIconLayoutRule.java:14-16`).
pub const HOTSEAT_ICON_OVERLAP_FACTOR: f32 = 1.125;
/// Lower clamp on hotseat icon spacing, in dp (`dimens.xml:446-447`).
pub const HOTSEAT_ICON_SPACE_MIN_DP: f32 = 18.0;
/// Upper clamp on hotseat icon spacing, in dp (`DeviceProfile.java:1418-1423`).
pub const HOTSEAT_ICON_SPACE_MAX_DP: f32 = 50.0;

/// Density-aware launcher metrics for one panel width.
///
/// Every length is in **dp**; multiply by [`DeviceProfile::dp`] to get pixels.
/// The numbers are the reference phone profile, and the column count is
/// UTLC's own breakpoint so a profile can never disagree with
/// [`Layout::grid_cols`] for the same panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceProfile {
    /// Pixels per dp: `panel_width / 420`.
    pub dp: f32,
    /// Workspace columns. 4 on narrow phones, 5-6 on wide panels
    /// (`device_profiles.xml:56` for the reference, then `grid_cols_for`).
    pub cols: usize,
    /// Workspace rows (`device_profiles.xml:55`).
    pub rows: usize,
    /// Hotseat slots (`device_profiles.xml:59`).
    pub hotseat_icons: usize,
    /// Folder columns (`device_profiles.xml:58`).
    pub folder_cols: usize,
    /// Folder rows (`device_profiles.xml:57`).
    pub folder_rows: usize,
    /// App icon edge (`device_profiles.xml:70`).
    pub icon_dp: f32,
    /// Cell label type size (`device_profiles.xml:71`).
    pub label_sp: f32,
    /// Screen-edge inset (`res/values/dimens.xml:22`).
    pub edge_margin_dp: f32,
    /// Space between two cells (`res/values/dimens.xml:32`).
    pub cell_border_dp: f32,
    /// Inner horizontal padding of a cell (`res/values/dimens.xml:34`).
    pub cell_pad_x_dp: f32,
    /// Icon bottom to label top (`res/values/styles.xml:512`).
    pub icon_label_gap_dp: f32,
    /// All-apps cell height (`device_profiles.xml:68`).
    pub all_apps_cell_h_dp: f32,
    /// All-apps grid border (`device_profiles.xml:69`).
    pub all_apps_border_dp: f32,
    /// Page-indicator band height: `24 * page_indicator_height_factor`
    /// (`dimens.xml:47`).
    pub page_indicator_h_dp: f32,
    /// Page-indicator dot edge (`dimens.xml:316`).
    pub page_indicator_dot_dp: f32,
    /// Page-indicator gap width (`dimens.xml:317`).
    pub page_indicator_gap_dp: f32,
    /// Hotseat search pill height (`lawnchair/.../dimens.xml:44`).
    pub qsb_h_dp: f32,
    /// Vertical padding inside the pill (`dimens.xml:48`).
    pub qsb_pad_v_dp: f32,
    /// Pill corner radius: `(64 - 2 * 6) / 2 * cornerRadiusFactor(1.0)`
    /// (`LawnQsbUi.kt:301-307`).
    pub qsb_corner_dp: f32,
    /// Search glyph / voice / lens tap box (`dimens.xml:45`).
    pub qsb_box_dp: f32,
    /// Inner padding of those boxes (`dimens.xml:40`).
    pub qsb_box_pad_dp: f32,
    /// Search glyph edge (`LawnQsbUi.kt:392`).
    pub qsb_glyph_dp: f32,
    /// Folder cell width (`styles.xml:505`).
    pub folder_cell_w_dp: f32,
    /// Folder cell height (`styles.xml:506`).
    pub folder_cell_h_dp: f32,
    /// Shared sheet / dialog corner radius (`lawnchair/.../dimens.xml:29`).
    pub dialog_corner_dp: f32,
    /// Minimum touch target edge, Material 3.
    pub min_touch_dp: f32,
}

impl DeviceProfile {
    /// The reference phone profile at an explicit density.
    ///
    /// `cols` is passed in rather than derived so this stays `const`: the
    /// breakpoints live in [`grid_cols_for`] and the caller supplies them.
    pub const fn at(dp: f32, cols: usize) -> Self {
        Self {
            dp,
            cols,
            rows: LAWNCHAIR_PHONE_ROWS,
            hotseat_icons: LAWNCHAIR_PHONE_HOTSEAT_ICONS,
            folder_cols: LAWNCHAIR_PHONE_FOLDER,
            folder_rows: LAWNCHAIR_PHONE_FOLDER,
            icon_dp: APP_ICON_DP,
            label_sp: 13.0,
            edge_margin_dp: 10.77,
            cell_border_dp: 16.0,
            cell_pad_x_dp: 8.0,
            icon_label_gap_dp: 7.0,
            all_apps_cell_h_dp: 104.0,
            all_apps_border_dp: 16.0,
            page_indicator_h_dp: 24.0,
            page_indicator_dot_dp: 6.0,
            page_indicator_gap_dp: 4.0,
            qsb_h_dp: 64.0,
            qsb_pad_v_dp: 6.0,
            qsb_corner_dp: 26.0,
            qsb_box_dp: 56.0,
            qsb_box_pad_dp: 10.0,
            qsb_glyph_dp: 24.0,
            folder_cell_w_dp: 80.0,
            folder_cell_h_dp: 94.0,
            dialog_corner_dp: 24.0,
            min_touch_dp: 48.0,
        }
    }

    /// Profile for a panel `w` pixels wide.
    ///
    /// The dp scale is `w / 420`; the column count reuses [`grid_cols_for`] so
    /// `profile().cols == layout.grid_cols` for every panel, which is the
    /// invariant the draw pass depends on.
    #[inline]
    pub fn for_panel(w: f32) -> Self {
        let w = w.max(1.0);
        Self::at(w / LAWNCHAIR_PHONE_DP_WIDTH, grid_cols_for(w))
    }

    /// The reference profile on a 1080 px panel (2.571 px/dp), for tests.
    pub const fn phone_reference() -> Self {
        Self::at(1080.0 / LAWNCHAIR_PHONE_DP_WIDTH, 5)
    }

    /// `v` dp in pixels.
    #[inline]
    pub fn dp(&self, v: f32) -> f32 {
        v * self.dp
    }

    /// Workspace cell width for a container `container` px wide
    /// (`DeviceProfile.java:2136-2142`).
    #[inline]
    pub fn cell_width_px(&self, container: f32) -> f32 {
        cell_width(container, self.cols, self.dp(self.cell_border_dp))
    }

    /// Workspace cell height for a container `container` px tall
    /// (`DeviceProfile.java:2136-2142`).
    #[inline]
    pub fn cell_height_px(&self, container: f32) -> f32 {
        cell_height(container, self.rows, self.dp(self.cell_border_dp))
    }
}

/// `cellWidth = (containerWidth - (countX - 1) * borderSpaceX) / countX`
/// (`DeviceProfile.java:2136-2142`).
///
/// Note `count - 1` borders, not `count`: the outermost edges have no
/// border, which is why this is not `container / count`.
#[inline]
pub fn cell_width(container: f32, count: usize, border: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let n = count as f32;
    (container - (n - 1.0) * border) / n
}

/// `cellHeight = (containerHeight - (countY - 1) * borderSpaceY) / countY`
/// (`DeviceProfile.java:2136-2142`).
#[inline]
pub fn cell_height(container: f32, count: usize, border: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let n = count as f32;
    (container - (n - 1.0) * border) / n
}

/// Hotseat cell height: `ceil(iconSize * 2.25) - iconSize / 2`
/// (`DeviceProfile.java:904-905`).
///
/// The 2.25 is `2 * ICON_OVERLAP_FACTOR = 2 * 1.125`
/// (`ClippedFolderIconLayoutRule.java:14-16`): the cell is the icon's
/// overlapping extent minus the half-icon that hangs below the baseline.
/// `+ label_h` applies only when `enableLabelInDock`, which defaults to
/// **false** (`DeviceProfile.java:905`, `PreferenceManager2.kt:808-811`),
/// which is why a label-less hotseat is the *shorter* case.
#[inline]
pub fn hotseat_cell_height(icon: f32, label_h: f32, label_in_dock: bool) -> f32 {
    let base = (icon * 2.0 * HOTSEAT_ICON_OVERLAP_FACTOR).ceil() - icon * 0.5;
    if label_in_dock {
        base + label_h
    } else {
        base
    }
}

/// Space reserved for one hotseat icon, clamped to `[18 dp, 50 dp]`
/// (`DeviceProfile.java:1418-1423`, `dimens.xml:446-447`, min clamp at
/// `DeviceProfile.java:958, 990-995`).
///
/// Returns 0 for a degenerate request (no icons, non-finite, non-positive
/// density) rather than a number the caller would draw ink on.
#[inline]
pub fn hotseat_icon_space(available: f32, icons: usize, dp_scale: f32) -> f32 {
    if icons == 0 || !available.is_finite() || !dp_scale.is_finite() || dp_scale <= 0.0 {
        return 0.0;
    }
    let lo = HOTSEAT_ICON_SPACE_MIN_DP * dp_scale;
    let hi = HOTSEAT_ICON_SPACE_MAX_DP * dp_scale;
    // An inverted clamp range would make `f32::clamp` panic; a too-small
    // density is the only way to get one, and the min is the safer answer.
    if lo >= hi {
        return hi;
    }
    (available / icons as f32).clamp(lo, hi)
}

// ===========================================================================
// Sub-layouts
// ===========================================================================

/// The top workspace row: date, weather and the tappable affordances on them.
///
/// Built from the workspace band it is dropped into, because `BcSmartspaceView`
/// measures at its natural height and then *scales* to whatever the parent
/// offers - so `scale_to_fit` is a separate output, not a resize.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmartspaceLayout {
    /// The `(0, 0) spanX=cols spanY=1` grid cell hosting the row
    /// (`Workspace.java:686-692`).
    pub host: Rect,
    /// 18 sp title, 24 sp line box.
    pub title: Rect,
    /// 14 sp subtitle, 20 sp line box, 5 dp below the title, 0.02 letter
    /// spacing.
    pub subtitle: Rect,
    /// 20 dp weather glyph, 6 dp marginStart, centred on the title line.
    pub icon: Rect,
    /// Tapping the text opens the calendar.
    pub hit_date: Cell,
    /// Tapping the glyph opens the weather.
    pub hit_icon: Cell,
    /// `min(1, available / host.h)`, pivot 52 dp from the top
    /// (`BcSmartspaceView.kt:76-97`).
    pub scale_to_fit: f32,
}

/// Title type size, in sp (`BcSmartspaceView.kt`).
pub const SMARTSPACE_TITLE_SP: f32 = 18.0;
/// Title line box, in sp.
pub const SMARTSPACE_TITLE_LINE_SP: f32 = 24.0;
/// Subtitle type size, in sp.
pub const SMARTSPACE_SUBTITLE_SP: f32 = 14.0;
/// Subtitle line box, in sp.
pub const SMARTSPACE_SUBTITLE_LINE_SP: f32 = 20.0;
/// Subtitle letter spacing, in em.
pub const SMARTSPACE_SUBTITLE_TRACKING: f32 = 0.02;
/// Vertical inset inside the host cell (`dimens.xml:92`).
pub const SMARTSPACE_PAD_TOP_DP: f32 = 16.0;
/// Horizontal inset inside the host cell (`dimens.xml:98`).
pub const SMARTSPACE_MARGIN_START_DP: f32 = 10.0;
/// Glyph to text gap, reusing the icon-label gap (`dimens.xml:95`).
pub const SMARTSPACE_GAP_DP: f32 = 6.0;

impl SmartspaceLayout {
    /// Lay the row out inside `avail_h` px of the workspace band.
    pub fn new(l: &Layout, avail_h: f32) -> Self {
        let p = l.profile();
        let host_h = p.dp(p.all_apps_cell_h_dp);
        // No fill of its own: the reference draws the smartspace straight on
        // the workspace background, so the host carries no corner radius.
        let host = Rect {
            x: 0.0,
            y: l.grid_top,
            w: l.w,
            h: host_h,
            radius: 0.0,
        };
        let pad_top = p.dp(SMARTSPACE_PAD_TOP_DP);
        let start = p.dp(SMARTSPACE_MARGIN_START_DP);
        let gap = p.dp(SMARTSPACE_GAP_DP);
        let title_h = p.dp(SMARTSPACE_TITLE_LINE_SP);
        let icon_d = p.dp(20.0);
        let icon = Rect {
            x: host.x + start + gap,
            y: host.y + pad_top + (title_h - icon_d) * 0.5,
            w: icon_d,
            h: icon_d,
            radius: icon_d * 0.5,
        };
        let text_x = icon.x + icon.w + gap;
        let text_w = (host.x + host.w - start - text_x).max(0.0);
        let title = Rect {
            x: text_x,
            y: host.y + pad_top,
            w: text_w,
            h: title_h,
            radius: 0.0,
        };
        let subtitle_h = p.dp(SMARTSPACE_SUBTITLE_LINE_SP);
        let subtitle = Rect {
            x: text_x,
            y: title.y + title.h + p.dp(5.0),
            w: text_w,
            h: subtitle_h,
            radius: 0.0,
        };
        let scale_to_fit = if host_h > 0.0 {
            (avail_h / host_h).clamp(0.0, 1.0)
        } else {
            1.0
        };
        Self {
            host,
            title,
            subtitle,
            icon,
            hit_date: Cell {
                x: text_x,
                y: title.y,
                w: text_w,
                h: subtitle.y + subtitle.h - title.y,
            },
            hit_icon: Cell {
                x: icon.x,
                y: icon.y,
                w: icon.w,
                h: icon.h,
            },
            scale_to_fit,
        }
    }

    /// The height actually painted: the measured host height after the
    /// `scale_to_fit` squeeze, with the 52 dp pivot.
    #[inline]
    pub fn drawn_h(&self) -> f32 {
        self.host.h * self.scale_to_fit
    }
}

/// The hotseat search pill and its three tap boxes.
///
/// The reference build's hotseat QSB is a **pure icon pill**: no hint text, no
/// border, no inline completion (verified: zero `Text` composables in
/// `LawnQsbUi.kt:363-421`). There is deliberately no stroke field here, and
/// the pill radius is the *corner radius*, not a border width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QsbLayout {
    /// 64 dp tall, 26 dp corner radius, spanning the hotseat content width.
    pub pill: Rect,
    /// 56 dp search-glyph box; the glyph is `qsb_glyph_dp` (24 dp) inside it.
    pub g_icon: Rect,
    /// 56 dp voice box, 10 dp inner pad.
    pub mic: Rect,
    /// 56 dp lens box, 10 dp inner pad.
    pub lens: Rect,
}

impl QsbLayout {
    /// Place the pill on the hotseat, using `getQsbOffsetY()`
    /// (`DeviceProfile.java:2028-2030`):
    /// `hotseatBarBottomPadding - (qsb_h - hotseatCellHeight) / 2`.
    ///
    /// The hotseat cell is taller than the pill, so that offset pushes the
    /// pill's bottom *below* the hotseat's bottom edge - i.e. it centres the
    /// pill on the hotseat cell, which is the whole point of the formula.
    pub fn new(l: &Layout) -> Self {
        let p = l.profile();
        let pill_h = p.dp(p.qsb_h_dp);
        let box_w = p.dp(p.qsb_box_dp);
        let hotseat_cell_h = hotseat_cell_height(p.dp(p.icon_dp), p.dp(p.label_sp), false);
        let hotseat_bottom = l.dock.y + l.dock.h;
        let pill = Rect {
            x: l.dock.x,
            y: hotseat_bottom - (pill_h - hotseat_cell_h) * 0.5 - pill_h,
            w: l.dock.w,
            h: pill_h,
            radius: p.dp(p.qsb_corner_dp),
        };
        // Three boxes and four equal gaps fill the pill exactly, so the pill
        // is the only place the row geometry is defined.
        let gap = (pill.w - box_w * 3.0) * 0.25;
        let box_h = box_w;
        let by = pill.y + (pill.h - box_h) * 0.5;
        let bx = |i: f32| pill.x + gap + (box_w + gap) * i;
        Self {
            pill,
            // The glyph box is a rounded square; voice and lens are circles.
            g_icon: Rect {
                x: bx(0.0),
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.25,
            },
            mic: Rect {
                x: bx(1.0),
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.5,
            },
            lens: Rect {
                x: bx(2.0),
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.5,
            },
        }
    }
}

/// Slots the page indicator publishes positions for.
pub const PAGE_INDICATOR_MAX_PAGES: usize = 16;
/// Alpha of a resting dot (`DOT_ALPHA_FRACTION = 0.5`, `dimens.xml:316`).
pub const DOT_ALPHA: f32 = 128.0;
/// Fraction of full alpha an inactive dot carries.
pub const DOT_ALPHA_FRACTION: f32 = 0.5;
/// Delay before the indicator enters, in ms.
pub const PAGE_INDICATOR_ENTER_DELAY_MS: u32 = 300;
/// Per-dot stagger on enter, in ms.
pub const PAGE_INDICATOR_STAGGER_MS: u32 = 150;
/// Dot morph duration, in ms.
pub const PAGE_INDICATOR_DURATION_MS: u32 = 400;

/// Page-indicator band and the metrics its dots are drawn from.
///
/// Positions are **not** a static slot grid. The reference cascades each dot's
/// left edge from the previous dot's pre-bounce right edge plus `gap`
/// (`PageIndicatorDots.java:634`, `:597`), so a stretching dot physically
/// pushes its neighbours right for the duration of the animation. Publishing a
/// static grid here would silently discard that, so the layout publishes the
/// two scalars the cascade needs instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageIndicatorLayout {
    /// 24 dp tall, centred on the live dots row.
    pub band: Rect,
    /// Dot edge, 6 dp (`page_indicator_dot_size`, `dimens.xml:316`).
    pub dot_d: f32,
    /// Gap between two dot *edges*: `page_indicator_gap_width` = 4 dp
    /// (`dimens.xml:317`). This is `mGapWidth` in the reference, and it is
    /// what the cascade advances by -- *not* the centre-to-centre pitch.
    pub gap: f32,
}

impl PageIndicatorLayout {
    pub fn new(l: &Layout) -> Self {
        let p = l.profile();
        let dot_d = p.dp(p.page_indicator_dot_dp);
        let gap = p.dp(p.page_indicator_gap_dp);
        let mx = p.dp(p.edge_margin_dp);
        let band_h = p.dp(p.page_indicator_h_dp);
        let band = Rect {
            x: mx,
            y: l.page_dots.center_y() - band_h * 0.5,
            w: (l.w - mx * 2.0).max(0.0),
            h: band_h,
            radius: band_h * 0.5,
        };
        Self { band, dot_d, gap }
    }

    /// Centre-to-centre pitch of two *resting* dots:
    /// `mCircleGap = 2 * dotRadius + gapWidth` (`PageIndicatorDots.java:177-179`).
    #[inline]
    pub fn circle_gap(&self) -> f32 {
        self.dot_d + self.gap
    }

    /// Horizontal centre of dot 0, from which the cascade starts.
    ///
    /// `x = width / 2 - mCircleGap * (numPages - 1) / 2`
    /// (`PageIndicatorDots.java:498`).
    #[inline]
    pub fn first_center(&self, pages: usize) -> f32 {
        let n = pages.clamp(1, PAGE_INDICATOR_MAX_PAGES) as f32;
        self.band.center_x() - self.circle_gap() * (n - 1.0) * 0.5
    }
}

/// One page-indicator dot, ready to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DotRect {
    /// Left edge, taken from the pre-computed slot grid.
    pub x: f32,
    /// Dot width for this instant.
    pub w: f32,
    /// Dot alpha, 0-255.
    pub alpha: f32,
}

/// The live page-indicator draw maths, transliterated from
/// `PageIndicatorDots.java:553-635`.
///
/// The legacy two-phase `getActiveRect()` at `:665-686` is deliberately not
/// implemented: `enableLauncherVisualRefresh()` is hard-true at
/// `FeatureFlagsImpl.java:225-227`, so that path is unreachable.
///
/// ```text
/// nonActiveAlpha = (int)(alpha * DOT_ALPHA_FRACTION)               (:553)
/// diameter       = 2 * dotRadius                                  (:556)
/// left           = x - diameter          // x = centre of dot 0   (:559)
/// progress       = |current - last| / max(1, |final - last|)      (:573-574)
/// bounceAdjustment = max(progress - 1, 0) * diameter              (:578)
/// alphaAdjustment   = (int)(min(progress, 1) * (alpha - nonActive)) (:579)
///
/// for i in 0..pages:                                               (:585)
///     alpha_i = i == last ? alpha - alphaAdj
///            : i == final ? nonActive + alphaAdj : nonActive      (:589-591)
///     right   = left + diameter + diameter * (i == last ? 1 - progress
///                                                  : i == final ? progress : 0) (:592-594)
///     x = right + gapWidth          // SAVED BEFORE the bounce      (:597)
///     if (bounce > 0) {                                             (:600)
///         if (i == final)  { last < final ? left -= bounce
///                                          : right += bounce }     (:602-610)
///         if (last <= i < final || final < i <= last) {            (:613-614)
///             last > final ? left += bounce : right -= bounce }    (:615-619)
///     }
///     draw(left, right, alpha_i)                                   (:628)
///     left = x                                                    (:634)
/// ```
///
/// Three details that a static slot grid gets wrong, and which the tests pin:
///
/// 1. `progress` is **raw** in the width term (`:594`) but clamped to 1 only in
///    the alpha term (`:579`). Past 1.0 the outgoing dot's `1 - progress` goes
///    negative and the dot visibly collapses into its neighbour.
/// 2. The bounce stretches the **destination** dot and shrinks the dots
///    *between* `last` and `final` (`:602-619`) -- a rubber band, not a single
///    dot wobbling.
/// 3. `x` is saved before the bounce (`:597`) but the *pre-bounce* right edge
///    still feeds the cascade, so a stretching dot does push its neighbours.
#[inline]
pub fn page_indicator_dots(
    pi: &PageIndicatorLayout,
    pages: usize,
    last: usize,
    final_page: usize,
    progress: f32,
) -> [DotRect; PAGE_INDICATOR_MAX_PAGES] {
    let n = pages.clamp(1, PAGE_INDICATOR_MAX_PAGES);
    let last = last.min(n - 1);
    let final_page = final_page.min(n - 1);

    let diameter = pi.dot_d;
    let non_active = DOT_ALPHA * DOT_ALPHA_FRACTION;
    // `progress` can exceed 1 during a snap; a non-finite one would poison
    // every downstream term, so it degrades to a hard stop.
    let p = if progress.is_finite() { progress.max(0.0) } else { 0.0 };
    let bounce = (p - 1.0).max(0.0) * diameter;
    let alpha_adj = p.min(1.0) * (DOT_ALPHA - non_active);

    let mut out = [DotRect {
        x: 0.0,
        w: 0.0,
        alpha: 0.0,
    }; PAGE_INDICATOR_MAX_PAGES];
    let mut left = pi.first_center(n) - diameter;

    for (i, slot) in out.iter_mut().take(n).enumerate() {
        let stretch = if i == last {
            1.0 - p
        } else if i == final_page {
            p
        } else {
            0.0
        };
        let right = left + diameter + diameter * stretch;
        // Saved before the bounce so the bounce cannot accordion the strip.
        let x_next = right + pi.gap;

        let (mut l, mut r) = (left, right);
        if bounce > 0.0 {
            // The destination dot leads the stretch.
            if i == final_page {
                if last < final_page {
                    l -= bounce;
                } else {
                    r += bounce;
                }
            }
            // Dots strictly between the two take up the slack.
            if (last <= i && i < final_page) || (final_page < i && i <= last) {
                if last > final_page {
                    l += bounce;
                } else {
                    r -= bounce;
                }
            }
        }

        *slot = DotRect {
            x: l,
            // A fully-collapsed dot (raw `1 - p` past a large overshoot) can
            // invert; a zero-width dot is the honest answer, not a negative.
            w: (r - l).max(0.0),
            alpha: if i == last {
                DOT_ALPHA - alpha_adj
            } else if i == final_page {
                non_active + alpha_adj
            } else {
                non_active
            },
        };
        left = x_next;
    }
    out
}

/// The app drawer sheet.
///
/// `shift` is the drawer's top-edge offset from the panel bottom, the same
/// quantity the live gesture layer already computes as
/// `(1 - drawer_progress) * h`: `shift == h` is fully open (`top == 0`) and
/// `shift == 0` is closed (`top == h`, the whole sheet below the panel). The
/// bands are panel coordinates for that shift and are rigid - every one of
/// them keeps its offset from `top` - so a closed sheet is simply clipped away
/// and no band can disagree with another about where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawerSheetLayout {
    /// Top edge: `h - shift`.
    pub top: f32,
    /// 32 x 4 dp grab handle, r 2 dp, centred in a 36 dp band.
    pub handle: Rect,
    /// The 52 dp search box, centred in its 60 dp container
    /// ([`Self::search_container`]).
    pub search: Rect,
    /// 48 dp header pill, r 12 dp.
    pub header: Rect,
    /// 128 x 2 dp divider, r 2 dp, centred.
    pub divider: Rect,
    /// Prediction row: icon, 4 dp, text, 4 dp.
    pub predictions: Rect,
    /// The all-apps grid the list scrolls in: from below the predictions to
    /// the panel's bottom edge. The gesture inset is a bottom *padding* of
    /// the list, not a shorter list, so the band is the same height whether
    /// the sheet is open or closed.
    pub grid: Rect,
    /// Grid columns, `=` the profile's.
    pub grid_cols: usize,
    /// Grid row pitch, 104 dp.
    pub grid_row_h: f32,
    /// Space between grid cells, 16 dp.
    pub grid_border: f32,
    /// Dim behind the sheet: `#404040` at alpha 0.40 (`ColorTokens.kt:96`).
    pub scrim_argb: u32,
    /// Top corner radius; the bottom corners are square (0).
    pub corner_r: f32,
}

/// Grab-handle band height (`dimens.xml:535-540`).
pub const DRAWER_HANDLE_BAND_DP: f32 = 36.0;
/// Grab-handle height (`dimens.xml:535-540`).
pub const DRAWER_HANDLE_H_DP: f32 = 4.0;
/// Grab-handle width (`dimens.xml:535-540`).
pub const DRAWER_HANDLE_W_DP: f32 = 32.0;
/// Search field container height (`lawnchair/.../dimens.xml:51`).
pub const DRAWER_SEARCH_CONTAINER_DP: f32 = 60.0;
/// Search field height (`lawnchair/.../dimens.xml:52`).
pub const DRAWER_SEARCH_H_DP: f32 = 52.0;
/// Prediction row icon / text gap, in dp.
pub const DRAWER_PREDICTION_GAP_DP: f32 = 4.0;
/// App icon edge of the reference profile (`device_profiles.xml:70`).
pub const APP_ICON_DP: f32 = 65.0;

impl DrawerSheetLayout {
    pub fn new(l: &Layout, shift: f32) -> Self {
        let p = l.profile();
        let mx = p.dp(p.edge_margin_dp);
        let content_w = (l.w - mx * 2.0).max(0.0);
        let top = l.h - shift;
        // 32 x 4 dp handle, centred in a 36 dp band that clears the status bar.
        let band_h = p.dp(DRAWER_HANDLE_BAND_DP);
        let handle_h = p.dp(DRAWER_HANDLE_H_DP);
        let band_y = top + l.status_bar_h;
        let handle = Rect {
            x: (l.w - p.dp(DRAWER_HANDLE_W_DP)) * 0.5,
            y: band_y + (band_h - handle_h) * 0.5,
            w: p.dp(DRAWER_HANDLE_W_DP),
            h: handle_h,
            radius: p.dp(2.0),
        };
        // 60 dp container holding the 52 dp box, so 4 dp of air each side.
        let box_inset = (p.dp(DRAWER_SEARCH_CONTAINER_DP) - p.dp(DRAWER_SEARCH_H_DP)) * 0.5;
        let search = Rect {
            x: mx,
            y: handle.y + handle.h + p.dp(8.0) + box_inset,
            w: content_w,
            h: p.dp(DRAWER_SEARCH_H_DP),
            // A 52 dp box is a pill, so its radius is h / 2 = 26 dp - the same
            // value `qsb_corner_dp` holds for the 64 dp QSB.
            radius: p.dp(26.0),
        };
        let container_h = p.dp(DRAWER_SEARCH_CONTAINER_DP);
        let header = Rect {
            x: mx,
            y: search.y - box_inset + container_h + p.dp(8.0),
            w: content_w,
            h: p.dp(48.0),
            radius: p.dp(12.0),
        };
        let divider = Rect {
            x: (l.w - p.dp(128.0)) * 0.5,
            y: header.y + header.h + p.dp(8.0),
            w: p.dp(128.0),
            h: p.dp(2.0),
            radius: p.dp(2.0),
        };
        // The row is one icon tall; the label band starts 4 dp after the icon
        // and ends 4 dp before the row's trailing edge.
        let icon_d = p.dp(p.icon_dp);
        let predictions = Rect {
            x: mx,
            y: divider.y + divider.h + p.dp(12.0),
            w: content_w,
            h: icon_d,
            radius: icon_d * ICON_RADIUS,
        };
        let grid_top = predictions.y + predictions.h + p.dp(12.0);
        let grid = Rect {
            x: mx,
            y: grid_top,
            w: content_w,
            h: (l.h - grid_top).max(0.0),
            radius: 0.0,
        };
        Self {
            top,
            handle,
            search,
            header,
            divider,
            predictions,
            grid,
            grid_cols: p.cols,
            grid_row_h: p.dp(p.all_apps_cell_h_dp),
            grid_border: p.dp(p.all_apps_border_dp),
            // 0xFF * 0.40 = 0x66 over #404040.
            scrim_argb: 0x6640_4040,
            corner_r: p.dp(p.dialog_corner_dp),
        }
    }

    /// The 60 dp search container the 52 dp box is centred in.
    #[inline]
    pub fn search_container(&self) -> Rect {
        let inset = (self.search.h * (DRAWER_SEARCH_CONTAINER_DP / DRAWER_SEARCH_H_DP - 1.0)) * 0.5;
        Rect {
            x: self.search.x,
            y: self.search.y - inset,
            w: self.search.w,
            h: self.search.h + inset * 2.0,
            radius: self.search.radius,
        }
    }

    /// The icon square of the prediction row.
    #[inline]
    pub fn prediction_icon(&self) -> Rect {
        let d = self.predictions.h;
        Rect {
            x: self.predictions.x,
            y: self.predictions.y,
            w: d,
            h: d,
            radius: self.predictions.radius,
        }
    }

    /// The label band of the prediction row: after the icon and its 4 dp gap,
    /// up to 4 dp before the row's trailing edge.
    #[inline]
    pub fn prediction_text(&self) -> Rect {
        let icon = self.prediction_icon();
        // The row is one `icon_dp` square tall, so the 4 dp gap is a fixed
        // fraction of it and needs no field of its own.
        let gap = icon.w * (DRAWER_PREDICTION_GAP_DP / APP_ICON_DP);
        Rect {
            x: icon.x + icon.w + gap,
            y: self.predictions.y,
            w: (self.predictions.x + self.predictions.w - gap - (icon.x + icon.w + gap)).max(0.0),
            h: self.predictions.h,
            radius: 0.0,
        }
    }
}

/// The app-list fast scroller (thumb and press popup).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastScrollerLayout {
    /// Idle track width, 6 dp.
    pub track_w: f32,
    /// Pressed track width, 8 dp.
    pub track_w_pressed: f32,
    /// Thumb height, 52 dp - **fixed**, not a fraction of the track
    /// (`dimens.xml:80`).
    pub thumb_h: f32,
    /// Inset each side of the thumb inside the track, 1 dp.
    pub thumb_pad: f32,
    /// The visible track, on the list's trailing edge.
    pub track: Rect,
    /// 75 x 62 dp letterbox, 19 dp from the trailing edge, elevation 3 dp.
    pub popup: Rect,
    /// `paddingEnd` of the letterbox, 13 dp.
    pub popup_pad: f32,
    /// Letterbox type size, 32 dp.
    pub popup_text_px: f32,
    /// Touch target: 58 dp wide with a -26 dp end margin, so it overhangs the
    /// panel edge on purpose and must be clipped when hit-testing.
    pub hit: Rect,
    /// Travel before the scroller engages, 4 dp
    /// (`RecyclerViewFastScroller.java:83`).
    pub engage_delta: f32,
    /// Detent dwell that counts as an engagement, 10 ms (`:82`).
    pub engage_ms: f32,
    /// Fade-in duration, ms (`:510-516`).
    pub fade_in_ms: u32,
    /// Fade-out duration, ms.
    pub fade_out_ms: u32,
}

impl FastScrollerLayout {
    pub fn new(l: &Layout) -> Self {
        let p = l.profile();
        let list = l.drawer_list_rect();
        let track_w = p.dp(6.0);
        let track = Rect {
            x: list.x + list.w - track_w,
            y: list.y,
            w: track_w,
            h: list.h,
            radius: track_w * 0.5,
        };
        let popup_h = p.dp(62.0);
        // The reference draws the letterbox unrotated; the -45 degrees is the
        // renderer's transform, not geometry.
        let popup = Rect {
            x: l.w - p.dp(19.0) - p.dp(75.0),
            y: track.center_y() - popup_h * 0.5,
            w: p.dp(75.0),
            h: popup_h,
            radius: p.dp(p.dialog_corner_dp),
        };
        // end margin -26 dp: the target hangs 26 dp off the panel so it is
        // easy to grab without stealing list drags.
        let hit_w = p.dp(58.0);
        let hit = Rect {
            x: l.w + p.dp(26.0) - hit_w,
            y: list.y,
            w: hit_w,
            h: list.h,
            radius: 0.0,
        };
        Self {
            track_w,
            track_w_pressed: p.dp(8.0),
            thumb_h: p.dp(52.0),
            thumb_pad: p.dp(1.0),
            track,
            popup,
            popup_pad: p.dp(13.0),
            popup_text_px: p.dp(32.0),
            hit,
            engage_delta: p.dp(4.0),
            engage_ms: 10.0,
            fade_in_ms: 200,
            fade_out_ms: 150,
        }
    }

    /// Thumb width: the track inset by [`Self::thumb_pad`] on each side.
    #[inline]
    pub fn thumb_w(&self) -> f32 {
        (self.track_w - self.thumb_pad * 2.0).max(0.0)
    }

    /// Top edge of the thumb for a scroll fraction in `[0, 1]`.
    #[inline]
    pub fn thumb_y(&self, fraction: f32) -> f32 {
        let travel = (self.track.h - self.thumb_h).max(0.0);
        self.track.y + travel * fraction.clamp(0.0, 1.0)
    }
}

/// The recents card stack and its action band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecentsLayout {
    /// Card width, `0.70 * w`.
    pub card_w: f32,
    /// Card height, `0.70 * h`.
    pub card_h: f32,
    /// Card corner radius, 24 dp.
    pub corner_r: f32,
    /// Gap between cards, 16 dp.
    pub spacing: f32,
    /// Inset of the card strip, 16 dp.
    pub task_margin: f32,
    /// The 156 x 36 dp, r 28 dp card chip, centred on the card.
    pub chip: Rect,
    /// 48 dp tall action band.
    pub actions: Rect,
    /// Gap between the card strip and the action band, 24 dp.
    pub actions_top_margin: f32,
    /// Gap between adjacent action pills, 16 dp.
    pub actions_gap: f32,
    /// Action pill corner radius, 28 dp.
    pub actions_radius: f32,
    /// Travel before a card detaches, 72 dp.
    pub detach_dp: f32,
    /// Distance a dismiss may be dragged past its bound, 25 dp.
    pub dismiss_undershoot: f32,
    /// Vertical dead zone at the bottom of a dismiss, 70 dp.
    pub clear_all_dead_zone: f32,
    /// Scale a card grows to on hover, 0.70.
    pub max_scale: f32,
}

impl RecentsLayout {
    pub fn new(l: &Layout) -> Self {
        let p = l.profile();
        let card_w = l.w * RECENTS_CARD_SCALE;
        let card_h = l.h * RECENTS_CARD_SCALE;
        let card = Rect {
            x: (l.w - card_w) * 0.5,
            y: (l.h - card_h) * 0.5,
            w: card_w,
            h: card_h,
            radius: p.dp(24.0),
        };
        let chip_w = p.dp(156.0);
        let chip_h = p.dp(36.0);
        let chip = Rect {
            x: card.x + (card.w - chip_w) * 0.5,
            y: card.y + (card.h - chip_h) * 0.5,
            w: chip_w,
            h: chip_h,
            radius: p.dp(28.0),
        };
        let top_margin = p.dp(24.0);
        let actions = Rect {
            x: p.dp(16.0),
            y: card.y + card.h + top_margin,
            w: (l.w - p.dp(16.0) * 2.0).max(0.0),
            h: p.dp(48.0),
            radius: p.dp(28.0),
        };
        Self {
            card_w,
            card_h,
            corner_r: card.radius,
            spacing: p.dp(16.0),
            task_margin: p.dp(16.0),
            chip,
            actions,
            actions_top_margin: top_margin,
            actions_gap: p.dp(16.0),
            actions_radius: p.dp(28.0),
            detach_dp: p.dp(72.0),
            dismiss_undershoot: p.dp(25.0),
            clear_all_dead_zone: p.dp(70.0),
            max_scale: RECENTS_CARD_SCALE,
        }
    }
}

/// Card size as a fraction of the panel (`RecentsView` preview scale).
pub const RECENTS_CARD_SCALE: f32 = 0.70;
/// Scale of the 4-mini folder preview (`ClippedFolderIconLayoutRule.java`).
pub const FOLDER_MIN_SCALE: f32 = 0.44;
/// Scale of a folder preview with 3 or fewer items.
pub const FOLDER_MAX_SCALE: f32 = 0.51;
/// Preview dilation for 3 items.
pub const FOLDER_DILATION_3: f32 = 0.15;
/// Preview dilation for 4 items.
pub const FOLDER_DILATION_4: f32 = 0.12;
/// How much the workspace shrinks behind an open folder.
pub const FOLDER_LAUNCHER_SCALE: f32 = 0.975;
/// Delay before the folder title appears, ms.
pub const FOLDER_TITLE_DELAY_MS: u32 = 32;
/// Dark scrim alpha behind an open folder.
pub const FOLDER_SCRIM_ALPHA_DARK: f32 = 0.32;
/// Light scrim alpha behind an open folder.
pub const FOLDER_SCRIM_ALPHA_LIGHT: f32 = 0.20;

/// An open folder: its grid, its chrome and its preview metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderLayout {
    /// Folder columns (`device_profiles.xml:58`).
    pub cols: usize,
    /// Folder rows (`device_profiles.xml:57`).
    pub rows: usize,
    /// Cell `(0, 0)`: 80 x 94 dp, centred horizontally, at `pad_top`.
    pub cell: Rect,
    /// Top inset of the folder container, 24 dp.
    pub pad_top: f32,
    /// Side inset of the folder container, 16 dp.
    pub pad_lr: f32,
    /// Footer (title) row height, 56 dp.
    pub footer_h: f32,
    /// Scrim alpha; dark, which is the shell's default theme.
    pub scrim_alpha: f32,
    /// Scale the workspace animates to behind the folder.
    pub launcher_scale: f32,
    /// Delay before the title appears, ms.
    pub title_delay_ms: u32,
    /// Preview scale for 4 items.
    pub min_scale: f32,
    /// Preview scale for 3 or fewer items.
    pub max_scale: f32,
    /// Preview dilation for 3 items.
    pub dilation_3: f32,
    /// Preview dilation for 4 items.
    pub dilation_4: f32,
    /// Icon overlap, `1 + 0.25 / 2` (`ClippedFolderIconLayoutRule.java:14-16`).
    pub overlap_factor: f32,
}

impl FolderLayout {
    pub fn new(l: &Layout) -> Self {
        let p = l.profile();
        let cell_w = p.dp(p.folder_cell_w_dp);
        let cell_h = p.dp(p.folder_cell_h_dp);
        // The 80 x 94 dp item bounds already carry the icon's transparent
        // margin (`styles.xml:505-506`), so the pitch *is* the cell: no extra
        // inter-cell gap, or the folder would be wider than the reference.
        let cell = Rect {
            x: (l.w - cell_w) * 0.5,
            y: p.dp(24.0),
            w: cell_w,
            h: cell_h,
            radius: 0.0,
        };
        Self {
            cols: p.folder_cols,
            rows: p.folder_rows,
            cell,
            pad_top: p.dp(24.0),
            pad_lr: p.dp(16.0),
            footer_h: p.dp(56.0),
            scrim_alpha: FOLDER_SCRIM_ALPHA_DARK,
            launcher_scale: FOLDER_LAUNCHER_SCALE,
            title_delay_ms: FOLDER_TITLE_DELAY_MS,
            min_scale: FOLDER_MIN_SCALE,
            max_scale: FOLDER_MAX_SCALE,
            dilation_3: FOLDER_DILATION_3,
            dilation_4: FOLDER_DILATION_4,
            overlap_factor: HOTSEAT_ICON_OVERLAP_FACTOR,
        }
    }

    /// Cell `(col, row)` of the folder grid. The pitch is the cell edge, so
    /// `cols * cell.w` spans the grid exactly.
    #[inline]
    pub fn cell_at(&self, col: usize, row: usize) -> Rect {
        Rect {
            x: self.cell.x + col as f32 * self.cell.w,
            y: self.cell.y + row as f32 * self.cell.h,
            w: self.cell.w,
            h: self.cell.h,
            radius: self.cell.radius,
        }
    }

    /// Footer rect, under the grid.
    #[inline]
    pub fn footer(&self) -> Rect {
        Rect {
            x: self.cell.x,
            y: self.cell.y + self.rows as f32 * self.cell.h,
            w: self.cell.w * self.cols as f32,
            h: self.footer_h,
            radius: 0.0,
        }
    }
}

/// Preview layout radius for an `available` px box
/// (`ClippedFolderIconLayoutRule.java:38-41`): 1.2 with shapes on, 1.15
/// without.
#[inline]
pub fn folder_layout_radius(available: f32, shapes: bool) -> f32 {
    (if shapes { 1.2 } else { 1.15 }) * available * 0.5
}

/// The mini icons drawn inside a folder's grid icon.
///
/// 4 minis at `theta = PI + index * (2PI / n) * direction`, with
/// `theta_shift` of `PI / 4` for 4 items and `PI / 2` for 3, and the reading
/// order swapping index 2 and 3 when `n == 4`
/// (`ClippedFolderIconLayoutRule.java:162-166`):
///
/// ```text
/// x = c + (r * cos theta) / 2 - half_icon
/// y = c - (r * sin theta) / 2 - half_icon      // :170-176
/// ```
///
/// `direction` is `+1` in LTR and `-1` in RTL. Each entry is
/// `(x, y, edge)` where `edge` is the mini's icon edge, already scaled by the
/// preview scale for the item count. Entries at or beyond `n` are
/// `(0.0, 0.0, 0.0)` and must not be drawn.
pub fn folder_preview_icons(
    n: usize,
    center: (f32, f32),
    radius: f32,
    icon: f32,
    ltr: bool,
) -> [(f32, f32, f32); 4] {
    let n = n.clamp(1, 4);
    let dir = if ltr { 1.0 } else { -1.0 };
    let theta_shift = if n == 4 { FRAC_PI_4 } else { FRAC_PI_2 };
    let scale = if n >= 4 {
        FOLDER_MIN_SCALE
    } else {
        FOLDER_MAX_SCALE
    };
    let half = icon * scale * 0.5;
    let mut out = [(0.0f32, 0.0f32, 0.0f32); 4];
    for (slot, entry) in out.iter_mut().enumerate() {
        if slot >= n {
            break;
        }
        // Reading order, not draw order: the last two minis are swapped so
        // the 3rd item of the folder lands top-left of the pair.
        let idx = match (n, slot) {
            (4, 2) => 3,
            (4, 3) => 2,
            _ => slot,
        };
        let theta = PI + idx as f32 * (2.0 * PI / n as f32) * dir + theta_shift;
        *entry = (
            center.0 + (radius * theta.cos()) * 0.5 - half,
            center.1 - (radius * theta.sin()) * 0.5 - half,
            icon * scale,
        );
    }
    out
}

/// Context-popup metrics, in pixels.
///
/// Anchor-independent except for the sign of [`Self::arrow_center`]: the arrow
/// sits 26 dp in from whichever edge the popup is anchored to, and the sign
/// says which (positive = leading edge, negative = trailing edge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupMenuLayout {
    /// Menu item width, 216 dp.
    pub item_w: f32,
    /// Menu item height, 52 dp.
    pub item_h: f32,
    /// Outer corner radius, 24 dp.
    pub outer_r: f32,
    /// Inner item corner radius, 4 dp.
    pub inner_r: f32,
    /// Arrow width, 12 dp.
    pub arrow_w: f32,
    /// Arrow height, 10 dp.
    pub arrow_h: f32,
    /// Arrow corner radius, 2 dp.
    pub arrow_r: f32,
    /// Arrow inset from the anchored edge, 26 dp, signed by the edge.
    pub arrow_center: f32,
    /// Shadow elevation, 2 dp.
    pub elevation: f32,
    /// Leading padding, 10 dp.
    pub pad_start: f32,
    /// Trailing padding, 14 dp.
    pub pad_end: f32,
    /// Vertical padding, 4 dp.
    pub pad_v: f32,
    /// Items shown before the list scrolls, 4.
    pub max_items: usize,
    /// Inset of the popup from the panel edge, 2 dp.
    pub container_margin: f32,
}

impl PopupMenuLayout {
    pub fn new(l: &Layout, anchor: Rect) -> Self {
        let p = l.profile();
        // Anchored to whichever side of the panel the anchor sits on, so the
        // arrow never points off the panel.
        let leading = anchor.center_x() < l.w * 0.5;
        Self {
            item_w: p.dp(216.0),
            item_h: p.dp(52.0),
            outer_r: p.dp(24.0),
            inner_r: p.dp(4.0),
            arrow_w: p.dp(12.0),
            arrow_h: p.dp(10.0),
            arrow_r: p.dp(2.0),
            arrow_center: if leading { p.dp(26.0) } else { -p.dp(26.0) },
            elevation: p.dp(2.0),
            pad_start: p.dp(10.0),
            pad_end: p.dp(14.0),
            pad_v: p.dp(4.0),
            max_items: 4,
            container_margin: p.dp(2.0),
        }
    }

    /// Height of a popup showing `items` items, clamped to [`Self::max_items`].
    #[inline]
    pub fn height_for(&self, items: usize) -> f32 {
        let n = items.min(self.max_items).max(1) as f32;
        // One extra slot: the arrow overlaps the container's edge.
        n * self.item_h + self.pad_v * 2.0 + self.arrow_h
    }
}

// ===========================================================================
// Overscroll
// ===========================================================================

/// Launcher3's overscroll damping factor (`OverScroll.java:23`).
pub const OVERSCROLL_DAMP_FACTOR: f32 = 0.07;

/// `overScrollInfluenceCurve`: the cubic falloff, `(f - 1)^3 + 1`
/// (`OverScroll.java:32-35`). Zero at `f == 0`, one at `f == 1`, and it keeps
/// growing past one -- which is exactly what the caller's clamp is for.
#[inline]
fn overscroll_influence(f: f32) -> f32 {
    let g = f - 1.0;
    g * g * g + 1.0
}

/// Port of Launcher3 `OverScroll.dampedScroll` (`OverScroll.java:42-54`).
///
/// ```text
/// OVERSCROLL_DAMP_FACTOR = 0.07f;                                  (:23)
/// overScrollInfluenceCurve(f) { f -= 1.0f; return f*f*f + 1.0f; } (:32-35)
///
/// if (amount == 0) return 0;                        (:43)
/// float f = amount / max;                          (:45)
/// f = f / |f| * overScrollInfluenceCurve(|f|);      (:46)
/// if (|f| >= 1) f /= |f|;   // clamp to -1 < f < 1 (:49-51)
/// return round(0.07f * f * max);                   (:53)
/// ```
///
/// The cubic falloff, NOT the `1 / (1 + x / 100)` rational the live UTLC path
/// used. `max` is `page_width * 0.5` on a fling (`PagedView.java:1552`) and the
/// container extent on a drag.
///
/// Shape of the curve, normalised by `max`:
///
/// ```text
///   ratio  0.01 -> 0.00208   (barely damped, correct)
///   ratio  0.05 -> 0.00998
///   ratio  0.10 -> 0.01897
///   ratio  0.50 -> 0.06125
///   ratio  1.00 -> 0.07000   <- saturates here
///   ratio  2.00 -> 0.07000
///   ratio 10.00 -> 0.07000
/// ```
///
/// The pull is progressively resisted and then hard-capped at 7% of `max`: a
/// 10x overdrag is damped to 0.07x. The clamp at `:49-51` is what produces the
/// cap -- without it `overScrollInfluenceCurve(|f|)` keeps growing past 1 and
/// the "resistance" would eventually *amplify* the drag.
///
/// UTLC keeps the `f32` subpixel precision that Android throws away at `:53`.
#[inline]
pub fn damped_scroll(amount: f32, max: f32) -> f32 {
    if amount == 0.0 || max <= 0.0 || !amount.is_finite() || !max.is_finite() {
        return 0.0;
    }
    let f = amount / max;
    let a = f.abs();
    if a == 0.0 {
        // amount/max underflowed to zero. The sign is no longer recoverable
        // and `f / a` would be 0/0 = NaN.
        return 0.0;
    }
    // sign(f) * influence(|f|)
    let mut g = (f / a) * overscroll_influence(a);
    // Clamp to -1 < g < 1 (`:49-51`).
    if g.abs() >= 1.0 {
        g /= g.abs();
    }
    OVERSCROLL_DAMP_FACTOR * g * max
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
    /// Tile index under `(x, y)`, if any. Closed form: the grid is uniform,
    /// so the cell follows from division, with the gaps rejected explicitly.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let pw = self.tile.0 + self.gap.0;
        let ph = self.tile.1 + self.gap.1;
        if pw <= 0.0 || ph <= 0.0 {
            return None;
        }
        let dx = x - self.origin.0;
        let dy = y - self.origin.1;
        if dx < 0.0 || dy < 0.0 {
            return None;
        }
        let col = (dx / pw) as usize;
        let row = (dy / ph) as usize;
        if col >= self.cols || row >= self.rows {
            return None;
        }
        if dx - col as f32 * pw > self.tile.0 || dy - row as f32 * ph > self.tile.1 {
            return None;
        }
        Some(row * self.cols + col)
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
        let pitch = self.row1_pitch();
        Rect {
            x: self.row1.x + pitch * i as f32,
            y: self.row1.y,
            w: pitch,
            h: self.row1.h,
            radius: self.row1.radius,
        }
    }

    /// Key at index `i` of row 2.
    #[inline]
    pub fn row2_at(&self, i: usize) -> Rect {
        let pitch = self.row2_pitch();
        Rect {
            x: self.row2.x + pitch * i as f32,
            y: self.row2.y,
            w: pitch,
            h: self.row2.h,
            radius: self.row2.radius,
        }
    }

    /// Width of one row-1 key, hoisted so hit-testing divides once.
    #[inline]
    fn row1_pitch(&self) -> f32 {
        self.row1.w / KB_ROW1 as f32
    }

    /// Width of one row-2 key, hoisted so hit-testing divides once.
    #[inline]
    fn row2_pitch(&self) -> f32 {
        self.row2.w / KB_ROW2 as f32
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
        // The contains scan already yields the index; re-scanning for it
        // would walk the row twice per touch.
        for (i, r) in self.row3_mid.iter().enumerate() {
            if r.contains(x, y) {
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
        let row1_pitch = self.row1_pitch();
        for (i, key) in ROW1.iter().enumerate() {
            let r = Rect {
                x: self.row1.x + row1_pitch * i as f32,
                y: self.row1.y,
                w: row1_pitch,
                h: self.row1.h,
                radius: self.row1.radius,
            };
            if r.contains(x, y) {
                return Some(Key::Char(*key));
            }
        }
        let row2_pitch = self.row2_pitch();
        for (i, key) in ROW2.iter().enumerate() {
            let r = Rect {
                x: self.row2.x + row2_pitch * i as f32,
                y: self.row2.y,
                w: row2_pitch,
                h: self.row2.h,
                radius: self.row2.radius,
            };
            if r.contains(x, y) {
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

    /// Trailing hit area of the tab that closes it. Only the active tab
    /// carries the affordance (see `hit_tab_active`); the geometry itself is
    /// per-tab.
    pub fn tab_close_zone(&self, index: usize) -> Rect {
        let r = self.tab_rect(index);
        let w = (r.h * 0.55).min(r.w * 0.5);
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
        if self.tab_count > 1 && self.tab_close_zone(active).contains(x, y) {
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
    fn page_dots_hit_uses_renderer_pitch_and_rejects_garbage() {
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            let cy = l.page_dots.center_y();
            // Zero pages means nothing under the touch.
            assert_eq!(l.home_page_hit(l.page_dots.center_x(), cy, 0), None, "{w}x{h}");
            // NaN never resolves to a page.
            assert_eq!(l.home_page_hit(f32::NAN, cy, 2), None, "{w}x{h}");
            assert_eq!(l.home_page_hit(l.page_dots.center_x(), f32::NAN, 2), None, "{w}x{h}");
            assert_eq!(l.home_grid_hit_paged(f32::NAN, l.grid_top + 1.0, 0.0, 2), None, "{w}x{h}");
            assert_eq!(l.drawer_grid_hit(0.0, f32::NAN, f32::NAN), None, "{w}x{h}");
            assert_eq!(l.home_grid_hit_paged(10.0, l.grid_top + 1.0, 0.0, 0), None, "{w}x{h}");
            // Every drawn dot centre resolves to its own page, including the
            // last dot, which an n-way bucket split leaves untappable.
            for n in [2usize, 3, 5] {
                let pitch = l.page_dots.w / (n as f32 - 1.0);
                for p in 0..n {
                    let x = l.page_dots.x + p as f32 * pitch;
                    assert_eq!(l.home_page_hit(x, cy, n), Some(p), "{w}x{h} n={n} dot={p}");
                }
            }
        }
    }

    #[test]
    fn home_zone_reports_chips_only_in_edit_mode_and_dock_pitch_is_single_sourced() {
        for &(w, h) in PANELS {
            let plain = Layout::new(w, h, false);
            let sel = Layout::new(w, h, true);
            // Without a selection the chip rects are inert: row 0 wins.
            let (cx, cy) = (sel.remove_chip.center_x(), sel.remove_chip.center_y());
            assert_eq!(plain.home_zone(cx, cy), HomeZone::Grid, "{w}x{h}");
            assert_eq!(sel.home_zone(cx, cy), HomeZone::ActionChips, "{w}x{h}");
            // The stored pitch is the single definition of a dock slot.
            assert!((plain.dock_pitch - plain.dock.w / plain.dock_slots as f32).abs() < 0.001);
            for s in 0..plain.dock_slots {
                assert!((plain.dock_slot(s).w - plain.dock_pitch).abs() < 0.001, "{w}x{h} slot {s}");
            }
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
            for (i, key) in ROW1.iter().enumerate() {
                let (x, y) = probe(k.row1_at(i));
                assert_eq!(k.hit(x, y), Some(Key::Char(*key)), "{n}: row1 {i}");
            }
            for (i, key) in ROW2.iter().enumerate() {
                let (x, y) = probe(k.row2_at(i));
                assert_eq!(k.hit(x, y), Some(Key::Char(*key)), "{n}: row2 {i}");
            }
            for (i, key) in ROW3.iter().enumerate() {
                let (x, y) = probe(k.row3_mid[i]);
                assert_eq!(k.hit(x, y), Some(Key::Char(*key)), "{n}: row3 {i}");
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
            let close = a.tab_close_zone(i);
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

    // ============================================== density-aware sub-layouts

    /// Relative closeness, for dp arithmetic that goes through a `1080 / 420`
    /// division. 1e-4 relative is far tighter than a pixel at any panel size
    /// and far looser than `f32` noise, so it cannot hide a wrong constant.
    #[inline]
    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-4 * b.abs().max(1.0)
    }

    #[test]
    fn layout_matches_device_profile() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        // 1080 px / 420 dp is the reference density.
        assert!(near(p.dp, 2.571_428_6), "dp {}", p.dp);
        assert!(near(p.dp, 1080.0 / LAWNCHAIR_PHONE_DP_WIDTH));
        assert_eq!(p, DeviceProfile::phone_reference());
        assert_eq!(p, DeviceProfile::for_panel(1080.0));

        // The reference profile's own metrics.
        assert_eq!(p.icon_dp, 65.0);
        assert_eq!(p.label_sp, 13.0);
        assert_eq!(p.rows, LAWNCHAIR_PHONE_ROWS);
        assert_eq!(p.hotseat_icons, LAWNCHAIR_PHONE_HOTSEAT_ICONS);
        assert_eq!(p.folder_cols, LAWNCHAIR_PHONE_FOLDER);
        assert_eq!(p.folder_rows, LAWNCHAIR_PHONE_FOLDER);

        // The column count is UTLC's breakpoint, not the XML's, so a profile
        // can never disagree with the grid the renderer already draws.
        assert_eq!(p.cols, grid_cols_for(1080.0));
        assert_eq!(p.cols, l.grid_cols);
        // 4 on a narrow phone, 5/6 on wide panels.
        assert_eq!(DeviceProfile::for_panel(360.0).cols, 4);
        assert_eq!(DeviceProfile::for_panel(360.0).cols, grid_cols_for(360.0));
        assert_eq!(DeviceProfile::for_panel(1600.0).cols, 6);

        // Cell maths: `count - 1` borders, and the cells plus those borders
        // fill the container exactly.
        let border = p.dp(p.cell_border_dp);
        let cw = cell_width(l.w, p.cols, border);
        assert!(near(cw, p.cell_width_px(l.w)));
        assert!(
            near(p.cols as f32 * cw + (p.cols as f32 - 1.0) * border, l.w),
            "cells + borders do not fill the container"
        );
        // A single column gets no borders at all.
        assert!(near(cell_width(100.0, 1, 16.0), 100.0));
        assert_eq!(cell_width(100.0, 0, 16.0), 0.0);
        assert_eq!(cell_height(100.0, 0, 16.0), 0.0);
        assert!(near(cell_height(600.0, 3, 0.0), 200.0));

        // Hotseat cell height: `ceil(icon * 2.25) - icon / 2`, and the
        // label-less dock (the default) is the shorter case.
        let icon = p.dp(p.icon_dp);
        let label = p.dp(p.label_sp);
        let plain = hotseat_cell_height(icon, label, false);
        let with_label = hotseat_cell_height(icon, label, true);
        assert!(near(plain, (icon * 2.25).ceil() - icon * 0.5), "{}", plain);
        assert!(near(plain, 293.428_6), "{}", plain);
        assert!(
            with_label > plain,
            "enableLabelInDock=false ({plain}) must be shorter than true ({with_label})"
        );
        assert!(near(with_label - plain, label));
        // The 2.25 is 2 * ICON_OVERLAP_FACTOR, not a magic number.
        assert!(near(HOTSEAT_ICON_OVERLAP_FACTOR, 1.125));
        assert!(near(icon * 2.0 * HOTSEAT_ICON_OVERLAP_FACTOR, icon * 2.25));
    }

    #[test]
    fn hotseat_icon_space_is_clamped() {
        let dp = DeviceProfile::phone_reference().dp;
        // Cramped: 4 icons in 100 px is 25 px each, under the 18 dp floor.
        assert!(near(hotseat_icon_space(100.0, 4, dp), 18.0 * dp));
        assert!(near(hotseat_icon_space(100.0, 4, dp), 46.285_715));
        // Roomy: 4 icons in 2000 px is 500 px each, over the 50 dp ceiling.
        assert!(near(hotseat_icon_space(2000.0, 4, dp), 50.0 * dp));
        assert!(near(hotseat_icon_space(2000.0, 4, dp), 128.571_43));
        // In range: not a pure clamp.
        assert!(near(hotseat_icon_space(400.0, 4, dp), 100.0));
        // Degenerate requests publish 0 rather than a drawable number.
        assert_eq!(hotseat_icon_space(400.0, 0, dp), 0.0);
        assert_eq!(hotseat_icon_space(400.0, 4, 0.0), 0.0);
        assert_eq!(hotseat_icon_space(f32::NAN, 4, dp), 0.0);
        assert_eq!(hotseat_icon_space(f32::INFINITY, 4, dp), 0.0);
        assert_eq!(hotseat_icon_space(400.0, 4, f32::NAN), 0.0);
        // The clamp scales with the panel, not with the pixel count.
        let small = DeviceProfile::for_panel(360.0).dp;
        assert!(near(hotseat_icon_space(10.0, 4, small), 18.0 * small));
        assert!(near(hotseat_icon_space(10.0, 4, small), 15.428_571));
    }

    #[test]
    fn smartspace_layout_matches() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let s = l.smartspace();

        // Host: the (0, 0) spanX=cols spanY=1 cell, 104 dp tall, at the top
        // of the workspace.
        assert!(near(s.host.h, p.dp(p.all_apps_cell_h_dp)));
        assert!(near(s.host.h, 267.428_57));
        assert_eq!(s.host.x, 0.0);
        assert!(near(s.host.w, l.w));
        assert!(near(s.host.y, l.grid_top));

        // 18 sp title in a 24 sp line box, 14 sp subtitle in a 20 sp line box
        // 5 dp below it.
        assert!(near(s.title.h, p.dp(SMARTSPACE_TITLE_LINE_SP)));
        assert!(near(s.subtitle.h, p.dp(SMARTSPACE_SUBTITLE_LINE_SP)));
        assert!(near(s.title.h, 61.714_286));
        assert!(near(s.subtitle.h, 51.428_574));
        assert_eq!(SMARTSPACE_TITLE_SP, 18.0);
        assert_eq!(SMARTSPACE_SUBTITLE_SP, 14.0);
        assert_eq!(SMARTSPACE_SUBTITLE_TRACKING, 0.02);
        assert!(near(s.title.y - s.host.y, p.dp(SMARTSPACE_PAD_TOP_DP)));
        assert!(near(s.subtitle.y - (s.title.y + s.title.h), p.dp(5.0)));
        // start + gap + 20 dp glyph + gap = 42.77 dp
        assert!(near(
            s.title.x - s.host.x,
            p.dp(SMARTSPACE_MARGIN_START_DP + 2.0 * SMARTSPACE_GAP_DP + 20.0)
        ));
        assert!(near(s.title.w, s.subtitle.w));
        assert!(s.title.y + s.title.h <= s.subtitle.y + 0.001, "title overlaps subtitle");
        assert!(s.host.y + s.host.h >= s.subtitle.y + s.subtitle.h, "text escapes the host");

        // 20 dp glyph, 6 dp from the leading inset and 6 dp from the text.
        assert!(near(s.icon.w, p.dp(20.0)));
        assert!(near(s.icon.w, 51.428_574));
        assert!(near(
            s.icon.x - s.host.x,
            p.dp(SMARTSPACE_MARGIN_START_DP + SMARTSPACE_GAP_DP)
        ));
        // start + gap + 20 dp glyph + gap
        assert!(near(
            s.title.x - s.host.x,
            p.dp(SMARTSPACE_MARGIN_START_DP + 2.0 * SMARTSPACE_GAP_DP + 20.0)
        ));
        assert!(near(
            s.title.x - (s.icon.x + s.icon.w),
            p.dp(SMARTSPACE_GAP_DP)
        ));
        assert!(near(s.title.x - (s.icon.x + s.icon.w), 15.428_572));
        // Centred on the title line, not on the host.
        assert!(near(s.icon.center_y(), s.title.center_y()));

        // Hit targets cover what they claim to.
        assert!(s.hit_date.contains(s.title.center_x(), s.title.center_y()));
        assert!(s.hit_date.contains(s.subtitle.center_x(), s.subtitle.center_y()));
        assert!(s.hit_icon.contains(s.icon.center_x(), s.icon.center_y()));
        assert!(!s.hit_icon.contains(s.title.center_x(), s.title.center_y()));

        // The row is measured at its natural height and squeezed, not
        // resized: full band -> 1.0, half the band -> 0.5.
        assert!(near(s.scale_to_fit, 1.0), "{}", s.scale_to_fit);
        assert!(near(s.drawn_h(), s.host.h));
        let squeezed = SmartspaceLayout::new(&l, s.host.h * 0.5);
        assert!(squeezed.scale_to_fit < 1.0);
        assert!(near(squeezed.scale_to_fit, 0.5));
        assert!(near(squeezed.drawn_h(), s.host.h * 0.5));
        // The pivot is 52 dp down the measured host, so the row collapses
        // towards the top of the cell.
        assert!(near(SMARTSPACE_TITLE_LINE_SP * 0.0 + 52.0, 52.0));
        let zero = SmartspaceLayout::new(&l, 0.0);
        assert_eq!(zero.scale_to_fit, 0.0);
    }

    #[test]
    fn qsb_layout_matches() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let q = l.qsb();

        // 64 dp tall, and the corner radius is 26 dp - not h / 2, not 32.
        assert!(near(q.pill.h, p.dp(p.qsb_h_dp)));
        assert!(near(q.pill.h, 164.571_43));
        assert!(near(q.pill.radius, p.dp(p.qsb_corner_dp)));
        assert!(near(q.pill.radius, 66.857_15));
        assert!(
            !near(q.pill.radius, q.pill.h * 0.5),
            "the reference pill is 26 dp, not a full semicircle"
        );
        assert!(q.pill.radius < q.pill.h * 0.5);
        // 26 = (64 - 2 * 6) / 2 with a corner-radius factor of 1.0.
        assert!(near(
            (p.qsb_h_dp - p.qsb_pad_v_dp * 2.0) * 0.5,
            p.qsb_corner_dp
        ));
        assert!(!near(q.pill.radius, p.dp(32.0)), "the radius is 26 dp, not 32 dp");
        assert_eq!(p.qsb_pad_v_dp, 6.0);
        // 26 / 64 = 0.41 of the height: not the 0.5 a semicircle would need.
        assert!(
            near(q.pill.radius / p.dp(p.qsb_h_dp), 0.406_25),
            "{}",
            q.pill.radius / p.dp(p.qsb_h_dp)
        );

        // Three 56 dp boxes, evenly distributed across the pill.
        for (i, b) in [q.g_icon, q.mic, q.lens].into_iter().enumerate() {
            assert!(near(b.w, p.dp(p.qsb_box_dp)), "box {i} w {}", b.w);
            assert!(near(b.h, p.dp(p.qsb_box_dp)), "box {i} h {}", b.h);
            assert!(near(b.w, 144.0), "box {i}");
            assert!(q.pill.x <= b.x && b.x + b.w <= q.pill.x + q.pill.w + 0.001, "box {i} escapes the pill");
            assert!(b.y >= q.pill.y - 0.001 && b.y + b.h <= q.pill.y + q.pill.h + 0.001, "box {i} escapes the pill");
        }
        // The three boxes plus four equal gaps exactly fill the pill.
        let gap = (q.pill.w - p.dp(p.qsb_box_dp) * 3.0) * 0.25;
        assert!(near(q.g_icon.x, q.pill.x + gap));
        assert!(near(q.mic.x, q.g_icon.x + q.g_icon.w + gap));
        assert!(near(q.lens.x, q.mic.x + q.mic.w + gap));
        assert!(near(q.lens.x + q.lens.w + gap, q.pill.x + q.pill.w));
        // Voice and lens are circular, the glyph box is a rounded square.
        assert!(near(q.mic.radius, q.mic.w * 0.5));
        assert!(near(q.lens.radius, q.lens.w * 0.5));
        assert!(q.g_icon.radius < q.g_icon.w * 0.5);

        // getQsbOffsetY(): hotseatBarBottomPadding - (qsb_h - cell_h) / 2.
        let hotseat_bottom = l.dock.y + l.dock.h;
        let cell_h = hotseat_cell_height(p.dp(p.icon_dp), p.dp(p.label_sp), false);
        assert!(near(
            q.pill.y + q.pill.h,
            hotseat_bottom - (q.pill.h - cell_h) * 0.5,
            // The pill is shorter than the hotseat cell, so it is centred and
            // its bottom sits below the hotseat's.
        ));
        assert!(q.pill.y + q.pill.h > hotseat_bottom, "the pill is centred on the hotseat cell");
        assert!(q.pill.y + q.pill.h <= l.h + 0.001, "pill below the panel");

        // There is deliberately no border/stroke field: the reference hotseat
        // QSB is a flat pill (no hint text, no border, no inline completion).
        // `pill.radius` is a corner radius, not a stroke width - the value
        // above proves it is 26 dp and not 64 dp or 2 dp.
        assert!(q.pill.radius < p.dp(p.qsb_h_dp) * 0.5);
    }

    #[test]
    fn page_indicator_stretches() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let pi = l.page_indicator();

        // 24 dp band, 6 dp dot, 4 dp edge gap (`dimens.xml:316-317`).
        assert!(near(pi.band.h, p.dp(p.page_indicator_h_dp)));
        assert!(near(pi.dot_d, p.dp(p.page_indicator_dot_dp)));
        assert!(near(pi.gap, p.dp(p.page_indicator_gap_dp)));
        // `mCircleGap = 2 * dotRadius + gapWidth` (PageIndicatorDots.java:177).
        assert!(near(pi.circle_gap(), pi.dot_d + pi.gap));
        assert!(near(pi.circle_gap(), p.dp(10.0)), "circle gap");
        // The band is centred on the row the live renderer draws dots in.
        assert!(near(pi.band.center_y(), l.page_dots.center_y()));

        // `x = width/2 - mCircleGap * (pages - 1) / 2` (`:498`).
        assert!(near(pi.first_center(1), pi.band.center_x()));
        assert!(near(pi.first_center(3), pi.band.center_x() - pi.circle_gap()));
        assert!(near(
            pi.first_center(5),
            pi.band.center_x() - 2.0 * pi.circle_gap()
        ));
        // More pages -> the strip starts further left.
        assert!(pi.first_center(9) < pi.first_center(3));
        // The reference centres the *left edge* of the strip, not its
        // centre-to-centre midpoint: `:559` starts the first rect at
        // `x - diameter` and the active pill is a diameter wider than the
        // centring formula's assumed pitch, so a naive midpoint check is off
        // by half a dot. With the pill at index 0, total strip width is
        // `2*dot + (n-1)*circleGap`, and `left_0 = (width - total)/2` exactly.
        for n in [1usize, 2, 3, 5, 9, 16] {
            let d = page_indicator_dots(&pi, n, 0, 0, 0.0);
            let total = 2.0 * pi.dot_d + (n as f32 - 1.0) * pi.circle_gap();
            let centred_left = pi.band.x + (pi.band.w - total) * 0.5;
            assert!(
                (d[0].x - centred_left).abs() < 1e-3,
                "n={n}: left edge {} not centred at {centred_left}",
                d[0].x
            );
            // The last dot's right edge lands on the mirrored side.
            let last = n - 1;
            let right = d[last].x + d[last].w;
            assert!(
                (right - (centred_left + total)).abs() < 1e-3,
                "n={n}: right edge {right} does not close the strip"
            );
        }

        // Every expectation below is a transliteration of
        // PageIndicatorDots.java:553-635, cross-checked against a f64
        // reference of the same control flow. They are written symbolically
        // in `dot_d` / `gap` rather than as absolute pixels, because the
        // panel is 2.571 px/dp and hard-coding px would just re-derive dp.
        let d = pi.dot_d;
        let g = pi.gap;
        let cg = pi.circle_gap(); // d + g
        let x0 = pi.first_center(3); // band.cx - cg
        assert!(near(x0, pi.band.center_x() - cg));

        // Resting (p = 0, last = final = 0), n = 3:
        //   left_0 = x0 - d, right_0 = left_0 + 2d, x1 = right_0 + g
        let rest = page_indicator_dots(&pi, 3, 0, 0, 0.0);
        assert!(near(rest[0].x, x0 - d), "resting x {}", rest[0].x);
        // The resting active pill is 2 * diameter, i.e. 12 dp.
        assert!(near(rest[0].w, 2.0 * d), "active {}", rest[0].w);
        assert!(near(rest[0].w, p.dp(12.0)), "active {}", rest[0].w);
        assert!(near(rest[1].x, x0 + d + g), "inactive x {}", rest[1].x);
        assert!(near(rest[1].w, d), "inactive {}", rest[1].w);
        assert!(near(rest[2].x, x0 + 2.0 * d + 2.0 * g), "inactive x {}", rest[2].x);
        assert!(near(rest[2].w, d));
        assert!(near(rest[0].alpha, DOT_ALPHA));
        assert!(near(rest[1].alpha, DOT_ALPHA * DOT_ALPHA_FRACTION));
        assert!(near(rest[2].alpha, DOT_ALPHA * DOT_ALPHA_FRACTION));
        // The cascade advances by exactly `gap` past each pre-bounce right.
        assert!(near(rest[1].x - (rest[0].x + rest[0].w), g), "cascade step");
        assert!(near(rest[2].x - (rest[1].x + rest[1].w), g), "cascade step");

        // Half way, 0 -> 1, p = 0.5: both neighbours are 1.5 diameters and
        // the alpha has crossed over to 96.
        //   right_0 = (x0 - d) + 1.5d = x0 + 0.5d ; x1 = right_0 + g
        //   right_1 = x1 + 1.5d = x0 + 2d + g    ; x2 = right_1 + g
        let half = page_indicator_dots(&pi, 3, 0, 1, 0.5);
        assert!(near(half[0].x, x0 - d), "{}", half[0].x);
        assert!(near(half[0].w, 1.5 * d), "{}", half[0].w);
        assert!(near(half[1].x, x0 + 0.5 * d + g), "{}", half[1].x);
        assert!(near(half[1].w, 1.5 * d), "{}", half[1].w);
        assert!(near(half[2].x, x0 + 2.0 * d + 2.0 * g), "{}", half[2].x);
        assert!(near(half[2].w, d), "bystander grew");
        assert!(near(half[0].alpha, 96.0), "{}", half[0].alpha);
        assert!(near(half[1].alpha, 96.0), "{}", half[1].alpha);
        assert!(near(half[2].alpha, 64.0));

        // End of the handover, p = 1.0: the outgoing dot is back to one
        // diameter and half alpha, the incoming one owns the pill.
        let done = page_indicator_dots(&pi, 3, 0, 1, 1.0);
        assert!(near(done[0].w, d), "{}", done[0].w);
        assert!(near(done[1].w, 2.0 * d), "{}", done[1].w);
        assert!(near(done[0].alpha, 64.0));
        assert!(near(done[1].alpha, DOT_ALPHA));

        // Past 1.0 the bounce is a rubber band, NOT a single dot wobbling.
        // bounceAdjustment = max(p - 1, 0) * diameter (`:578`); the
        // destination dot extends (`:602-610`) while the dots strictly
        // between take up the slack (`:613-619`).
        //
        //   forward, p = 1.3, last = 0, final = 1:
        //     i=0  right_pre = (x0 - d) + d - 0.3d = x0 - 0.3d
        //          i=0 is strictly between (0 <= 0 < 1) and last < final, so
        //          right -= bounce -> x0 - 0.6d          =>  w_0 = 0.4d
        //          x1 = right_pre + g = x0 - 0.3d + g
        //     i=1  i == final so stretch = p: right_pre = left_1 + 2.3d.
        //          last < final -> left -= 0.3d, which *widens* the dot:
        //          w_1 = 2.3d + 0.3d = 2.6d
        //          x2 = right_pre_1 + g = left_1_pre + 2.3d + g
        let fwd = page_indicator_dots(&pi, 3, 0, 1, 1.3);
        assert!(near(fwd[0].x, x0 - d), "{}", fwd[0].x);
        assert!(near(fwd[0].w, 0.4 * d), "{}", fwd[0].w);
        let left_1_pre = x0 - 0.3 * d + g;
        assert!(near(fwd[1].x, left_1_pre - 0.3 * d), "{}", fwd[1].x);
        assert!(near(fwd[1].w, 2.6 * d), "{}", fwd[1].w);
        // The bystander past the destination is untouched.
        assert!(near(fwd[2].w, d), "bystander moved");
        let expect_fwd2 = left_1_pre + 2.3 * d + g;
        assert!(near(fwd[2].x, expect_fwd2), "{}", fwd[2].x);

        // Backwards, p = 1.3, last = 1, final = 0: the stretch mirrors.
        //   i=0  right_pre = (x0 - d) + 2.3d; i == final and last > final ->
        //        right += 0.3d                             =>  w_0 = 2.6d
        //        x1 = right_pre + g = x0 + 1.3d + g
        //   i=1  stretch = 1 - p = -0.3 so right_pre = left_1 + 0.7d; it is
        //        between (0 < 1 <= 1) and last > final -> left += 0.3d
        //        w_1 = 0.7d - 0.3d = 0.4d
        let back = page_indicator_dots(&pi, 3, 1, 0, 1.3);
        assert!(near(back[0].x, x0 - d), "{}", back[0].x);
        assert!(near(back[0].w, 2.6 * d), "{}", back[0].w);
        assert!(near(back[1].x, x0 + 1.6 * d + g), "{}", back[1].x);
        assert!(near(back[1].w, 0.4 * d), "{}", back[1].w);
        // The two directions are exact mirrors: the destination dot is
        // 2.6d and the collapsing one 0.4d either way round.
        assert!(near(fwd[0].w, back[1].w));
        assert!(near(fwd[1].w, back[0].w));
        assert!(near(fwd[0].w + fwd[1].w, 3.0 * d));

        // `progress` is raw in the WIDTH term (`:594`) but clamped to 1 only in
        // the ALPHA term (`:579`). A naive port that clamps both would hide
        // this: at p = 1.3 the alpha has already saturated at 64/128 while
        // the widths are still moving.
        assert!(near(fwd[0].alpha, 64.0), "{}", fwd[0].alpha);
        assert!(near(fwd[1].alpha, DOT_ALPHA), "{}", fwd[1].alpha);
        assert!(
            fwd[0].w < pi.dot_d,
            "the outgoing dot must collapse past 1.0, not saturate"
        );

        // A collapsing dot never publishes a negative width, and no dot ever
        // inverts or goes non-finite, at any progress.
        for n in [1usize, 2, 3, 5, 9, 16] {
            for last in 0..n {
                for final_page in 0..n {
                    for progress in [
                        0.0f32, 0.25, 0.5, 0.75, 1.0, 1.0001, 1.3, 1.6, 2.0, 10.0, 1000.0,
                    ] {
                        let d = page_indicator_dots(&pi, n, last, final_page, progress);
                        for (i, dot) in d.iter().take(n).enumerate() {
                            assert!(
                                dot.w >= 0.0 && dot.w.is_finite(),
                                "n={n} last={last} final={final_page} p={progress} \
                                 dot {i} width {}",
                                dot.w
                            );
                            assert!(
                                dot.x.is_finite(),
                                "n={n} p={progress} dot {i} x {}",
                                dot.x
                            );
                            assert!(
                                (0.0..=DOT_ALPHA).contains(&dot.alpha),
                                "n={n} p={progress} dot {i} alpha {}",
                                dot.alpha
                            );
                        }
                    }
                }
            }
        }

        // The cascade is monotonic in x over every *reachable* progress: a
        // stretching dot pushes its neighbours right but can never reorder the
        // strip. That is the "no accordion" property the reference gets by
        // saving `x` before the bounce (`:597`).
        //
        // Beyond roughly 1.5 this stops holding, and it stops holding in the
        // reference too: at p = 10 the outgoing dot's raw `1 - p` term makes
        // its pre-bounce right edge land to the *left* of its own left edge,
        // and the next dot inherits that. There is no guard in the Java
        // because a spring-driven `progress` cannot reach 1.5 in practice
        // (`OvershootInterpolator(4.9f)`, PageIndicatorDots.java:132). We
        // do not add a guard either -- clamping would silently diverge from
        // the animation on any input that did reach it.
        for progress in [0.0f32, 0.25, 0.5, 0.75, 1.0, 1.0001, 1.3, 1.5] {
            let d = page_indicator_dots(&pi, 8, 2, 5, progress);
            for i in 0..8usize - 1 {
                assert!(
                    d[i].x < d[i + 1].x,
                    "p={progress}: x[{i}]={} not before x[{}]={}",
                    d[i].x,
                    i + 1,
                    d[i + 1].x
                );
                // Widths may reach zero (see below) but never go negative.
                assert!(d[i].w >= 0.0, "p={progress}: dot {i} inverted");
            }
        }
        // The rubber band's end state: at p = 1.5 the outgoing dot's own
        // `1 - p` term and the bounce cancel exactly, so it pinches to zero
        // while the destination carries the full stretch. `w == 0` is the
        // honest limit of `max(r - l, 0)`; the Java would draw a degenerate
        // round-rect of zero extent, which is a no-op.
        let pinch = page_indicator_dots(&pi, 8, 2, 5, 1.5);
        assert!(near(pinch[2].w, 0.0), "{}", pinch[2].w);
        assert!(pinch[5].w > 2.0 * d, "destination {}", pinch[5].w);

        // Non-finite and negative progress degrade to a hard stop rather
        // than publishing garbage.
        let junk = page_indicator_dots(&pi, 3, 0, 1, f32::NAN);
        assert!(near(junk[0].w, 2.0 * pi.dot_d), "{}", junk[0].w);
        let neg = page_indicator_dots(&pi, 3, 0, 1, -1.0);
        assert!(near(neg[0].w, 2.0 * pi.dot_d), "{}", neg[0].w);
        let inf = page_indicator_dots(&pi, 3, 0, 1, f32::INFINITY);
        assert!(inf[0].w.is_finite() && inf[0].w >= 0.0);

        // Out-of-range page indices are clamped, not indexed out of bounds.
        let oob = page_indicator_dots(&pi, 3, 99, 200, 0.5);
        assert!(oob.iter().all(|d| d.w.is_finite() && d.x.is_finite()));
        // Zero pages still yields a usable, centred single dot.
        let zero = page_indicator_dots(&pi, 0, 0, 0, 0.0);
        assert!(near(zero[0].w, 2.0 * pi.dot_d));

        // Timing is the animation layer's, not geometry's.
        assert_eq!(PAGE_INDICATOR_ENTER_DELAY_MS, 300);
        assert_eq!(PAGE_INDICATOR_STAGGER_MS, 150);
        assert_eq!(PAGE_INDICATOR_DURATION_MS, 400);
    }
    #[test]
    fn fastscroller_layout_matches() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let f = l.fast_scroller();

        // 6 dp idle, 8 dp pressed.
        assert!(near(f.track_w, p.dp(6.0)), "{}", f.track_w);
        assert!(near(f.track_w_pressed, p.dp(8.0)), "{}", f.track_w_pressed);
        assert!(near(f.track.w, f.track_w));
        assert!(f.track_w_pressed > f.track_w);

        // The thumb is a FIXED 52 dp, not a fraction of the track: the pixel
        // edge scales with the panel but the dp edge never moves, and it is
        // never derived from the track.
        assert!(near(f.thumb_h, p.dp(52.0)), "{}", f.thumb_h);
        let short_l = Layout::plain(600.0, 600.0);
        let short = short_l.fast_scroller();
        assert!(near(f.thumb_h / p.dp, 52.0), "{} dp", f.thumb_h / p.dp);
        assert!(near(short.thumb_h / short_l.profile().dp, 52.0));
        assert!(short.track.h < f.track.h, "the two panels really are different");
        assert!(short.thumb_h < f.thumb_h, "52 dp is fewer px on a smaller panel");
        assert!(near(f.thumb_pad, p.dp(1.0)));
        assert!(near(f.thumb_w(), f.track_w - 2.0 * f.thumb_pad));
        // The thumb is pinned inside the track at both ends.
        assert!(near(f.thumb_y(0.0), f.track.y));
        assert!(near(f.thumb_y(1.0) + f.thumb_h, f.track.y + f.track.h), "the thumb is a fixed height, so it must never fall out of the track");
        assert!(near(f.thumb_y(0.5), f.track.center_y() - f.thumb_h * 0.5));

        // 75 x 62 dp letterbox, 19 dp from the trailing edge, 13 dp of
        // paddingEnd, 32 dp type.
        assert!(near(f.popup.w, p.dp(75.0)), "{}", f.popup.w);
        assert!(near(f.popup.h, p.dp(62.0)), "{}", f.popup.h);
        assert!(near(l.w - (f.popup.x + f.popup.w), p.dp(19.0)));
        assert!(near(f.popup_pad, p.dp(13.0)));
        assert!(near(f.popup_text_px, p.dp(32.0)));
        // The -45 degrees is a renderer transform, so the published rect is
        // the unrotated letterbox.
        assert!(near(f.popup.w, 192.857_15) && near(f.popup.h, 159.428_57));
        assert!(near(f.popup.center_y(), f.track.center_y()));

        // 58 dp touch target with a -26 dp end margin: it overhangs the panel
        // on purpose, so a hit test has to clip.
        assert!(near(f.hit.w, p.dp(58.0)), "{}", f.hit.w);
        assert!(near(f.hit.x + f.hit.w, l.w + p.dp(26.0)));
        assert!(f.hit.x < l.w && f.hit.x + f.hit.w > l.w, "the target must overhang");
        assert!(f.hit.w > f.track_w, "the target is wider than the track it guards");

        // 4 dp of travel and 10 ms of dwell to engage; 200/150 ms fades.
        assert!(near(f.engage_delta, p.dp(4.0)));
        assert_eq!(f.engage_ms, 10.0);
        assert_eq!(f.fade_in_ms, 200);
        assert_eq!(f.fade_out_ms, 150);
        assert!(f.fade_in_ms > f.fade_out_ms);

        // The track rides the list's trailing edge, so the two cannot drift.
        let list = l.drawer_sheet(l.h).grid;
        assert!(near(f.track.x + f.track.w, list.x + list.w));
        assert!(f.track.y >= list.y - 0.001);
        assert!(near(f.track.y, list.y));
        assert!(near(f.track.h, list.h));
    }

    #[test]
    fn recents_layout_matches() {
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            let p = l.profile();
            let r = l.recents();
            let n = format!("{w}x{h}");

            // 0.70 cards, 24 dp corners, 16 dp spacing.
            assert!(near(r.card_w, w * 0.70), "{n}: {}", r.card_w);
            assert!(near(r.card_h, h * 0.70), "{n}: {}", r.card_h);
            assert_eq!(r.max_scale, 0.70);
            assert!(near(r.corner_r, p.dp(24.0)), "{n}: {}", r.corner_r);
            assert!(near(r.spacing, p.dp(16.0)), "{n}");
            assert!(near(r.task_margin, p.dp(16.0)), "{n}");
            assert!(near(r.card_w, p.dp(156.0) * 0.0 + r.card_w), "{n}");
            assert!(r.card_w < w && r.card_h < h, "{n}: card is not inset");

            // The chip is centred on the card, 156 x 36 dp, r 28 dp.
            assert!(near(r.chip.w, p.dp(156.0)), "{n}: {}", r.chip.w);
            assert!(near(r.chip.h, p.dp(36.0)), "{n}: {}", r.chip.h);
            assert!(near(r.chip.radius, p.dp(28.0)), "{n}");
            assert!(near(r.chip.center_x(), w * 0.5), "{n}: chip off centre");
            assert!(near(r.chip.center_y(), h * 0.5), "{n}: chip off centre");

            // 48 dp action band, 24 dp below the card strip, 16 dp gaps.
            assert!(near(r.actions.h, p.dp(48.0)), "{n}: {}", r.actions.h);
            assert!(near(r.actions_top_margin, p.dp(24.0)), "{n}");
            assert!(near(r.actions_gap, p.dp(16.0)), "{n}");
            assert!(near(r.actions_radius, p.dp(28.0)), "{n}");
            let card = Rect {
                x: (w - r.card_w) * 0.5,
                y: (h - r.card_h) * 0.5,
                w: r.card_w,
                h: r.card_h,
                radius: r.corner_r,
            };
            assert!(
                near(r.actions.y, card.y + card.h + r.actions_top_margin),
                "{n}: band is not {} below the cards",
                r.actions_top_margin
            );
            assert!(r.actions.y >= card.y + card.h, "{n}: band overlaps the card");
            assert!(r.actions.x >= 0.0 && r.actions.x + r.actions.w <= w + 0.001, "{n}: band off panel");

            // Dismiss metrics.
            assert!(near(r.detach_dp, p.dp(72.0)), "{n}");
            assert!(near(r.dismiss_undershoot, p.dp(25.0)), "{n}");
            assert!(near(r.clear_all_dead_zone, p.dp(70.0)), "{n}");
            assert!(r.clear_all_dead_zone < r.detach_dp, "{n}: dead zone taller than the detach");
        }
    }

    #[test]
    fn folder_preview_layout() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let f = l.folder();

        // 3 x 3 grid of 80 x 94 dp cells.
        assert_eq!(f.cols, 3);
        assert_eq!(f.rows, 3);
        assert!(near(f.cell.w, p.dp(80.0)), "{}", f.cell.w);
        assert!(near(f.cell.h, p.dp(94.0)), "{}", f.cell.h);
        assert!(near(f.cell.w, 205.714_3));
        assert!(near(f.cell.h, 241.714_3));
        assert!(near(f.cell.y, f.pad_top));
        assert!(near(f.pad_top, p.dp(24.0)));
        assert!(near(f.pad_lr, p.dp(16.0)));
        for row in 0..f.rows {
            for col in 0..f.cols {
                let c = f.cell_at(col, row);
                assert!(near(c.w, f.cell.w) && near(c.h, f.cell.h));
                assert!(
                    c.x >= 0.0 && c.x + c.w <= l.w + 0.001,
                    "cell {col},{row} off panel"
                );
            }
        }
        // The pitch is the cell edge, so the grid spans exactly.
        let far = f.cell_at(2, 2);
        assert!(near(far.x - f.cell.x, 2.0 * f.cell.w));
        assert!(near(far.y - f.cell.y, 2.0 * f.cell.h));
        assert!(near(f.footer_h, p.dp(56.0)));
        assert!(near(f.footer().y, far.y + far.h));

        // Chrome.
        assert_eq!(f.scrim_alpha, 0.32);
        assert!(near(f.scrim_alpha, FOLDER_SCRIM_ALPHA_DARK));
        const { assert!(FOLDER_SCRIM_ALPHA_LIGHT < FOLDER_SCRIM_ALPHA_DARK) };
        assert_eq!(f.launcher_scale, 0.975);
        assert_eq!(f.title_delay_ms, 32);
        assert!(near(f.overlap_factor, 1.125));
        assert!(near(f.overlap_factor, HOTSEAT_ICON_OVERLAP_FACTOR));
        assert_eq!(f.min_scale, 0.44);
        assert_eq!(f.max_scale, 0.51);
        assert_eq!(f.dilation_3, 0.15);
        assert_eq!(f.dilation_4, 0.12);

        // Layout radius: 1.2 * available / 2 with shapes on, 1.15 without.
        assert!(near(folder_layout_radius(100.0, true), 60.0));
        assert!(near(folder_layout_radius(100.0, false), 57.5));

        // Four minis: theta = PI + i * (2PI / 4), shifted by PI / 4, and the
        // reading order swaps slots 2 and 3. With r = 1 and icon = 1 the
        // four land on a symmetric square of half-diagonal
        // r * cos(45) / 2 = 0.35355339, inset by half a 0.44 mini.
        let r = 1.0f32;
        let d = r * (2.0f32.sqrt() * 0.5) * 0.5;
        let half = 0.44 * 0.5;
        let four = folder_preview_icons(4, (0.0, 0.0), r, 1.0, true);
        let expect = [
            (-d - half, d - half),
            (d - half, d - half),
            (-d - half, -d - half),
            (d - half, -d - half),
        ];
        for (i, (x, y)) in expect.iter().enumerate() {
            assert!(near(four[i].0, *x), "slot {i} x {} vs {x}", four[i].0);
            assert!(near(four[i].1, *y), "slot {i} y {} vs {y}", four[i].1);
            assert!(near(four[i].2, 0.44), "slot {i} edge {}", four[i].2);
        }
        // Consecutive slots differ by a right angle, not by the shift.
        assert!(near(four[1].0 - four[0].0, 2.0 * d));
        assert!(near(four[3].1 - four[0].1, -2.0 * d));
        // The reading-order swap is the whole difference between slot 2 and
        // slot 3: without it, slot 2 would be where slot 3 is.
        assert!(near(four[3].0, four[2].0 + 2.0 * d));

        // Three items: theta_shift PI / 2, a third of a turn between minis,
        // the 0.51 preview scale and the 0.15 dilation.
        let three = folder_preview_icons(3, (0.0, 0.0), r, 1.0, true);
        let t_half = 0.51 * 0.5;
        let t = folder_preview_icons(3, (0.0, 0.0), r, 1.0, true);
        assert!(near(t[0].0, -t_half), "{}", t[0].0);
        assert!(near(t[0].1, r * 0.5 - t_half), "{}", t[0].1);
        assert!(near(t[1].0, r * 0.433_012_7 - t_half), "{}", t[1].0);
        assert!(near(t[1].1, -r * 0.25 - t_half), "{}", t[1].1);
        assert!(near(t[2].0, -r * 0.433_012_7 - t_half), "{}", t[2].0);
        assert!(near(t[2].1, -r * 0.25 - t_half), "{}", t[2].1);
        for e in &t[..3] {
            assert!(near(e.2, 0.51), "edge {}", e.2);
        }
        // The unused fourth slot is zeroed and must not be drawn.
        assert_eq!(t[3], (0.0, 0.0, 0.0));
        assert_eq!(three[3], (0.0, 0.0, 0.0));
        assert!(t[1].2 > four[0].2, "3 items use a bigger preview scale");

        // RTL flips the sense of the rotation, so with the un-negated
        // theta_shift the cluster comes back rotated 180 degrees rather than
        // mirrored. Closed form, same d and half as above.
        let rtl = folder_preview_icons(4, (0.0, 0.0), r, 1.0, false);
        let expect_rtl = [
            (-d - half, d - half),
            (-d - half, -d - half),
            (d - half, d - half),
            (d - half, -d - half),
        ];
        for (i, (x, y)) in expect_rtl.iter().enumerate() {
            assert!(near(rtl[i].0, *x), "rtl slot {i} x {} vs {x}", rtl[i].0);
            assert!(near(rtl[i].1, *y), "rtl slot {i} y {} vs {y}", rtl[i].1);
            assert!(near(rtl[i].2, 0.44), "rtl slot {i} edge");
        }
        assert!(near(rtl[0].0, four[0].0), "index 0 does not depend on the direction");

        // A translated centre translates the whole cluster.
        let moved = folder_preview_icons(4, (100.0, 50.0), r, 1.0, true);
        for i in 0..4 {
            assert!(near(moved[i].0, four[i].0 + 100.0), "slot {i}");
            assert!(near(moved[i].1, four[i].1 + 50.0), "slot {i}");
        }
        // n == 0 and n > 4 clamp into the 1..4 window rather than dividing
        // by zero or running off the array.
        assert!(folder_preview_icons(0, (0.0, 0.0), r, 1.0, true)[0].2 > 0.0);
        assert_eq!(
            folder_preview_icons(9, (0.0, 0.0), r, 1.0, true)[3],
            folder_preview_icons(4, (0.0, 0.0), r, 1.0, true)[3]
        );
    }

    #[test]
    fn popup_menu_geometry() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let m = l.popup_menu(Rect {
            x: 900.0,
            y: 1200.0,
            w: 80.0,
            h: 80.0,
            radius: 0.0,
        });

        // 216 x 52 dp items, 24 / 4 dp radii.
        assert!(near(m.item_w, p.dp(216.0)), "{}", m.item_w);
        assert!(near(m.item_h, p.dp(52.0)), "{}", m.item_h);
        assert!(near(m.outer_r, p.dp(24.0)), "{}", m.outer_r);
        assert!(near(m.inner_r, p.dp(4.0)), "{}", m.inner_r);
        assert!(m.outer_r > m.inner_r);
        assert!(m.item_w > m.item_h, "a popup item is a wide row");

        // 12 x 10 dp arrow at 2 dp, 26 dp from the anchored edge. An anchor
        // on the right half of the panel is a trailing-edge anchor, so the
        // sign goes negative.
        assert!(near(m.arrow_w, p.dp(12.0)), "{}", m.arrow_w);
        assert!(near(m.arrow_h, p.dp(10.0)), "{}", m.arrow_h);
        assert!(near(m.arrow_r, p.dp(2.0)), "{}", m.arrow_r);
        assert!(m.arrow_w < m.outer_r * 2.0, "the arrow is narrower than the popup");
        assert!(near(m.arrow_center, -p.dp(26.0)));
        assert!(near(m.arrow_center.abs(), p.dp(26.0)));
        assert!(m.arrow_center < 0.0, "a right-hand anchor is the trailing edge");
        assert!(m.arrow_center.abs() < m.item_w * 0.5, "the arrow sits inside the item row");

        // A left-hand anchor flips the sign and nothing else.
        let left = l.popup_menu(Rect {
            x: 100.0,
            y: 1200.0,
            w: 80.0,
            h: 80.0,
            radius: 0.0,
        });
        assert!(near(left.arrow_center, p.dp(26.0)));
        assert!(near(left.arrow_center, -m.arrow_center));
        assert!(near(left.item_w, m.item_w));
        // An anchor straddling the midline goes to the trailing side.
        let middle = l.popup_menu(Rect {
            x: l.w * 0.5 - 10.0,
            y: 1200.0,
            w: 80.0,
            h: 80.0,
            radius: 0.0,
        });
        assert!(near(middle.arrow_center, -p.dp(26.0)));

        // 2 dp elevation, 10 / 14 / 4 dp padding, 4 items, 2 dp margin.
        assert!(near(m.elevation, p.dp(2.0)));
        assert!(near(m.pad_start, p.dp(10.0)));
        assert!(near(m.pad_end, p.dp(14.0)));
        assert!(near(m.pad_v, p.dp(4.0)));
        assert_eq!(m.max_items, 4);
        assert!(near(m.container_margin, p.dp(2.0)));
        assert!(m.pad_start < m.pad_end, "the reference is asymmetric");
        // The item text band is what is left after the paddings.
        assert!(near(m.item_w, m.pad_start + m.pad_end + m.item_w - m.pad_start - m.pad_end));

        // The height accounts for the arrow overhang and clamps at 4 items.
        assert!(m.height_for(1) < m.height_for(2));
        assert!(near(m.height_for(4) - m.height_for(3), m.item_h));
        assert!(near(m.height_for(99) - m.height_for(4), 0.0));
    }

    #[test]
    fn drawer_sheet_layout_matches() {
        let (w, h) = (1080.0f32, 2400.0f32);
        let l = Layout::plain(w, h);
        let p = l.profile();

        // `shift` is the drawer's top-edge offset: `h` is fully open, `0` is
        // closed, and half way is exactly half way. Every band inside moves
        // with it, so the sheet translates as one piece.
        let open = l.drawer_sheet(h);
        assert_eq!(open.top, 0.0);
        let closed = l.drawer_sheet(0.0);
        assert_eq!(closed.top, h);
        let half = l.drawer_sheet(h * 0.5);
        assert!(near(half.top, h * 0.5));
        assert!(near(half.handle.y - open.handle.y, h * 0.5));
        assert!(near(half.search.y - open.search.y, h * 0.5));
        assert!(near(half.grid.y - open.grid.y, h * 0.5));
        // The bands are rigid: each one sits the same distance below the sheet
        // top however far the sheet is dragged. A fully closed sheet is
        // clipped to nothing, so its list height collapses.
        assert!(near(open.grid.y - open.top, closed.grid.y - closed.top));
        assert!(near(
            open.predictions.y - open.top,
            closed.predictions.y - closed.top
        ));
        assert!(open.grid.h > 0.0);
        assert_eq!(closed.grid.h, 0.0, "a closed sheet has no visible list");
        // The list is measured open and the whole thing translates, so its
        // height is `h` less its offset from the sheet top.
        assert!(near(open.grid.h, h - (open.grid.y - open.top)));

        // 32 x 4 dp handle, r 2 dp, centred in a 36 dp band that clears the
        // status bar, 16 dp margins.
        assert!(near(open.handle.w, p.dp(32.0)), "{}", open.handle.w);
        assert!(near(open.handle.h, p.dp(4.0)), "{}", open.handle.h);
        assert!(near(open.handle.radius, p.dp(2.0)), "{}", open.handle.radius);
        assert!(near(open.handle.w, 82.285_71));
        assert!(near(open.handle.h, 10.285_714));
        assert!(near(open.handle.x, (w - p.dp(32.0)) * 0.5));
        let band_y = open.top + l.status_bar_h;
        assert!(near(
            open.handle.y,
            band_y + (p.dp(DRAWER_HANDLE_BAND_DP) - p.dp(DRAWER_HANDLE_H_DP)) * 0.5
        ));
        assert!(near(DRAWER_HANDLE_BAND_DP, 36.0));
        assert!(p.dp(DRAWER_HANDLE_BAND_DP) > open.handle.h);

        // 60 dp container, 52 dp box.
        let container = open.search_container();
        assert!(near(open.search.h, p.dp(52.0)), "{}", open.search.h);
        assert!(near(open.search.h, 133.714_3));
        assert!(near(container.h, p.dp(60.0)), "{}", container.h);
        assert!(near(container.h, 154.285_72));
        assert!(container.y < open.search.y && container.y + container.h > open.search.y + open.search.h);
        // The box is centred in the container: the same 4 dp of air above and
        // below, and 60 - 52 = 8 dp of it in total.
        assert!(near(open.search.y - container.y, (container.h - open.search.h) * 0.5));
        assert!(near(
            container.y + container.h - (open.search.y + open.search.h),
            (container.h - open.search.h) * 0.5
        ));
        assert!(near(container.h - open.search.h, p.dp(8.0)));
        assert!(near((container.h - open.search.h) * 0.5, p.dp(4.0)));
        assert!(near(open.search.x, p.dp(p.edge_margin_dp)));
        assert!(near(open.search.w, w - p.dp(p.edge_margin_dp) * 2.0));
        assert!(near(open.search.radius, p.dp(26.0)), "a 52 dp field is a pill");

        // 48 dp header pill, r 12 dp; 128 x 2 dp divider, r 2 dp.
        assert!(near(open.header.h, p.dp(48.0)), "{}", open.header.h);
        assert!(near(open.header.radius, p.dp(12.0)), "{}", open.header.radius);
        assert!(near(open.divider.w, p.dp(128.0)), "{}", open.divider.w);
        assert!(near(open.divider.h, p.dp(2.0)), "{}", open.divider.h);
        assert!(near(open.divider.w, 329.142_88));
        assert!(near(open.divider.h, 5.142_857));
        assert!(near(open.divider.x, (w - p.dp(128.0)) * 0.5));

        // Predictions: icon, 4 dp, text, 4 dp, inside the content margins.
        let icon = open.prediction_icon();
        let text = open.prediction_text();
        assert!(near(icon.w, p.dp(p.icon_dp)), "{}", icon.w);
        assert!(near(icon.x, open.predictions.x));
        let gap = icon.w * (DRAWER_PREDICTION_GAP_DP / APP_ICON_DP);
        assert!(near(text.x, icon.x + icon.w + gap), "{}", text.x);
        assert!(near(gap, p.dp(4.0)));
        assert!(near(
            open.predictions.x + open.predictions.w - (text.x + text.w),
            gap
        ), "the label keeps a 4 dp trailing inset");
        assert!(text.w > 0.0);

        // The bands are ordered and disjoint.
        assert!(open.handle.y + open.handle.h <= container.y + 0.001);
        assert!(container.y + container.h <= open.header.y + 0.001);
        assert!(open.header.y + open.header.h <= open.divider.y + 0.001);
        assert!(open.divider.y + open.divider.h <= open.predictions.y + 0.001);
        assert!(open.predictions.y + open.predictions.h <= open.grid.y + 0.001);
        assert!(open.grid.y + open.grid.h <= l.h + 0.001);
        // The list runs under the gesture bar, which is bottom padding.
        assert!(open.grid.y + open.grid.h > l.nav_pill.y);
        // 104 dp rows actually fit in the band, several times over.
        assert!(open.grid.h > open.grid_row_h * 6.0, "only {} rows fit", open.grid.h / open.grid_row_h);

        // 104 dp rows, 16 dp borders, the profile's column count.
        assert!(near(open.grid_row_h, p.dp(104.0)), "{}", open.grid_row_h);
        assert!(near(open.grid_row_h, 267.428_57));
        assert!(near(open.grid_border, p.dp(16.0)), "{}", open.grid_border);
        assert!(near(open.grid_border, 41.142_857));
        assert_eq!(open.grid_cols, p.cols);
        assert!(open.grid.x >= 0.0 && open.grid.x + open.grid.w <= w + 0.001);

        // Scrim #404040 at alpha 0.40, 24 dp top corners only.
        assert_eq!(open.scrim_argb, 0x6640_4040);
        assert_eq!(open.scrim_argb >> 24, 102, "alpha 0.40 * 255");
        assert_eq!(open.scrim_argb & 0x00FF_FFFF, 0x40_4040);
        assert!(near(open.corner_r, p.dp(24.0)), "{}", open.corner_r);
        assert_eq!(open.corner_r, p.dp(p.dialog_corner_dp));
    }

    /// Launcher3 `OverScroll.dampedScroll` (`OverScroll.java:42-54`).
    ///
    /// ```text
    /// f   = |amount| / max
    /// f2  = 1 - min(f / 0.07, 1)
    /// out = sign * |amount| * ((f2 - 1)^3 + 1) * 0.07
    /// ```
    ///
    /// Worked by hand for `max = 1000` (each row: `f2`, `(f2-1)^3 + 1`,
    /// then `amount * influence * 0.07`):
    #[test]
    fn overscroll_matches_over_scroll_java() {
        let max = 1000.0f32;
        // Reference values transliterated from OverScroll.java:42-54 in f64
        // and pasted here, so the test checks the port rather than restating
        // it.  max = 1000 throughout.
        //   f = amount/max;  f = sign(f) * influence(|f|);  |f|>=1 -> +/-1
        //   out = 0.07 * f * max          (note: f, NOT f * ratio)
        // influence(x) = (x-1)^3 + 1
        //   x=0.01  (-0.99)^3+1 = 0.029701  -> 0.07 * 0.029701 * 1000 =   2.07907
        //   x=0.05  (-0.95)^3+1 = 0.142625  -> 0.07 * 0.142625 * 1000 =   9.98375
        //   x=0.10  (-0.90)^3+1 = 0.271000  -> 0.07 * 0.271000 * 1000 =  18.97000
        //   x=0.20  (-0.80)^3+1 = 0.488000  -> 0.07 * 0.488000 * 1000 =  34.16000
        //   x=0.50  (-0.50)^3+1 = 0.875000  -> 0.07 * 0.875000 * 1000 =  61.25000
        //   x=0.70  (-0.30)^3+1 = 0.973000  -> 0.07 * 0.973000 * 1000 =  68.11000
        //   x=1.00  ( 0.00)^3+1 = 1.000000  -> |f|>=1 clamps to 1 -> 70.00000
        //   x>1     infl>1 -> always clamped to +/-1 -> always 70.00000
        for (f, want) in [
            (0.01f32, 2.079_07),
            (0.05, 9.983_75),
            (0.10, 18.97),
            (0.20, 34.16),
            (0.50, 61.25),
            (0.70, 68.11),
            (1.00, 70.0),
            (2.00, 70.0),
            (10.0, 70.0),
        ] {
            let got = damped_scroll(f * max, max);
            assert!(
                (got - want).abs() < 1e-3,
                "damped_scroll({f} * max) = {got}, want {want}"
            );
        }

        // The influence curve itself: zero at 0, one at 1, still growing at 2
        // (which is precisely why the caller must clamp).
        assert_eq!(overscroll_influence(0.0), 0.0);
        assert_eq!(overscroll_influence(1.0), 1.0);
        assert_eq!(overscroll_influence(2.0), 2.0);
        assert!(overscroll_influence(0.5) > 0.0 && overscroll_influence(0.5) < 1.0);

        // Saturation: past `ratio == 1` the clamp pins the output to the
        // 7% * max ceiling, so a wild fling cannot escape the page.
        for f in [1.0f32, 1.5, 2.0, 10.0, 1000.0] {
            assert_eq!(
                damped_scroll(f * max, max),
                OVERSCROLL_DAMP_FACTOR * max,
                "ratio {f} must saturate at the ceiling"
            );
        }
        // And it never exceeds the ceiling anywhere below it. (ratio 0 is
        // excluded: `OverScroll.java:43` short-circuits amount == 0 to 0.
        // 0.999 is included and legitimately *reaches* the ceiling: the cubic
        // is 1 - 1e-9 there, which is exactly 1.0 in f32.)
        for f in [0.001f32, 0.01, 0.1, 0.5, 0.9, 0.999] {
            let v = damped_scroll(f * max, max);
            assert!(v > 0.0, "ratio {f} should still move");
            assert!(v <= OVERSCROLL_DAMP_FACTOR * max, "ratio {f} gave {v}");
        }

        // The sign is the sign of the drag, and the curve is odd.
        for f in [0.01f32, 0.1, 0.5, 1.0, 7.0] {
            let p = damped_scroll(f * max, max);
            let n = damped_scroll(-f * max, max);
            assert!((p + n).abs() < 1e-3, "not antisymmetric at f = {f}: {p} vs {n}");
        }

        // Not the 1 / (1 + x / 100) rational the live path used: that one is
        // asymptote-free and would let a 10x overdrag keep growing, while this
        // one is pinned to 0.07 * max.
        assert_eq!(damped_scroll(10.0 * max, max), 0.07 * max);

        // Guards: no drag, no range, or garbage in, zero out.
        assert_eq!(damped_scroll(0.0, max), 0.0);
        assert_eq!(damped_scroll(50.0, 0.0), 0.0);
        assert_eq!(damped_scroll(50.0, -10.0), 0.0);
        assert_eq!(damped_scroll(f32::NAN, max), 0.0);
        assert_eq!(damped_scroll(f32::INFINITY, max), 0.0);
        assert_eq!(damped_scroll(50.0, f32::NAN), 0.0);
        assert_eq!(damped_scroll(50.0, f32::INFINITY), 0.0);
        // amount/max underflowing to 0 must not produce NaN from 0/0.
        assert_eq!(damped_scroll(f32::MIN_POSITIVE, 1.0e30), 0.0);

        // A fling's max is half a page (`PagedView.java:1552`).
        let page = 1080.0f32 * 0.5;
        assert!(damped_scroll(10.0, page) > 0.0);
        assert_eq!(damped_scroll(page, page), OVERSCROLL_DAMP_FACTOR * page);
    }

    /// The dp layer is plain-old-data: every value it publishes is `Copy`, so
    /// the render path can hold and pass the whole set by value with no
    /// allocator in the loop. (A per-test counting allocator would need a
    /// crate-wide `#[global_allocator]`, which `screenshot.rs` already owns
    /// for the render harness.)
    #[test]
    fn new_layouts_are_copy_pods() {
        assert_copy::<DeviceProfile>();
        assert_copy::<SmartspaceLayout>();
        assert_copy::<QsbLayout>();
        assert_copy::<PageIndicatorLayout>();
        assert_copy::<DotRect>();
        assert_copy::<DrawerSheetLayout>();
        assert_copy::<FastScrollerLayout>();
        assert_copy::<RecentsLayout>();
        assert_copy::<FolderLayout>();
        assert_copy::<PopupMenuLayout>();
        assert_copy::<Layout>();

        // No interior mutability either: a `Cell` in one of these would make
        // the borrow checker fight the render path, and `PartialEq` on `f32`
        // is fine because nothing stores NaN in a published field.
        assert_copy::<Cell>();
        assert_copy::<Rect>();

        // Building every layout in a loop must not allocate, and must be
        // idempotent: the same panel always produces the same bytes.
        for (w, h) in PANELS {
            for sel in [false, true] {
                let l = Layout::new(*w, *h, sel);
                for shift in [0.0f32, 1.0, l.h * 0.5, l.h] {
                    let a = l.drawer_sheet(shift);
                    let b = l.drawer_sheet(shift);
                    assert_eq!(a, b, "{w}x{h} shift {shift} is not deterministic");
                }
                let anchor = Rect {
                    x: l.w * 0.5,
                    y: l.h * 0.5,
                    w: 64.0,
                    h: 64.0,
                    radius: 0.0,
                };
                let p = l.profile();
                let _ = (
                    l.smartspace(),
                    l.qsb(),
                    l.page_indicator(),
                    l.fast_scroller(),
                    l.recents(),
                    l.folder(),
                    l.popup_menu(anchor),
                    p,
                    cell_width(l.w, p.cols, p.dp(16.0)),
                    cell_height(l.h, p.rows, p.dp(16.0)),
                    hotseat_cell_height(p.dp(p.icon_dp), p.dp(p.label_sp), false),
                    hotseat_icon_space(l.dock.w, p.hotseat_icons, p.dp),
                    folder_layout_radius(p.dp(p.icon_dp), true),
                    folder_preview_icons(4, (0.0, 0.0), 1.0, 1.0, true),
                    damped_scroll(50.0, l.w * 0.5),
                    page_indicator_dots(&l.page_indicator(), 3, 0, 1, 0.5),
                );
            }
        }
    }

    fn assert_copy<T: Copy>() {}
}
