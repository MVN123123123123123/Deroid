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
    /// `true` for a box with no area.
    ///
    /// The feature-detection idiom, not a nicety: several geometries here are
    /// legitimately zero-sized to mean "do not draw me" -- a folder's page
    /// indicator when the folder fits on one page -- and a caller testing
    /// `w == 0.0` will eventually forget the `h`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
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

    /// The user's workspace column count, or 0 for the width heuristic.
    ///
    /// Private with a builder ([`Layout::with_grid`]) because setting it is only
    /// correct together with the row override and the clamp, and three public
    /// fields invite a caller to set one of them.
    grid_cols_override: usize,
    /// The user's workspace row count, or 0 for the profile default.
    grid_rows_override: usize,

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
    /// User type scale, `1.0` at the system default.
    ///
    /// Threaded through every em / line-box / label metric in this module
    /// (and into the smartspace and QSB sub-layouts, which read it off the
    /// `Layout`). It is deliberately a separate knob from `label_scale`,
    /// which is the panel-height breakpoint the renderer selects glyph weights
    /// with: the two multiply out independently, so a 1.3x user font on a
    /// 1080x2400 panel is `label_scale == 2` at `font_scale == 1.3`.
    ///
    /// A non-finite value is coerced to `1.0` and a negative one to `0.0`
    /// rather than poisoning every downstream rectangle.
    pub font_scale: f32,
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
    /// Build the layout for a panel of `w` x `h` pixels at the system type
    /// scale.
    ///
    /// `has_selection` reserves the action-chip band above the grid, which is
    /// how the workspace makes room for the edit-mode actions.
    ///
    /// A thin wrapper over [`Layout::new_scaled`] with `font_scale = 1.0`, so
    /// every existing caller keeps the exact geometry it had. Callers that can
    /// see a user type scale must call `new_scaled` directly -- which is what
    /// the shell now does, threading
    /// `LauncherState::font_scale` through.
    #[inline]
    pub fn new(w: f32, h: f32, has_selection: bool) -> Self {
        Self::new_scaled(w, h, 1.0, has_selection)
    }

    /// A layout that honours a user type scale, for the shell's own call sites.
    ///
    /// Named so that a caller reaching for the wrong one is visible at the call
    /// site rather than in a diff: `new` pins the scale to 1.0 and
    /// `new_scaled` is the only way to move it.
    #[inline]
    pub fn for_shell(w: f32, h: f32, font_scale: f32, has_selection: bool) -> Self {
        Self::new_scaled(w, h, font_scale, has_selection)
    }

    /// Override the workspace grid dimensions with the user's setting.
    ///
    /// `LauncherState::grid_cols` and `grid_rows` have been persisted and
    /// offered in Settings since the store existed, and nothing read them: the
    /// grid came from [`grid_cols_for`]'s width heuristic, so "Workspace columns"
    /// was a control that moved and changed nothing.
    ///
    /// `0` means "auto" on both axes and defers to the heuristic, which is what
    /// the setting's own label says. A non-zero value is clamped to 1..=12 rather
    /// than trusted: a stored 0 would divide by zero in the row-pitch arithmetic,
    /// and a stored 400 would produce cells narrower than an icon, which reads as
    /// a crash rather than as a setting.
    ///
    /// # Why this is a builder and not a parameter
    ///
    /// Every existing call site wants the heuristic, so adding a parameter would
    /// mean editing a dozen constructors to pass `0, 0`. A builder keeps the
    /// default path untouched and makes the override visible at the one call site
    /// that wants it.
    #[inline]
    pub fn with_grid(mut self, cols: usize, rows: usize) -> Self {
        // 12 is the settings panel's own maximum, so a value above it can only
        // come from a hand-edited state file.
        const MAX: usize = 12;
        self.grid_cols_override = if cols == 0 { 0 } else { cols.clamp(1, MAX) };
        self.grid_rows_override = if rows == 0 { 0 } else { rows.clamp(1, MAX) };
        self
    }

    /// Build the layout for a panel of `w` x `h` pixels with a user type
    /// scale.
    ///
    /// `font_scale` multiplies the label line box (and, through it, the grid
    /// row pitch) and the sp-sized line boxes of the smartspace. It does not
    /// touch the column count, the icon edge, or any hit-test pitch: a user
    /// who turns their font up gets taller rows, not a different grid.
    ///
    /// Non-finite input degrades to `1.0` and a negative scale to `0.0` (rows
    /// collapse onto the icon) rather than producing NaN geometry.
    pub fn new_scaled(w: f32, h: f32, font_scale: f32, has_selection: bool) -> Self {
        let w = w.max(1.0);
        let h = h.max(1.0);
        let font_scale = if font_scale.is_finite() {
            font_scale.max(0.0)
        } else {
            1.0
        };
        let p = DeviceProfile::for_panel(w);
        // 12.6 dp on the reference profile, i.e. exactly the 3% of the panel
        // width `PANEL_PAD_FRACTION` used to carry - now a length that can be
        // retuned on its own.
        let pad = p.dp(p.panel_pad_dp);
        // Touch targets must be at least 48dp on *either* axis, so they scale
        // with the short edge: on a 2000x1000 panel a width-derived minimum
        // would be 96px and eat the whole workspace. `touch_target_dp` is
        // `TOUCH_TARGET_FRACTION` of the 420 dp reference width, so
        // `(short / w) * dp(token)` is that fraction of the short edge at
        // any density.
        let min_touch = (w.min(h) / w) * p.dp(p.touch_target_dp);

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
        // Every length below comes from the profile's calibrated dp tokens
        // rather than a fraction of the panel or of the icon - see
        // `DeviceProfile::grid_metrics` for the derivation of each one.
        let grid_cols = grid_cols_for(w);
        let col_pitch = w / grid_cols as f32;
        // The renderer's type scale is an integer index into a 15/30/45 px
        // ladder (`font::em_px` = `EM_BASE_PX * scale`), and `em_px_at` then
        // multiplies by the panel-width factor. Pick the rung whose resolved
        // em is closest to the panel's 14 sp body size -- i.e. to
        // `14 * dp_scale`, where `dp_scale` is `w / 420`.
        //
        // This was `if h >= 1600.0 { 2 } else { 1 }`, which keyed off the
        // panel's *height* while `em_px_at` scales by its *width*. The two
        // disagreed, and the disagreement cost real type: a 720x1280 phone --
        // an ordinary 16:9 handset -- took the low rung and rendered 12 px
        // labels, which is 1.7% of the panel width. The height heuristic also
        // gave a landscape 2000x1000 tablet a 66 px em, larger than most of
        // its own chrome.
        let target_body_px = font::body_em_px_for(w);
        let label_scale = [1usize, 2, 3]
            .into_iter()
            .min_by(|&a, &b| {
                let (ea, eb) = (
                    (font::em_px_at(a, w as usize) - target_body_px).abs(),
                    (font::em_px_at(b, w as usize) - target_body_px).abs(),
                );
                ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(2);
        // Label line box: em height plus descender, derived from the same
        // type scale the renderer draws with, so geometry and type agree.
        let label_em = font::em_px_at(label_scale, w as usize);
        let gm = p.grid_metrics(font_scale, label_em);
        let icon_size = gm.icon;
        let row_pitch = gm.cell_h;

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

        // The icon yields so the label never has to.
        //
        // The body em is a fixed fraction of the panel's dp scale, but the
        // grid band is a fixed fraction of the panel's *height*. On a short or
        // wide panel those two disagree: a 20% larger label can cost a whole
        // row, and a launcher that shows one row of six icons is not a
        // launcher. So when the natural row count would fall below
        // [`MIN_GRID_ROWS`], the icon and its two icon-relative gaps scale
        // down to make room and the label keeps every pixel it asked for.
        //
        // The label is the thing this whole change is about (plan 6.3: text
        // must render "legibly at true mobile scale without colliding"), so
        // it is the last thing that should give. The split is exact rather
        // than proportional: the icon-derived part of the row scales by `k`
        // and the label part does not scale at all.
        const MIN_GRID_ROWS: f32 = 2.0;
        let band = grid_bottom - grid_top;
        let (icon_size, row_pitch) = {
            let natural = if row_pitch > 0.0 && row_pitch.is_finite() {
                (band / row_pitch).floor()
            } else {
                MIN_GRID_ROWS
            };
            let icon_part = gm.cell_h - gm.label_h;
            if natural >= MIN_GRID_ROWS || icon_part <= 0.0 {
                (icon_size, row_pitch)
            } else {
                // The largest `k` for which the shrunken row still fits
                // `MIN_GRID_ROWS` times in the band. Solved from the budget
                // rather than as a ratio of the two pitches: `k = band /
                // (MIN_ROWS * pitch)` makes the result land *exactly* on the
                // boundary, and one ULP of `f32` short of it floors back to
                // `MIN_ROWS - 1`. The 0.999 factor is the slack that keeps the
                // `floor()` below on the right side of it.
                let budget = band / MIN_GRID_ROWS * 0.999;
                let k = ((budget - gm.label_h) / icon_part).clamp(0.25, 1.0);
                (icon_size * k, icon_part * k + gm.label_h)
            }
        };
        let max_rows = ((band / row_pitch).floor().max(0.0)) as usize;

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
            grid_cols_override: 0,
            grid_rows_override: 0,
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
            font_scale,
        }
    }

    /// Convenience constructor with no edit-mode band.
    #[inline]
    pub fn plain(w: f32, h: f32) -> Self {
        Self::new(w, h, false)
    }

    /// Convenience constructor with no edit-mode band at a user type scale.
    #[inline]
    pub fn plain_scaled(w: f32, h: f32, font_scale: f32) -> Self {
        Self::new_scaled(w, h, font_scale, false)
    }

    /// The em the renderer draws workspace labels at, in pixels, *before* the
    /// user font scale.
    ///
    /// The single source of truth for the type size the grid geometry is
    /// measured against: [`DeviceProfile::grid_metrics`] takes it, so the
    /// row pitch and the glyphs cannot disagree.
    #[inline]
    pub fn label_em_px(&self) -> f32 {
        font::em_px_at(self.label_scale, self.w as usize)
    }

    /// [`Self::label_em_px`] with the user font scale applied: the size to
    /// rasterise workspace labels at.
    #[inline]
    pub fn scaled_label_em_px(&self) -> f32 {
        self.label_em_px() * self.font_scale
    }

    /// The grid's vertical metrics, from the panel's own profile.
    ///
    /// `Layout::icon_size` and `Layout::row_pitch` are exactly
    /// [`GridMetrics::icon`] and [`GridMetrics::cell_h`], so retuning a dp
    /// token on a copied profile shows up here cell for cell.
    #[inline]
    pub fn grid_metrics(&self) -> GridMetrics {
        self.profile()
            .grid_metrics(self.font_scale, self.label_em_px())
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

    /// Geometry of the icon tile for drawer row-major `index`, scrolled by
    /// `scroll_y` px.
    ///
    /// The scroll offset is a *parameter* rather than a stored field for the
    /// same reason the rest of this file's metrics are functions of
    /// `(w, h)`: `Layout` is a `Copy` value threaded through the render path,
    /// and a mutable scroll field on it would make the "pure geometry" claim
    /// false and force every reader to know which copy is current.
    ///
    /// Rows scrolled above the grid's top edge are NOT clamped here. The
    /// caller culls with [`Self::visible_row_range`], and a row that is
    /// partly scrolled off has to keep its true position or it would jump
    /// when it re-enters.
    #[inline]
    pub fn drawer_icon_cell_scrolled(&self, index: usize, scroll_y: f32) -> Rect {
        let row = index / self.grid_cols;
        let col = index % self.grid_cols;
        Rect {
            x: col as f32 * self.col_pitch,
            y: self.drawer_grid_top + row as f32 * self.row_pitch - scroll_y,
            w: self.col_pitch,
            h: self.drawer_icon,
            radius: self.drawer_icon * ICON_RADIUS,
        }
    }

    /// The half-open range of drawer rows that can be on screen at `scroll_y`,
    /// out of `total_rows` rows of *content*.
    ///
    /// `total_rows` is the content height and is deliberately NOT
    /// [`Self::drawer_rows`], which is how many rows fit in the viewport. Using
    /// the viewport count as the content bound is what made the scroll a
    /// decoration: every row past the first screen was culled by the renderer
    /// *and* rejected by the hit test, so a 137-app catalogue had 18 rows that
    /// no scroll position could ever reach.
    ///
    /// Row `r` occupies `[grid_top + r*pitch - scroll, ... + pitch)`, and is
    /// visible when its bottom is below the band's top *and* its top is above
    /// the band's bottom. Solving both for `r`:
    ///
    /// ```text
    ///   (r + 1) * pitch >  scroll        ->  r >  scroll/pitch - 1
    ///    r * pitch <  band + scroll      ->  r < (band + scroll)/pitch
    /// ```
    ///
    /// with `band = grid_bottom - grid_top`. So `first = floor(scroll/pitch)`
    /// and `last = ceil((band + scroll)/pitch)`, both clamped. The derived
    /// bounds, rather than the viewport's own `drawer_rows`, are what make
    /// `last` reach the final row at maximum scroll instead of stopping
    /// `drawer_rows` short of it.
    #[inline]
    pub fn visible_row_range(&self, scroll_y: f32, total_rows: usize) -> core::ops::Range<usize> {
        if self.drawer_rows == 0 || self.row_pitch <= 0.0 || total_rows == 0 {
            return 0..0;
        }
        if !scroll_y.is_finite() {
            return 0..0;
        }
        let band = self.drawer_grid_bottom - self.drawer_grid_top;
        let first = (scroll_y / self.row_pitch).floor().max(0.0) as usize;
        let last = (((band + scroll_y) / self.row_pitch).ceil().max(0.0) as usize).min(total_rows);
        first.min(total_rows)..last.max(first).min(total_rows)
    }

    /// The largest legal `drawer_scroll_y` for `total_apps` rows, px.
    ///
    /// Zero when the content fits: a list that cannot scroll must not be
    /// draggable, or the rubber band has nothing to resist against and the
    /// grid slides off its own band for no reason.
    #[inline]
    pub fn drawer_max_scroll_y(&self, total_apps: usize) -> f32 {
        let rows = total_apps.div_ceil(self.grid_cols.max(1));
        let content_h = rows as f32 * self.row_pitch;
        (content_h - (self.drawer_grid_bottom - self.drawer_grid_top)).max(0.0)
    }

    /// Geometry of the icon tile for drawer row-major `index`.
    #[inline]
    pub fn drawer_icon_cell(&self, index: usize) -> Rect {
        self.drawer_icon_cell_scrolled(index, 0.0)
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
        if self.has_selection && (self.remove_chip.contains(x, y) || self.move_chip.contains(x, y))
        {
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

    /// Drawer grid cell under `(x, y)`, accounting for `scroll_y`, out of
    /// `total_apps` apps of content.
    ///
    /// The same arithmetic as [`Self::drawer_grid_hit`] with the scroll added
    /// before the row is derived. Both must agree: the renderer draws from
    /// `drawer_icon_cell_scrolled` and this resolves the tap, so a
    /// scroll-aware draw with a scroll-blind tap would put a press on a
    /// different app than the one under the finger.
    ///
    /// `total_apps` bounds the *content*, not the viewport. Capping at
    /// `self.drawer_rows` here would make every app past the first screen
    /// untappable no matter how far the list was scrolled.
    #[inline]
    pub fn drawer_grid_hit_scrolled(
        &self,
        drawer_y_offset: f32,
        x: f32,
        y: f32,
        scroll_y: f32,
        total_apps: usize,
    ) -> Option<usize> {
        if !drawer_y_offset.is_finite() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        if x < 0.0 || x >= self.w || self.drawer_rows == 0 || self.grid_cols == 0 {
            return None;
        }
        let local_y = y - drawer_y_offset;
        if local_y < self.drawer_grid_top || local_y >= self.drawer_grid_bottom {
            return None;
        }
        let col = (x / self.col_pitch).min(self.grid_cols as f32 - 1.0) as usize;
        // A negative row means "the finger is above the top of the list",
        // which is not a cell. Testing `row_f < 0.0` before the `as usize`
        // cast is what turns that into a miss instead of wrapping to ~1.8e19
        // and indexing off the end of the catalogue.
        let row_f = (local_y - self.drawer_grid_top + scroll_y) / self.row_pitch;
        if !row_f.is_finite() || row_f < 0.0 {
            return None;
        }
        let row = row_f as usize;
        if row * self.grid_cols + col >= total_apps {
            return None;
        }
        Some(row * self.grid_cols + col)
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
        // The clear affordance occupies the trailing 48dp of the field -
        // `min_touch_dp`, the Material 3 floor, not a fraction of the panel.
        let p = self.profile();
        let clear_w = p.dp(p.min_touch_dp);
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
        let mut p = DeviceProfile::for_panel(self.w);
        // The user's grid setting, applied over the width heuristic.
        //
        // `profile()` recomputes from `self.w` on every call rather than holding a
        // cached profile, so an override has to live on the `Layout` and be folded
        // in here. Zero on either axis means "auto" and leaves the heuristic alone,
        // which is the default and therefore a no-op for every existing call site.
        if self.grid_cols_override != 0 {
            p.cols = self.grid_cols_override;
        }
        if self.grid_rows_override != 0 {
            p.rows = self.grid_rows_override;
        }
        p
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
// Everything below is the density-aware layer: the dp numbers for a panel and
// the sub-layouts derived from them. It is now the *single* source of truth
// for the grid engine as well as for the overlays - the fraction constants at
// the top of the file stay published (the renderer and other modules still
// read them) but nothing derives a *new* length from them any more: each
// fraction has been folded into a dp token calibrated to reproduce it exactly
// on the reference panel, so retuning one can never silently move another.

/// Panel width, in dp, of the reference phone profile
/// (`lawnchair/res/xml/device_profiles.xml`, `4_by_6`).
///
/// This is the **standardised XXHDPI calibration baseline** for every dp
/// number in this file. A 1080 px panel is XXHDPI, so
/// `1080 / 420 = 2.5714286` px per dp, and that is the density all of them
/// were measured at. It is also why the grid tokens below are calibrated
/// against *this* panel: a token is only "the reference value" if it agrees
/// with the fraction geometry here, and each comment says which ones do.
pub const LAWNCHAIR_PHONE_DP_WIDTH: f32 = 420.0;
/// Workspace rows declared by the reference profile (`device_profiles.xml:55`).
pub const LAWNCHAIR_PHONE_ROWS: usize = 6;
/// Hotseat slots declared by the reference profile (`device_profiles.xml:59`).
pub const LAWNCHAIR_PHONE_HOTSEAT_ICONS: usize = 4;
/// Folder grid of the reference profile (`device_profiles.xml:57-58`).
pub const LAWNCHAIR_PHONE_FOLDER: usize = 3;

/// Smallest folder grid the reference will configure.
///
/// `FolderPreferences.kt` exposes `folderColumns` and `folderRows` as sliders
/// over `2..5` (`FolderPreferences.kt:84,90`). Below 2 the "grid" is a single
/// cell, which is an app icon wearing a folder's clothes.
pub const FOLDER_GRID_MIN: usize = 2;
/// Largest folder grid the reference will configure (`FolderPreferences.kt:84,90`).
pub const FOLDER_GRID_MAX: usize = 5;

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
///
/// # Two families of token
///
/// * `icon_dp`, `label_sp`, `cell_pad_x_dp`, ... are the **XML reference
///   values** - literals lifted out of `device_profiles.xml` and
///   `dimens.xml`. They are what the dp sub-layouts (drawer sheet, folder,
///   QSB, ...) are measured against.
/// * `grid_icon_dp`, `grid_label_gap_dp`, `grid_cell_bottom_gap_dp`,
///   `panel_pad_dp` and `touch_target_dp` are the **calibrated** tokens:
///   each is the dp equivalent of a fraction the grid engine used to carry,
///   chosen so the reference panel (1080 px / 420 dp) comes out
///   pixel-identical. They are derived from [`LAWNCHAIR_PHONE_DP_WIDTH`] and
///   the column count rather than from the XML, because the XML's own numbers
///   would move the reference panel (the XML workspace grid is 4 columns
///   wide; UTLC's breakpoint is 5 at 1080 px).
///
/// [`DeviceProfile::grid_metrics`] is what turns the calibrated tokens into
/// the pixel rectangles the grid actually publishes.
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
    ///
    /// A parameter with [`LAWNCHAIR_PHONE_FOLDER`] as its default because the
    /// reference makes it one: `prefs2.folderColumns`
    /// (`preferences2/PreferenceManager2.kt:706-711`) is a user setting over
    /// `2..5` (`ui/preferences/destinations/FolderPreferences.kt:80-84`),
    /// fed into `DeviceProfileOverrides` at `:122` and read as
    /// `numFolderColumns` (`DeviceProfile.java:198,462`). UTLC had it as a
    /// constant, so a user who set 2 in the reference could not reproduce it
    /// here. Zero is clamped to [`FOLDER_GRID_MIN`] -- see
    /// [`Self::with_folder_grid`].
    pub folder_cols: usize,
    /// Folder rows (`device_profiles.xml:57`), a parameter for the same
    /// reason as [`Self::folder_cols`]: `prefs.folderRows`
    /// (`preferences/PreferenceManager.kt:114`), same `2..5` slider
    /// (`FolderPreferences.kt:86-91`).
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
    ///
    /// This is the *absolute* floor, in dp: it is what a hit target is grown
    /// to when nothing bigger is on offer (see
    /// [`Layout::drawer_search_hit`]). It is deliberately **not** the value
    /// the grid engine scales its chrome by - that one is
    /// [`Self::touch_target_dp`], and `48 dp` on a 420 dp panel would be a
    /// third of the screen.
    pub min_touch_dp: f32,
    // ---- calibrated grid tokens (see the struct docs) -------------------
    /// Workspace grid icon edge, in dp: `(420 / cols) * ICON_FRACTION`.
    ///
    /// 5 columns -> `84 * 0.56` = 47.04 dp, which on the 1080 px reference
    /// panel is `47.04 * 2.5714286` = 120.96 px: exactly the old
    /// `(w / cols) * 0.56`. Because the panel width is `420 dp` at *every*
    /// density, the column is `(420 / cols)` dp at *every* width too, so the
    /// identity holds for 4, 5 and 6 columns alike.
    pub grid_icon_dp: f32,
    /// Icon bottom to label top, in dp: `grid_icon_dp * LABEL_GAP`.
    ///
    /// 5 columns -> `47.04 * 0.22` = 10.35 dp == 26.61 px, the old
    /// `icon_size * LABEL_GAP`. Floored at the reference's own
    /// `icon_label_gap_dp` (7 dp) so a retuned icon can never crowd its
    /// label tighter than Lawnchair does.
    pub grid_label_gap_dp: f32,
    /// Extra space below a label before the next cell starts, in dp:
    /// `grid_icon_dp * CELL_BOTTOM_GAP`.
    ///
    /// 5 columns -> `47.04 * 0.20` = 9.41 dp == 24.19 px, the old
    /// `icon_size * CELL_BOTTOM_GAP`.
    pub grid_cell_bottom_gap_dp: f32,
    /// Panel side padding, in dp: `420 * PANEL_PAD_FRACTION` = 12.6 dp.
    ///
    /// That is 3% of the panel width at *any* density, i.e. exactly the old
    /// `w * 0.030`, but it is now a length like every other one and can be
    /// retuned on its own.
    pub panel_pad_dp: f32,
    /// Minimum touch target as a fraction of the panel's **short** edge, in
    /// dp on the reference profile: `420 * TOUCH_TARGET_FRACTION` = 20.16 dp.
    ///
    /// Scaled against `min(w, h)` rather than `w`, so a 2000x1000 panel
    /// still gets a sane target; see [`Layout::new_scaled`].
    pub touch_target_dp: f32,
}

impl DeviceProfile {
    /// The reference phone profile at an explicit density.
    ///
    /// `cols` is passed in rather than derived so this stays `const`: the
    /// breakpoints live in [`grid_cols_for`] and the caller supplies them.
    pub const fn at(dp: f32, cols: usize) -> Self {
        // The panel is `LAWNCHAIR_PHONE_DP_WIDTH` dp wide at *every* density,
        // so one grid column is `(420 / cols)` dp whatever the panel is. The
        // calibrated grid tokens below are that column pitch with the old
        // fractions folded in, which is what makes them reproduce the shipped
        // geometry exactly rather than approximately.
        //
        // `cols` itself is stored verbatim - a caller that asks for 0 columns
        // gets 0 columns back - but the division is guarded, because a
        // `NaN` token would poison every rectangle downstream.
        let col_dp = LAWNCHAIR_PHONE_DP_WIDTH / if cols == 0 { 1 } else { cols } as f32;
        let grid_icon_dp = col_dp * ICON_FRACTION;
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
            grid_icon_dp,
            grid_label_gap_dp: grid_icon_dp * LABEL_GAP,
            grid_cell_bottom_gap_dp: grid_icon_dp * CELL_BOTTOM_GAP,
            panel_pad_dp: LAWNCHAIR_PHONE_DP_WIDTH * PANEL_PAD_FRACTION,
            touch_target_dp: LAWNCHAIR_PHONE_DP_WIDTH * TOUCH_TARGET_FRACTION,
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

    /// This profile with a user-chosen folder grid.
    ///
    /// The bounds are the reference's own slider range, `2..5`
    /// (`ui/preferences/destinations/FolderPreferences.kt:84,90`), and they
    /// are *clamped* rather than refused so a state file from a build with a
    /// wider range still yields a drawable folder. Zero clamps to
    /// [`FOLDER_GRID_MIN`], because the field's "zero means derive it"
    /// convention belongs to the *workspace* grid
    /// ([`Self::cols`] comes from [`grid_cols_for`]) and the reference has no
    /// derived folder grid to fall back to: `numFolderColumns` is whatever
    /// the preference says (`DeviceProfile.java:462`).
    ///
    /// Not `const` because the clamp needs a comparison; the `at`/`for_panel`
    /// constructors stay `const` so the reference profile remains a constant.
    ///
    /// Call site: `main.rs`, where `LauncherState::folder_cols` /
    /// `folder_rows` are read, once per frame that builds the layout. Not yet
    /// wired -- `LauncherState` has no such fields yet.
    pub fn with_folder_grid(mut self, cols: usize, rows: usize) -> Self {
        self.folder_cols = cols.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX);
        self.folder_rows = rows.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX);
        self
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

    /// Hotseat icon spacing for this profile, in pixels.
    ///
    /// The one-line form of [`hotseat_icon_space`]; `icon_size` and
    /// `n_icons` are pixels and a count respectively, and the `[18 dp, 50 dp]
    /// clamp is this profile's own density.
    #[inline]
    pub fn hotseat_icon_space(&self, hotseat_w: f32, n_icons: usize, icon_size: f32) -> f32 {
        hotseat_icon_space(hotseat_w, n_icons, icon_size, self.dp)
    }

    /// The workspace grid's vertical metrics for this profile at
    /// `font_scale`, in pixels.
    ///
    /// This is the *only* place the calibrated grid tokens are turned into
    /// lengths, so `Layout::icon_size` / `Layout::row_pitch` and anything
    /// derived from a mutated profile can never disagree. Two floors keep a
    /// hostile retune inside the reference's own cell: the icon leaves
    /// `cell_pad_x_dp` of clear air on each side of its column, and the label
    /// never sits closer to the icon than `icon_label_gap_dp`.
    ///
    /// `font_scale` is a *type* scale, so it multiplies the label line box and
    /// nothing else - never the icon and never the column pitch.
    /// `label_em_px` is the em the renderer draws workspace labels at
    /// ([`Layout::label_em_px`]); it is passed in rather than re-derived here
    /// so the geometry and the glyphs can never disagree about the type size.
    #[inline]
    pub fn grid_metrics(&self, font_scale: f32, label_em_px: f32) -> GridMetrics {
        let col = LAWNCHAIR_PHONE_DP_WIDTH / self.cols.max(1) as f32 * self.dp;
        let icon = self
            .dp(self.grid_icon_dp)
            .min(col - 2.0 * self.dp(self.cell_pad_x_dp));
        let label_gap = self
            .dp(self.grid_label_gap_dp)
            .max(self.dp(self.icon_label_gap_dp));
        let label_h = label_em_px * 1.2 * font_scale.max(0.0);
        let cell_bottom_gap = self.dp(self.grid_cell_bottom_gap_dp);
        GridMetrics {
            icon,
            label_gap,
            label_h,
            cell_bottom_gap,
            cell_h: icon + label_gap + label_h + cell_bottom_gap,
        }
    }
}

/// The vertical metrics of one workspace grid row, in pixels.
///
/// See [`DeviceProfile::grid_metrics`]. Plain-old-data on purpose: the render
/// path holds one by value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridMetrics {
    /// Icon edge.
    pub icon: f32,
    /// Icon bottom to label top.
    pub label_gap: f32,
    /// Label line box, already multiplied by the font scale.
    pub label_h: f32,
    /// Extra space below the label before the next cell starts.
    pub cell_bottom_gap: f32,
    /// One grid row: `icon + label_gap + label_h + cell_bottom_gap`.
    pub cell_h: f32,
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

/// Space *between* two hotseat icons, clamped to `[18 dp, 50 dp]`
/// (`DeviceProfile.java:1417-1424`, `dimens.xml:446-447`, min clamp at
/// `DeviceProfile.java:958, 990-995`).
///
/// ```text
/// numBorders          = numShownHotseatIcons - 1 + numExtraBorder  // :1418
/// if (numBorders <= 0) return 0;                                   // :1419
/// hotseatIconsTotalPx = iconSizePx * numShownHotseatIcons          // :1421
/// hotseatBorderSpacePx = (hotseatWidthPx - hotseatIconsTotalPx) / numBorders  // :1422
/// return min(hotseatBorderSpacePx, getHotseatProfile().getMaxIconSpacePx());  // :1423
/// ```
///
/// Note what this is *not*: it is not `hotseatWidth / icons`, which is the
/// **pitch** (icon + border) and therefore overstates the gap by a whole icon
/// on every slot. The reference divides the space the icons do **not** fill
/// by the number of gaps, which is `icons - 1`, not `icons`: the outer edges
/// of the strip have no border, exactly as in
/// [`cell_width`]. Dividing by `icons` also makes the answer depend on the
/// icon size when it cannot - a 65 dp icon and a 16 dp icon in the same
/// hotseat must not land on the same number.
///
/// The lower clamp is applied here too (the reference applies it a few lines
/// later, at `:958` / `:990-995`); keeping it in one place means a caller
/// cannot forget it.
///
/// Returns 0 for a degenerate request - no icons, at most one icon (so no
/// border to space), non-finite input, or a non-positive density - rather
/// than a number the caller would draw ink on.
#[inline]
pub fn hotseat_icon_space(available: f32, icons: usize, icon_px: f32, dp_scale: f32) -> f32 {
    if icons <= 1 || !available.is_finite() || !dp_scale.is_finite() || dp_scale <= 0.0 {
        return 0.0;
    }
    if !icon_px.is_finite() || icon_px < 0.0 {
        return 0.0;
    }
    let lo = HOTSEAT_ICON_SPACE_MIN_DP * dp_scale;
    let hi = HOTSEAT_ICON_SPACE_MAX_DP * dp_scale;
    // An inverted clamp range would make `f32::clamp` panic; a too-small
    // density is the only way to get one, and the min is the safer answer.
    if lo >= hi {
        return hi;
    }
    let icons_total_w = icon_px * icons as f32;
    let num_borders = (icons - 1) as f32;
    ((available - icons_total_w) / num_borders).clamp(lo, hi)
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
    ///
    /// The two line boxes are **sp**, so they take the user font scale
    /// ([`Layout::font_scale`]) as well as the panel density: at `1.0` this is
    /// the shipped geometry exactly, and at `1.3` the title/subtitle grow
    /// while the 20 dp weather glyph and the 104 dp host do not.
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
        let title_h = p.dp(SMARTSPACE_TITLE_LINE_SP) * l.font_scale;
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
        let subtitle_h = p.dp(SMARTSPACE_SUBTITLE_LINE_SP) * l.font_scale;
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

/// Which part of the hotseat search pill a touch landed on.
///
/// The reference's `QsbIconId` (`qsb/LawnQsbUi.kt:57-62`: `SEARCH`, `MIC`,
/// `LENS`, `CLEAR`), in the order the layout lays them out. UTLC's `QsbLayout`
/// already computed all four boxes and the shell had **no** `QsbLayout::hit`,
/// so `grep qsb` over `utlc/src/main.rs` returned zero: the pill was drawn with
/// three glyphs and no way to touch any of them. Worse, the pill's rect
/// overlaps the dock's, so a tap in the middle of the search bar fell through to
/// `home_dock_hit` and launched a random hotseat app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsbHit {
    /// The pill's empty body, or the leading search glyph. Opens the drawer with
    /// the keyboard up, which is the reference's `onQsbClick` when
    /// `matchHotseatQsbStyle` is set (`LawnQsbLayout.kt:105-114`).
    Search,
    /// The voice box. Only drawn when the platform reports a speech daemon.
    Mic,
    /// The camera/lens box.
    Lens,
    /// Not the pill.
    None,
}

/// The hotseat search pill and its three tap boxes.
///
/// The reference build's hotseat QSB is a **pure icon pill**: no hint text, no
/// border, no inline completion (verified: zero `Text` composables in
/// `LawnQsbUi.kt:363-421`). There is deliberately no stroke field here, and
/// the pill radius is the *corner radius*, not a border width.
///
/// The row is *weighted*, not evenly gapped: the start box is pinned to the
/// leading edge behind a [`QSB_START_INSET_DP`] inset, the end boxes are a
/// cluster flush to the trailing edge, and all the slack lives in the one
/// flexible spacer between them. See [`QsbLayout::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QsbLayout {
    /// 64 dp tall, 26 dp corner radius, spanning the hotseat content width.
    pub pill: Rect,
    /// 56 dp search-glyph box at the leading edge, `QSB_START_INSET_DP` in;
    /// the glyph is `qsb_glyph_dp` (24 dp) inside it.
    pub g_icon: Rect,
    /// 56 dp voice box, first of the trailing cluster.
    pub mic: Rect,
    /// 56 dp lens box, second of the trailing cluster and pulled
    /// `QSB_END_OFFSET_DP` inboard by the reference's `offset(x = -6.dp)`.
    pub lens: Rect,
}

impl QsbLayout {
    /// Which box a touch at `(x, y)` is in, trailing boxes winning.
    ///
    /// Order matters and matches the draw order: the glyphs are painted over the
    /// pill, so a touch in the mic or lens box belongs to the glyph, not to the
    /// pill behind it. [`Self::hit`] therefore tests the trailing cluster first
    /// and the pill last.
    ///
    /// This is the counterpart to the draw call. The pill is drawn from this
    /// struct and, for the first time, tapped against it as well -- which is
    /// what stops a search-bar tap from being read as a dock tap, since the two
    /// rects overlap vertically by about 165 px on a 1080x2400 panel.
    pub fn hit(&self, x: f32, y: f32) -> QsbHit {
        if self.lens.contains(x, y) {
            return QsbHit::Lens;
        }
        if self.mic.contains(x, y) {
            return QsbHit::Mic;
        }
        if self.pill.contains(x, y) {
            return QsbHit::Search;
        }
        QsbHit::None
    }

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
        // `enableLabelInDock` is off, but the label metric still measures the
        // cell it would have added; the user font scale moves it.
        let hotseat_cell_h =
            hotseat_cell_height(p.dp(p.icon_dp), p.dp(p.label_sp) * l.font_scale, false);
        let hotseat_bottom = l.dock.y + l.dock.h;
        let pill = Rect {
            x: l.dock.x,
            y: hotseat_bottom - (pill_h - hotseat_cell_h) * 0.5 - pill_h,
            w: l.dock.w,
            h: pill_h,
            radius: p.dp(p.qsb_corner_dp),
        };
        // `LawnQsbUi.kt:363-421` is a Compose `Row` of
        //   Box(56dp)  Spacer(weight = 1f)  Row { QsbIcon, QsbIcon(-6dp) }
        // and nothing else. Three consequences the "three boxes and four equal
        // gaps" model this replaces got wrong:
        //
        // 1. The start box is **anchored to the leading edge** with a
        //    `QSB_START_INSET_DP` inset and is *not* centred - it does not
        //    move when the pill is resized.
        // 2. The end icons are a **trailing cluster**: mic and lens are
        //    adjacent (`qsb_icon_width` each, no gap between them) and the
        //    cluster is flush to the trailing edge. The free space all lands
        //    in the one flexible spacer, not split into four equal gaps, so
        //    the end icons are *anchored* and the start icon is *pinned*.
        // 3. The last end icon carries `offset(x = -6.dp)`
        //    (`LawnQsbUi.kt:414-416`), so the lens is drawn 6 dp inboard of
        //    where the Row laid it out. That is a paint offset, not a layout
        //    one: the cluster is still `2 * box_w` wide.
        let inset = p.dp(QSB_START_INSET_DP);
        let end_off = p.dp(QSB_END_OFFSET_DP);
        let cluster_w = box_w * 2.0;
        let cluster_x = pill.x + pill.w - cluster_w;
        let box_h = box_w;
        let by = pill.y + (pill.h - box_h) * 0.5;
        Self {
            pill,
            // The glyph box is a rounded square; voice and lens are circles.
            g_icon: Rect {
                x: pill.x + inset,
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.25,
            },
            mic: Rect {
                x: cluster_x,
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.5,
            },
            lens: Rect {
                x: cluster_x + box_w - end_off,
                y: by,
                w: box_w,
                h: box_h,
                radius: box_w * 0.5,
            },
        }
    }
}

/// Leading inset of the start (search-glyph / provider logo) box inside the
/// QSB pill, in dp.
///
/// The Compose `Row` itself is unpadded (`LawnQsbUi.kt:363-366`); this is the
/// inset the hotseat QSB is given, and it is what keeps the logo off the
/// pill's rounded left cap.
pub const QSB_START_INSET_DP: f32 = 16.0;
/// How far the last end icon is pulled inboard of the pill's trailing edge,
/// in dp: `offset(x = (-6).dp)` (`LawnQsbUi.kt:414-416`).
///
/// A paint offset, not a layout one: the end cluster still occupies
/// `2 * qsb_icon_width`, so the mic - not the lens - is what the trailing edge
/// is flush with.
pub const QSB_END_OFFSET_DP: f32 = 6.0;

/// Slots the page indicator publishes positions for.
pub const PAGE_INDICATOR_MAX_PAGES: usize = 16;
/// Alpha a *resting* dot paints at, from the view's own alpha
/// (`PAGE_INDICATOR_ALPHA = 255`, `PageIndicatorDots.java:81`).
///
/// This is the ceiling of the whole handover: `page_indicator_dots`
/// interpolates between [`Self::DOT_INACTIVE_ALPHA`] and this value, and no
/// dot ever publishes more. Treating the *inactive* constant (128) as the
/// ceiling instead - which is what this module used to do - halves the
/// resting active dot to 128 and caps the incoming one at 64, so the whole
/// indicator is drawn at half opacity on a white wallpaper.
pub const PAGE_INDICATOR_ALPHA: f32 = 255.0;
/// The reference's own nominal inactive-dot constant (`DOT_ALPHA = 128`,
/// `PageIndicatorDots.java:82`).
///
/// Published for fidelity only. It is *not* the value a resting inactive dot
/// draws with: the draw path computes `nonActiveAlpha = (int)(alpha *
/// DOT_ALPHA_FRACTION)` from the live `alpha` (`:553`), so at rest that is
/// `255 * 0.5` = 127.5 - see [`Self::DOT_INACTIVE_ALPHA`].
pub const DOT_ALPHA: f32 = 128.0;
/// Fraction of full alpha an inactive dot carries
/// (`DOT_ALPHA_FRACTION = 0.5f`, `PageIndicatorDots.java:83`).
pub const DOT_ALPHA_FRACTION: f32 = 0.5;
/// Alpha a resting **inactive** dot paints at:
/// `PAGE_INDICATOR_ALPHA * DOT_ALPHA_FRACTION` = **127.5**
/// (`PageIndicatorDots.java:553`, `nonActiveAlpha = (int)(alpha * 0.5f)`).
///
/// The reference truncates to `int`, so AOSP actually paints 127; UTLC keeps
/// the `f32` the arithmetic produces, which is what the tests pin.
pub const DOT_INACTIVE_ALPHA: f32 = PAGE_INDICATOR_ALPHA * DOT_ALPHA_FRACTION;
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
/// 4. `alpha` is the **view's** alpha, `PAGE_INDICATOR_ALPHA` = 255
///    (`:81`), so a resting active dot is fully opaque and a resting
///    bystander is `255 * 0.5` = 127.5 (`:553`). The 128 in `DOT_ALPHA`
///    (`:82`) is a nominal constant, not the draw path's ceiling.
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
    // :553 - `nonActiveAlpha = (int)(mAlpha * DOT_ALPHA_FRACTION)`. The
    // ceiling is the *view's* alpha (255), not the nominal 128 constant, so
    // a resting indicator is fully opaque with half-alpha bystanders.
    let non_active = PAGE_INDICATOR_ALPHA * DOT_ALPHA_FRACTION;
    // `progress` can exceed 1 during a snap; a non-finite one would poison
    // every downstream term, so it degrades to a hard stop.
    let p = if progress.is_finite() {
        progress.max(0.0)
    } else {
        0.0
    };
    let bounce = (p - 1.0).max(0.0) * diameter;
    let alpha_adj = p.min(1.0) * (PAGE_INDICATOR_ALPHA - non_active);

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
                PAGE_INDICATOR_ALPHA - alpha_adj
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
    /// The prediction row's container: 108 dp tall
    /// ([`PREDICTION_ROW_H_DP`]), holding a 65 dp icon with its label
    /// *underneath* ([`Self::prediction_icon`], [`Self::prediction_label`]).
    pub predictions: Rect,
    /// Icon edge inside the prediction row, px (`icon_dp` = 65 dp).
    pub pred_icon_d: f32,
    /// Icon-to-label gap inside the prediction row, px
    /// (`PREDICTION_ICON_PAD_DP` = 7 dp).
    pub pred_icon_pad: f32,
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
    /// Icon edge inside a grid cell, px (`grid_icon_dp`).
    ///
    /// Deliberately *not* `icon_dp` (the all-apps icon size), which is the
    /// prediction row's edge. The main grid's icon is `col_dp *
    /// ICON_FRACTION`, derived from the column width so it is exact at every
    /// column count -- using `icon_dp` here would blow the tile up from 121 px
    /// to 167 px and overflow a cell sized for the former.
    pub grid_icon_d: f32,
    /// Icon bottom to label top, in px (`grid_label_gap_dp`).
    pub grid_label_gap: f32,
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
///
/// The declared `all_apps_predicted_icon_vertical_padding` is **8 dp** and is
/// already counted twice inside [`PREDICTION_ROW_H_DP`]; the 4 dp here is the
/// older flat gap the row used to carry and is retained only so a caller can
/// recognise it. The row's actual icon-to-label gap is
/// [`PREDICTION_ICON_PAD_DP`].
pub const DRAWER_PREDICTION_GAP_DP: f32 = 4.0;
/// App icon edge of the reference profile (`device_profiles.xml:70`).
pub const APP_ICON_DP: f32 = 65.0;
/// Icon-to-label gap of the prediction row, in dp
/// (`all_apps_icon_drawable_padding`, 7 dp) - `PredictionRowView.java:152`.
pub const PREDICTION_ICON_PAD_DP: f32 = 7.0;
/// Label line box of the prediction row, in dp
/// (`Utilities.calculateTextHeight(allAppsIconTextSize)`, 16 dp)
/// - `PredictionRowView.java:153-154`.
pub const PREDICTION_LABEL_H_DP: f32 = 16.0;
/// Vertical padding inside the prediction row, in dp
/// (`all_apps_predicted_icon_vertical_padding`, 8 dp, counted **twice**)
/// - `PredictionRowView.java:155`.
pub const PREDICTION_PAD_V_DP: f32 = 8.0;
/// Extra height the row claims over a regular A-Z row, in dp
/// (`all_apps_search_top_row_extra_height`, 4 dp)
/// - `PredictionRowView.java:93-94, 158-160`.
pub const PREDICTION_TOP_EXTRA_DP: f32 = 4.0;
/// Measured height of the prediction row, in dp:
/// `65 + 7 + 16 + 2 * 8 + 4` = **108**
/// (`PredictionRowView.getExpectedHeight()`, `PredictionRowView.java:149-161`).
///
/// `onMeasure` hands this to `MeasureSpec.makeMeasureSpec(..., EXACTLY)`
/// (`:136-140`), so it is the row's height by construction, not a suggestion.
/// The row is *not* one icon tall: an icon-only band silently drops the label
/// and the two 8 dp of padding off the bottom of the sheet.
pub const PREDICTION_ROW_H_DP: f32 = APP_ICON_DP
    + PREDICTION_ICON_PAD_DP
    + PREDICTION_LABEL_H_DP
    + PREDICTION_PAD_V_DP * 2.0
    + PREDICTION_TOP_EXTRA_DP;

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
        // `PredictionRowView.getExpectedHeight()` (:149-161) measures
        //   iconHeight(65) + iconPadding(7) + textHeight(16) + 2 * vPad(8)
        //   + topRowExtra(4)                                          = 108 dp
        // and `onMeasure` hands that to `EXACTLY` (:136-140), so the row is
        // 108 dp tall *by construction* - not one icon tall with a label
        // floating outside it. The four are laid out the same way the main
        // drawer grid lays them out: icon on top, label underneath.
        let icon_d = p.dp(p.icon_dp);
        let row_h = p.dp(PREDICTION_ROW_H_DP);
        let predictions = Rect {
            x: mx,
            y: divider.y + divider.h + p.dp(12.0),
            w: content_w,
            h: row_h,
            radius: 0.0,
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
            pred_icon_d: icon_d,
            pred_icon_pad: p.dp(PREDICTION_ICON_PAD_DP),
            grid,
            grid_cols: p.cols,
            grid_row_h: p.dp(p.all_apps_cell_h_dp),
            grid_border: p.dp(p.all_apps_border_dp),
            grid_icon_d: p.dp(p.grid_icon_dp),
            grid_label_gap: p.dp(p.grid_label_gap_dp),
            // 0xFF * 0.40 = 0x66 over #404040.
            scrim_argb: 0x6640_4040,
            corner_r: p.dp(p.dialog_corner_dp),
        }
    }

    /// The full cell of grid `(col, row)`: the *hit* target.
    ///
    /// The cell is the column pitch wide and the row pitch tall, including the
    /// border and the label band, because a launcher cell is tappable over its
    /// whole area -- not just over the icon. `scroll_y` shifts the row up
    /// without clamping, so a row that is half scrolled off keeps its true
    /// position instead of jumping when it re-enters.
    #[inline]
    pub fn grid_cell(&self, col: usize, row: usize, scroll_y: f32) -> Rect {
        let pitch = self.grid_cell_pitch();
        Rect {
            x: self.grid.x + col as f32 * pitch,
            y: self.grid.y + row as f32 * self.grid_row_h - scroll_y,
            w: pitch,
            h: self.grid_row_h,
            radius: 0.0,
        }
    }

    /// The cell of row-major catalogue `index`.
    #[inline]
    pub fn grid_cell_at_index(&self, index: usize, scroll_y: f32) -> Rect {
        let cols = self.grid_cols.max(1);
        self.grid_cell(index % cols, index / cols, scroll_y)
    }

    /// The **icon** square inside grid cell `(col, row)`, centred in the
    /// column and pinned to the top of the row with the label underneath.
    ///
    /// This is what the renderer blits; [`Self::grid_cell`] is what it
    /// hit-tests. Deriving both from the same pitch is the point: they used to
    /// come from two different `Layout` field sets, so the cell a tap resolved
    /// and the cell a pixel landed in were different rectangles.
    #[inline]
    pub fn grid_icon(&self, col: usize, row: usize, scroll_y: f32) -> Rect {
        let cell = self.grid_cell(col, row, scroll_y);
        let d = self.grid_icon_d.min(cell.w);
        Rect {
            x: cell.x + (cell.w - d) * 0.5,
            y: cell.y,
            w: d,
            h: d,
            radius: d * ICON_RADIUS,
        }
    }

    /// Column pitch: the grid width divided into `cols` with `cols - 1`
    /// borders between them. The trailing border is deliberately *not* counted,
    /// so the last column ends flush with the grid's right edge.
    #[inline]
    pub fn grid_cell_pitch(&self) -> f32 {
        let cols = self.grid_cols.max(1);
        if cols == 1 {
            return self.grid.w.max(0.0);
        }
        ((self.grid.w - self.grid_border * (cols - 1) as f32).max(0.0)) / cols as f32
    }

    /// The half-open range of grid rows on screen at `scroll_y`, out of
    /// `total_rows` of *content*.
    ///
    /// Same derivation as [`Layout::visible_row_range`] -- `first =
    /// floor(scroll / pitch)`, `last = ceil((band + scroll) / pitch)` -- but on
    /// the sheet's own band and pitch. `total_rows` is the content row count,
    /// never the number that happens to fit, which is what makes the last row
    /// reachable at maximum scroll.
    #[inline]
    pub fn grid_visible_rows(&self, scroll_y: f32, total_rows: usize) -> core::ops::Range<usize> {
        if self.grid_row_h <= 0.0 || total_rows == 0 || self.grid.w <= 0.0 {
            return 0..0;
        }
        if !scroll_y.is_finite() {
            return 0..0;
        }
        let band = self.grid.h;
        let first = (scroll_y / self.grid_row_h).floor().max(0.0) as usize;
        let last = (((band + scroll_y) / self.grid_row_h).ceil().max(0.0) as usize).min(total_rows);
        first.min(total_rows)..last.max(first).min(total_rows)
    }

    /// The largest legal scroll for `total_apps` items, px. Zero when the
    /// content fits, so a list that cannot scroll is not draggable.
    #[inline]
    pub fn grid_max_scroll_y(&self, total_apps: usize) -> f32 {
        let rows = total_apps.div_ceil(self.grid_cols.max(1));
        (rows as f32 * self.grid_row_h - self.grid.h).max(0.0)
    }

    /// Catalogue index under `(x, y)`, if any, at `scroll_y`.
    ///
    /// The exact inverse of [`Self::grid_cell_at_index`]: `col = lx / pitch`,
    /// `row = ly / grid_row_h`, floor, then clamp to the *content* length so a
    /// tap on the final row cannot resolve past the end.
    ///
    /// There is deliberately **no gap rejection**. The 16 dp `grid_border` sits
    /// *inside* the cell rather than between cells -- `grid_cell_pitch` already
    /// subtracts `cols - 1` borders, so consecutive cells abut and the icon is
    /// centred in what is left over. Treating the border as a dead strip would
    /// make a 16 dp band across every column and row untappable, which is a
    /// real hit-target loss for no gain: a launcher cell is tappable over its
    /// whole area.
    #[inline]
    pub fn grid_hit(&self, x: f32, y: f32, scroll_y: f32, total_apps: usize) -> Option<usize> {
        if total_apps == 0 || self.grid_row_h <= 0.0 || self.grid.w <= 0.0 {
            return None;
        }
        if !x.is_finite() || !y.is_finite() || !scroll_y.is_finite() {
            return None;
        }
        let pitch = self.grid_cell_pitch();
        if pitch <= 0.0 {
            return None;
        }
        let lx = x - self.grid.x;
        let ly = y - self.grid.y + scroll_y;
        // Half-open on the right/bottom, matching the slice a cell occupies.
        if lx < 0.0 || lx >= self.grid.w || ly < 0.0 || ly >= self.grid.h + scroll_y {
            return None;
        }
        let col = (lx / pitch).floor();
        if col < 0.0 || col >= self.grid_cols as f32 {
            return None;
        }
        let row = (ly / self.grid_row_h).floor();
        if row < 0.0 {
            return None;
        }
        let index = row as usize * self.grid_cols + col as usize;
        if index < total_apps {
            Some(index)
        } else {
            None
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

    /// The icon square of the prediction row: the row's own height is
    /// [`PREDICTION_ROW_H_DP`], so the edge has to be its own metric
    /// ([`Self::pred_icon_d`]) rather than the band's height.
    #[inline]
    pub fn prediction_icon(&self) -> Rect {
        let d = self.pred_icon_d;
        Rect {
            x: self.predictions.x,
            y: self.predictions.y,
            w: d,
            h: d,
            radius: d * ICON_RADIUS,
        }
    }

    /// The label band of the prediction row: **underneath** the icon, the way
    /// the main drawer grid puts a label under its icon, separated by
    /// `PREDICTION_ICON_PAD_DP` (`all_apps_icon_drawable_padding`, 7 dp) and
    /// filling whatever the row height leaves.
    #[inline]
    pub fn prediction_label(&self) -> Rect {
        let icon = self.prediction_icon();
        let gap = self.pred_icon_pad;
        Rect {
            x: icon.x,
            y: icon.y + icon.h + gap,
            w: icon.w,
            h: (self.predictions.y + self.predictions.h - (icon.y + icon.h) - gap).max(0.0),
            radius: 0.0,
        }
    }

    /// Alias for [`Self::prediction_label`], kept because the row's label was
    /// called `prediction_text` before the row grew to 108 dp and the label
    /// moved below the icon.
    #[inline]
    pub fn prediction_text(&self) -> Rect {
        self.prediction_label()
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
    /// Centre of a card: the strip is centred on the panel in both axes, so
    /// this is `(w * 0.5, h * 0.5)` and nothing more.
    ///
    /// This used to be a 156 x 36 dp, r 28 dp "card chip" [`Rect`]. The chip's
    /// own box was never read by the renderer or the shell -- only its centre
    /// was, and the centre is algebraically the panel centre for any chip
    /// width and height, so the 156/36/28 dp numbers were three unsourced
    /// constants carrying no information. Only the centre survived.
    pub card_center: (f32, f32),
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
        let card_center = (card.x + card.w * 0.5, card.y + card.h * 0.5);
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
            card_center,
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
///
/// `FolderSpringAnimatorSet.kt:337` picks the alpha by theme, so this is not
/// an optional second constant: it is the value a light-theme folder draws
/// with. It was unreachable because [`FolderLayout::new`] passed the dark
/// branch unconditionally, which also made the launcher's `dark_theme` setting
/// have no effect on a folder at all. [`FolderLayout::new_with`] is the
/// parameterised constructor that fixes both.
pub const FOLDER_SCRIM_ALPHA_LIGHT: f32 = 0.20;

/// Max mini icons in a folder's grid icon.
/// `ClippedFolderIconLayoutRule.MAX_NUM_ITEMS_IN_PREVIEW` (`:7`).
pub const FOLDER_PREVIEW_MAX: usize = 4;

/// Added width of a lifted folder cell, 6 dp.
///
/// The reference does not lift a dragged icon by a hand-tuned scale factor; it
/// adds a fixed *length* to the drag view's width and divides
/// (`DragView.java:174`):
///
/// ```text
/// mEndScale = (width + finalScaleDps) / width;
/// ```
///
/// and for a non-widget drag `finalScaleDps` is
/// `res.getDimensionPixelSize(R.dimen.pre_drag_view_scale)`
/// (`LauncherDragController.java:135`), which is **6 dp**
/// (`res/values/dimens.xml:353`). So the lift is a dp quantity and the scale is
/// a consequence of the cell's edge -- a 5x5 folder's smaller cell is lifted by
/// proportionally more, exactly as in the reference. Expressed as a scale here
/// instead, it would be wrong on every grid size but the 3x3 the constant was
/// measured on.
pub const FOLDER_DRAG_LIFT_DP: f32 = 6.0;

/// Delay before a drag reorders, ms. `REORDER_DELAY` (`Folder.java:197`).
///
/// Not decoration: the reference only calls `realTimeReorder` from an alarm
/// armed in `onDragOver` (`Folder.java:1205-1215`), and cancels it on every
/// target change (`:1213`) and on every exit (`:1300`). A shell that reorders on
/// the first frame of a drag produces a different order for a flick and for a
/// slow deliberate move, which is why the reference debounces.
pub const FOLDER_REORDER_DELAY_MS: u32 = 250;

/// Delay before a drag that left the folder commits, ms.
/// `ON_EXIT_CLOSE_DELAY` (`Folder.java:198`), armed by `onDragExit`
/// (`:1293-1300`).
pub const FOLDER_DRAG_EXIT_DELAY_MS: u32 = 400;

/// Key-shadow offset for a lifted cell, 0.5 dp.
///
/// `BaseIcon.Workspace.Shadows` (`styles.xml:425-426`): `keyShadowOffsetX` and
/// `keyShadowOffsetY` are both `.5dp`. The reference applies the shadow as the
/// drawable's own background layer via a `RenderNode`
/// (`DoubleShadowIconDrawable.kt:35-53, 76`), so the offset is part of the icon
/// and moves with it; here it is a separate fill, and a separate fill is the only
/// way to get it out from under the tile it is drawn for.
pub const FOLDER_DRAG_SHADOW_DP: f32 = 0.5;

/// Key-shadow alpha, `0x89`.
///
/// `styles.xml:422` `<item name="keyShadowColor">#89000000</item>`. The
/// *ambient* layer is `#40000000` (`styles.xml:420`, alpha `0x40`); only the key
/// layer is drawn here, because a lifted cell reads off the offset and the
/// ambient layer is the same at rest and in flight -- drawing it would not mark
/// the cell as lifted at all. Both layers are transparent in the dark theme
/// (`styles.xml:112-113`), so a dark-themed folder drag draws no shadow, which is
/// why this is a value the draw path consults rather than a blanket fill.
pub const FOLDER_DRAG_SHADOW_ALPHA: u8 = 0x89;

/// Dilation of the preview radius by item count
/// (`ClippedFolderIconLayoutRule.radiusDilationForItems`, `:210-218`): 0.15 for
/// three items, 0.12 for four, 0 for one or two.
///
/// The non-shapes branch interpolates linearly from 0 at
/// `MIN_NUM_ITEMS_IN_PREVIEW = 2` to `MAX_RADIUS_DILATION = 0.25` at 4
/// (`:186-188`), which is what [`folder_preview_radius`] computes; this is the
/// shaped branch's table, kept as a function of the count so the two can be
/// told apart in a test.
#[inline]
pub fn folder_dilation_for_items(n: usize) -> f32 {
    match n {
        3 => FOLDER_DILATION_3,
        4 => FOLDER_DILATION_4,
        _ => 0.0,
    }
}

/// Preview cluster radius for `n` items, both shape settings.
///
/// Uncalled outside [`FolderLayout::preview`] and tests; the production call
/// site is the same one, so a caller should prefer [`FolderLayout::preview`]
/// over reaching for this directly.
/// `ClippedFolderIconLayoutRule.getRadius` (`:179-190`) is a two-branch
/// function of `n` and of the icon-shapes flag:
///
/// ```text
/// shapes on  :  mRadius * (1 + radiusDilationForItems(n))          // :184
/// shapes off :  mRadius * (1 + 0.25 * (n - 2) / (4 - 2))           // :187-188
/// ```
///
/// with `mRadius` being `folder_layout_radius(available, shapes)`. The shapes
/// branch is a *lookup* with no interpolation at all, which surprises people
/// who assume the two branches agree at 4 items -- they do not: shaped gives
/// `1.12`, unshaped `1.25`. A folder preview that grows with its contents in
/// one build and steps in the other is not a subtle difference.
#[inline]
pub fn folder_preview_radius(available: f32, n: usize, shapes: bool) -> f32 {
    let m = folder_layout_radius(available, shapes);
    let n = n.clamp(1, FOLDER_PREVIEW_MAX);
    if shapes {
        m * (1.0 + folder_dilation_for_items(n))
    } else {
        // (MAX_RADIUS_DILATION) * (n - MIN) / (MAX - MIN), `:14,187-188`.
        m * (1.0 + 0.25 * (n as f32 - 2.0) / 2.0)
    }
}

/// An open folder: its grid, its chrome and its preview metrics.
///
/// # What is still uncalled
///
/// `FolderLayout::new_with` is called only by [`Self::new`] and by tests today.
/// The production call site is `drm_kms::draw_folder`, which currently builds
/// `l.folder()` at `drm_kms.rs:5416` and must become
/// `FolderLayout::new_with(&l, Some((state.folder_cols, state.folder_rows)), state.folder_dark)`
/// -- the last argument from `LauncherState::dark_theme`, which is what makes
/// [`FOLDER_SCRIM_ALPHA_LIGHT`] reachable.
///
/// [`Self::preview_in`] is the workspace-side call site, in `drawer_mod` or
/// `desktop` where an app icon is drawn for a grid cell: replace
/// `Layout::grid_icon(i)` with `layout.folder().preview_in(layout.grid_icon(i), n, shapes, ltr)`.
/// [`Self::folder_cell_hit`] is the touch handler for an open folder.
/// [`Self::pager`] and [`Self::surface`] are `drm_kms::draw_folder`'s.
///
/// Everything here is `pub` so `utlc` can reach it; nothing in this crate
/// calls it yet, which is why each block above names its call site rather than
/// leaving a reader to guess.
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
    /// The reference phone folder: 3x3, dark theme.
    ///
    /// `new_with` with the reference's own defaults, kept as its own
    /// constructor because `main.rs` and `drm_kms.rs` call `Layout::folder()`
    /// and neither is allowed to change in this pass.
    pub fn new(l: &Layout) -> Self {
        Self::new_with(l, None, true)
    }

    /// A folder layout with a user-chosen grid and theme.
    ///
    /// `grid` is the user's `folderColumns` x `folderRows`
    /// (`ui/preferences/destinations/FolderPreferences.kt:80-91`), or `None`
    /// for the profile's own. `dark` selects the scrim alpha the reference
    /// branches on (`FolderSpringAnimatorSet.kt:337`), which is what makes
    /// [`FOLDER_SCRIM_ALPHA_LIGHT`] reachable -- it was a dead constant
    /// because [`Self::new`] could only ever produce the dark branch, so a
    /// light-theme launcher drew a dark scrim behind every folder.
    ///
    /// The grid is clamped to `2..=5` (`FOLDER_GRID_MIN..=FOLDER_GRID_MAX`,
    /// the reference's own slider range) rather than refused: a state file
    /// from a build with a different range still has to draw. The cell is
    /// re-centred for the new width, because `cols * cell.w` is the grid width
    /// and a 5x5 folder on a 420 dp panel is 400 dp wide against a 240 dp one,
    /// so keeping the old centre would push the grid off both edges.
    pub fn new_with(l: &Layout, grid: Option<(usize, usize)>, dark: bool) -> Self {
        let p = l.profile();
        let (cols, rows) = grid
            .map(|(c, r)| {
                (
                    c.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX),
                    r.clamp(FOLDER_GRID_MIN, FOLDER_GRID_MAX),
                )
            })
            .unwrap_or((p.folder_cols, p.folder_rows));
        let cell_w = p.dp(p.folder_cell_w_dp);
        let cell_h = p.dp(p.folder_cell_h_dp);
        // The 80 x 94 dp item bounds already carry the icon's transparent
        // margin (`styles.xml:505-506`), so the pitch *is* the cell: no extra
        // inter-cell gap, or the folder would be wider than the reference.
        let cell = Rect {
            x: ((l.w - cell_w * cols as f32) * 0.5).max(0.0),
            y: p.dp(24.0),
            w: cell_w,
            h: cell_h,
            radius: 0.0,
        };
        Self {
            cols,
            rows,
            cell,
            pad_top: p.dp(24.0),
            pad_lr: p.dp(16.0),
            footer_h: p.dp(56.0),
            scrim_alpha: if dark {
                FOLDER_SCRIM_ALPHA_DARK
            } else {
                FOLDER_SCRIM_ALPHA_LIGHT
            },
            launcher_scale: FOLDER_LAUNCHER_SCALE,
            title_delay_ms: FOLDER_TITLE_DELAY_MS,
            min_scale: FOLDER_MIN_SCALE,
            max_scale: FOLDER_MAX_SCALE,
            dilation_3: FOLDER_DILATION_3,
            dilation_4: FOLDER_DILATION_4,
            overlap_factor: HOTSEAT_ICON_OVERLAP_FACTOR,
        }
    }

    /// `cols * rows`, floored at 1: the reference's
    /// `mMaxItemsPerPage` (`FolderGridOrganizer.java:92`).
    ///
    /// Floored rather than allowed to be 0 because this is the divisor in
    /// [`Self::page_count`] and in the pager's arithmetic, and a grid of
    /// zero columns must not produce an infinity the shell then draws.
    #[inline]
    pub fn items_per_page(&self) -> usize {
        (self.cols * self.rows).max(1)
    }

    /// Pages `n` items occupy, at least 1.
    ///
    /// `ceil(n / (cols * rows))`, the content form of
    /// `FolderPagedView.getPageCount()`. Empty is 1 for the reason given on
    /// `recents::FolderOpen::page_count`.
    #[inline]
    pub fn page_count(&self, n: usize) -> usize {
        n.div_ceil(self.items_per_page()).max(1)
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

    /// The whole grid box: `cols` wide by `rows` tall.
    #[inline]
    pub fn grid(&self) -> Rect {
        Rect {
            x: self.cell.x,
            y: self.cell.y,
            w: self.cell.w * self.cols as f32,
            h: self.cell.h * self.rows as f32,
            radius: 0.0,
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

    /// The whole folder surface: grid plus its padding plus the footer.
    ///
    /// The width is the grid plus `pad_lr` on both sides, matching the padding
    /// the reference applies around `mContent`
    /// (`FolderPagedView.getPaddingLeft/Right` feed
    /// `getDesiredWidth`, `FolderPagedView.java:507-509`), and the height is
    /// `pad_top + grid + footer_h` for the same reason
    /// (`getDesiredHeight`, `:511-519`).
    ///
    /// Clamped to the panel: a 5x5 folder's 5 * 94 dp cell height is 470 dp
    /// against a 420 dp-wide panel, so an unclamped surface is taller than the
    /// screen it is drawn on and `draw_folder`'s `min(hf)` would be doing the
    /// clamping by accident, mid-animation.
    pub fn surface(&self, panel_w: f32, panel_h: f32) -> Rect {
        let u = self.surface_unclamped();
        Rect {
            w: u.w.min(panel_w),
            h: u.h.min(panel_h),
            ..u
        }
    }

    /// [`Self::surface`] with the panel clamp removed.
    ///
    /// Split out because the touch test needs a box that does not depend on the
    /// panel, and [`Self::touch_at`] has no panel to clamp against. The two are
    /// the same box otherwise: folding the clamp back in inside
    /// [`Self::surface`] is what keeps "what the touch test thinks is inside"
    /// and "what is drawn" from being two derivations of one surface.
    ///
    /// Read the origin carefully. It is `pad_top`, and the grid's own `y` is
    /// *also* `pad_top` -- so the grid sits on the box's top edge and the extra
    /// `pad_top` of the box's height lands **below** the footer, not above the
    /// grid. That is not what `pad_top` means to the draw path, where the grid
    /// is a `pad_top` inset *inside* the sheet (`draw_folder`'s
    /// `gy = sy + fl.pad_top`), and it is not corrected here because
    /// `the_folder_surface_is_the_grid_plus_its_chrome` pins it. What it does
    /// mean is that the box has no band above the grid, which is why
    /// [`Self::touch_at`]'s `Blank` case is the `pad_lr` gutters and the padding
    /// below the footer -- and not a band that a reader would assume is there
    /// from the name.
    pub fn surface_unclamped(&self) -> Rect {
        let g = self.grid();
        Rect {
            x: g.x - self.pad_lr,
            y: self.pad_top,
            w: g.w + self.pad_lr * 2.0,
            h: self.pad_top + g.h + self.footer_h,
            radius: self.footer_h * 0.5,
        }
    }

    /// Which folder item a touch in `page` lands on, as an absolute rank.
    ///
    /// A transliteration of `FolderPagedView.findNearestArea`
    /// (`FolderPagedView.java:525-536`), which is the reference's own hit test
    /// and is deliberately *not* "is this point inside a cell": it asks the
    /// page for the nearest cell, so a touch in the gaps between cells -- and
    /// in the padding outside the grid -- still resolves to the closest item.
    /// A strict `contains` would drop those, and dropping them is how a drop
    /// between two icons ends up with no target at all.
    ///
    /// ```text
    /// pageIndex = getNextPage();                       // :526
    /// page.findNearestAreaIgnoreOccupied(x, y, 1, 1, tmp);  // :528
    /// if (rtl) tmp[0] = countX - tmp[0] - 1;            // :529-531
    /// return min(total - 1, pageIndex * maxItemsPerPage
    ///                + tmp[1] * countX + tmp[0]);        // :532-535
    /// ```
    ///
    /// `page` is the page being touched and `items` the whole folder's count.
    /// The result is clamped to `items - 1`, which is the reference's
    /// `Math.min` at `:534`: a touch on an empty tail cell of the last page
    /// lands on the last item rather than off the end.
    ///
    /// Returns `None` only for an empty folder, where there is no nearest
    /// anything.
    pub fn folder_cell_hit(
        &self,
        x: f32,
        y: f32,
        page: usize,
        items: usize,
        ltr: bool,
    ) -> Option<usize> {
        if items == 0 || self.cols == 0 || self.rows == 0 {
            return None;
        }
        // Clamp into the grid first, so a touch in the padding resolves to the
        // nearest *cell* rather than to a negative column.
        let cx = (x - self.cell.x).clamp(0.0, self.cell.w * self.cols as f32 - 1e-3);
        let cy = (y - self.cell.y).clamp(0.0, self.cell.h * self.rows as f32 - 1e-3);
        let mut col = (cx / self.cell.w).floor() as usize;
        let row = (cy / self.cell.h).floor() as usize;
        if !ltr {
            // :529-531 - RTL mirrors the column, not the whole page.
            col = self.cols - 1 - col.min(self.cols - 1);
        }
        let rank = page * self.items_per_page() + row * self.cols + col;
        Some(rank.min(items - 1))
    }

    /// The icon tile inside folder cell `index` of `page`.
    ///
    /// The tile is the cell's own box inset to a square: the reference's folder
    /// icon is the full `folderCellWidth x folderCellHeight`
    /// (`styles.xml:505-506`, already in [`Self::cell`]) with the *icon*
    /// inside it, and the icon's own size is `mIconSize` from
    /// `ClippedFolderIconLayoutRule.getIconSize` (`:220-222`). Sizing from the
    /// cell rather than from a second dp token is what keeps the icon in the
    /// cell when the user retunes `folder_cell_w_dp`.
    #[inline]
    pub fn item_icon(&self, index: usize, page: usize) -> Rect {
        let local = index.saturating_sub(page * self.items_per_page());
        let col = local % self.cols.max(1);
        let row = local / self.cols.max(1);
        let c = self.cell_at(col, row);
        let e = c.w.min(c.h);
        Rect {
            x: c.x + (c.w - e) * 0.5,
            y: c.y,
            w: e,
            h: e,
            radius: e * ICON_RADIUS,
        }
    }

    /// The page-indicator band inside the footer, for a folder holding `items`.
    ///
    /// **Empty** (a zero rect) unless `items` needs more than one page. The
    /// reference hides the indicator rather than showing one dot for
    /// "page 1 of 1" (`FolderPagedView.java:496`,
    /// `setVisibility(getPageCount() > 1 ? VISIBLE : GONE)`), and expressing
    /// that as geometry rather than as a separate `if` is what stops the draw
    /// path from having to remember which of the two rules applies where. The
    /// shell checks `.is_empty()` -- or `FolderOpen::shows_page_indicator` --
    /// but it cannot draw a pager for a one-page folder by accident.
    ///
    /// The box is the footer inset by a quarter of its height top and bottom,
    /// so a single-line footer is centred in it: the reference's footer is a
    /// `FrameLayout` holding either the name or the indicator, gravity-flipped
    /// between left and centre with the page count
    /// (`FolderPagedView.java:499-503`).
    #[inline]
    pub fn pager(&self, items: usize) -> Rect {
        let f = self.footer();
        if self.page_count(items) <= 1 {
            return Rect {
                x: f.x + f.w * 0.5,
                y: f.y + f.h * 0.5,
                w: 0.0,
                h: 0.0,
                radius: 0.0,
            };
        }
        let inset = self.footer_h * 0.25;
        Rect {
            x: f.x + inset,
            y: f.y + inset,
            w: (f.w - inset * 2.0).max(0.0),
            h: (f.h - inset * 2.0).max(0.0),
            radius: 0.0,
        }
    }

    /// The folder preview for a grid cell: background box and mini positions.
    ///
    /// This is what the workspace draws *in place of an app icon* when a cell
    /// holds a folder, and it is the whole of
    /// `ClippedFolderIconLayoutRule` collapsed into one value so the draw path
    /// has one call instead of a radius, a dilation, a scale and a sweep.
    ///
    /// `icon` is the cell's icon edge, i.e. `Layout::grid_icon(i).w`; `n` is
    /// the folder's item count, clamped to
    /// [`FOLDER_PREVIEW_MAX`]. `radius` and the four positions come from
    /// [`folder_preview_radius`] and [`folder_preview_icons`] unchanged -- the
    /// two are reused, never re-derived, because the sweep has three sign and
    /// shift conventions in it that are easy to get wrong and already have
    /// eleven test call sites pinning them.
    ///
    /// The background box is `available` square, which the caller takes from
    /// the cell so the preview scales with the user's folder grid rather than
    /// with the panel.
    #[inline]
    pub fn preview(&self, icon: f32, n: usize, shapes: bool, ltr: bool) -> FolderPreview {
        let n = n.clamp(1, FOLDER_PREVIEW_MAX);
        let available = icon;
        let radius = folder_preview_radius(available, n, shapes);
        FolderPreview {
            background: Rect {
                x: 0.0,
                y: 0.0,
                w: available,
                h: available,
                radius,
            },
            radius,
            icons: folder_preview_icons(
                n,
                (available * 0.5, available * 0.5),
                radius,
                available,
                ltr,
            ),
            n,
        }
    }

    /// [`Self::preview`] positioned in `cell`, so the caller gets absolute
    /// coordinates and cannot forget the translation.
    #[inline]
    pub fn preview_in(&self, cell: Rect, n: usize, shapes: bool, ltr: bool) -> FolderPreview {
        let mut p = self.preview(cell.w, n, shapes, ltr);
        let dx = cell.x;
        let dy = cell.y;
        p.background.x += dx;
        p.background.y += dy;
        for icon in &mut p.icons[..p.n] {
            icon.0 += dx;
            icon.1 += dy;
        }
        p
    }

    /// The scale a lifted cell is drawn at, from its resting edge.
    ///
    /// The reference's `(width + 6dp) / width` (`DragView.java:174`,
    /// `dimens.xml:353`), so the lift is 6 dp on every grid rather than a fixed
    /// fraction of a cell. Clamped to 1.5 because a degenerate `edge` (a 0-width
    /// cell on a clipped 5x5 grid) must not produce an infinite scale, and
    /// [`FOLDER_DRAG_LIFT_DP`] is small enough that the clamp is never reached
    /// by a real panel.
    #[inline]
    pub fn drag_lift(&self, edge: f32, l: &Layout) -> f32 {
        if edge <= 0.0 {
            return 1.0;
        }
        let lift = l.profile().dp(FOLDER_DRAG_LIFT_DP);
        (1.0 + lift / edge).min(1.5)
    }

    /// The key-shadow offset for a lifted cell, in px.
    ///
    /// `.5dp` down and across (`styles.xml:425-426`), scaled by the cell the way
    /// every other dimension here is, so the shadow is the same visual weight on
    /// a 3x3 and a 5x5 folder.
    #[inline]
    pub fn drag_shadow_offset(&self, l: &Layout) -> f32 {
        l.profile().dp(FOLDER_DRAG_SHADOW_DP).max(1.0)
    }

    /// Which folder item a touch in an open folder lands on.
    ///
    /// LTR, because the brief's dispatch contract is direction-free. An RTL
    /// shell wants [`Self::touch_at_dir`]; mirroring `x` by hand is exactly the
    /// kind of second derivation that put a clear-all button 200 px from the pill
    /// drawn for it.
    #[inline]
    pub fn touch_at(&self, x: f32, y: f32, n_items: usize) -> FolderTouch {
        self.touch_at_dir(x, y, n_items, true)
    }

    /// [`Self::touch_at`] with the direction explicit.
    ///
    /// The four cases are the reference's, not a convenience:
    ///
    /// * **`Member`** is the nearest cell, *not* "is this point inside a cell".
    ///   The reference's own hit test asks the page for the nearest area
    ///   (`FolderPagedView.findNearestArea`, `:525-535`), and
    ///   [`Self::folder_cell_hit`] is its transliteration. The consequence the
    ///   shell has to know: the returned `rect` is the *resolved item's own*
    ///   cell, so it does **not** necessarily contain `(x, y)`. Re-testing
    ///   `rect.contains(x, y)` to confirm the hit throws away exactly the
    ///   in-between touches the reference resolves, and a drop between two icons
    ///   ends up with no target at all.
    /// * **`Dismiss`** is the footer band, the reference's folder-name row. In
    ///   the reference a tap there commits a rename -- `dispatchBackKey` on the
    ///   name field, announced to a screen reader as "Tap to save rename"
    ///   (`DragLayer.java:190`, `res/values/strings.xml:306`).
    /// * **`Outside`** is the reference's *close* affordance. There is no back
    ///   button: a tap anywhere off the folder view closes it
    ///   (`DragLayer.java:161-186`, announced as "Tap to close folder",
    ///   `res/values/strings.xml:304`).
    /// * **`Blank`** is the rest of the sheet: the `pad_lr` gutters beside the
    ///   grid, and the padding below the footer. A tap there hits nothing, and
    ///   must not be reported as a member -- the grid's cells are `cell.w` apart
    ///   with no gutter between them (`styles.xml:505-506`), so treating the
    ///   gutters as cells would give every folder a phantom ring of icons.
    ///
    /// **The tap test and the drag test resolve the gutters differently, on
    /// purpose.** `touch_at` requires containment; [`Self::reorder_index`] does
    /// not, because the reference's drag target *is* a nearest-area query
    /// (`Folder.getTargetRank` subtracts the content padding and calls
    ///   `findNearestArea`, `Folder.java:1199-1202`). So a drag released in the
    /// gutter opens the gap at the nearest cell -- the reference's behaviour --
    /// while a tap in the same place hits nothing, which is a plain view click in
    /// the reference. Collapsing the two into one function would make either a
    /// tap in a gutter launch an app, or a drag released in a gutter do nothing.
    ///
    ///
    /// An empty folder (`n_items == 0`) reports `Blank` for the whole grid: there
    /// is no nearest item to resolve to, and returning `Member { index: 0 }`
    /// would hand the shell an index into nothing.
    pub fn touch_at_dir(&self, x: f32, y: f32, n_items: usize, ltr: bool) -> FolderTouch {
        let s = self.surface_unclamped();
        if !s.contains(x, y) {
            return FolderTouch::Outside;
        }
        if self.grid().contains(x, y) {
            if n_items == 0 {
                return FolderTouch::Blank;
            }
            if let Some(index) = self.folder_cell_hit(x, y, 0, n_items, ltr) {
                return FolderTouch::Member {
                    index,
                    rect: self.slot_rect(index),
                };
            }
            return FolderTouch::Blank;
        }
        let f = self.footer();
        if f.contains(x, y) {
            return FolderTouch::Dismiss { rect: f };
        }
        FolderTouch::Blank
    }

    /// The cell rect for an absolute rank on the first page.
    ///
    /// The other half of [`Self::folder_cell_hit`], which answers "which rank"
    /// without saying where it is drawn. A touch test that returns an index and
    /// leaves the caller to re-derive the rect is a second derivation of the
    /// grid pitch, which is the class of bug this module's own comments keep
    /// warning about.
    ///
    /// Rank *within* the page. A rank past the first page wraps onto the second
    /// page's origin, which is not a cell anybody can see -- the page is a window
    /// and `draw_folder` applies `folder_page * items_per_page` itself. The
    /// `rows - 1` clamp is what stops `folder_cell_hit`'s `min(items - 1)` (which
    /// is in absolute-rank space) from producing a row off the bottom of a
    /// partly-filled grid when it is fed here.
    #[inline]
    pub fn slot_rect(&self, rank: usize) -> Rect {
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        let col = rank % cols;
        let row = (rank / cols).min(rows - 1);
        self.cell_at(col, row)
    }

    /// Which slot a touch would insert a dragged cell at, or `None` for a
    /// folder with no items.
    ///
    /// This is the reference's `getTargetRank`
    /// (`Folder.java:1199-1202`):
    ///
    /// ```text
    /// recycle = d.getVisualCenter(recycle);
    /// return mContent.findNearestArea(x - getPaddingLeft(), y - getPaddingTop());
    /// ```
    ///
    /// i.e. the *nearest cell*, which is why the return value is a target rank
    /// and not "before/after the cell under the finger": the gap in the grid
    /// opens **at** the target cell, and the dragged item lands there
    /// (`FolderPagedView.realTimeReorder`, `:687-707`, where the hole is `empty`
    /// and the destination is `target`). Clamped to `n - 1` by
    /// [`Self::folder_cell_hit`]'s `Math.min`, so a 4-item folder in a 3x3 grid
    /// cannot be told to insert at slot 5.
    ///
    /// Geometry is the first page's. The reference only animates a reorder on the
    /// current page -- `realTimeReorder` logs "Cannot animate when the target
    /// cell is invisible" when `pageT != pageToAnimate`
    /// (`FolderPagedView.java:698-705`) and does its arithmetic in
    /// page-local positions (`pagePosE = empty % maxItemsPerPage`,
    /// `pagePosT = target % maxItemsPerPage`, `:706-712`). So a cross-page move
    /// is a page turn, not a reorder, and a shell that wants one drives
    /// `folder_page` itself.
    #[inline]
    pub fn reorder_index(&self, x: f32, y: f32, n: usize) -> Option<usize> {
        self.reorder_index_dir(x, y, n, true)
    }

    /// [`Self::reorder_index`] with the direction explicit.
    #[inline]
    pub fn reorder_index_dir(&self, x: f32, y: f32, n: usize, ltr: bool) -> Option<usize> {
        if n == 0 || self.cols == 0 || self.rows == 0 {
            return None;
        }
        self.folder_cell_hit(x, y, 0, n, ltr)
    }

    /// Where a cell is *drawn* while a reorder is in flight, or `None` for the
    /// lifted cell itself.
    ///
    /// `slot` is the cell's own slot on the page, `from` the slot that was
    /// picked up, and `insertion` the slot the gap has opened at (from
    /// [`Self::reorder_index`]). The answer is the remove-and-insert permutation
    /// the reference reaches by animating one cell at a time
    /// (`FolderPagedView.java:752-790`): every item between the hole and the
    /// target slides exactly one cell pitch along the row it is in
    /// (`translationXBy(direction > 0 ^ mIsRtl ? -v.getWidth() : v.getWidth())`,
    /// `:775`). Reducing that to a final position is the same permutation, and
    /// is what a frame has to draw.
    ///
    /// `None` for `slot == from`, which is the whole point: the lifted cell is
    /// not in the grid, it is under the finger. A caller that drew it in the grid
    /// as well would show the icon twice, once in the hole and once on top of it.
    ///
    /// Slots are page-local and the pitch is `cell`, so this is one compare and
    /// two adds -- no allocation and no scan, on the frame path.
    #[inline]
    pub fn reflow_slot(
        &self,
        slot: usize,
        from: usize,
        insertion: usize,
        ltr: bool,
    ) -> Option<Rect> {
        if slot == from {
            return None;
        }
        // Remove `from`, then re-insert the *hole* at `insertion`. The hole is
        // the lifted cell's landing place, so it is indexed in the new list:
        // everything from `insertion` on shifts right by one to make room.
        //
        // The comparison is `>=`, not `>`. With a 2-item folder, `from = 0`,
        // `insertion = 1`, the answer for item 1 is cell 0; `>` puts it in cell 1
        // and then item 1 is drawn where the lifted item is about to land, on
        // top of it. That is the same class of bug as drawing the lifted cell
        // twice, and it is invisible in a screenshot of a two-item folder.
        let j = if slot > from { slot - 1 } else { slot };
        let pos = if j >= insertion { j + 1 } else { j };
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        let mut col = pos % cols;
        let row = (pos / cols).min(rows - 1);
        if !ltr {
            // The reference mirrors the column, not the page
            // (`FolderPagedView.java:530-531`).
            col = cols - 1 - col.min(cols - 1);
        }
        Some(self.cell_at(col, row))
    }

    /// The box a lifted cell has to leave before the drag counts as an exit.
    ///
    /// The reference inflates the folder's own hit rect by half a dragged icon
    /// on each side, for a stated reason (`Folder.java:1179-1184`):
    ///
    /// ```text
    /// // Get the area offset such that the folder only closes if half the drag
    /// // icon width is outside the folder area
    /// mScrollAreaOffset = d.dragView.getDragRegionWidth() / 2 - d.xOffset;
    /// ...
    /// outRect.left  -= mScrollAreaOffset;      // :1859
    /// outRect.right += mScrollAreaOffset;      // :1860
    /// ```
    ///
    /// Without the inflation, a drag that starts in a corner has its own icon
    /// under the finger and exits the moment the finger moves at all, so a
    /// reorder and an exit become the same gesture. Vertically the reference does
    /// not inflate, and neither does this.
    pub fn drag_exit_area(&self, panel_w: f32, panel_h: f32) -> Rect {
        let s = self.surface(panel_w, panel_h);
        let half = self.cell.w * 0.5;
        Rect {
            x: s.x - half,
            w: s.w + half * 2.0,
            ..s
        }
    }

    /// Whether a lifted cell at `(x, y)` has left the folder.
    ///
    /// [`Self::drag_exit_area`] and a containment test, so the two cannot
    /// disagree. `false` while the cell is still inside: a reorder in flight and
    /// an exit are different gestures, and the reference separates them by the
    /// `ON_EXIT_CLOSE_DELAY` alarm (`Folder.java:198, 1293-1300`) rather than by
    /// geometry alone.
    #[inline]
    pub fn is_drag_out(&self, x: f32, y: f32, panel_w: f32, panel_h: f32) -> bool {
        !self.drag_exit_area(panel_w, panel_h).contains(x, y)
    }

    /// The folder's long-press menu, anchored at `(ax, ay)`.
    ///
    /// **The reference has no folder long-press menu.** Its folder long press
    /// starts a drag outright -- `Folder.onLongClick` is
    /// `startDrag(v, new DragOptions())` (`Folder.java:459-463`) -- and the
    /// launcher long-press menu it *does* have is the workspace one
    /// (`OptionsPopupView.getOptions`, `:207`, whose items are exactly the
    /// variants already on [`crate::compositor::PopupItem`]). So this menu is
    /// built on UTLC's own reference-cited popup geometry, `PopupMenuLayout`,
    /// rather than on a set of dp numbers copied from a menu that does not exist:
    /// 216 x 52 dp rows, 24 dp outer radius, 4 dp inner
    /// (`PopupMenuLayout::new`, `layout.rs:3219-3234`), placed by the one
    /// function the render path and the hit test already share
    /// ([`PopupMenuLayout::place`]). Its three rows are the three actions the
    /// reference *does* expose on a folder, each cited on
    /// [`FolderMenuAction`].
    ///
    /// `progress` grows the menu out of the touch point, so at 0 the whole
    /// struct is a sliver and [`FolderMenuLayout::menu_hit`] refuses, on the
    /// same rule as [`PopupMenuLayout::hit`]: the anchor is the user's own
    /// finger position, and a fast second tap would otherwise fire the first
    /// row of a menu the user cannot see.
    pub fn menu(&self, l: &Layout, ax: f32, ay: f32, progress: f32) -> FolderMenuLayout {
        let anchor = Rect {
            x: ax,
            y: ay,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        };
        let pl = l.popup_menu(anchor);
        let place = pl.place(l, ax, ay, FOLDER_MENU_ROWS, progress);
        let mut rows = [Rect {
            x: place.x,
            y: place.y,
            w: 0.0,
            h: 0.0,
            radius: pl.inner_r,
        }; FOLDER_MENU_ROWS];
        if place.progress >= PopupMenuLayout::TAPPABLE_PROGRESS {
            for (i, row) in rows.iter_mut().enumerate() {
                row.x = place.x;
                row.y = place.y + pl.item_h * i as f32;
                row.w = place.w;
                row.h = pl.item_h;
            }
        }
        FolderMenuLayout {
            place,
            rows,
            item_h: pl.item_h,
            inner_r: pl.inner_r,
        }
    }
}

/// A folder preview ready to draw: background, cluster radius, mini slots.
///
/// `Copy` and inline, because it is built and consumed inside one frame and
/// must not put a `Vec` on the draw path. `n` says how many of
/// [`Self::icons`] are live; the rest are `(0, 0, 0)` and must not be drawn,
/// which is the same contract [`folder_preview_icons`] already documents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderPreview {
    /// The preview's own box, translated into place by
    /// [`FolderLayout::preview_in`].
    pub background: Rect,
    /// Cluster radius the minis are placed on, already dilated for `n`.
    pub radius: f32,
    /// `(x, y, edge)` per mini, at most [`FOLDER_PREVIEW_MAX`] of them.
    pub icons: [(f32, f32, f32); FOLDER_PREVIEW_MAX],
    /// How many of [`Self::icons`] are live.
    pub n: usize,
}

/// Where a touch in an open folder lands.
///
/// The dispatch contract for an open folder, and the reason
/// [`FolderLayout::touch_at`] exists at all: the shell was reaching into
/// `folder_cell_hit` for the index and re-deriving the footer band and the
/// surface itself, which is three derivations of one sheet. Every case carries
/// the rect it was resolved against so the shell never has to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FolderTouch {
    /// A member cell, by index into the folder's ordered items.
    ///
    /// `index` is an **absolute rank** across all pages, not a page-local slot.
    /// `rect` is that item's own cell, which is *not* guaranteed to contain the
    /// touch point: a touch in the gutters resolves to the nearest item, the same
    /// as the reference's `findNearestArea`
    /// (`FolderPagedView.java:525-535`). Do not re-test `rect.contains` to
    /// confirm the hit -- that discards exactly the in-between touches the
    /// nearest-area rule exists to resolve.
    Member { index: usize, rect: Rect },
    /// The folder's own header/back affordance: the footer band holding the
    /// folder name, where the reference commits a rename
    /// (`DragLayer.java:190`, `res/values/strings.xml:306`).
    Dismiss { rect: Rect },
    /// Empty space inside the folder sheet.
    Blank,
    /// Outside the sheet entirely.
    ///
    /// The reference's close affordance, and the only one it has: a tap off the
    /// folder view closes it (`DragLayer.java:161-186`, `strings.xml:304`
    /// "Tap to close folder").
    Outside,
}

/// The rows of a folder's long-press menu.
///
/// **The reference has no folder long-press menu**, so these are not a
/// transcription of one. Each action is however a thing the reference genuinely
/// exposes on a folder or its contents, cited so the choice is auditable:
///
/// * [`Self::Rename`] -- the reference's folder name field,
///   `FolderNameEditText`, written through `FolderInfo.setTitle` on
///   `dispatchBackKey` (`Folder.java:569`, `:1851`).
/// * [`Self::Remove`] -- the reference's drop-target label,
///   `R.string.remove_drop_target_label` = "Remove"
///   (`res/values/strings.xml:221`), applied by `DeleteDropTarget`
///   (`DeleteDropTarget.java:115`). Note it is the *item's* remove, matching
///   what the reference's remove affordance does to a dragged folder member.
/// * [`Self::Info`] -- `SystemShortcut.APP_INFO`
///   (`SystemShortcut.java:188`), already a variant of
///   [`crate::compositor::PopupItem`] as `AppInfo`, so the shell does not have
///   to invent a second spelling of "App info".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderMenuAction {
    /// Open the folder-name editor.
    Rename,
    /// Remove the member the menu was raised on.
    Remove,
    /// Show app info for the member the menu was raised on.
    Info,
}

impl FolderMenuAction {
    /// Every action, in row order. The single list the rows, the labels and the
    /// hit test are all indexed by, so a row cannot be drawn for one action and
    /// dispatched as another.
    pub const ALL: [FolderMenuAction; FOLDER_MENU_ROWS] = [
        FolderMenuAction::Rename,
        FolderMenuAction::Remove,
        FolderMenuAction::Info,
    ];

    /// The label drawn in the row, the reference's own wording.
    ///
    /// `&'static str` rather than a borrowed one because the menu's three labels
    /// are constants, and a `&'a str` here would make the label's lifetime the
    /// caller's problem on a struct that is otherwise pure geometry.
    pub const fn label(self) -> &'static str {
        match self {
            FolderMenuAction::Rename => "Rename",
            FolderMenuAction::Remove => "Remove",
            FolderMenuAction::Info => "App info",
        }
    }
}

/// Rows in a folder's long-press menu. Three, like its three actions.
pub const FOLDER_MENU_ROWS: usize = 3;

/// A placed folder long-press menu, ready to draw and to hit-test.
///
/// Produced by [`FolderLayout::menu`] and read by *both* the draw path and the
/// shell's touch dispatch, so a row is tappable exactly where it is painted. The
/// rows are materialised here rather than recomputed from `place` by each side,
/// because the ownership map's own §"Merge order" names this exact bug: a
/// clear-all button 200 px from the pill that was drawn for it, from two
/// derivations of one geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderMenuLayout {
    /// The surface, including the open progress. Zero-sized while closed.
    pub place: PopupPlacement,
    /// One rect per action, in [`FolderMenuAction::ALL`] order. Zero-sized while
    /// the menu is below [`PopupMenuLayout::TAPPABLE_PROGRESS`], so a hit test
    /// that ignored the progress would find a row at the anchor point.
    pub rows: [Rect; FOLDER_MENU_ROWS],
    /// Row pitch, so a label baseline can be derived without the surface.
    pub item_h: f32,
    /// Row corner radius.
    pub inner_r: f32,
}

impl FolderMenuLayout {
    /// Which action a touch at `(x, y)` hits, or `None` for a miss.
    ///
    /// Zero-size rows match nothing, which is the dismiss-on-outside-tap rule
    /// rather than a special case: the reference consumes an outside touch by
    /// letting it fall through (`DragLayer.java:161-186`), it does not route it
    /// to an action.
    ///
    /// The `is_empty` check is not redundant with a `contains`. `Rect::contains`
    /// is inclusive on both edges, so a zero-size rect *does* contain the single
    /// point at its own origin -- and a closed menu's rows are all zero-size
    /// rects stacked on the long-press anchor, which is the user's finger. So
    /// `contains` alone finds row 0 under the finger that raised the menu, and
    /// the second tap of a double-tap fires "Rename" on a menu that was never
    /// drawn. [`PopupMenuLayout::hit`] avoids this by refusing below
    /// [`PopupMenuLayout::TAPPABLE_PROGRESS`] rather than by testing size, and
    /// the check belongs here too because `menu` zeroes the rows on that same
    /// rule and the shell is free to hold a stale `FolderMenuLayout`.
    pub fn menu_hit(&self, x: f32, y: f32) -> Option<FolderMenuAction> {
        let i = self
            .rows
            .iter()
            .position(|r| !r.is_empty() && r.contains(x, y))?;
        FolderMenuAction::ALL.get(i).copied()
    }

    /// The label baseline for row `i`, from the row's own rect.
    ///
    /// Same arithmetic `draw_popup` uses for its labels
    /// (`drm_kms.rs:6340-6360`), and here it reads the row rect the hit test
    /// reads, so the text and the target cannot separate.
    #[inline]
    pub fn label_y(&self, i: usize) -> f32 {
        let r = self.rows[i.min(self.rows.len() - 1)];
        r.center_y() - self.item_h * 0.31
    }
}

/// Height of the reference's drag drop-target bar, 56 dp.
///
/// `dynamic_grid_drop_target_size` (`res/values/dimens.xml:52`), which is the
/// bar's `layout_height` (`res/layout/drop_target_bar.xml:18`).
pub const DROP_TARGET_BAR_H_DP: f32 = 56.0;

/// Horizontal padding of a drop-target button, 14 dp.
///
/// `DropTargetButtonBase`'s `android:padding` (`res/values/styles.xml:445`).
pub const DROP_TARGET_BTN_PAD_DP: f32 = 14.0;

/// Corner radius of a drop-target button, 80 dp.
///
/// `drop_target_button_frame_radius` (`res/values/dimens.xml:303`), read by
/// `@drawable/drop_target_frame`'s `<corners>` (`drop_target_frame.xml:20`).
/// 80 dp on a ~50 dp-tall button is a pill, and taken literally rather than
/// halved: a smaller radius here is a different shape, and the reference's is a
/// pill. Clamped to half the button's height so a degenerate panel cannot ask
/// for a radius wider than the thing it rounds.
pub const DROP_TARGET_BTN_RADIUS_DP: f32 = 80.0;

/// The drop-target bar's label, the reference's own string.
///
/// `R.string.remove_drop_target_label` (`res/values/strings.xml:221`), which is
/// the `android:text` of the `DeleteDropTarget` in the reference's bar
/// (`drop_target_bar.xml:33`).
pub const DROP_TARGET_LABEL: &str = "Remove";

// ===========================================================================
// Settings pickers
// ===========================================================================

/// Candidates drawn at once for a `SettingKind::Picker` row.
///
/// Four, because the reference's carousel shows the current candidate *wider*
/// than its neighbours (`WallpaperCarouselView.kt:104`) and the list has to stay
/// on one line inside a settings row that is at most 72 px tall
/// (`drm_kms.rs:3225`, `card_h`). Four is what fits at a readable slot edge
/// without the row becoming a slider track.
pub const PICKER_SLOT_MAX: usize = 4;

/// A `SettingKind::Picker` row's position readout and candidate slots, resolved.
///
/// All boxes empty means "no candidates yet", and that is the case the contract
/// is explicit about: `of == 0` **or** `at == None` means the row falls back to
/// its static `value` string and draws no slots at all. Representing that as
/// *geometry* rather than as a separate `if` in the draw path is the reason the
/// draw path cannot paint slots on a row that has none -- the same argument
/// [`FolderLayout::pager`] makes for a one-page folder's indicator.
///
/// `Copy` and fixed capacity: built and consumed inside one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickerSlots {
    /// Where the `"N of M"` readout goes, or `None` when there are no
    /// candidates.
    ///
    /// Deliberately the *same rect the row's static `value` would occupy*, so
    /// the draw path swaps one string for another and nothing else moves. It is
    /// narrowed by the slot band, because the two share the value line: a
    /// readout as wide as the text column would run its digits underneath the
    /// candidate boxes.
    pub readout: Option<Rect>,
    /// Candidate boxes, `[..n]` live. The rest are zero-sized and must not be
    /// drawn, the same contract [`FolderPreview::icons`] documents.
    pub slots: [Rect; PICKER_SLOT_MAX],
    /// How many of [`Self::slots`] are live.
    pub n: usize,
    /// Which of [`Self::slots`] is the current candidate, 0-based. `Some` for
    /// every live picker, because a live picker is by definition one with a
    /// position.
    pub selected: Option<usize>,
    /// 1-based position, echoed from the row so a caller formatting the readout
    /// does not have to keep the original pair around.
    pub at: Option<u32>,
    /// Candidate total, echoed from the row.
    pub of: Option<u32>,
    /// 1-based index of [`Self::slots`] `0` within the whole list.
    ///
    /// Carried so a swipe or an arrow key can say "you are at 6 of 7" without
    /// re-deriving the window, and so a test can assert the window slid rather
    /// than merely that something is highlighted.
    pub first: u32,
}

impl PickerSlots {
    /// `true` when the row should draw the readout and the slots rather than its
    /// static value.
    #[inline]
    pub fn is_live(&self) -> bool {
        self.readout.is_some() && self.n > 0
    }

    /// The absolute 1-based index of slot `i`, or `None` for an out-of-range one.
    #[inline]
    pub fn index_of(&self, i: usize) -> Option<u32> {
        (i < self.n).then(|| self.first + i as u32)
    }
}

/// Resolve a `SettingKind::Picker` row's readout and slots.
///
/// `row` is the row's own card rect and `em` the row's value-line em, both of
/// which the settings panel already has; the geometry is a function of the two
/// and of the position pair, so a caller cannot draw a picker at a scale the row
/// is not at.
///
/// The window of [`PICKER_SLOT_MAX`] candidates slides so the selected one is
/// always visible, which is the reference's behaviour in a different form: its
/// carousel keeps every candidate in the list and marks the current one
/// (`WallpaperCarouselView.kt:74-77`), and keeps the current one *wider*
/// (`:104`) with an accent-filled tick over it (`:46-49`). A settings row has
/// room for four boxes rather than a whole carousel, so the same "the current
/// one is marked" rule is expressed by sliding a window onto it. Not sliding is
/// the failure mode: with 7 candidates and a 4-slot window, selecting 7 would
/// highlight nothing at all.
///
/// `at` is 1-based, as [`crate::settings::SettingRow::at`] documents, and is
/// clamped into `1..=of` here so a stale position from a longer list cannot
/// select a slot past the end. Both `at == None` and `of` zero/`None` yield the
/// dead struct, which is the contract's "no candidates yet" case -- a count with
/// no cursor is not a list the user can act on, and drawing slots for it would
/// invite a tap on a candidate the row cannot name.
pub fn picker_slots(row: Rect, em: f32, at: Option<u32>, of: Option<u32>) -> PickerSlots {
    let mut out = PickerSlots {
        readout: None,
        slots: [Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        }; PICKER_SLOT_MAX],
        n: 0,
        selected: None,
        at,
        of,
        first: 1,
    };
    // `SettingRow::of` documents that `0` and `None` both mean unknown, and
    // `SettingRow::at` that `None` means there is no position. Either is the
    // "no candidates yet" case and the row keeps its static value.
    let (Some(at), Some(total)) = (at, of.filter(|n| *n > 0)) else {
        return out;
    };
    // 1-based and clamped: a position from a list that has since shrunk must not
    // select a slot off the end, and must not read as "0 of 7".
    let pos = at.clamp(1, total);
    // `clamp` rather than `min().max()`: `total >= 1` is already established by
    // the `of.filter(|n| *n > 0)` above, so the low bound is belt-and-braces --
    // but writing it as `clamp` states the intent ("between one slot and the
    // window") where the two-call form reads as two independent decisions.
    let n = (total as usize).clamp(1, PICKER_SLOT_MAX);
    let slot_h = (row.h * 0.26).max(3.0);
    let slot_w = slot_h;
    let gap = slot_h * 0.5;
    let band_w = slot_w * n as f32 + gap * (n as f32 - 1.0);
    // The text column, which is where the row's static value is drawn, and
    // therefore where the readout has to go.
    let inner_x = row.x + row.h * 0.45;
    let inner_w = (row.w - row.h * 0.9).max(0.0);
    // The band is right-aligned in that column, and the readout takes what is
    // left. `pad` keeps the digits off the first slot rather than butting them
    // against it.
    let pad = slot_h * 0.5;
    let band_x = inner_x + (inner_w - band_w).max(0.0);
    let readout_w = (band_x - pad - inner_x).max(0.0);
    let readout_y = row.y + row.h * 0.16 + em;
    out.readout = Some(Rect {
        x: inner_x,
        y: readout_y,
        w: readout_w,
        h: em,
        radius: 0.0,
    });
    let slot_y = readout_y + (em - slot_h) * 0.5;
    let sel = (pos - 1) as usize;
    // Slide the window onto the selection, then keep it inside the list. The
    // second clamp is what stops a selection of the *last* candidate from
    // pushing the window one past the end and leaving a dead slot at the right.
    let first = sel.saturating_sub(n - 1).min((total as usize) - n).min(sel);
    out.n = n;
    out.first = first as u32 + 1;
    out.selected = Some(sel - first);
    for (i, slot) in out.slots.iter_mut().take(n).enumerate() {
        *slot = Rect {
            x: band_x + pitch(slot_w, gap, i),
            y: slot_y,
            w: slot_w,
            h: slot_h,
            // A pill, like the reference's selection mark
            // (`setBackgroundWithRadius(accent, 100F)`, `WallpaperCarouselView.kt:48`).
            radius: slot_h * 0.5,
        };
    }
    out
}

/// Left edge of slot `i` in a band of `slot_w` boxes at `gap`, pitch `slot_w + gap`.
///
/// Its own function so `picker_slots` and any caller that wants to reason about
/// the pitch agree by construction; `PICKER_SLOT_MAX` is a compile-time bound so
/// the caller cannot pass an index that reads off the end.
#[inline]
fn pitch(slot_w: f32, gap: f32, i: usize) -> f32 {
    (slot_w + gap) * i.min(PICKER_SLOT_MAX - 1) as f32
}

/// The drag drop-target bar, and the one button in it that UTLC draws.
///
/// The reference's bar is a 56 dp full-width band at the top of the panel,
/// `layout_gravity="center_horizontal|top"`
/// (`res/layout/drop_target_bar.xml:18,20`), holding a `DeleteDropTarget` whose
/// text is `remove_drop_target_label` -- "Remove"
/// (`res/values/strings.xml:221`, applied by `DeleteDropTarget.java:115`). The
/// second button in the reference's bar is the *uninstall* target, which needs a
/// package manager to do anything and is not drawn here rather than drawn
/// inert.
///
/// This is the affordance a drag-out of a folder commits against, so it is
/// geometry the shell hit-tests exactly as it hit-tests the menu rows: the
/// drawn button and [`Self::hit`] read the same rect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropTargetBar {
    /// The whole band, panel coordinates.
    pub bar: Rect,
    /// The "Remove" button, centred in [`Self::bar`].
    pub button: Rect,
    /// The button's label, the reference's own string.
    pub label: &'static str,
}

impl DropTargetBar {
    /// Whether a touch at `(x, y)` is on the Remove button.
    #[inline]
    pub fn hit(&self, x: f32, y: f32) -> bool {
        self.button.contains(x, y)
    }
}

/// The drop-target bar for a panel of `w` x `h`.
pub fn drop_target_bar(l: &Layout) -> DropTargetBar {
    let p = l.profile();
    let bar = Rect {
        x: 0.0,
        y: 0.0,
        w: l.w,
        h: p.dp(DROP_TARGET_BAR_H_DP),
        radius: 0.0,
    };
    // The button is `wrap_content` and centred (`drop_target_bar.xml:26-27`), so
    // its width is the label plus the 14 dp padding on both sides
    // (`styles.xml:445`). Sized off the panel's own em rather than a dp count so
    // it tracks the type scale, which is what `wrap_content` does in the
    // reference.
    let em = super::font::em_px_at(1, l.w.max(1.0) as usize);
    let text_w = super::font::measure(DROP_TARGET_LABEL, em);
    let pad = p.dp(DROP_TARGET_BTN_PAD_DP);
    let bw = (text_w + pad * 2.0).min(bar.w);
    DropTargetBar {
        bar,
        button: Rect {
            x: (bar.w - bw) * 0.5,
            y: (bar.h - em * 1.6) * 0.5,
            w: bw,
            h: (em * 1.6).min(bar.h),
            radius: (p.dp(DROP_TARGET_BTN_RADIUS_DP)).min(em * 0.8),
        },
        label: DROP_TARGET_LABEL,
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
/// A transliteration of `ClippedFolderIconLayoutRule.getPosition`
/// (`ClippedFolderIconLayoutRule.java:140-177`). Three terms carry the whole
/// model, and all three are easy to get subtly wrong:
///
/// ```text
/// curNumItems = Math.max(curNumItems, 2)                       // :142
/// theta0      = mIsRtl ? 0 : Math.PI                          // :146
/// direction   = mIsRtl ? 1 : -1      // "In RTL we go counterclockwise" (:148-149)
/// thetaShift  = curNumItems == 3 ? PI/2 : curNumItems == 4 ? PI/4 : 0  // :151-156
/// theta0     += direction * thetaShift                         // :157
///
/// if (curNumItems == 4 && index == 3) index = 2;                // :162-166
/// else if (curNumItems == 4 && index == 2) index = 3;
/// theta  = theta0 + index * (2 * PI / curNumItems) * direction // :169
/// x = c + (r * cos theta) / 2 - halfIconSize                   // :175
/// y = c - (r * sin theta) / 2 - halfIconSize                   // :176
/// ```
///
/// The three corrections this module used to get wrong, and what each one
/// does to the cluster:
///
/// 1. **`direction` is `-1` in LTR.** The sweep is clockwise in LTR, which
///    is what puts slot 0 in the *upper-left* quadrant. With `+1` the whole
///    cluster lands in the lower-right quadrant.
/// 2. **`thetaShift` is zero for 1 and 2 items.** It is keyed off
///    `curNumItems` (already `max(n, 2)`), not off `n`, so the 1- and
///    2-item previews get no rotation at all. Applying `PI/2` to a
///    1-item preview rotates a single mini a quarter turn for nothing.
/// 3. **The shift is signed by `direction`** (`theta0 += direction *
///    thetaShift`, not `theta0 + thetaShift`). Un-negated, an RTL cluster
///    comes back rotated a half turn instead of mirrored.
///
/// With the sign convention above, LTR slots read **0 = top-left,
/// 1 = top-right, 2 = bottom-left, 3 = bottom-right**; the RTL cluster is
/// the same square read right-to-left.
///
/// Each entry is `(x, y, edge)` where `edge` is the mini's icon edge, already
/// scaled by the preview scale for the item count. Entries at or beyond `n`
/// are `(0.0, 0.0, 0.0)` and must not be drawn.
pub fn folder_preview_icons(
    n: usize,
    center: (f32, f32),
    radius: f32,
    icon: f32,
    ltr: bool,
) -> [(f32, f32, f32); 4] {
    let n = n.clamp(1, 4);
    // :142 - "The case of two items is homomorphic to the case of one."
    let cur_num_items = n.max(2);
    // :146 / :148-149 - LTR starts at PI and sweeps with direction -1.
    let dir = if ltr { -1.0 } else { 1.0 };
    let mut theta0 = if ltr { PI } else { 0.0 };
    // :151-156 - keyed off `curNumItems`, so 1 and 2 items get no shift.
    let theta_shift = match cur_num_items {
        3 => FRAC_PI_2,
        4 => FRAC_PI_4,
        _ => 0.0,
    };
    // :157 - the shift rides on `direction`, so RTL mirrors rather than
    // rotating.
    theta0 += dir * theta_shift;
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
        // :162-166 - "We want the items to appear in reading order": with
        // four items on a circle the 3rd and 4th angles are swapped, so the
        // 3rd folder entry lands top-left of the bottom pair.
        let idx = match (n, slot) {
            (4, 2) => 3,
            (4, 3) => 2,
            _ => slot,
        };
        // :169
        let theta = theta0 + idx as f32 * (2.0 * PI / cur_num_items as f32) * dir;
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
    /// Items shown before the list scrolls.
    ///
    /// The reference collapses a popup to a single icon strip once it exceeds
    /// `SHORTCUT_COLLAPSE_THRESHOLD = 6` (`PopupContainerWithArrow.java:95`),
    /// so 6 is the largest list that is ever drawn as rows. This used to be 4
    /// here while `draw_popup` capped at 6 and `height_for` capped at 4 -- three
    /// disagreeing ceilings, which meant the surface was sized for four rows and
    /// six were drawn into it. One constant, read by all three call sites now.
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
            max_items: 6,
            container_margin: p.dp(2.0),
        }
    }

    /// Height of a popup showing `items` items, clamped to [`Self::max_items`].
    #[inline]
    pub fn height_for(&self, items: usize) -> f32 {
        let n = self.rows_for(items) as f32;
        // One extra slot: the arrow overlaps the container's edge.
        n * self.item_h + self.pad_v * 2.0 + self.arrow_h
    }

    /// The row count actually drawn for `items`.
    #[inline]
    pub fn rows_for(&self, items: usize) -> usize {
        items.min(self.max_items).max(1)
    }

    /// Resolve an anchor, an item count and the open progress into the rectangle
    /// the popup is actually drawn into.
    ///
    /// This is the *only* place the placement arithmetic lives. It used to be
    /// inline in `draw_popup` while the shell had no idea it existed, so the
    /// comment in the renderer claiming "the same struct the shell hit-tests
    /// against" described a hit test that was never written. The renderer and
    /// [`Self::hit`] now both call this, so a row is tappable exactly where it
    /// is painted -- which was the whole point of routing both through
    /// `PopupMenuLayout`.
    pub fn place(
        &self,
        l: &Layout,
        ax: f32,
        ay: f32,
        items: usize,
        progress: f32,
    ) -> PopupPlacement {
        let rows = self.rows_for(items);
        // The menu grows out of the touch point, so its size is a function of
        // the open progress rather than being full size from frame one.
        let m = progress.clamp(0.0, 1.0);
        let grow = m * m * (3.0 - 2.0 * m);
        let pw = (self.item_w * grow).max(0.0);
        let ph = (self.item_h * rows as f32 * grow).max(0.0);
        // Centred on the anchor, then clamped inside the panel.
        let x = (ax - pw * 0.5).clamp(l.w * 0.02, (l.w * 0.98 - pw).max(l.w * 0.02));
        let y0 = ay - ph * 0.5;
        let y = if y0 + ph > l.h * 0.98 {
            (l.h * 0.98 - ph).max(0.0)
        } else {
            y0.max(0.0)
        };
        PopupPlacement {
            x,
            y,
            w: pw,
            h: ph,
            rows,
            progress: m,
        }
    }

    /// The open progress at which rows become tappable.
    ///
    /// Below this the surface is a sliver centred on the long-press point, and
    /// because the anchor is the user's own finger position a second tap lands
    /// inside it. The first row of an icon menu is the affirmative one
    /// ("App info"), so a fast re-tap would fire the wrong action on a menu the
    /// user could not see. Half open is roughly the midpoint of the reference's
    /// own 250 ms reveal, and it is well before the menu reads as present.
    pub const TAPPABLE_PROGRESS: f32 = 0.5;

    /// Which row is under `(x, y)`, or `None` for a miss.
    ///
    /// Row `i` is the band `[y + i * item_h, y + (i + 1) * item_h)`, the same
    /// arithmetic `draw_popup` uses for its label baseline. A touch in the
    /// container's vertical padding lands on no row, which is what lets the
    /// caller treat it as an outside tap and dismiss.
    ///
    /// Returns `None` until the popup is [`Self::TAPPABLE_PROGRESS`] open.
    #[inline]
    pub fn hit(&self, place: &PopupPlacement, x: f32, y: f32) -> Option<usize> {
        if place.progress < Self::TAPPABLE_PROGRESS {
            return None;
        }
        if place.w < 1.0 || place.h < 1.0 {
            return None;
        }
        if x < place.x || x >= place.x + place.w {
            return None;
        }
        if y < place.y || y >= place.y + place.h {
            return None;
        }
        let i = ((y - place.y) / self.item_h) as usize;
        (i < place.rows).then_some(i)
    }
}

/// Where a popup is drawn, and how many rows fit in it.
///
/// Produced by [`PopupMenuLayout::place`], consumed by the renderer and by
/// [`PopupMenuLayout::hit`]. `Copy`, so the hit test allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupPlacement {
    /// Left edge, panel coordinates.
    pub x: f32,
    /// Top edge, panel coordinates.
    pub y: f32,
    /// Surface width; 0 while the open progress is 0.
    pub w: f32,
    /// Surface height; 0 while the open progress is 0.
    pub h: f32,
    /// Rows drawn, `1..=max_items`.
    pub rows: usize,
    /// Open progress this placement was resolved at, `0.0..=1.0`.
    ///
    /// Carried so [`PopupMenuLayout::hit`] can refuse to dispatch a row while the
    /// menu is still a sliver on top of the finger that raised it.
    pub progress: f32,
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
        if !self.tabs.contains(x, y) && self.add_tab_rect().map(|r| r.contains(x, y)) != Some(true)
        {
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
                assert!(
                    l.status_bar_h < l.clock_y,
                    "{name}: status bar overlaps clock"
                );
                assert!(l.clock_y < l.search.y, "{name}: clock overlaps search");
                assert!(
                    l.search.y + l.search.h <= l.grid_top + 0.001,
                    "{name}: search overlaps grid"
                );
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
                assert!(
                    l.grid_bottom <= l.page_dots.y + 0.001,
                    "{name}: grid overlaps dots"
                );
                assert!(
                    l.page_dots.y + l.page_dots.h <= l.dock.y + 0.001,
                    "{name}: dots overlap dock"
                );
                assert!(
                    l.dock.y + l.dock.h <= l.nav_pill.y + 0.001,
                    "{name}: dock overlaps nav"
                );
                assert!(
                    l.nav_pill.y + l.nav_pill.h <= h + 0.001,
                    "{name}: nav off screen"
                );
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
            assert_eq!(
                l.home_page_hit(l.page_dots.center_x(), cy, 0),
                None,
                "{w}x{h}"
            );
            // NaN never resolves to a page.
            assert_eq!(l.home_page_hit(f32::NAN, cy, 2), None, "{w}x{h}");
            assert_eq!(
                l.home_page_hit(l.page_dots.center_x(), f32::NAN, 2),
                None,
                "{w}x{h}"
            );
            assert_eq!(
                l.home_grid_hit_paged(f32::NAN, l.grid_top + 1.0, 0.0, 2),
                None,
                "{w}x{h}"
            );
            assert_eq!(l.drawer_grid_hit(0.0, f32::NAN, f32::NAN), None, "{w}x{h}");
            assert_eq!(
                l.home_grid_hit_paged(10.0, l.grid_top + 1.0, 0.0, 0),
                None,
                "{w}x{h}"
            );
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
                assert!(
                    (plain.dock_slot(s).w - plain.dock_pitch).abs() < 0.001,
                    "{w}x{h} slot {s}"
                );
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
        assert_eq!(
            l.home_grid_hit_paged(l.col_pitch * 3.0 + 2.0, y, 0.0, 3),
            Some((0, 3))
        );
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
        assert_eq!(
            l.drawer_grid_hit(0.0, icon.center_x(), icon.center_y()),
            Some(2)
        );
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
                assert!(
                    l.search.h >= (w * 0.02).min(w.min(h) * 0.09),
                    "{w}x{h}: search pill too short"
                );
                assert!(
                    l.remove_chip.h >= (w * 0.02).min(w.min(h) * 0.048),
                    "{w}x{h}: chip too short"
                );
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
            assert!(
                last <= k.frame.y + k.frame.h + 0.001,
                "{n}: last row escapes frame"
            );
            // Row 1 keys tile the row exactly once.
            let total: f32 = (0..KB_ROW1).map(|i| k.row1_at(i).w).sum();
            assert!((total - k.row1.w).abs() < 0.5, "{n}: row1 width {}", total);
            // Every key is a distinct, non-empty rect.
            for r in [
                k.row1,
                k.row2,
                k.row3_shift,
                k.row3_backspace,
                k.row4_hide,
                k.row4_space,
                k.row4_enter,
            ] {
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
            assert_eq!(
                k.hit(w * 0.5, k.frame.y - 1.0),
                None,
                "{w}x{h}: above frame"
            );
            assert_eq!(
                k.hit(-1.0, k.frame.center_y()),
                None,
                "{w}x{h}: left of frame"
            );
            assert_eq!(
                k.hit(w + 1.0, k.frame.center_y()),
                None,
                "{w}x{h}: right of frame"
            );
        }
    }

    // ---------------------------------------------------------- app surfaces

    #[test]
    fn app_bar_buttons_are_inside_the_bar_and_disjoint() {
        for &(w, h) in PANELS {
            for panel in [AppPanel::Browser, AppPanel::Terminal, AppPanel::Messages] {
                let a = AppLayout::new(w, h, panel, 2);
                let n = format!("{w}x{h} {panel:?}");
                assert!(
                    a.bar.x <= a.back.x && a.back.x + a.back.w <= a.bar.x + a.bar.w,
                    "{n}: back outside bar"
                );
                assert!(
                    a.bar.x <= a.close.x && a.close.x + a.close.w <= a.bar.x + a.bar.w,
                    "{n}: close outside bar"
                );
                assert!(a.back.x + a.back.w <= a.close.x, "{n}: back overlaps close");
                assert!(
                    a.close.y + a.close.h <= a.bar.y + a.bar.h,
                    "{n}: close below bar"
                );
                // Content never starts above the bar.
                assert!(
                    a.scroll_top >= a.bar.y + a.bar.h - 0.001,
                    "{n}: content under bar"
                );
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
                    assert!(
                        r.x + r.w <= a.tabs.x + a.tabs.w + 0.001,
                        "{label}: tab {i} off strip"
                    );
                    prev_right = r.x + r.w;
                }
                if let Some(add) = a.add_tab_rect() {
                    assert!(add.x >= prev_right, "{label}: + overlaps last tab");
                    assert_eq!(
                        a.hit_tab(add.center_x(), add.center_y()),
                        Some(TabHit::Add),
                        "{label}"
                    );
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
            assert_eq!(
                a.hit_tab_active(x, y, i),
                Some(TabHit::Select(i)),
                "tab {i} active select"
            );
        }
        // With a single tab there is nothing to close, so the whole strip
        // selects.
        let one = AppLayout::new(1080.0, 2400.0, AppPanel::Terminal, 1);
        let r = one.tab_rect(0);
        assert_eq!(
            one.hit_tab_active(r.center_x(), r.center_y(), 0),
            Some(TabHit::Select(0))
        );
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
                assert!(
                    c.x >= 0.0 && c.x + c.w <= w + 0.001,
                    "{n}: tile {i} off panel"
                );
                assert_eq!(
                    s.tiles.hit(c.center_x(), c.center_y()),
                    Some(i),
                    "{n}: tile {i}"
                );
                // A point one pixel right of the tile is the next tile or a gap.
                let after = c.x + c.w + 1.0;
                assert_ne!(
                    s.tiles.hit(after, c.center_y()),
                    Some(i),
                    "{n}: tile {i} bleed"
                );
            }
            assert_eq!(
                s.zone(s.brightness.center_x(), s.brightness.center_y()),
                ShadeZone::Brightness,
                "{n}"
            );
            assert_eq!(
                s.zone(s.notifs[1].center_x(), s.notifs[1].center_y()),
                ShadeZone::Notifications,
                "{n}"
            );
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
            assert!(
                s.notifs[s.notifs.len() - 1].y + s.notifs[2].h <= h,
                "{w}x{h}: last card off screen"
            );
        }
    }

    // ============================================== density-aware sub-layouts

    /// Relative closeness, for dp arithmetic that goes through a `1080 / 420`
    /// division. 1e-4 relative is far tighter than a pixel at any panel size
    /// and far looser than `f32` noise, so it cannot hide a wrong constant.
    #[inline]
    fn near(a: f32, b: f32) -> bool {
        near_tol(a, b, 1e-4)
    }

    fn near_tol(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol * b.abs().max(1.0)
    }

    /// Task 5.2: the grid engine reads dp tokens, not panel fractions.
    ///
    /// Two halves. First, the *calibration*: every calibrated token must
    /// reproduce the fraction it replaced, on the reference panel AND on every
    /// other panel, because the panel is 420 dp wide at any density. Second,
    /// the *wiring*: `Layout::new_scaled` and `DeviceProfile::grid_metrics`
    /// must be the same source, and `Layout`'s published `icon_size` /
    /// `row_pitch` must be exactly the metrics that came out of it.
    #[test]
    fn grid_fractions_are_calibrated_dp_tokens() {
        // The reference panel: 1080 px / 420 dp = 2.5714286 px per dp.
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        assert_eq!(p.cols, 5);
        // 0.56 of a 84 dp column (420 / 5) is 47.04 dp, i.e. 120.96 px.
        assert!(near(p.grid_icon_dp, 47.04), "{}", p.grid_icon_dp);
        assert!(near(
            p.grid_icon_dp,
            (LAWNCHAIR_PHONE_DP_WIDTH / 5.0) * ICON_FRACTION
        ));
        assert!(
            near(p.dp(p.grid_icon_dp), 120.96),
            "{}",
            p.dp(p.grid_icon_dp)
        );
        assert!(near(l.icon_size, 120.96), "{}", l.icon_size);
        assert!(near(l.icon_size, l.col_pitch * ICON_FRACTION));
        // 0.22 of the icon and 0.20 of it again, as dp.
        assert!(near(p.grid_label_gap_dp, p.grid_icon_dp * LABEL_GAP));
        assert!(
            near(p.grid_label_gap_dp, 10.348_8),
            "{}",
            p.grid_label_gap_dp
        );
        assert!(
            near(p.dp(p.grid_label_gap_dp), 26.611_2),
            "{}",
            p.dp(p.grid_label_gap_dp)
        );
        assert!(near(
            p.grid_cell_bottom_gap_dp,
            p.grid_icon_dp * CELL_BOTTOM_GAP
        ));
        assert!(
            near(p.grid_cell_bottom_gap_dp, 9.408),
            "{}",
            p.grid_cell_bottom_gap_dp
        );
        assert!(
            near(p.dp(p.grid_cell_bottom_gap_dp), 24.192),
            "{}",
            p.dp(p.grid_cell_bottom_gap_dp)
        );
        // 3% of the panel, and 4.8% of its short edge, as dp.
        assert!(near(p.panel_pad_dp, 12.6), "{}", p.panel_pad_dp);
        assert!(near(
            p.panel_pad_dp,
            LAWNCHAIR_PHONE_DP_WIDTH * PANEL_PAD_FRACTION
        ));
        assert!(near(p.touch_target_dp, 20.16), "{}", p.touch_target_dp);
        assert!(near(
            p.touch_target_dp,
            LAWNCHAIR_PHONE_DP_WIDTH * TOUCH_TARGET_FRACTION
        ));

        // The calibration is not a coincidence of the 5-column case: because
        // the panel is `LAWNCHAIR_PHONE_DP_WIDTH` dp wide at EVERY density,
        // `dp * 420 / cols` is the column pitch on every panel, so the tokens
        // reproduce the fractions for 4, 5 and 6 columns alike.
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            let p = l.profile();
            assert_eq!(p.cols, grid_cols_for(w), "{w}x{h}");
            assert!(
                near(l.icon_size, (w / p.cols as f32) * ICON_FRACTION),
                "{w}x{h}"
            );
            assert!(near(l.icon_size, p.dp(p.grid_icon_dp)), "{w}x{h}");
            // And the side gutter is still 3% of the panel on all of them.
            assert!(near(l.search.x, w * PANEL_PAD_FRACTION), "{w}x{h}");
            assert!(near(l.search.x, p.dp(p.panel_pad_dp)), "{w}x{h}");
            assert!(near(l.search.w, w - 2.0 * p.dp(p.panel_pad_dp)), "{w}x{h}");
            // The touch minimum still tracks the SHORT edge: a 2000x1000
            // panel must not grow a 96 px target.
            let short = w.min(h);
            assert!(near(
                p.dp(p.touch_target_dp) * short / w,
                short * TOUCH_TARGET_FRACTION
            ));
        }

        // Single-sourcing: what `Layout` publishes IS the profile's metrics.
        let m = l.grid_metrics();
        assert!(near(l.icon_size, m.icon));
        assert!(near(l.row_pitch, m.cell_h));
        assert!(near(
            m.cell_h,
            m.icon + m.label_gap + m.label_h + m.cell_bottom_gap
        ));
        assert!(near(m.label_gap, l.icon_size * LABEL_GAP));
        assert!(near(m.cell_bottom_gap, l.icon_size * CELL_BOTTOM_GAP));
        assert!(near(m.label_h, l.label_em_px() * 1.2));
        assert!(near(l.grid_cell(0).h, l.row_pitch));
        assert!(near(l.grid_icon(0).w, l.icon_size));
        // The cell's inner horizontal padding floor (`cell_pad_x_dp`) does not
        // bind on the reference panel - the icon is 47.04 dp inside an 84 dp
        // column - but it is the ceiling on a pathological retune.
        assert!(l.icon_size + 2.0 * p.dp(p.cell_pad_x_dp) <= l.col_pitch);
        let huge = DeviceProfile {
            grid_icon_dp: 400.0,
            ..p
        };
        assert!(near(
            huge.grid_metrics(1.0, l.label_em_px()).icon,
            l.col_pitch - 2.0 * p.dp(p.cell_pad_x_dp)
        ));
        // The label gap is floored at the reference's own 7 dp.
        let tight = DeviceProfile {
            grid_label_gap_dp: 0.0,
            ..p
        };
        assert!(near(
            tight.grid_metrics(1.0, l.label_em_px()).label_gap,
            p.dp(p.icon_label_gap_dp)
        ));
        assert_eq!(p.icon_label_gap_dp, 7.0);
        assert_eq!(p.cell_pad_x_dp, 8.0);
        assert_eq!(p.min_touch_dp, 48.0);
    }

    /// Task 5.2's verification test: mutate a dp token on a copied profile and
    /// watch the generated rectangles move by exactly the amount the token
    /// moved.
    #[test]
    fn mutating_a_profile_dp_token_scales_the_grid_rectangles() {
        let l = Layout::plain(1080.0, 2400.0);
        let p = l.profile();
        let em = l.label_em_px();
        let base = p.grid_metrics(l.font_scale, em);

        // Widening the icon token widens the icon and adds exactly that much
        // to the row - the label terms are untouched, because a bigger icon
        // must not re-type the label. (10 dp is inside the `cell_pad_x_dp`
        // ceiling; the clamp is exercised in the test above.)
        let wide = DeviceProfile {
            grid_icon_dp: p.grid_icon_dp + 10.0,
            ..p
        };
        let big = wide.grid_metrics(l.font_scale, em);
        assert!(
            near(big.icon, base.icon + p.dp(10.0)),
            "{} vs {}",
            big.icon,
            base.icon
        );
        assert!(near(big.icon, l.grid_icon(0).w + p.dp(10.0)));
        assert!(near(big.label_gap, base.label_gap), "the gap did not move");
        assert!(near(big.label_h, base.label_h), "the label was re-typed");
        assert!(near(big.cell_bottom_gap, base.cell_bottom_gap));
        assert!(
            near(big.cell_h - base.cell_h, p.dp(10.0)),
            "the row grew by the icon"
        );
        // The rectangles themselves: an icon tile is 10 dp wider, and its
        // left edge moves in by 5 dp because it stays centred in the same
        // column.
        let c0 = l.grid_cell(0);
        let i0 = l.grid_icon(0);
        assert!(near(i0.w, base.icon));
        assert!(near(i0.x - c0.x, (c0.w - base.icon) * 0.5));
        let i0_big = Rect {
            x: c0.x + (c0.w - big.icon) * 0.5,
            w: big.icon,
            ..i0
        };
        assert!(near(i0_big.w, i0.w + p.dp(10.0)));
        assert!(
            i0_big.x < i0.x,
            "a wider icon is pushed towards the column centre"
        );
        assert!(near(i0.x - i0_big.x, p.dp(5.0)));
        assert!(near(c0.w - (i0_big.x - c0.x) * 2.0, i0_big.w));

        // Moving the label gap token moves the label, not the icon.
        let loose = DeviceProfile {
            grid_label_gap_dp: p.grid_label_gap_dp + 10.0,
            ..p
        };
        let g = loose.grid_metrics(l.font_scale, em);
        assert!(near(g.icon, base.icon));
        assert!(near(g.label_gap, base.label_gap + p.dp(10.0)));
        assert!(near(g.cell_h - base.cell_h, p.dp(10.0)));

        // The cell-bottom token is independent again.
        let airy = DeviceProfile {
            grid_cell_bottom_gap_dp: p.grid_cell_bottom_gap_dp + 5.0,
            ..p
        };
        let a = airy.grid_metrics(l.font_scale, em);
        assert!(near(a.cell_h - base.cell_h, p.dp(5.0)));
        assert!(near(a.icon, base.icon) && near(a.label_gap, base.label_gap));

        // The panel padding token moves the gutter, and only the gutter: the
        // grid pitch is the full panel width, so the cells do not move.
        let inset = DeviceProfile {
            panel_pad_dp: p.panel_pad_dp + 8.0,
            ..p
        };
        let pad = inset.dp(inset.panel_pad_dp);
        assert!(near(pad, p.dp(p.panel_pad_dp) + p.dp(8.0)));
        assert!(
            near(l.col_pitch, 216.0),
            "the column pitch ignores the gutter"
        );
        // The rows that *do* follow the gutter follow it by exactly the delta.
        let narrower = l.search.w - (pad - p.dp(p.panel_pad_dp)) * 2.0;
        assert!(near(narrower, l.search.w - 2.0 * p.dp(8.0)));
        assert!(narrower < l.search.w);

        // A token that is not a length at all - the column count - re-derives
        // every grid token from scratch, so a 4-column phone gets its own
        // (larger, relative to the column) icon.
        let four = DeviceProfile::for_panel(360.0);
        assert_eq!(four.cols, 4);
        assert!(
            near(four.grid_icon_dp, 105.0 * ICON_FRACTION),
            "{}",
            four.grid_icon_dp
        );
        assert!(near(
            DeviceProfile::for_panel(1600.0).grid_icon_dp,
            70.0 * ICON_FRACTION
        ));
        assert!(near(
            four.dp(four.grid_icon_dp),
            (360.0 / 4.0) * ICON_FRACTION
        ));
        // `cols == 0` must not divide by zero.
        let zero = DeviceProfile::at(2.0, 0);
        assert_eq!(zero.cols, 0);
        assert!(zero.grid_icon_dp.is_finite() && zero.grid_icon_dp > 0.0);
        assert!(zero.grid_metrics(1.0, 16.0).cell_h.is_finite());
    }

    /// Task 6.3: a user font scale changes the label metrics and the row pitch,
    /// and changes *nothing* about how many columns the grid has.
    #[test]
    fn font_scale_scales_labels_but_not_the_grid() {
        let w = 1080.0f32;
        let h = 2400.0f32;
        let one = Layout::new_scaled(w, h, 1.0, false);
        let big = Layout::new_scaled(w, h, 1.5, false);
        assert!((one.font_scale - 1.0).abs() < f32::EPSILON);
        assert!((big.font_scale - 1.5).abs() < f32::EPSILON);
        // `Layout::new` is the 1.0 wrapper, byte for byte.
        assert_eq!(one, Layout::new(w, h, false));
        assert_eq!(one, Layout::plain(w, h));
        assert_eq!(big, Layout::plain_scaled(w, h, 1.5));

        // The label: em * 1.2 (descender) * font_scale. `label_em_px` is the
        // *unscaled* em - it is the type size the profile is measured against -
        // so it is `scaled_label_em_px` that a 1.5x user font moves.
        assert!(near(big.label_em_px(), one.label_em_px()));
        assert!(near(
            big.scaled_label_em_px(),
            one.scaled_label_em_px() * 1.5
        ));
        assert!(near(big.scaled_label_em_px(), one.label_em_px() * 1.5));
        assert!(near(
            big.scaled_label_em_px(),
            big.label_em_px() + big.label_em_px() * 0.5
        ));
        let m1 = one.grid_metrics();
        let m15 = big.grid_metrics();
        assert!(near(m1.label_h, one.label_em_px() * 1.2));
        assert!(
            near(m15.label_h, m1.label_h * 1.5),
            "{} vs {}",
            m15.label_h,
            m1.label_h
        );
        assert!(near(m15.label_h, big.scaled_label_em_px() * 1.2));
        // The row grows by exactly the extra label, because the row is
        // icon + gap + label + bottom gap and only the label moved.
        assert!(near(m15.icon, m1.icon), "the icon must not scale");
        assert!(near(m15.label_gap, m1.label_gap), "the gap must not scale");
        assert!(near(m15.cell_bottom_gap, m1.cell_bottom_gap));
        assert!(near(m15.cell_h - m1.cell_h, m1.label_h * 0.5));
        assert!(near(big.row_pitch - one.row_pitch, m1.label_h * 0.5));

        // The grid's *column* logic is untouched: same columns, same column
        // pitch, same icon edge, same cell count at the same row.
        assert_eq!(big.grid_cols, one.grid_cols);
        assert_eq!(big.grid_cols, grid_cols_for(w));
        assert_eq!(big.label_scale, one.label_scale);
        assert!(near(big.col_pitch, one.col_pitch));
        assert!(near(big.icon_size, one.icon_size));
        assert!(near(big.icon_radius, one.icon_radius));
        assert!(near(big.grid_cell(3).w, one.grid_cell(3).w));
        assert!(near(big.grid_cell(3).x, one.grid_cell(3).x));
        assert_eq!(big.grid_cell(3).y, one.grid_cell(3).y, "row 0 is unmoved");
        // Row 1 does move down, because the row above it got taller.
        assert!(big.grid_cell(one.grid_cols).y > one.grid_cell(one.grid_cols).y);
        assert!(near(
            big.grid_cell(one.grid_cols).y - one.grid_cell(one.grid_cols).y,
            m1.label_h * 0.5
        ));
        // Taller rows means fewer of them fit.
        assert!(big.max_rows <= one.max_rows);
        assert!(
            big.max_rows + 1 >= one.max_rows,
            "1.5x should cost at most one row"
        );
        // The bands above the grid do not move: a font scale is a *type* scale.
        assert_eq!(big.grid_top, one.grid_top);
        assert_eq!(big.search, one.search);
        assert_eq!(big.clock_y, one.clock_y);
        assert_eq!(big.dock, one.dock);
        // The sp-sized smartspace line boxes scale with it; the 20 dp weather
        // glyph and the 104 dp host do not.
        let s1 = one.smartspace();
        let s15 = big.smartspace();
        assert!(near(s15.title.h, s1.title.h * 1.5));
        assert!(near(s15.subtitle.h, s1.subtitle.h * 1.5));
        assert!(near(s15.icon.w, s1.icon.w));
        assert!(near(s15.host.h, s1.host.h));
        // Sanity on the guards: garbage in, sane geometry out.
        for bad in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let l = Layout::new_scaled(w, h, bad, false);
            assert!(l.row_pitch.is_finite() && l.row_pitch > 0.0, "{bad}");
            assert!(l.icon_size.is_finite() && l.icon_size > 0.0, "{bad}");
            assert!(l.max_rows > 0, "{bad}");
            assert!(l.font_scale.is_finite() && l.font_scale >= 0.0, "{bad}");
        }
    }

    /// Task 5.5: the prediction row measures 108 dp and stacks its icon above
    /// its label, exactly as `PredictionRowView.getExpectedHeight()` says.
    #[test]
    fn prediction_row_measures_108dp_with_the_label_under_the_icon() {
        // The four dp terms of `PredictionRowView.java:149-161`.
        assert_eq!(APP_ICON_DP, 65.0, "device_profiles.xml:70 iconSize");
        assert_eq!(
            PREDICTION_ICON_PAD_DP, 7.0,
            "getAllAppsIconDrawablePadding()"
        );
        assert_eq!(PREDICTION_LABEL_H_DP, 16.0, "calculateTextHeight(13 sp)");
        assert_eq!(
            PREDICTION_PAD_V_DP, 8.0,
            "all_apps_predicted_icon_vertical_padding"
        );
        assert_eq!(
            PREDICTION_TOP_EXTRA_DP, 4.0,
            "all_apps_search_top_row_extra_height"
        );
        assert_eq!(PREDICTION_ROW_H_DP, 108.0);
        assert!((PREDICTION_ROW_H_DP - 108.0).abs() < 1e-6);
        // 65 + 7 + 16 + 8 + 8 + 4.
        assert!((PREDICTION_ROW_H_DP - (65.0 + 7.0 + 16.0 + 8.0 + 8.0 + 4.0)).abs() < 1e-6);
        // It is emphatically NOT one icon tall: that was the bug. Both tokens
        // are compile-time constants, so this is checked at compile time --
        // if either dp token is ever re-sourced the crate stops building
        // rather than the test quietly going red in CI.
        const { assert!(PREDICTION_ROW_H_DP > APP_ICON_DP) };
        assert!((PREDICTION_ROW_H_DP - APP_ICON_DP - 43.0).abs() < 1e-6);

        // On the reference panel the row is 108 dp of pixels, on every panel.
        for &(w, h) in PANELS {
            let l = Layout::plain(w, h);
            let p = l.profile();
            let sheet = l.drawer_sheet(h);
            let expect = p.dp(PREDICTION_ROW_H_DP);
            assert!(
                (sheet.predictions.h - expect).abs() < 1e-3,
                "{w}x{h}: prediction row is {} px, expected {expect}",
                sheet.predictions.h
            );
            // Icon on top, label underneath, both inside the row, and the grid
            // starts below the whole thing.
            let icon = sheet.prediction_icon();
            let label = sheet.prediction_label();
            assert!(near(icon.y, sheet.predictions.y), "{w}x{h}");
            assert!(near(icon.w, p.dp(APP_ICON_DP)), "{w}x{h}");
            assert!(
                near(label.y - (icon.y + icon.h), p.dp(PREDICTION_ICON_PAD_DP)),
                "{w}x{h}"
            );
            assert!(label.h > 0.0, "{w}x{h}: no room for the label");
            assert!(
                label.y + label.h <= sheet.predictions.y + sheet.predictions.h + 1e-3,
                "{w}x{h}: the label escapes the row"
            );
            assert!(
                sheet.grid.y >= sheet.predictions.y + sheet.predictions.h,
                "{w}x{h}"
            );
            // 43 dp of the row is not the icon, and it is all of it accounted
            // for: 7 pad + 16 label + 2 * 8 pad + 4 extra.
            assert!(near(
                sheet.predictions.h - icon.h,
                p.dp(PREDICTION_ICON_PAD_DP
                    + PREDICTION_LABEL_H_DP
                    + PREDICTION_PAD_V_DP * 2.0
                    + PREDICTION_TOP_EXTRA_DP)
            ));
        }

        // The 108 px band is rigid: dragging the sheet moves the whole row,
        // label included, and never resizes it.
        let l = Layout::plain(1080.0, 2400.0);
        let open = l.drawer_sheet(l.h);
        let half = l.drawer_sheet(l.h * 0.5);
        let shut = l.drawer_sheet(0.0);
        for s in [open, half, shut] {
            assert!(near(s.predictions.h, open.predictions.h));
            assert!(near(s.prediction_label().h, open.prediction_label().h));
            assert!(near(
                s.prediction_label().y - s.prediction_icon().y,
                open.prediction_label().y - open.prediction_icon().y
            ));
        }
        assert!(near(
            shut.predictions.y - shut.top,
            open.predictions.y - open.top
        ));
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
        let p = DeviceProfile::phone_reference();
        let dp = p.dp;
        let icon = p.dp(p.icon_dp);
        assert!(near(icon, 167.142_86), "{icon}");

        // THE reference case: the hotseat QSB strip on a 1080 px panel is the
        // cell-layout content width (420 - 2 * cell_layout_padding dp) and
        // holds `hotseat_icons` 65 dp icons. `DeviceProfile.java:1421-1422`
        // divides the space the icons do NOT fill by the number of *borders*
        // (`icons - 1`, not `icons`):
        //
        //   (1024.6157 - 4 * 167.1429) / 3 = 118.6814 px = 46.1538 dp
        //
        // The old pitch division (`width / icons`) answered 256.15 px here:
        // it counts the icon itself as a gap, so every slot in the dock was
        // over-spaced by a whole icon and the cluster did not fit the strip.
        let strip = p.dp(LAWNCHAIR_PHONE_DP_WIDTH - 2.0 * p.edge_margin_dp);
        // Spelled at f32 precision on purpose: `1_024.615_74` is not
        // representable, and both spellings parse to the same f32
        // (0x448013b4), so this is the same assertion, not a loosened one.
        assert!(near(strip, 1_024.615_7), "{strip}");
        let got = p.hotseat_icon_space(strip, p.hotseat_icons, icon);
        assert!(near(got, 118.681_43), "hotseat gap is {got} px");
        assert!(near(got / dp, 46.153_847), "hotseat gap is {} dp", got / dp);
        // The whole model in one line: borders, not icons, and the icons are
        // removed before the division.
        assert!(near(got, (strip - icon * 4.0) / 3.0));
        assert!(
            !near(got, strip / 4.0),
            "the gap must not be the icon pitch"
        );
        // 46.15 dp is inside [18, 50], so this is the raw border space, not a
        // clamp; and the six borders of the 4-icon cluster... there are three
        // of them, and they plus the icons exactly fill the strip.
        assert!(got > p.dp(HOTSEAT_ICON_SPACE_MIN_DP) && got < p.dp(HOTSEAT_ICON_SPACE_MAX_DP));
        assert!(
            near(got * 3.0 + icon * 4.0, strip),
            "3 gaps + 4 icons = the strip"
        );

        // The answer is independent of the icon size once the icons fit -
        // that is the whole point of dividing the *leftover* by the borders.
        // A pitch division would have moved with it.
        let roomy = 2000.0;
        assert!(near(
            hotseat_icon_space(roomy, 4, icon, dp),
            p.dp(HOTSEAT_ICON_SPACE_MAX_DP)
        ));
        // Cramped: 4 x 65 dp icons do not fit in 100 px, so the leftover is
        // negative and the 18 dp floor takes over.
        assert!(near(hotseat_icon_space(100.0, 4, icon, dp), 18.0 * dp));
        assert!(near(hotseat_icon_space(100.0, 4, icon, dp), 46.285_715));
        // In range, and *not* a pure clamp: 3 gaps of 87.44 px.
        let mid = 930.9;
        assert!(near(
            hotseat_icon_space(mid, 4, icon, dp),
            (mid - icon * 4.0) / 3.0
        ));
        assert!(near(hotseat_icon_space(mid, 4, icon, dp), 87.442_86));
        // The gap responds to the icon size on a *fixed* strip, because the
        // model divides the leftover rather than the whole width. A pitch
        // division (`strip / icons`) would have been constant.
        let fat_icons = p.dp(75.0);
        let fat = p.hotseat_icon_space(strip, 4, fat_icons);
        assert!(near(fat, (strip - fat_icons * 4.0) / 3.0));
        assert!(
            fat < got,
            "bigger icons on the same strip must close the gap"
        );
        // 10 dp of extra icon per slot takes 4 * 10 dp off the strip and
        // gives it back over 3 borders, so the gap loses 13.33 dp.
        assert!(near_tol(got - fat, 34.285_715, 1e-3), "{}", got - fat);
        assert!(near_tol(fat, 84.395_71, 1e-3), "{fat}");
        // And shrinking them past the 50 dp ceiling clamps rather than running
        // away: 20 dp icons would open a 272 px gap, which the reference caps.
        let small_icons = p.dp(20.0);
        assert_eq!(
            p.hotseat_icon_space(strip, 4, small_icons),
            p.dp(HOTSEAT_ICON_SPACE_MAX_DP)
        );

        // Degenerate requests publish 0 rather than a drawable number.
        assert_eq!(hotseat_icon_space(400.0, 0, icon, dp), 0.0);
        // One icon has no border to space, and the reference returns 0 for
        // `numBorders <= 0` (`DeviceProfile.java:1419`).
        assert_eq!(hotseat_icon_space(400.0, 1, icon, dp), 0.0);
        assert_eq!(hotseat_icon_space(400.0, 4, icon, 0.0), 0.0, "no density");
        assert_eq!(hotseat_icon_space(400.0, 4, f32::NAN, dp), 0.0);
        assert_eq!(hotseat_icon_space(400.0, 4, -1.0, dp), 0.0);
        assert_eq!(hotseat_icon_space(f32::NAN, 4, icon, dp), 0.0);
        assert_eq!(hotseat_icon_space(f32::INFINITY, 4, icon, dp), 0.0);
        assert_eq!(hotseat_icon_space(400.0, 4, icon, f32::NAN), 0.0);
        // The clamp scales with the panel, not with the pixel count.
        let small = DeviceProfile::for_panel(360.0).dp;
        assert!(near(hotseat_icon_space(10.0, 4, icon, small), 18.0 * small));
        assert!(near(hotseat_icon_space(10.0, 4, icon, small), 15.428_571));
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
        assert!(
            s.title.y + s.title.h <= s.subtitle.y + 0.001,
            "title overlaps subtitle"
        );
        assert!(
            s.host.y + s.host.h >= s.subtitle.y + s.subtitle.h,
            "text escapes the host"
        );

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
        assert!(s
            .hit_date
            .contains(s.subtitle.center_x(), s.subtitle.center_y()));
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
        assert!(
            !near(q.pill.radius, p.dp(32.0)),
            "the radius is 26 dp, not 32 dp"
        );
        assert_eq!(p.qsb_pad_v_dp, 6.0);
        // 26 / 64 = 0.41 of the height: not the 0.5 a semicircle would need.
        assert!(
            near(q.pill.radius / p.dp(p.qsb_h_dp), 0.406_25),
            "{}",
            q.pill.radius / p.dp(p.qsb_h_dp)
        );

        // Three 56 dp boxes. `LawnQsbUi.kt:363-421` lays them out as
        //   Row { Box(56dp), Spacer(weight = 1f), Row { QsbIcon, QsbIcon(-6dp) } }
        // so the start box is PINNED to the leading edge and the two end
        // boxes are a CLUSTER flush to the trailing edge. The "three boxes and
        // four equal gaps" model this replaces slid all three boxes by a
        // quarter of the slack each, which meant the mic and the lens drifted
        // with the pill width instead of staying anchored to the edge.
        for (i, b) in [q.g_icon, q.mic, q.lens].into_iter().enumerate() {
            assert!(near(b.w, p.dp(p.qsb_box_dp)), "box {i} w {}", b.w);
            assert!(near(b.h, p.dp(p.qsb_box_dp)), "box {i} h {}", b.h);
            assert!(near(b.w, 144.0), "box {i}");
            assert!(
                q.pill.x <= b.x && b.x + b.w <= q.pill.x + q.pill.w + 0.001,
                "box {i} escapes the pill"
            );
            assert!(
                b.y >= q.pill.y - 0.001 && b.y + b.h <= q.pill.y + q.pill.h + 0.001,
                "box {i} escapes the pill"
            );
        }
        assert_eq!(QSB_START_INSET_DP, 16.0);
        assert_eq!(QSB_END_OFFSET_DP, 6.0);
        // (1) The start box is flush to the leading edge behind its 16 dp
        // inset, and does NOT move when the pill is resized.
        assert!(near(q.g_icon.x, q.pill.x + p.dp(QSB_START_INSET_DP)));
        assert!(near(q.g_icon.x, q.pill.x + 41.142_86));
        // (2) Mic and lens are a trailing cluster of two 56 dp boxes: no gap
        // between them, and the cluster is flush to the pill's right edge.
        let cluster_w = p.dp(p.qsb_box_dp) * 2.0;
        assert!(near(q.mic.x, q.pill.x + q.pill.w - cluster_w));
        assert!(near(q.mic.x, 759.6), "mic box x is {}", q.mic.x);
        // (3) The lens is painted 6 dp inboard of where the Row laid it out
        // (`offset(x = (-6).dp)`, `LawnQsbUi.kt:414-416`); the *layout* edge of
        // the cluster is still 2 * box_w, which is what the mic is flush with.
        assert!(near(q.lens.x, q.mic.x + q.mic.w - p.dp(QSB_END_OFFSET_DP)));
        assert!(near(q.lens.x, q.mic.x + q.mic.w - 15.428_572));
        assert!(
            q.lens.x + q.lens.w < q.pill.x + q.pill.w - 0.001,
            "the -6 dp offset pulls the lens off the trailing edge"
        );
        assert!(near(
            q.lens.x + q.lens.w,
            q.pill.x + q.pill.w - p.dp(QSB_END_OFFSET_DP)
        ));
        // The free space is all in the one flexible spacer between them, and
        // it is several box widths wide on the reference panel - i.e. the
        // four-equal-gaps model is not accidentally reproduced.
        let spacer = q.mic.x - q.g_icon.x;
        assert!(near(
            spacer,
            q.pill.w - p.dp(QSB_START_INSET_DP) - cluster_w
        ));
        assert!(
            spacer > q.g_icon.w * 3.0,
            "the spacer swallows the slack: {spacer}"
        );
        assert!(
            !near(spacer, (q.pill.w - cluster_w * 1.5) * 0.25),
            "not four equal gaps"
        );
        // Mic and lens never overlap, and the start box never reaches the
        // cluster.
        assert!(
            q.g_icon.x + q.g_icon.w <= q.mic.x,
            "start box collides with the cluster"
        );
        assert!(q.mic.x + q.mic.w > q.lens.x, "the cluster boxes overlap");
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
        assert!(
            q.pill.y + q.pill.h > hotseat_bottom,
            "the pill is centred on the hotseat cell"
        );
        assert!(q.pill.y + q.pill.h <= l.h + 0.001, "pill below the panel");

        // There is deliberately no border/stroke field: the reference hotseat
        // QSB is a flat pill (no hint text, no border, no inline completion).
        // `pill.radius` is a corner radius, not a stroke width - the value
        // above proves it is 26 dp and not 64 dp or 2 dp.
        assert!(q.pill.radius < p.dp(p.qsb_h_dp) * 0.5);
    }

    /// The drawer's cull range and hit test must cover the whole catalogue.
    ///
    /// The two used to be bounded by `drawer_rows` -- the number of rows that
    /// *fit on screen* -- so with a 137-app catalogue (28 rows, 10 visible)
    /// eighteen rows were culled by the renderer and rejected by the tap, at
    /// every scroll offset. The scroll moved the list; it did not reveal
    /// anything.
    #[test]
    fn the_drawer_cull_and_hit_cover_every_row_of_the_catalogue() {
        for n in [1usize, 30, 100, 137, 512] {
            let l = Layout::plain(1080.0, 2400.0);
            let rows = n.div_ceil(l.grid_cols);
            let max = l.drawer_max_scroll_y(n);

            // Every row must be inside the cull range at SOME scroll offset.
            let mut seen = vec![false; rows];
            for step in 0..=(rows as i32) {
                let scroll = (step as f32 * l.row_pitch).min(max);
                for r in l.visible_row_range(scroll, rows) {
                    if r < rows {
                        seen[r] = true;
                    }
                }
                if seen.iter().all(|b| *b) {
                    break;
                }
            }
            for (r, ok) in seen.iter().enumerate() {
                assert!(
                    *ok,
                    "{n} apps: row {r} of {rows} is never drawn at any scroll offset"
                );
            }

            // And the last row must be tappable at maximum scroll.
            if rows > 0 {
                let last_cell = l.drawer_icon_cell_scrolled((rows - 1) * l.grid_cols, max);
                let got = l.drawer_grid_hit_scrolled(
                    0.0,
                    last_cell.center_x(),
                    last_cell.center_y(),
                    max,
                    n,
                );
                assert_eq!(
                    got,
                    Some((rows - 1) * l.grid_cols),
                    "{n} apps: the last row must be tappable at max scroll"
                );
            }
        }
    }

    /// A list that fits on screen must report no travel, and must not
    /// rubber-band either.
    #[test]
    fn a_drawer_that_fits_does_not_scroll_or_band() {
        let l = Layout::plain(1080.0, 2400.0);
        let fits = l.drawer_rows * l.grid_cols;
        assert_eq!(l.drawer_max_scroll_y(fits), 0.0);
        assert_eq!(l.drawer_max_scroll_y(fits - 1), 0.0);
        // At scroll 0 the cull range is the *visible* rows, not the whole
        // catalogue -- that is the point of it. It is the viewport's row
        // count, or one more when the band does not divide evenly, because
        // the row straddling the bottom edge is still partly on screen and
        // dropping it would leave a gap.
        let vr = l.visible_row_range(0.0, fits);
        assert_eq!(vr.start, 0);
        assert!(
            vr.end == l.drawer_rows || vr.end == l.drawer_rows + 1,
            "at rest the cull range is the visible rows: {} for {}",
            vr.end,
            l.drawer_rows
        );
        assert!(vr.end <= fits, "and never past the content");
        // A single row past the visible band starts the travel.
        assert!(l.drawer_max_scroll_y(fits + 1) > 0.0);
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
        assert!(near(
            pi.first_center(3),
            pi.band.center_x() - pi.circle_gap()
        ));
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
        assert!(
            near(rest[2].x, x0 + 2.0 * d + 2.0 * g),
            "inactive x {}",
            rest[2].x
        );
        assert!(near(rest[2].w, d));
        // RESTING ALPHA. `PageIndicatorDots.java:81` publishes
        // `PAGE_INDICATOR_ALPHA = 255` and `:553` computes
        // `nonActiveAlpha = (int)(alpha * 0.5f)`, so a resting indicator is a
        // fully opaque pill with 127.5-alpha bystanders. The 128 constant at
        // `:82` is nominal and is NOT the draw path's ceiling: using it as one
        // drew the whole indicator at half opacity (128 active / 64 idle).
        assert!(
            near(rest[0].alpha, 255.0),
            "resting active {}",
            rest[0].alpha
        );
        assert!(near(rest[1].alpha, 127.5), "resting idle {}", rest[1].alpha);
        assert!(near(rest[2].alpha, 127.5), "resting idle {}", rest[2].alpha);
        assert!(near(rest[0].alpha, PAGE_INDICATOR_ALPHA));
        assert!(near(
            rest[1].alpha,
            PAGE_INDICATOR_ALPHA * DOT_ALPHA_FRACTION
        ));
        assert!(near(rest[1].alpha, DOT_INACTIVE_ALPHA));
        assert_eq!(PAGE_INDICATOR_ALPHA, 255.0, "PageIndicatorDots.java:81");
        assert_eq!(DOT_ALPHA_FRACTION, 0.5, "PageIndicatorDots.java:83");
        assert_eq!(
            DOT_ALPHA, 128.0,
            "PageIndicatorDots.java:82 is nominal only"
        );
        assert!(near(DOT_INACTIVE_ALPHA, 127.5));
        assert!(
            !near(DOT_INACTIVE_ALPHA, DOT_ALPHA),
            "the draw path's idle alpha is 255 * 0.5, not the nominal 128"
        );
        // The cascade advances by exactly `gap` past each pre-bounce right.
        assert!(near(rest[1].x - (rest[0].x + rest[0].w), g), "cascade step");
        assert!(near(rest[2].x - (rest[1].x + rest[1].w), g), "cascade step");

        // Half way, 0 -> 1, p = 0.5: both neighbours are 1.5 diameters and
        // the alpha has crossed the midpoint, 255 -> 191.25, with the
        // bystander sitting at the 127.5 floor.
        //   right_0 = (x0 - d) + 1.5d = x0 + 0.5d ; x1 = right_0 + g
        //   right_1 = x1 + 1.5d = x0 + 2d + g    ; x2 = right_1 + g
        let half = page_indicator_dots(&pi, 3, 0, 1, 0.5);
        assert!(near(half[0].x, x0 - d), "{}", half[0].x);
        assert!(near(half[0].w, 1.5 * d), "{}", half[0].w);
        assert!(near(half[1].x, x0 + 0.5 * d + g), "{}", half[1].x);
        assert!(near(half[1].w, 1.5 * d), "{}", half[1].w);
        assert!(near(half[2].x, x0 + 2.0 * d + 2.0 * g), "{}", half[2].x);
        assert!(near(half[2].w, d), "bystander grew");
        assert!(near(half[0].alpha, 191.25), "{}", half[0].alpha);
        assert!(near(half[1].alpha, 191.25), "{}", half[1].alpha);
        assert!(near(half[2].alpha, DOT_INACTIVE_ALPHA), "{}", half[2].alpha);

        // End of the handover, p = 1.0: the outgoing dot is back to one
        // diameter and the inactive alpha, the incoming one owns the pill at
        // full 255.
        let done = page_indicator_dots(&pi, 3, 0, 1, 1.0);
        assert!(near(done[0].w, d), "{}", done[0].w);
        assert!(near(done[1].w, 2.0 * d), "{}", done[1].w);
        assert!(near(done[0].alpha, DOT_INACTIVE_ALPHA), "{}", done[0].alpha);
        assert!(
            near(done[1].alpha, PAGE_INDICATOR_ALPHA),
            "{}",
            done[1].alpha
        );
        // The alpha range is exactly the full 0-255 span at rest, not 0-128.
        assert!(near(PAGE_INDICATOR_ALPHA, 255.0));
        assert!(near(PAGE_INDICATOR_ALPHA - DOT_INACTIVE_ALPHA, 127.5));

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
        // this: at p = 1.3 the alpha has already saturated at 127.5/255 while
        // the widths are still moving.
        assert!(near(fwd[0].alpha, DOT_INACTIVE_ALPHA), "{}", fwd[0].alpha);
        assert!(near(fwd[1].alpha, PAGE_INDICATOR_ALPHA), "{}", fwd[1].alpha);
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
                            assert!(dot.x.is_finite(), "n={n} p={progress} dot {i} x {}", dot.x);
                            assert!(
                                (0.0..=PAGE_INDICATOR_ALPHA).contains(&dot.alpha),
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
        assert!(
            short.track.h < f.track.h,
            "the two panels really are different"
        );
        assert!(
            short.thumb_h < f.thumb_h,
            "52 dp is fewer px on a smaller panel"
        );
        assert!(near(f.thumb_pad, p.dp(1.0)));
        assert!(near(f.thumb_w(), f.track_w - 2.0 * f.thumb_pad));
        // The thumb is pinned inside the track at both ends.
        assert!(near(f.thumb_y(0.0), f.track.y));
        assert!(
            near(f.thumb_y(1.0) + f.thumb_h, f.track.y + f.track.h),
            "the thumb is a fixed height, so it must never fall out of the track"
        );
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
        assert!(
            f.hit.x < l.w && f.hit.x + f.hit.w > l.w,
            "the target must overhang"
        );
        assert!(
            f.hit.w > f.track_w,
            "the target is wider than the track it guards"
        );

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
            assert!(r.card_w < w && r.card_h < h, "{n}: card is not inset");

            // The card centre is the panel centre in both axes: that is the
            // point the dismiss travel budget and the drawn rect are built
            // from, so it is worth pinning exactly.
            assert!(
                near(r.card_center.0, w * 0.5),
                "{n}: {} vs {}",
                r.card_center.0,
                w * 0.5
            );
            assert!(
                near(r.card_center.1, h * 0.5),
                "{n}: {} vs {}",
                r.card_center.1,
                h * 0.5
            );
            // ...and it really is the card's own centre, not just the panel's.
            assert!(
                near(r.card_center.0, (w - r.card_w) * 0.5 + r.card_w * 0.5),
                "{n}"
            );
            assert!(
                near(r.card_center.1, (h - r.card_h) * 0.5 + r.card_h * 0.5),
                "{n}"
            );

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
            assert!(
                r.actions.y >= card.y + card.h,
                "{n}: band overlaps the card"
            );
            assert!(
                r.actions.x >= 0.0 && r.actions.x + r.actions.w <= w + 0.001,
                "{n}: band off panel"
            );

            // Dismiss metrics.
            assert!(near(r.detach_dp, p.dp(72.0)), "{n}");
            assert!(near(r.dismiss_undershoot, p.dp(25.0)), "{n}");
            assert!(near(r.clear_all_dead_zone, p.dp(70.0)), "{n}");
            assert!(
                r.clear_all_dead_zone < r.detach_dp,
                "{n}: dead zone taller than the detach"
            );
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

        // Four minis, transliterated from
        // `ClippedFolderIconLayoutRule.getPosition` (:140-177):
        //   curNumItems = 4, theta0 = PI (LTR), direction = -1 (:146, :149)
        //   thetaShift  = PI / 4, applied SIGNED: theta0 -= PI / 4 = 3PI/4 (:157)
        //   index swap  2 <-> 3 (:162-166)
        //   theta_i     = 3PI/4 - i * PI/2
        // With r = 1 and icon = 1 the four land on a symmetric square of
        // half-diagonal r * cos(45) / 2 = 0.35355339, inset by half a 0.44
        // mini. In *reading order* that is:
        //
        //     slot 0 = top-left      slot 1 = top-right
        //     slot 2 = bottom-left   slot 3 = bottom-right
        //
        // (y grows downward, so top is the more negative y).
        let r = 1.0f32;
        let d = r * (2.0f32.sqrt() * 0.5) * 0.5;
        let half = 0.44 * 0.5;
        let four = folder_preview_icons(4, (0.0, 0.0), r, 1.0, true);
        let expect = [
            (-d - half, -d - half), // slot 0, theta = 3PI/4   (top-left)
            (d - half, -d - half),  // slot 1, theta =  PI/4   (top-right)
            (-d - half, d - half),  // slot 2, theta = -3PI/4  (bottom-left)
            (d - half, d - half),   // slot 3, theta = - PI/4  (bottom-right)
        ];
        for (i, (x, y)) in expect.iter().enumerate() {
            assert!(near(four[i].0, *x), "slot {i} x {} vs {x}", four[i].0);
            assert!(near(four[i].1, *y), "slot {i} y {} vs {y}", four[i].1);
            assert!(near(four[i].2, 0.44), "slot {i} edge {}", four[i].2);
        }
        // Reading order is the whole contract: the four slots form a 2x2
        // grid, slot 0 top-left, and a `direction` of +1 in LTR would put the
        // whole cluster in the lower-right quadrant and fail all three.
        assert!(
            near(four[0].0, four[2].0),
            "slots 0 and 2 share the left column"
        );
        assert!(
            near(four[1].0, four[3].0),
            "slots 1 and 3 share the right column"
        );
        assert!(four[0].0 < four[1].0, "the left column comes first");
        assert!(
            near(four[0].1, four[1].1),
            "slots 0 and 1 share the top row"
        );
        assert!(
            near(four[2].1, four[3].1),
            "slots 2 and 3 share the bottom row"
        );
        assert!(four[0].1 < four[2].1, "the top row comes first");
        // Consecutive slots differ by a right angle, not by the shift.
        assert!(near(four[1].0 - four[0].0, 2.0 * d));
        assert!(near(four[3].1 - four[0].1, 2.0 * d));
        // The reading-order swap is the whole difference between slot 2 and
        // slot 3: without it, slot 2 would be where slot 3 is.
        assert!(near(four[3].0, four[2].0 + 2.0 * d));

        // Three items: curNumItems = 3 so thetaShift = PI / 2 SIGNED (i.e.
        // theta0 = PI - PI/2 = PI/2, :157), a third of a turn between minis
        // (:169), the 0.51 preview scale and the 0.15 dilation.
        //   theta_0 = PI/2   -> ( 0,        -1) * r/2
        //   theta_1 = -PI/6  -> ( cos30,   +sin30) * r/2
        //   theta_2 = -5PI/6 -> (-cos30,   +sin30) * r/2
        let three = folder_preview_icons(3, (0.0, 0.0), r, 1.0, true);
        let t_half = 0.51 * 0.5;
        let t = folder_preview_icons(3, (0.0, 0.0), r, 1.0, true);
        assert!(near(t[0].0, -t_half), "{}", t[0].0);
        assert!(near(t[0].1, -(r * 0.5 + t_half)), "{}", t[0].1);
        assert!(near(t[1].0, r * 0.433_012_7 - t_half), "{}", t[1].0);
        assert!(near(t[1].1, r * 0.25 - t_half), "{}", t[1].1);
        assert!(near(t[2].0, -r * 0.433_012_7 - t_half), "{}", t[2].0);
        assert!(near(t[2].1, r * 0.25 - t_half), "{}", t[2].1);
        // One mini sits alone at the apex, the other two share the bottom
        // row: that is the reference's "homomorphic" 3-item model, and it
        // only holds because the shift is keyed off `curNumItems` (:151-156).
        assert!(
            t[0].1 < t[1].1 && t[0].1 < t[2].1,
            "the apex mini is on top"
        );
        assert!(near(t[1].1, t[2].1), "the bottom pair shares a baseline");
        assert!(near(t[1].0 - t[2].0, 2.0 * r * 0.433_012_7));
        for e in &t[..3] {
            assert!(near(e.2, 0.51), "edge {}", e.2);
        }
        // The unused fourth slot is zeroed and must not be drawn.
        assert_eq!(t[3], (0.0, 0.0, 0.0));
        assert_eq!(three[3], (0.0, 0.0, 0.0));
        assert!(t[1].2 > four[0].2, "3 items use a bigger preview scale");

        // One and two items: `curNumItems = max(n, 2)` (:142) makes both the
        // 1- and the 2-item preview a *two*-point circle, and the shift
        // table (:151-156) therefore contributes nothing. A shift applied
        // for n < 3 would rotate a lone mini a quarter turn for nothing.
        let one = folder_preview_icons(1, (0.0, 0.0), r, 1.0, true);
        let o_half = 0.51 * 0.5;
        assert!(near(one[0].0, -r * 0.5 - o_half), "{}", one[0].0);
        assert!(near(one[0].1, -o_half), "{}", one[0].1);
        assert!(near(one[0].2, 0.51), "edge {}", one[0].2);
        assert_eq!(one[1], (0.0, 0.0, 0.0), "one mini uses one slot only");
        let two = folder_preview_icons(2, (0.0, 0.0), r, 1.0, true);
        assert!(near(two[0].0, -r * 0.5 - o_half), "{}", two[0].0);
        assert!(near(two[1].0, r * 0.5 - o_half), "{}", two[1].0);
        assert!(near(two[0].1, two[1].1), "the pair shares a baseline");
        assert!(near(two[1].0 - two[0].0, r), "the pair is one radius apart");

        // RTL: `theta0 = 0` and `direction = +1` (:146, :149), and the shift
        // is signed so it *mirrors* instead of rotating a half turn (:157).
        // The square is the same one, read right-to-left:
        //   slot 0 = top-right      slot 1 = top-left
        //   slot 2 = bottom-right   slot 3 = bottom-left
        let rtl = folder_preview_icons(4, (0.0, 0.0), r, 1.0, false);
        let expect_rtl = [
            (d - half, -d - half),  // slot 0, theta =  PI/4  (top-right)
            (-d - half, -d - half), // slot 1, theta = 3PI/4  (top-left)
            (d - half, d - half),   // slot 2, theta = 7PI/4  (bottom-right)
            (-d - half, d - half),  // slot 3, theta = 5PI/4  (bottom-left)
        ];
        for (i, (x, y)) in expect_rtl.iter().enumerate() {
            assert!(near(rtl[i].0, *x), "rtl slot {i} x {} vs {x}", rtl[i].0);
            assert!(near(rtl[i].1, *y), "rtl slot {i} y {} vs {y}", rtl[i].1);
            assert!(near(rtl[i].2, 0.44), "rtl slot {i} edge");
        }
        // The RTL cluster is the LTR one with each row read backwards: the
        // top pair and the bottom pair swap within themselves, and no slot
        // changes row. An un-negated shift would put every slot on the wrong
        // side of the horizontal axis instead.
        for (rtl_i, ltr_i) in [(0usize, 1usize), (1, 0), (2, 3), (3, 2)] {
            assert!(
                near(rtl[rtl_i].0, four[ltr_i].0) && near(rtl[rtl_i].1, four[ltr_i].1),
                "rtl slot {rtl_i} is not the mirror of ltr slot {ltr_i}"
            );
        }
        // The mirror is a mirror: RTL slot 0 is the top-*right* one, so the
        // second slot of each row is the one on the left, and no slot
        // changes row. An un-negated shift would put every slot on the wrong
        // side of the horizontal axis instead.
        assert!(rtl[1].0 < rtl[0].0, "rtl slot 1 is left of slot 0");
        assert!(rtl[3].0 < rtl[2].0, "rtl slot 3 is left of slot 2");
        assert!(
            rtl[0].1 < rtl[2].1 && rtl[1].1 < rtl[3].1,
            "rows do not swap"
        );
        assert!(
            near(rtl[0].1, rtl[1].1) && near(rtl[2].1, rtl[3].1),
            "rows stay level"
        );

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

    /// The reference exposes `folderColumns` and `folderRows` over `2..5`
    /// (`ui/preferences/destinations/FolderPreferences.kt:84,90`). The value
    /// must reach the geometry, and out-of-range must not: the bounds are what
    /// stop a 1-wide "folder" (an app icon in costume) or a 9-wide one that
    /// cannot fit the panel.
    #[test]
    fn the_folder_grid_is_a_parameter_with_the_reference_bounds() {
        let l = Layout::plain(1080.0, 2400.0);

        // Default: the reference phone profile, 3x3 (`device_profiles.xml:57-58`).
        assert_eq!(l.folder().cols, LAWNCHAIR_PHONE_FOLDER);
        assert_eq!(l.folder().rows, LAWNCHAIR_PHONE_FOLDER);
        assert_eq!(LAWNCHAIR_PHONE_FOLDER, 3);

        for (c, r) in [(2usize, 2usize), (5, 5), (4, 2)] {
            let f = FolderLayout::new_with(&l, Some((c, r)), true);
            assert_eq!((f.cols, f.rows), (c, r), "{c}x{r} must survive verbatim");
            assert_eq!(f.items_per_page(), c * r);
        }

        // Out of range is clamped, not refused: a state file from a build with
        // a different range still has to draw a folder.
        for (c, r, want) in [
            (0usize, 0usize, (FOLDER_GRID_MIN, FOLDER_GRID_MIN)),
            (1, 1, (FOLDER_GRID_MIN, FOLDER_GRID_MIN)),
            (9, 9, (FOLDER_GRID_MAX, FOLDER_GRID_MAX)),
            (usize::MAX, usize::MAX, (FOLDER_GRID_MAX, FOLDER_GRID_MAX)),
        ] {
            let f = FolderLayout::new_with(&l, Some((c, r)), true);
            assert_eq!((f.cols, f.rows), want, "{c}x{r} must clamp");
        }
        assert_eq!((FOLDER_GRID_MIN, FOLDER_GRID_MAX), (2, 5));

        // And through the profile, which is the path the shell will take.
        let p = l.profile().with_folder_grid(2, 5);
        assert_eq!((p.folder_cols, p.folder_rows), (2, 5));
        assert_eq!(
            l.profile().with_folder_grid(0, 0).folder_cols,
            FOLDER_GRID_MIN
        );
        let big = l.profile().with_folder_grid(usize::MAX, usize::MAX);
        assert_eq!(big.folder_cols, FOLDER_GRID_MAX);
        assert_eq!(big.folder_rows, FOLDER_GRID_MAX);
        // Each axis is independent, so a 5-wide 2-tall folder is legal.
        assert_eq!(
            l.profile().with_folder_grid(5, 0).folder_rows,
            FOLDER_GRID_MIN
        );
        // A profile copy leaves the workspace grid alone -- the folder grid is
        // a separate preference (`DeviceProfileOverrides.kt:120-122`).
        assert_eq!(p.cols, l.profile().cols);
        assert_eq!(p.rows, l.profile().rows);
    }

    /// A wider grid must stay on the panel, which means the grid is *re-
    /// centred* rather than keeping the 3x3 centre. A 5x5 folder is 5 * 80 dp
    /// wide against 3 * 80 dp, so the old origin would hang half the grid off
    /// the right edge.
    #[test]
    fn a_wider_folder_grid_is_recentred_on_the_panel() {
        let l = Layout::plain(1080.0, 2400.0);
        for cols in FOLDER_GRID_MIN..=FOLDER_GRID_MAX {
            let f = FolderLayout::new_with(&l, Some((cols, 3)), true);
            let g = f.grid();
            assert!(
                g.x >= -0.001 && g.x + g.w <= l.w + 0.001,
                "{cols} wide grid spans {}..{} of {}",
                g.x,
                g.x + g.w,
                l.w
            );
            assert!(near(g.center_x(), l.w * 0.5), "a folder is panel-centred");
        }
    }

    /// `FOLDER_SCRIM_ALPHA_LIGHT` was unreachable: `FolderLayout::new` passed
    /// the dark constant unconditionally, so a light-theme launcher drew a
    /// dark scrim. The reference branches on the theme
    /// (`FolderSpringAnimatorSet.kt:337`).
    #[test]
    fn the_scrim_alpha_follows_the_theme() {
        let l = Layout::plain(1080.0, 2400.0);
        assert!(
            near(l.folder().scrim_alpha, FOLDER_SCRIM_ALPHA_DARK),
            "the default is dark"
        );
        assert!(near(
            FolderLayout::new_with(&l, None, true).scrim_alpha,
            FOLDER_SCRIM_ALPHA_DARK
        ));
        assert!(near(
            FolderLayout::new_with(&l, None, false).scrim_alpha,
            FOLDER_SCRIM_ALPHA_LIGHT
        ));
        // The light scrim is the weaker one; the reversed relation would mean
        // one of the two constants is a typo.
        // The light scrim is the weaker one; the reversed relation would mean
        // one of the two constants is a typo. Bound to locals first so this
        // is a runtime check rather than a constant `true` the optimiser would
        // delete along with the assertion that documented it.
        let (light, dark) = (FOLDER_SCRIM_ALPHA_LIGHT, FOLDER_SCRIM_ALPHA_DARK);
        assert!(
            light < dark,
            "light {light} must be weaker than dark {dark}"
        );
    }

    /// `getRadius` (`ClippedFolderIconLayoutRule.java:179-190`) is a *table*
    /// in the shapes-on branch and an *interpolation* in the shapes-off
    /// branch, and the two disagree at four items: 1.12 against 1.25. This is
    /// the test that keeps them from being unified by accident.
    #[test]
    fn the_preview_radius_has_two_different_branches() {
        assert_eq!(FOLDER_PREVIEW_MAX, 4);
        // The table, :210-218.
        assert!(near(folder_dilation_for_items(1), 0.0));
        assert!(near(folder_dilation_for_items(2), 0.0));
        assert!(near(folder_dilation_for_items(3), FOLDER_DILATION_3));
        assert!(near(folder_dilation_for_items(4), FOLDER_DILATION_4));
        assert!(
            near(folder_dilation_for_items(9), 0.0),
            "clamped to the 4-item row"
        );

        let a = 100.0;
        let m_shapes = folder_layout_radius(a, true);
        let m_flat = folder_layout_radius(a, false);
        assert!(near(m_shapes, 1.2 * a * 0.5) && near(m_flat, 1.15 * a * 0.5));

        for n in 1..=4 {
            let shaped = folder_preview_radius(a, n, true);
            let flat = folder_preview_radius(a, n, false);
            // mRadius * (1 + dilation), :184
            assert!(
                near(shaped, m_shapes * (1.0 + folder_dilation_for_items(n))),
                "shaped n={n}: {shaped}"
            );
            // mRadius * (1 + 0.25 * (n - 2) / 2), :187-188
            let want = m_flat * (1.0 + 0.25 * (n as f32 - 2.0) / 2.0);
            assert!(near(flat, want), "flat n={n}: {flat} vs {want}");
        }
        // The divergence at 4 items is the whole point.
        assert!(near(folder_preview_radius(a, 4, true), m_shapes * 1.12));
        assert!(near(folder_preview_radius(a, 4, false), m_flat * 1.25));
        assert!(
            !near(
                folder_preview_radius(a, 4, true),
                folder_preview_radius(a, 4, false)
            ),
            "the two branches are different functions, not one of them wrong"
        );
        // The cluster grows monotonically with the item count, which is the
        // property the workspace relies on to draw a 4-item folder bigger than
        // a 1-item one.
        let mut prev = 0.0;
        for n in 1..=4 {
            let r = folder_preview_radius(a, n, false);
            assert!(r > prev, "n={n} radius {r} must exceed {prev}");
            prev = r;
        }
    }

    /// The preview is one call so the draw path cannot half-apply it, and it
    /// must be the *same* sweep and radius the standalone helpers produce --
    /// re-deriving either is what put the icon in the wrong quadrant before.
    #[test]
    fn a_folder_preview_reuses_the_sweep_and_the_radius() {
        let l = Layout::plain(1080.0, 2400.0);
        let f = l.folder();
        let icon = f.cell.w;

        for n in 1..=FOLDER_PREVIEW_MAX {
            let p = f.preview(icon, n, true, true);
            assert_eq!(p.n, n);
            assert_eq!(p.background.w, icon, "the preview box is the cell edge");
            let r = folder_preview_radius(icon, n, true);
            assert!(near(p.radius, r), "n={n}: {} vs {r}", p.radius);
            assert!(near(p.background.radius, r), "the box carries the radius");
            let direct = folder_preview_icons(n, (icon * 0.5, icon * 0.5), r, icon, true);
            for (i, (mine, theirs)) in p.icons.iter().zip(direct.iter()).enumerate().take(n) {
                assert_eq!(*mine, *theirs, "slot {i} must be the same point");
            }
            // Unused slots stay dead and must not be drawn.
            for i in n..FOLDER_PREVIEW_MAX {
                assert_eq!(p.icons[i], (0.0, 0.0, 0.0), "slot {i} must stay empty");
            }
            // Every mini stays inside the preview box.
            for i in 0..n {
                let (x, y, e) = p.icons[i];
                assert!(
                    x >= -e && x + e <= icon + e && y >= -e && y + e <= icon + e,
                    "mini {i} of {n} escapes the box"
                );
            }
        }
        // Over the cap it clamps rather than indexing past the array.
        assert_eq!(
            f.preview(icon, 9, true, true),
            f.preview(icon, 4, true, true)
        );
        assert_eq!(
            f.preview(icon, 0, true, true),
            f.preview(icon, 1, true, true)
        );

        // `preview_in` is a pure translation, so it must not move anything
        // relative to the cell.
        let cell = Rect {
            x: 300.0,
            y: 800.0,
            w: icon,
            h: icon,
            radius: 0.0,
        };
        let at = f.preview_in(cell, 4, true, true);
        let raw = f.preview(icon, 4, true, true);
        assert!(near(at.background.x, raw.background.x + cell.x));
        assert!(near(at.background.y, raw.background.y + cell.y));
        for i in 0..4 {
            assert!(near(at.icons[i].0, raw.icons[i].0 + cell.x));
            assert!(near(at.icons[i].1, raw.icons[i].1 + cell.y));
        }
    }

    /// A folder with more than one page's items gets a pager and one without
    /// gets an empty rect, because the reference hides it
    /// (`FolderPagedView.java:496`). 9 is the first item count that pages at
    /// the reference's 3x3 grid, which is the "> 9 items" boundary.
    #[test]
    fn the_pager_appears_past_the_first_page_of_items() {
        let l = Layout::plain(1080.0, 2400.0);
        let f = l.folder();
        let per_page = f.items_per_page();
        assert_eq!(per_page, 9, "3x3 on the reference profile");

        // Not one page: empty rect, centred so `.is_empty()` reads true and
        // the draw path has no way to tell "no pager" from "pager at 0,0".
        for n in [0usize, 1, 8, 9] {
            let p = f.pager(n);
            assert!(p.is_empty(), "{n} items is one page, so no pager: {p:?}");
        }
        // More than one page: a real box inside the footer.
        for n in [10usize, 18, 19, 100] {
            let p = f.pager(n);
            assert!(!p.is_empty(), "{n} items needs a pager");
            let footer = f.footer();
            assert!(
                p.y >= footer.y && p.y + p.h <= footer.y + footer.h + 0.001,
                "{n}: the pager is inside the footer"
            );
            assert!(
                near(p.center_x(), footer.center_x()),
                "the pager is centred"
            );
            assert!(p.h > 0.0 && p.w > 0.0);
        }

        // The page arithmetic the shell must agree with.
        assert_eq!(f.page_count(0), 1, "an empty folder is one page");
        assert_eq!(f.page_count(9), 1);
        assert_eq!(f.page_count(10), 2);
        assert_eq!(f.page_count(18), 2);
        assert_eq!(f.page_count(19), 3);
        // And it follows the grid, not a fixed 9.
        let big = FolderLayout::new_with(&l, Some((5, 5)), true);
        assert_eq!(big.items_per_page(), 25);
        assert_eq!(big.page_count(24), 1);
        assert_eq!(big.page_count(26), 2);
        assert!(
            !big.pager(26).is_empty(),
            "26 items at 25 per page needs dots"
        );
    }

    /// The reference's hit test is *nearest cell*, not "inside a cell"
    /// (`FolderPagedView.findNearestArea`, `:525-536`). A drop in a gap must
    /// still find a target, and a drop past the last item must clamp onto the
    /// last item rather than report nothing.
    #[test]
    fn folder_cell_hit_is_nearest_not_contains() {
        let l = Layout::plain(1080.0, 2400.0);
        let f = l.folder();
        let g = f.grid();

        // Centre of each of the nine cells maps to its own index.
        for row in 0..f.rows {
            for col in 0..f.cols {
                let c = f.cell_at(col, row);
                let hit = f
                    .folder_cell_hit(c.center_x(), c.center_y(), 0, 9, true)
                    .expect("a full folder has a nearest cell");
                assert_eq!(hit, row * f.cols + col, "cell {col},{row}");
            }
        }

        // A point in the *gap* between two cells still resolves, and resolves
        // to the cell it is nearer. This is the difference from `contains`,
        // and it is the reason `findNearestArea` is the port and a
        // `Rect::contains` loop is not.
        let a = f.cell_at(0, 0);
        let b = f.cell_at(1, 0);
        let gap = (a.x + a.w + b.x) * 0.5;
        let hit = f.folder_cell_hit(gap, a.center_y(), 0, 9, true).unwrap();
        assert!(
            hit == 0 || hit == 1,
            "a gap resolves to one of its neighbours"
        );

        // Outside the grid entirely -- in the padding -- resolves to the edge. The
        // vertical centre is row 1, so the edge indices are `cols` and
        // `2 * cols`, not 0 and `cols - 1`.
        assert_eq!(
            f.folder_cell_hit(-500.0, g.center_y(), 0, 9, true),
            Some(f.cols),
            "far left clamps to column 0"
        );
        assert_eq!(
            f.folder_cell_hit(10_000.0, g.center_y(), 0, 9, true),
            Some(2 * f.cols - 1),
            "far right clamps to the last column"
        );
        // And vertically, into the first and last rows. `cell_at(0, 0)`'s x is
        // used rather than the grid centre, which sits in the *middle* column.
        let top_left = f.cell_at(0, 0);
        assert_eq!(
            f.folder_cell_hit(top_left.center_x(), -500.0, 0, 9, true),
            Some(0)
        );
        // Below the grid clamps the *row* only: the column is untouched, so
        // this is the bottom-left cell rather than the last one.
        assert_eq!(
            f.folder_cell_hit(top_left.center_x(), 10_000.0, 0, 9, true),
            Some(f.cols * (f.rows - 1))
        );

        // Past the last item on the last page clamps onto the last item, the
        // reference's `Math.min(total - 1, ...)`.
        assert_eq!(
            f.folder_cell_hit(g.center_x(), g.center_y(), 0, 4, true),
            Some(3)
        );

        // Page 1 continues where page 0 stopped, at `page * per_page`.
        assert_eq!(
            f.folder_cell_hit(a.center_x(), a.center_y(), 1, 18, true),
            Some(9),
            "page 1 cell 0 is rank 9"
        );
        assert_eq!(
            f.folder_cell_hit(b.center_x(), b.center_y(), 1, 18, true),
            Some(10)
        );

        // RTL mirrors the column, not the page (`FolderPagedView.java:529-531`).
        assert_eq!(
            f.folder_cell_hit(a.center_x(), a.center_y(), 0, 9, false),
            Some(f.cols - 1),
            "RTL mirrors within the page"
        );
        assert_eq!(
            f.folder_cell_hit(a.center_x(), a.center_y(), 1, 18, false),
            Some(9 + f.cols - 1)
        );

        // An empty folder has nothing to be nearest to.
        assert_eq!(
            f.folder_cell_hit(g.center_x(), g.center_y(), 0, 0, true),
            None
        );
    }

    /// The surface is what the container morph grows into. It must be on the
    /// panel at every grid the reference will configure: a 5x5 folder's cells
    /// are 470 dp tall against a 420 dp panel, so an unclamped surface is
    /// taller than the screen.
    #[test]
    fn the_folder_surface_is_the_grid_plus_its_chrome() {
        let l = Layout::plain(1080.0, 2400.0);
        let f = l.folder();
        let s = f.surface(l.w, l.h);
        let g = f.grid();
        assert!(near(s.w, g.w + f.pad_lr * 2.0), "{:?}", s);
        assert!(near(s.h, f.pad_top + g.h + f.footer_h));
        assert!(near(s.x, g.x - f.pad_lr));
        assert!(near(s.y, f.pad_top));
        assert!(
            near(s.radius, f.footer_h * 0.5),
            "a sheet's corner is its half-height"
        );

        // Clamped, and never degenerate.
        for c in FOLDER_GRID_MIN..=FOLDER_GRID_MAX {
            for r in FOLDER_GRID_MIN..=FOLDER_GRID_MAX {
                let f = FolderLayout::new_with(&l, Some((c, r)), true);
                let s = f.surface(l.w, l.h);
                assert!(
                    s.w > 0.0 && s.h > 0.0,
                    "{c}x{r} must have a drawable surface"
                );
                assert!(
                    s.w <= l.w + 0.001 && s.h <= l.h + 0.001,
                    "{c}x{r} surface {}x{} escapes the panel {}x{}",
                    s.w,
                    s.h,
                    l.w,
                    l.h
                );
            }
        }
    }

    /// `item_icon` is derived from the cell so a retuned `folder_cell_w_dp`
    /// moves the icon with it, and the icon must be square and inside its cell.
    #[test]
    fn a_folder_item_icon_fits_its_cell_and_tracks_the_page() {
        let l = Layout::plain(1080.0, 2400.0);
        let f = l.folder();
        for row in 0..f.rows {
            for col in 0..f.cols {
                let index = row * f.cols + col;
                let c = f.cell_at(col, row);
                let i = f.item_icon(index, 0);
                assert!(near(i.w, i.h), "an icon is square: {:?}", i);
                assert!(
                    near(i.w, c.w.min(c.h)),
                    "sized from the cell, not a second token"
                );
                assert!(near(i.center_x(), c.center_x()), "centred in its cell");
                assert!(i.y >= c.y - 0.001 && i.y + i.h <= c.y + c.h + 0.001);
                // Page 1 shifts the mapping by a page.
                let p1 = f.item_icon(index + f.items_per_page(), 1);
                assert!(
                    near(p1.x, i.x) && near(p1.y, i.y),
                    "the same cell on page 1"
                );
            }
        }
        // An index past the page maps back onto the last cell rather than
        // indexing off the end: `saturating_sub` at page 1 for index 0.
        let a = f.item_icon(0, 0);
        let b = f.item_icon(0, 1);
        assert!(
            near(a.x, b.x),
            "page 1 of an out-of-range index is page 0's cell"
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
        assert!(
            m.arrow_w < m.outer_r * 2.0,
            "the arrow is narrower than the popup"
        );
        assert!(near(m.arrow_center, -p.dp(26.0)));
        assert!(near(m.arrow_center.abs(), p.dp(26.0)));
        assert!(
            m.arrow_center < 0.0,
            "a right-hand anchor is the trailing edge"
        );
        assert!(
            m.arrow_center.abs() < m.item_w * 0.5,
            "the arrow sits inside the item row"
        );

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

        // 2 dp elevation, 10 / 14 / 4 dp padding, 2 dp margin.
        assert!(near(m.elevation, p.dp(2.0)));
        assert!(near(m.pad_start, p.dp(10.0)));
        assert!(near(m.pad_end, p.dp(14.0)));
        assert!(near(m.pad_v, p.dp(4.0)));
        // `SHORTCUT_COLLAPSE_THRESHOLD = 6` (`PopupContainerWithArrow.java:95`):
        // the reference collapses a popup to a single icon strip past six rows,
        // so six is the largest list that is ever drawn as rows. This was 4
        // while `draw_popup` capped at 6 and `height_for` capped at 4, which
        // meant the surface was sized for four rows and six were drawn into it.
        assert_eq!(m.max_items, 6);
        assert!(near(m.container_margin, p.dp(2.0)));
        assert!(m.pad_start < m.pad_end, "the reference is asymmetric");
        // The item text band is what is left after the paddings.
        assert!(near(
            m.item_w,
            m.pad_start + m.pad_end + m.item_w - m.pad_start - m.pad_end
        ));

        // The height accounts for the arrow overhang and clamps at `max_items`.
        assert!(m.height_for(1) < m.height_for(2));
        assert!(near(m.height_for(4) - m.height_for(3), m.item_h));
        assert!(near(m.height_for(99) - m.height_for(6), 0.0));
    }

    /// `place` and `hit` are the shared geometry the renderer and the shell's
    /// tap path both call, so the invariant that matters is: a point inside a
    /// drawn row resolves to that row, and a point outside resolves to nothing.
    #[test]
    fn a_popup_row_is_tappable_exactly_where_it_is_drawn() {
        let (w, h) = (1080.0f32, 2400.0f32);
        let l = Layout::plain(w, h);
        let anchor = Rect {
            x: 540.0,
            y: 1200.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        };
        let m = l.popup_menu(anchor);
        let place = m.place(&l, 540.0, 1200.0, 4, 1.0);

        // Fully open, so the surface is its real size and the rows are bands of
        // exactly `item_h`.
        assert_eq!(place.rows, 4);
        assert!(near(place.w, m.item_w));
        assert!(near(place.h, m.item_h * 4.0));
        assert!(
            near(place.x + place.w * 0.5, 540.0),
            "centred on the anchor"
        );

        for i in 0..place.rows {
            let cy = place.y + (i as f32 + 0.5) * m.item_h;
            assert_eq!(m.hit(&place, place.x + 1.0, cy), Some(i), "row {i} centre");
            assert_eq!(
                m.hit(&place, place.x + place.w - 1.0, cy),
                Some(i),
                "row {i} right edge"
            );
        }

        // Just past the last row is a miss, not a wrap into row 0.
        assert_eq!(m.hit(&place, place.x + 1.0, place.y + place.h), None);
        // Left of the surface, and above it.
        assert_eq!(m.hit(&place, place.x - 1.0, place.y + m.item_h * 0.5), None);
        assert_eq!(m.hit(&place, place.x + 1.0, place.y - 1.0), None);
    }

    /// A popup is not tappable until it is half open: at 20% the surface is a
    /// sliver centred on the long-press point, and a fast re-tap on that same
    /// point would land in row 0 -- which is "App info" on an icon menu.
    #[test]
    fn a_popup_is_never_tappable_until_it_is_half_open() {
        let (w, h) = (1080.0f32, 2400.0f32);
        let l = Layout::plain(w, h);
        let m = l.popup_menu(Rect {
            x: 540.0,
            y: 1200.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        });
        for progress in [0.0f32, 0.01, 0.2, 0.49] {
            let place = m.place(&l, 540.0, 1200.0, 4, progress);
            assert_eq!(m.hit(&place, 540.0, 1200.0), None, "progress {progress}");
        }
        // Half open it starts dispatching, and still resolves row 0 at the
        // anchor because that is where row 0's centre is.
        let place = m.place(&l, 540.0, 1200.0, 4, 0.5);
        assert_eq!(m.hit(&place, 540.0, 1200.0), Some(0));
    }

    /// The surface is clamped inside the panel, and a long list clamps its row
    /// count rather than overflowing.
    #[test]
    fn a_popup_is_clamped_inside_the_panel() {
        let (w, h) = (1080.0f32, 2400.0f32);
        let l = Layout::plain(w, h);
        let m = l.popup_menu(Rect {
            x: 540.0,
            y: 1200.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        });

        // Anchored near the bottom edge.
        let bottom = m.place(&l, 540.0, h - 4.0, 4, 1.0);
        assert!(
            bottom.y + bottom.h <= h * 0.98 + 0.5,
            "bottom edge stays in the panel"
        );

        // Anchored at a corner.
        let corner = m.place(&l, 8.0, 8.0, 4, 1.0);
        assert!(corner.x >= l.w * 0.02 - 0.5 && corner.y >= 0.0);

        // Nine rows is more than the reference ever draws as rows.
        let long = m.place(&l, 540.0, 1200.0, 9, 1.0);
        assert_eq!(long.rows, m.max_items);
        assert!(near(long.h, m.item_h * m.max_items as f32));
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
        assert!(
            near(open.handle.radius, p.dp(2.0)),
            "{}",
            open.handle.radius
        );
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
        assert!(
            container.y < open.search.y
                && container.y + container.h > open.search.y + open.search.h
        );
        // The box is centred in the container: the same 4 dp of air above and
        // below, and 60 - 52 = 8 dp of it in total.
        assert!(near(
            open.search.y - container.y,
            (container.h - open.search.h) * 0.5
        ));
        assert!(near(
            container.y + container.h - (open.search.y + open.search.h),
            (container.h - open.search.h) * 0.5
        ));
        assert!(near(container.h - open.search.h, p.dp(8.0)));
        assert!(near((container.h - open.search.h) * 0.5, p.dp(4.0)));
        assert!(near(open.search.x, p.dp(p.edge_margin_dp)));
        assert!(near(open.search.w, w - p.dp(p.edge_margin_dp) * 2.0));
        assert!(
            near(open.search.radius, p.dp(26.0)),
            "a 52 dp field is a pill"
        );

        // 48 dp header pill, r 12 dp; 128 x 2 dp divider, r 2 dp.
        assert!(near(open.header.h, p.dp(48.0)), "{}", open.header.h);
        assert!(
            near(open.header.radius, p.dp(12.0)),
            "{}",
            open.header.radius
        );
        assert!(near(open.divider.w, p.dp(128.0)), "{}", open.divider.w);
        assert!(near(open.divider.h, p.dp(2.0)), "{}", open.divider.h);
        assert!(near(open.divider.w, 329.142_88));
        assert!(near(open.divider.h, 5.142_857));
        assert!(near(open.divider.x, (w - p.dp(128.0)) * 0.5));

        // Predictions: a 108 dp container holding a 65 dp icon with its label
        // *underneath*, laid out the way the main drawer grid lays them out.
        //
        // `PredictionRowView.getExpectedHeight()` (`PredictionRowView.java:
        // 149-161`) is
        //     iconHeight(65) + iconPadding(7) + textHeight(16)
        //   + mVerticalPadding * 2 (8 + 8) + topRowExtra(4)          = 108 dp
        // and `onMeasure` hands it to `MeasureSpec.EXACTLY` (:136-140), so
        // this is the row's height by construction. The old band was one icon
        // (65 dp) tall, which silently dropped the label, the 7 dp drawable
        // padding, the 16 dp line box and both 8 dp pads off the bottom of the
        // sheet.
        assert_eq!(
            PREDICTION_ICON_PAD_DP, 7.0,
            "all_apps_icon_drawable_padding"
        );
        assert_eq!(PREDICTION_LABEL_H_DP, 16.0, "calculateTextHeight(13 sp)");
        assert_eq!(
            PREDICTION_PAD_V_DP, 8.0,
            "all_apps_predicted_icon_vertical_padding"
        );
        assert_eq!(
            PREDICTION_TOP_EXTRA_DP, 4.0,
            "all_apps_search_top_row_extra_height"
        );
        assert_eq!(PREDICTION_ROW_H_DP, 108.0);
        assert!(near(
            PREDICTION_ROW_H_DP,
            APP_ICON_DP
                + PREDICTION_ICON_PAD_DP
                + PREDICTION_LABEL_H_DP
                + PREDICTION_PAD_V_DP * 2.0
                + PREDICTION_TOP_EXTRA_DP
        ));
        assert!(
            near(open.predictions.h, p.dp(PREDICTION_ROW_H_DP)),
            "{}",
            open.predictions.h
        );
        assert!(
            near(open.predictions.h, 277.714_3),
            "{}",
            open.predictions.h
        );
        // The row is a *container*, not a tile, so it carries no corner radius
        // of its own; the icon inside it does.
        assert_eq!(open.predictions.radius, 0.0);
        assert!(
            open.predictions.h > p.dp(p.icon_dp),
            "the row is more than an icon"
        );
        // Icon: 65 dp, flush to the row's leading edge, at its top.
        let icon = open.prediction_icon();
        assert!(near(icon.w, p.dp(p.icon_dp)), "{}", icon.w);
        assert!(near(icon.h, p.dp(p.icon_dp)), "{}", icon.h);
        assert!(near(icon.x, open.predictions.x));
        assert!(near(icon.y, open.predictions.y));
        assert!(near(icon.radius, icon.w * ICON_RADIUS));
        // Label: below the icon, one 7 dp drawable padding down, same width.
        let text = open.prediction_label();
        assert_eq!(text, open.prediction_text(), "the alias must not drift");
        assert!(near(text.x, icon.x));
        assert!(near(text.w, icon.w));
        assert!(near(
            text.y - (icon.y + icon.h),
            p.dp(PREDICTION_ICON_PAD_DP)
        ));
        assert!(near(text.y - (icon.y + icon.h), 18.0));
        assert!(text.h > 0.0, "the label has to fit inside the row");
        assert!(
            text.y + text.h <= open.predictions.y + open.predictions.h + 0.001,
            "the label escapes the row"
        );
        // The two bands stack, they do not sit side by side.
        assert!(
            icon.y + icon.h <= text.y,
            "the label is under the icon, not beside it"
        );
        // And the 12 dp gap to the grid is measured from the ROW's bottom, not
        // from the icon's: the list must not start on top of the label.
        assert!(near(
            open.grid.y - (open.predictions.y + open.predictions.h),
            12.0 * p.dp
        ));
        assert!(
            open.grid.y > text.y + text.h,
            "the grid does not overlap the label"
        );

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
        assert!(
            open.grid.h > open.grid_row_h * 6.0,
            "only {} rows fit",
            open.grid.h / open.grid_row_h
        );

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
            assert!(
                (p + n).abs() < 1e-3,
                "not antisymmetric at f = {f}: {p} vs {n}"
            );
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
        assert_copy::<GridMetrics>();
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
                    hotseat_icon_space(l.dock.w, p.hotseat_icons, p.dp(p.icon_dp), p.dp),
                    folder_layout_radius(p.dp(p.icon_dp), true),
                    folder_preview_icons(4, (0.0, 0.0), 1.0, 1.0, true),
                    damped_scroll(50.0, l.w * 0.5),
                    page_indicator_dots(&l.page_indicator(), 3, 0, 1, 0.5),
                );
            }
        }
    }

    // =======================================================================
    // Folder gestures: touch dispatch, reorder, drag-out, menu
    // =======================================================================

    /// A 3x3 folder on a reference panel, which is what the shell runs.
    fn folder() -> (Layout, FolderLayout) {
        let l = Layout::new(1080.0, 2400.0, false);
        let f = FolderLayout::new_with(&l, Some((3, 3)), true);
        (l, f)
    }

    /// The centre of a folder cell, for driving the touch tests.
    ///
    /// Page-local slot to grid position, the same `col = slot % cols` split
    /// [`FolderLayout::item_icon`] uses. A helper that divided by
    /// `items_per_page` instead would put slot 3 in *column* 3 of a 3-column
    /// grid -- off the right edge, where every touch test reads "outside" and
    /// every assertion about it fails for a reason that has nothing to do with
    /// the code under test.
    fn cell_pt(f: &FolderLayout, slot: usize) -> (f32, f32) {
        let cols = f.cols.max(1);
        f.cell_at(slot % cols, slot / cols).center()
    }

    /// `surface_unclamped` and the panel-clamped `surface` agree everywhere they
    /// overlap, and the clamp only ever shrinks.
    ///
    /// The reason this is a test rather than an obvious identity: `surface` is
    /// `draw_folder`'s box and `surface_unclamped` is `touch_at`'s, and the
    /// failure the split could introduce is a touch test that thinks a point is
    /// inside a sheet that is not drawn, or the reverse, on a panel short enough
    /// for the clamp to bite. A 5x5 folder is the case that bites.
    #[test]
    fn surface_and_its_unclamped_form_differ_only_by_the_clamp() {
        for &(w, h) in PANELS {
            let l = Layout::new(w, h, false);
            for grid in [(2usize, 2usize), (3, 3), (5, 5)] {
                let f = FolderLayout::new_with(&l, Some(grid), true);
                let u = f.surface_unclamped();
                let s = f.surface(w, h);
                assert!(
                    near(u.x, s.x) && near(u.y, s.y),
                    "{w}x{h} {grid:?}: origin moved"
                );
                assert!(near(u.radius, s.radius), "{w}x{h} {grid:?}: radius moved");
                assert!(
                    s.w <= u.w + 1e-3 && s.h <= u.h + 1e-3,
                    "{w}x{h} {grid:?}: the clamp grew the box"
                );
                assert!(
                    s.w > 0.0 && s.h > 0.0,
                    "{w}x{h} {grid:?}: the clamp emptied it"
                );
                // The surface contains the whole grid -- but only where the clamp
                // did not bite. On a 600x600 panel a 5x5 folder's grid is
                // genuinely taller than the screen, and truncating the surface to
                // the panel is the documented behaviour, not a bug to assert away.
                if u.w <= w + 1e-3 && u.h <= h + 1e-3 {
                    let g = f.grid();
                    assert!(
                        s.contains(g.x, g.y) && s.contains(g.x + g.w - 1.0, g.y + g.h - 1.0),
                        "{w}x{h} {grid:?}: the grid escapes a surface that fits"
                    );
                } else {
                    assert!(
                        near(s.w, w) || near(s.h, h),
                        "{w}x{h} {grid:?}: clamp missed"
                    );
                }
            }
        }
    }

    /// A touch on a cell is that cell, and a touch *outside* every cell still
    /// resolves to the nearest one.
    ///
    /// The second half is the reference's own rule and the reason
    /// `FolderTouch::Member::rect` is documented as not necessarily containing
    /// the touch point: `FolderPagedView.findNearestArea` (`:525-535`) asks the
    /// page for the nearest area rather than testing containment, and a strict
    /// `contains` drops the in-between taps a drag-to-reorder is made of.
    #[test]
    fn touch_at_resolves_a_cell_by_nearest_area_not_containment() {
        let (_, f) = folder();
        let n = 6usize;
        for slot in 0..n {
            let (x, y) = cell_pt(&f, slot);
            match f.touch_at(x, y, n) {
                FolderTouch::Member { index, rect } => {
                    assert_eq!(index, slot, "cell {slot} resolved to {index}");
                    let own = f.cell_at(slot % 3, slot / 3);
                    assert!(
                        near(rect.x, own.x) && near(rect.y, own.y),
                        "slot {slot}: the rect is not the cell's own"
                    );
                }
                other => panic!("cell {slot} resolved to {other:?}"),
            }
        }
        // 12 dp to the left of the whole grid is in the `pad_lr` gutter: outside
        // every cell, so a *tap* there hits nothing. See the note on
        // `touch_at_dir` for why the drag resolves the same point differently --
        // `a_tap_and_a_drag_disagree_in_the_gutter_on_purpose` is the test for
        // that half.
        let g = f.grid();
        let (x0, y0) = (g.x - 12.0, cell_pt(&f, 0).1);
        assert_eq!(
            f.touch_at(x0, y0, n),
            FolderTouch::Blank,
            "the left gutter is not a cell"
        );
    }

    /// A tap and a drag resolve the folder's gutter differently, and both are the
    /// reference's behaviour.
    ///
    /// The reference has two different functions and they are not the same
    /// function. A *tap* is a plain view click, so the `pad_lr` gutter belongs
    /// to the container and hits nothing. A *drag* target is
    /// `Folder.getTargetRank` -> `findNearestArea`
    /// (`Folder.java:1199-1202`, `FolderPagedView.java:525-535`), which
    /// deliberately resolves the nearest cell rather than testing containment,
    /// because "dropping between two icons" has to land somewhere. One function
    /// for both would mean either a tap in a gutter launches an app, or a drag
    /// released in a gutter does nothing -- and the second is a silent data loss
    /// on drop, which is the worse of the two.
    #[test]
    fn a_tap_and_a_drag_disagree_in_the_gutter_on_purpose() {
        let (_, f) = folder();
        let n = 6usize;
        let g = f.grid();
        let (x, y) = (g.x - 12.0, cell_pt(&f, 0).1);
        assert_eq!(
            f.touch_at(x, y, n),
            FolderTouch::Blank,
            "a tap in the gutter"
        );
        assert_eq!(
            f.reorder_index(x, y, n),
            Some(0),
            "a drag released in the gutter opens the gap at the nearest cell"
        );
        // And they agree everywhere that is not the gutter, which is what makes
        // the disagreement a decision rather than an accident.
        for slot in 0..n {
            let (cx, cy) = cell_pt(&f, slot);
            let tap = match f.touch_at(cx, cy, n) {
                FolderTouch::Member { index, .. } => index,
                other => panic!("cell {slot}: {other:?}"),
            };
            assert_eq!(f.reorder_index(cx, cy, n), Some(tap), "cell {slot}");
        }
    }

    /// The footer is the dismiss band, the gutters are blank, and off the sheet
    /// is outside.
    ///
    /// The gutter case is the one a nearest-area hit test gets wrong in the other
    /// direction. There is no gap *between* cells to mistake for one --
    /// `styles.xml:505-506` puts the icon's transparent margin inside the cell
    /// already -- but there is 16 dp of `pad_lr` outside the grid, and that is
    /// real space. Treating it as a cell would give every folder a phantom ring
    /// of icons.
    #[test]
    fn the_footer_is_the_dismiss_band_and_the_gutters_are_blank() {
        let (_, f) = folder();
        let s = f.surface_unclamped();
        let n = 4usize;
        assert_eq!(
            f.touch_at(s.x + 2.0, f.cell.y + f.cell.h * 0.5, n),
            FolderTouch::Blank,
            "the pad_lr gutter is blank"
        );
        // The blank band is *below* the footer, not above the grid --
        // `surface_unclamped`'s doc explains why, and the assertion is here so
        // the explanation cannot quietly become wrong.
        let footer = f.footer();
        assert_eq!(
            f.touch_at(s.center_x(), s.y + s.h - 1.0, n),
            FolderTouch::Blank,
            "the padding below the footer is blank"
        );
        let (fx, fy) = footer.center();
        match f.touch_at(fx, fy, n) {
            FolderTouch::Dismiss { rect } => {
                assert!(near(rect.y, footer.y) && near(rect.h, footer.h));
            }
            other => panic!("the footer resolved to {other:?}, not the dismiss band"),
        }
        assert_eq!(
            f.touch_at(s.center_x(), s.y - 5.0, n),
            FolderTouch::Outside,
            "above the sheet is outside"
        );
        assert_eq!(
            f.touch_at(s.center_x(), s.y + s.h + 5.0, n),
            FolderTouch::Outside,
            "below the sheet is outside"
        );
    }

    /// An empty folder has no member to resolve to, so its grid is blank rather
    /// than handing the shell an index into nothing.
    #[test]
    fn an_empty_folder_never_reports_a_member() {
        let (_, f) = folder();
        for slot in 0..9 {
            let (x, y) = cell_pt(&f, slot);
            assert_eq!(
                f.touch_at(x, y, 0),
                FolderTouch::Blank,
                "cell {slot} of an empty folder"
            );
        }
    }

    /// The touch result and the insertion index resolve the same cell, read two
    /// ways -- which is the whole point of returning the rect.
    ///
    /// Both go through `folder_cell_hit`, so a tap on a cell and a drag dropped
    /// on that same cell must name the same item. Two derivations of "which item
    /// is that" is the bug the `Rect` in `FolderTouch::Member` exists to
    /// prevent, and the shell has no way to detect it from the outside.
    #[test]
    fn touch_and_reorder_resolve_the_same_cell() {
        let (_, f) = folder();
        for n in [1usize, 4, 6, 9] {
            for slot in 0..n {
                let (x, y) = cell_pt(&f, slot);
                let touched = match f.touch_at(x, y, n) {
                    FolderTouch::Member { index, .. } => index,
                    other => panic!("n={n} slot={slot}: {other:?}"),
                };
                let dropped = f
                    .reorder_index(x, y, n)
                    .unwrap_or_else(|| panic!("n={n} slot={slot}: no insertion index"));
                assert_eq!(touched, dropped, "n={n} slot={slot}");
            }
        }
    }

    /// An insertion index is clamped to the item count, so a 4-item folder in a
    /// 3x3 grid cannot be told to insert at slot 5.
    ///
    /// The reference's clamp is `Math.min(allocated - 1, ...)`
    /// (`FolderPagedView.java:533`). Without it a drag released on the empty
    /// tail of a partly-filled folder opens a gap past the last item, and the
    /// reorder is a no-op that still looks like it worked.
    #[test]
    fn an_insertion_index_never_runs_past_the_last_item() {
        let (_, f) = folder();
        let g = f.grid();
        let (x, y) = (g.x + g.w - 1.0, g.y + g.h - 1.0);
        assert_eq!(f.reorder_index(x, y, 4), Some(3));
        assert_eq!(f.reorder_index(x, y, 1), Some(0));
        assert_eq!(f.reorder_index(x, y, 9), Some(8));
        // No items at all: no insertion point, not slot 0.
        assert_eq!(f.reorder_index(x, y, 0), None);
    }

    /// The reflow is the remove-and-insert permutation: every cell is occupied
    /// exactly once except the one the gap opened at.
    ///
    /// Stated as a bijection rather than as a table of expected coordinates
    /// because that is the property that matters. A reflow that duplicated a cell
    /// or dropped one looks correct in a screenshot and loses an app on reorder,
    /// and the arithmetic producing it has three off-by-one opportunities --
    /// `slot > from`, `insertion > pos`, and the `from` case itself -- which is
    /// exactly the kind of code where an invariant and a worked example catch
    /// different things.
    #[test]
    fn reflow_is_a_permutation_with_one_gap() {
        let (_, f) = folder();
        for n in 1..=9usize {
            for from in 0..n {
                for insertion in 0..n {
                    let mut seen = [false; 9];
                    let mut drawn = 0usize;
                    for slot in 0..n {
                        match f.reflow_slot(slot, from, insertion, true) {
                            // The lifted cell is not in the grid at all.
                            None => assert_eq!(
                                slot, from,
                                "n={n} from={from} ins={insertion}: only the lifted cell \
                                 may be skipped, but slot {slot} was"
                            ),
                            Some(r) => {
                                let col = ((r.x - f.cell.x) / f.cell.w).round() as usize;
                                let row = ((r.y - f.cell.y) / f.cell.h).round() as usize;
                                let idx = row * 3 + col;
                                assert!(
                                    !seen[idx],
                                    "n={n} from={from} ins={insertion}: two cells landed on \
                                     slot {idx}"
                                );
                                // The one hole is the insertion point, and it is
                                // where the lifted cell is about to land.
                                assert_ne!(
                                    idx, insertion,
                                    "n={n} from={from} ins={insertion}: a cell was drawn in \
                                     the gap"
                                );
                                seen[idx] = true;
                                drawn += 1;
                            }
                        }
                    }
                    // Every cell but the insertion point is occupied, and the
                    // insertion point is inside the grid.
                    assert_eq!(drawn, n - 1, "n={n} from={from} ins={insertion}");
                    assert!(insertion < n, "n={n} from={from} ins={insertion}");
                    let mut occupied = 0usize;
                    for (i, s) in seen.iter().enumerate().take(n) {
                        assert_eq!(
                            *s,
                            i != insertion,
                            "n={n} from={from} ins={insertion}: slot {i} occupancy"
                        );
                        occupied += usize::from(*s);
                    }
                    assert_eq!(occupied, n - 1, "n={n} from={from} ins={insertion}");
                }
            }
        }
    }

    /// A one-step move shifts exactly one neighbour, in the direction the hole
    /// moved, and a no-op move shifts nothing.
    ///
    /// The direction split is the reference's `direction = 1` / `-1`
    /// (`FolderPagedView.java:719, 737`), and the no-op is its
    /// `if (target == empty) return;` (`:715-717`) -- which is the case that
    /// would otherwise run a 230 ms animation (`REORDER_ANIMATION_DURATION`,
    /// `:74`) with nothing to animate.
    #[test]
    fn a_one_step_move_shifts_exactly_one_neighbour() {
        let (_, f) = folder();
        // 0 -> 1: cell 0 leaves, cell 1 slides left into the hole.
        assert!(f.reflow_slot(0, 0, 1, true).is_none());
        let a = f.reflow_slot(1, 0, 1, true).unwrap();
        assert!(near(a.x, f.cell_at(0, 0).x), "cell 1 slid left");
        assert!(near(a.y, f.cell_at(0, 0).y), "cell 1 changed row");
        // 0 -> 2: cells 1 and 2 both slide left, onto 0 and 1.
        assert!(near(
            f.reflow_slot(1, 0, 2, true).unwrap().x,
            f.cell_at(0, 0).x
        ));
        assert!(near(
            f.reflow_slot(2, 0, 2, true).unwrap().x,
            f.cell_at(1, 0).x
        ));
        // 2 -> 0: the other direction, cells 0 and 1 slide right.
        assert!(near(
            f.reflow_slot(0, 2, 0, true).unwrap().x,
            f.cell_at(1, 0).x
        ));
        // 2 -> 2: nothing moves.
        assert!(f.reflow_slot(2, 2, 2, true).is_none());
        assert!(near(
            f.reflow_slot(0, 2, 2, true).unwrap().x,
            f.cell_at(0, 0).x
        ));
        assert!(near(
            f.reflow_slot(1, 2, 2, true).unwrap().x,
            f.cell_at(1, 0).x
        ));
    }

    /// RTL mirrors a reflow's columns and nothing else.
    ///
    /// The reference mirrors the *column*, not the whole page:
    /// `sTmpArray[0] = page.getCountX() - sTmpArray[0] - 1`
    /// (`FolderPagedView.java:530-531`). So the rows must be untouched, and the
    /// permutation must still be a permutation -- an RTL build that mirrored
    /// positions instead of columns would reorder a *different* order, not the
    /// same order drawn mirrored.
    #[test]
    fn rtl_mirrors_the_reflow_columns_and_nothing_else() {
        let (_, f) = folder();
        // Slot 0 is the lifted one, so it has no grid cell to compare.
        for slot in 1..9usize {
            let ltr = f.reflow_slot(slot, 0, 4, true).unwrap();
            let rtl = f.reflow_slot(slot, 0, 4, false).unwrap();
            assert!(near(ltr.y, rtl.y), "slot {slot}: the row moved under RTL");
            let ltr_col = ((ltr.x - f.cell.x) / f.cell.w).round() as usize;
            let rtl_col = ((rtl.x - f.cell.x) / f.cell.w).round() as usize;
            assert_eq!(
                rtl_col,
                2 - ltr_col,
                "slot {slot}: the column is not mirrored"
            );
        }
    }

    /// The drag has to leave the sheet by more than half a cell before it
    /// counts as an exit -- the reference's stated reason at `Folder.java:1180`.
    ///
    /// Without the inflation, a drag beginning in a corner has its own lifted
    /// icon under the finger and would report "outside" on the very first frame,
    /// so a reorder and an exit would be the same gesture.
    #[test]
    fn drag_out_needs_half_a_cell_of_margin() {
        let (_, f) = folder();
        let (pw, ph) = (1080.0f32, 2400.0f32);
        let s = f.surface(pw, ph);
        let half = f.cell.w * 0.5;
        assert!(!f.is_drag_out(s.center_x(), s.center_y(), pw, ph));
        assert!(
            !f.is_drag_out(s.x - half * 0.5, s.center_y(), pw, ph),
            "half a cell of slack is not an exit"
        );
        assert!(f.is_drag_out(s.x - half * 1.5, s.center_y(), pw, ph));
        // No vertical inflation, so one pixel below the sheet is an exit.
        assert!(f.is_drag_out(s.center_x(), s.y + s.h + 1.0, pw, ph));
        // And the inflated box really is the surface grown by half a cell.
        let area = f.drag_exit_area(pw, ph);
        assert!(near(area.w, s.w + half * 2.0) && near(area.h, s.h));
        assert!(near(area.x, s.x - half));
    }

    /// The menu places three rows in `FolderMenuAction::ALL` order, every row is
    /// hit-testable exactly where it is drawn, and a closed menu hits nothing.
    ///
    /// The closed half is the assertion that matters most: the anchor is the
    /// user's own finger position, so a menu still a sliver around that finger
    /// would have a row under it and a second tap would fire "Rename" on a menu
    /// the user had not seen. The reference's popup has the same shape and the
    /// same guard (`PopupMenuLayout::TAPPABLE_PROGRESS`, `layout.rs:3302`).
    #[test]
    fn the_menu_places_three_rows_and_a_closed_menu_hits_nothing() {
        let (l, f) = folder();
        let (ax, ay) = (l.w * 0.5, l.h * 0.5);
        let shut = f.menu(&l, ax, ay, 0.0);
        assert!(shut.place.w < 1.0 && shut.place.h < 1.0);
        assert!(shut.menu_hit(ax, ay).is_none());
        for r in shut.rows {
            assert!(r.is_empty(), "a closed menu must have no row geometry");
        }
        let open = f.menu(&l, ax, ay, 1.0);
        assert_eq!(open.place.rows, FOLDER_MENU_ROWS);
        for (i, action) in FolderMenuAction::ALL.iter().enumerate() {
            let r = open.rows[i];
            assert!(!r.is_empty(), "row {i} is empty");
            assert!(near(r.h, open.item_h), "row {i} is not one item tall");
            let (x, y) = r.center();
            assert_eq!(open.menu_hit(x, y), Some(*action), "row {i} hit");
            // The label baseline is inside its own row, so text and target agree.
            let base = open.label_y(i);
            assert!(
                base > r.y && base < r.y + r.h,
                "row {i}: label baseline {base} is outside {:?}",
                (r.y, r.y + r.h)
            );
        }
        for i in 1..FOLDER_MENU_ROWS {
            assert!(near(
                open.rows[i].y,
                open.rows[i - 1].y + open.rows[i - 1].h
            ));
        }
        for r in open.rows {
            assert!(r.x >= open.place.x - 1e-3 && r.x + r.w <= open.place.x + open.place.w + 1e-3);
        }
        // A point off the surface hits nothing, which is the dismiss rule.
        assert!(open.menu_hit(open.place.x - 10.0, open.place.y).is_none());
    }

    /// The menu is the same three rows wherever it is anchored, and it stays on
    /// the panel at every edge and corner.
    ///
    /// The edge case is why `PopupMenuLayout::place` exists
    /// (`layout.rs:3261-3292`): a long press near an edge would otherwise place
    /// half the menu off-panel, and the row that goes missing would be the
    /// first one -- "Rename".
    #[test]
    fn the_menu_stays_on_the_panel_at_every_anchor() {
        let (l, f) = folder();
        for &(ax, ay) in &[
            (0.0f32, 0.0f32),
            (l.w, 0.0),
            (0.0, l.h),
            (l.w, l.h),
            (l.w * 0.5, 0.0),
            (l.w * 0.5, l.h),
            (0.0, l.h * 0.5),
            (l.w, l.h * 0.5),
            (l.w * 0.5, l.h * 0.5),
        ] {
            let m = f.menu(&l, ax, ay, 1.0);
            let p = m.place;
            assert!(
                p.x >= -1e-3 && p.y >= -1e-3,
                "anchor ({ax},{ay}) off the top/left"
            );
            assert!(
                p.x + p.w <= l.w + 1e-3 && p.y + p.h <= l.h + 1e-3,
                "anchor ({ax},{ay}) runs off the bottom/right"
            );
            for (i, action) in FolderMenuAction::ALL.iter().enumerate() {
                let (x, y) = m.rows[i].center();
                assert_eq!(
                    m.menu_hit(x, y),
                    Some(*action),
                    "anchor ({ax},{ay}): row {i} is not hit-testable where drawn"
                );
            }
        }
    }

    /// The drop-target bar is full width at the top, with the button centred in
    /// it and the reference's own label.
    #[test]
    fn the_drop_target_bar_holds_one_centred_remove_button() {
        for &(w, h) in PANELS {
            let l = Layout::new(w, h, false);
            let b = drop_target_bar(&l);
            assert!(
                near(b.bar.w, w) && near(b.bar.h, l.profile().dp(DROP_TARGET_BAR_H_DP)),
                "{w}x{h}"
            );
            assert!(
                b.button.center_x() > w * 0.2 && b.button.center_x() < w * 0.8,
                "{w}x{h}: the button is not centred"
            );
            assert!(b.button.w <= b.bar.w && b.button.h <= b.bar.h, "{w}x{h}");
            assert_eq!(b.label, "Remove", "the reference's own string");
            assert!(b.hit(b.button.center_x(), b.button.center_y()), "{w}x{h}");
            assert!(
                !b.hit(0.0, b.bar.h + 1.0),
                "{w}x{h}: below the bar is not the button"
            );
        }
    }

    // =======================================================================
    // Settings pickers
    // =======================================================================

    /// A row rect shaped like the settings panel's own card.
    fn picker_row() -> Rect {
        Rect {
            x: 100.0,
            y: 500.0,
            w: 880.0,
            h: 72.0,
            radius: 17.0,
        }
    }

    /// "No candidates yet" is a dead geometry, and it is dead for all three of
    /// the reasons the contract gives.
    ///
    /// `SettingRow::of` documents that `0` and `None` both mean unknown, and
    /// `SettingRow::at` that `None` means the list is not known -- which is what
    /// `settings::build` produces. All four combinations must land on the same
    /// dead struct, or the panel draws slots for a picker that has none.
    #[test]
    fn a_picker_with_no_candidates_draws_nothing() {
        for (at, of) in [
            (None, None),
            (None, Some(7)),
            (Some(1), None),
            (Some(1), Some(0)),
        ] {
            let p = picker_slots(picker_row(), 34.0, at, of);
            assert!(!p.is_live(), "at={at:?} of={of:?} must not be live");
            assert_eq!(p.n, 0, "at={at:?} of={of:?}");
            assert!(p.readout.is_none(), "at={at:?} of={of:?}");
            assert_eq!(p.selected, None, "at={at:?} of={of:?}");
            for s in p.slots {
                assert!(
                    s.is_empty(),
                    "at={at:?} of={of:?}: a dead slot has geometry"
                );
            }
        }
    }

    /// A live picker reports its position and draws four slots, right-aligned
    /// inside the row, with the selected one marked.
    #[test]
    fn a_live_picker_reports_its_position_and_marks_the_selection() {
        let row = picker_row();
        let p = picker_slots(row, 34.0, Some(3), Some(7));
        assert!(p.is_live());
        assert_eq!(
            p.n, PICKER_SLOT_MAX,
            "a 7-candidate picker fills the window"
        );
        assert_eq!(p.first, 1, "position 3 of 7 is inside the first window");
        assert_eq!(p.selected, Some(2), "position 3 is slot index 2");
        assert_eq!(p.index_of(2), Some(3), "slot 2 is candidate 3");
        // The readout sits exactly where the row's static value would be, so the
        // panel swaps one string for another instead of growing a second line.
        let r = p.readout.unwrap();
        assert!(near(r.y, row.y + row.h * 0.16 + 34.0));
        assert!(near(r.x, row.x + row.h * 0.45));
        // The slots trail the readout, stay inside the row, and do not overlap it.
        for s in p.slots.iter().take(p.n) {
            assert!(
                s.x >= row.x && s.x + s.w <= row.x + row.w,
                "a slot leaves the row"
            );
            assert!(s.x >= r.x + r.w - 1.0, "a slot overlaps the readout");
            assert!(near(s.w, s.h), "a slot is square");
            assert!(near(s.radius, s.h * 0.5), "a slot is a pill");
        }
        // And the band is contiguous, so the slots read as one strip.
        let pitch = p.slots[1].x - p.slots[0].x;
        for i in 1..p.n {
            assert!(
                near(p.slots[i].x, p.slots[i - 1].x + pitch),
                "slot {i} is off pitch"
            );
        }
    }

    /// The four-slot window slides onto the selection and stops at the end of
    /// the list.
    ///
    /// This is the failure the window exists to prevent. With 7 candidates and a
    /// 4-slot window, selecting 7 means the window has to move -- and it has to
    /// move to cover 4..7, not 4..8, or the fourth slot names a candidate that
    /// does not exist. The reference's carousel keeps every candidate in the
    /// list and marks the current one (`WallpaperCarouselView.kt:74-77`), so it
    /// cannot have this bug; a sliding window can, and the `min` is the fix.
    #[test]
    fn the_window_slides_onto_the_selection_and_stops_at_the_end() {
        let row = picker_row();
        // Fewer candidates than slots: every candidate is its own slot.
        let p = picker_slots(row, 34.0, Some(2), Some(3));
        assert_eq!(p.n, 3, "a 3-candidate picker draws three slots, not four");
        assert_eq!(p.first, 1);
        assert_eq!(p.selected, Some(1));
        assert!(
            p.slots[3].is_empty(),
            "the fourth slot is padding, not a hole"
        );
        assert_eq!(picker_slots(row, 34.0, Some(1), Some(7)).first, 1);
        // Position 7 of 7: the window is at the back and covers exactly 4..7.
        let last = picker_slots(row, 34.0, Some(7), Some(7));
        assert_eq!(
            last.first, 4,
            "the window must not run past the last candidate"
        );
        assert_eq!(last.n, 4);
        assert_eq!(
            last.selected,
            Some(3),
            "the last candidate is the last slot"
        );
        assert_eq!(last.index_of(3), Some(7));
        for i in 0..last.n {
            assert!(
                last.index_of(i).unwrap() <= 7,
                "slot {i} names a candidate past 7"
            );
        }
        // The window really moved: same list, different geometry.
        let first = picker_slots(row, 34.0, Some(1), Some(7));
        assert_ne!(first.first, last.first, "the window did not slide");
    }

    /// A position from a list that has since shrunk is clamped, not trusted.
    ///
    /// `at` is 1-based and comes from the shell's picker model, which is
    /// rebuilt whenever the wallpaper directory is rescanned. Between the
    /// rebuild and the next frame the list can be shorter, and an unclamped `at`
    /// would either select a slot off the end -- nothing highlighted at all -- or
    /// read "0 of 7".
    ///
    /// `at == None` is the contract's "no candidates yet" even when `of` is
    /// known, so it is dead geometry: a count with no cursor is not a list the
    /// user can act on, and drawing slots for it would invite a tap on a
    /// candidate the row cannot name.
    #[test]
    fn a_stale_position_is_clamped_into_the_list() {
        let row = picker_row();
        for at in [Some(0u32), Some(1), Some(7), Some(99)] {
            let p = picker_slots(row, 34.0, at, Some(7));
            assert_eq!(p.at, at);
            assert!(p.is_live(), "at={at:?}");
            let sel = p.selected.expect("a live picker always has a position");
            assert!(
                p.index_of(sel).unwrap() <= 7,
                "at={at:?} selected a slot past 7"
            );
        }
        assert!(
            !picker_slots(row, 34.0, None, Some(7)).is_live(),
            "a count with no cursor draws no slots"
        );
    }

    fn assert_copy<T: Copy>() {}
}
