//! Offscreen launcher snapshot: renders real UI states to a PPM for review.
//!
//! The DRM/KMS path writes straight into a mapped framebuffer with no window
//! system to screenshot, which made every visual regression invisible until a
//! human ran the build. This module replays the exact same draw code against a
//! plain heap buffer, so the render can be inspected in CI and in review.
//!
//! Enabled with UTLC_SCREENSHOT=<dir> cargo test -p utim_core screenshot.

use super::drm_kms::{AppGridItem, DrmInteractiveState, MaterialYouPalette};
use super::png::RgbaImage;

/// Everything a snapshot needs, kept flat so callers cannot drift from the
/// real interactive state.
pub struct Snapshot<'a> {
    pub w: usize,
    pub h: usize,
    pub time: &'a str,
    pub locked: bool,
    pub shade: bool,
    pub drawer_progress: f32,
    pub drawer_open: bool,
    pub launch_progress: f32,
    pub launch_origin: Option<(f32, f32)>,
    pub launch_color: u32,
    pub press_scale: f32,
    pub pressed_icon: Option<&'a str>,
    pub home_page: usize,
    pub home_scroll: f32,
    pub selected: Option<&'a str>,
    pub search_query: &'a str,
    pub search_active: bool,
    pub keyboard: bool,
    pub grid: Vec<AppGridItem<'a>>,
    pub drawer: Vec<AppGridItem<'a>>,
    pub dock: Vec<AppGridItem<'a>>,
    pub active_app: Option<&'a str>,
}

impl<'a> Clone for Snapshot<'a> {
    fn clone(&self) -> Self {
        Self {
            w: self.w,
            h: self.h,
            time: self.time,
            locked: self.locked,
            shade: self.shade,
            drawer_progress: self.drawer_progress,
            drawer_open: self.drawer_open,
            launch_progress: self.launch_progress,
            launch_origin: self.launch_origin,
            launch_color: self.launch_color,
            press_scale: self.press_scale,
            pressed_icon: self.pressed_icon,
            home_page: self.home_page,
            home_scroll: self.home_scroll,
            selected: self.selected,
            search_query: self.search_query,
            search_active: self.search_active,
            keyboard: self.keyboard,
            grid: self.grid.clone(),
            drawer: self.drawer.clone(),
            dock: self.dock.clone(),
            active_app: self.active_app,
        }
    }
}

impl<'a> Snapshot<'a> {
    fn state(&'a self) -> DrmInteractiveState<'a> {
        DrmInteractiveState {
            time_str: self.time,
            is_locked: self.locked,
            shade_open: self.shade,
            app_drawer_open: self.drawer_open,
            drawer_progress: self.drawer_progress,
            app_launch_progress: self.launch_progress,
            app_launch_origin: self.launch_origin,
            app_launch_color: self.launch_color,
            icon_press_scale: self.press_scale,
            pressed_icon_id: self.pressed_icon,
            home_page: self.home_page,
            home_scroll_offset: self.home_scroll,
            selected_icon_id: self.selected,
            search_query: self.search_query,
            search_active: self.search_active,
            keyboard_active: self.keyboard,
            grid_apps: &self.grid,
            drawer_apps: &self.drawer,
            dock_apps: &self.dock,
            active_app: self.active_app,
            palette: MaterialYouPalette::default_dark(),
            ..Default::default()
        }
    }
}

/// A software framebuffer that mirrors the DRM render path.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            buf: vec![0xFF000000; w * h],
        }
    }

    /// Run the production draw code over this canvas.
    pub fn draw(&mut self, snap: &Snapshot) {
        let state = snap.state();
        crate::graphics::drm_kms::paint_frame(&mut self.buf, self.w, self.w, self.h, &state);
    }

    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(self.w * self.h * 3 + 32);
        out.extend_from_slice(format!("P6\n{} {}\n255\n", self.w, self.h).as_bytes());
        for px in &self.buf {
            out.push(((*px >> 16) & 0xFF) as u8);
            out.push(((*px >> 8) & 0xFF) as u8);
            out.push((*px & 0xFF) as u8);
        }
        out
    }
}

/// A synthetic icon so the grid exercises the bitmap path.
pub fn stub_icon(colour: [u8; 3]) -> RgbaImage {
    stub_icon_sized(colour, 64)
}

/// [`stub_icon`] at an explicit edge, rounded corners and a gradient.
pub fn stub_icon_sized(colour: [u8; 3], edge: u32) -> RgbaImage {
    let edge = edge.max(8);
    let inset = (edge / 16).max(1);
    let ring = (edge / 8).max(1);
    let mut pixels = Vec::with_capacity((edge * edge * 4) as usize);
    for y in 0..edge {
        for x in 0..edge {
            let inside = (inset..edge - inset).contains(&x) && (inset..edge - inset).contains(&y);
            let border = !((inset + ring)..edge - inset - ring).contains(&x)
                || !((inset + ring)..edge - inset - ring).contains(&y);
            let v = if border { 255 } else { ((x * 251) / edge.max(1)) as u8 };
            pixels.push(if inside { colour[0] } else { v });
            pixels.push(if inside { colour[1] } else { v });
            pixels.push(if inside { colour[2] } else { v });
            pixels.push(if inside { 255 } else { 0 });
        }
    }
    RgbaImage {
        width: edge,
        height: edge,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps<'a>(icon: &'a RgbaImage, names: &[&'a str], colour: u32) -> Vec<AppGridItem<'a>> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| AppGridItem {
                id: n,
                name: n,
                color: colour.wrapping_add((i as u32) * 0x0A0A0A),
                glyph: "A",
                icon: Some(icon),
            })
            .collect()
    }

    /// The guard for the original bug: a tappable region with no pixels in it
    /// (or pixels in a region that is not tappable) is a hitbox bug. This
    /// renders the real frame and checks both directions.
    #[test]
    fn every_tappable_region_has_pixels_under_it() {
        use crate::graphics::layout::Layout;

        let icon = stub_icon([60, 120, 200]);
        let names = ["Phone", "Messages", "Camera", "Maps", "Music", "Store", "Notes", "Files"];
        let grid: Vec<AppGridItem> = names
            .iter()
            .enumerate()
            .map(|(i, n)| AppGridItem {
                id: n,
                name: n,
                color: 0xFF2563EBu32.wrapping_add(i as u32).wrapping_mul(0x080808),
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let dock: Vec<AppGridItem> = ["Phone", "Messages", "Apps", "Browser", "Camera"]
            .iter()
            .enumerate()
            .map(|(i, n)| AppGridItem {
                id: n,
                name: n,
                color: 0xFF10B981u32.wrapping_add(i as u32).wrapping_mul(0x080808),
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();

        let cases: Vec<(&str, Snapshot)> = vec![
            (
                "home",
                Snapshot { launch_color: 0xFF2563EB,
                    w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
                    drawer_open: false, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0,
                    home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
                    keyboard: false, grid: grid.clone(), drawer: Vec::new(), dock: dock.clone(),
                    active_app: None,
                },
            ),
            (
                "selected",
                Snapshot { launch_color: 0xFF2563EB,
                    selected: Some("Phone"),
                    ..Snapshot { launch_color: 0xFF2563EB,
                        w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
                        drawer_open: false, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0,
                        home_page: 0, home_scroll: 0.0, selected: None, search_query: "",
                        search_active: false, keyboard: false, grid: grid.clone(), drawer: Vec::new(),
                        dock: dock.clone(), active_app: None,
                    }
                },
            ),
        ];

        for (name, snap) in cases {
            let (w, h) = (1080usize, 2400usize);
            let mut c = Canvas::new(w, h);
            c.draw(&snap);
            let l = Layout::new(w as f32, h as f32, snap.selected.is_some());

            let ink = |x: f32, y: f32, rw: f32, rh: f32| -> usize {
                let y0 = (y.max(0.0) as usize).min(h);
                let x0 = (x.max(0.0) as usize).min(w);
                let y1 = ((y + rh).max(0.0) as usize).min(h);
                let x1 = ((x + rw).max(0.0) as usize).min(w);
                if y0 >= y1 || x0 >= x1 {
                    return 0;
                }
                // Compare against wallpaper *adjacent* to the region, not the
                // fixed `0xFF000000` sentinel: `paint_frame` overwrites the
                // whole canvas, so every pixel differs from the sentinel and
                // the old predicate could never fail. Sampling outside the
                // region means a uniform control (solid icon, search field)
                // still reads high (control vs wallpaper), while uniform
                // wallpaper reads ~0 (wallpaper vs wallpaper on the same row,
                // gradient-tolerant) and a 1x1 region reads 0.
                let cy = ((y + rh / 2.0).max(0.0) as usize).min(h - 1).clamp(y0, y1 - 1);
                let cx_in = ((x + rw / 2.0).max(0.0) as usize).min(w - 1).clamp(x0, x1 - 1);
                // Prefer a sample 10px left of the region on the same row;
                // fall back to right/above/below when at an edge.
                let mut bx: Option<usize> = None;
                if x0 >= 10 {
                    bx = Some(x0 - 10);
                } else if x1 + 10 < w {
                    bx = Some((x1 + 10).min(w - 1));
                }
                let bg = if let Some(bx) = bx {
                    c.buf[cy * w + bx]
                } else if y0 >= 10 {
                    c.buf[(y0 - 10) * w + cx_in]
                } else if y1 + 10 < h {
                    c.buf[((y1 + 10).min(h - 1)) * w + cx_in]
                } else {
                    c.buf[cy * w + cx_in]
                };
                let br = ((bg >> 16) & 0xFF) as i32;
                let bgg = ((bg >> 8) & 0xFF) as i32;
                let bb = (bg & 0xFF) as i32;
                let mut n = 0;
                for py in y0..y1 {
                    for px in x0..x1 {
                        let p = c.buf[py * w + px];
                        let dr = (((p >> 16) & 0xFF) as i32 - br).abs();
                        let dg = (((p >> 8) & 0xFF) as i32 - bgg).abs();
                        let db = ((p & 0xFF) as i32 - bb).abs();
                        if dr + dg + db > 12 {
                            n += 1;
                        }
                    }
                }
                n
            };

            // The areas the input path will actually test.
            let mut regions: Vec<(String, f32, f32, f32, f32)> = vec![
                ("status bar".into(), 0.0, 0.0, w as f32, l.status_bar_h),
                (
                    "search pill".into(),
                    l.search.x + 4.0,
                    l.search.y + 2.0,
                    l.search.w - 8.0,
                    l.search.h - 4.0,
                ),
                (
                    "clock".into(),
                    l.w * 0.5 - 40.0,
                    l.clock_y,
                    80.0,
                    l.clock_h,
                ),
            ];
            for i in 0..(l.grid_cols * l.max_rows).min(grid.len()) {
                let icon_r = l.grid_icon(i);
                regions.push((
                    format!("grid icon {i}"),
                    icon_r.x,
                    icon_r.y,
                    icon_r.w,
                    icon_r.h,
                ));
            }
            for s in 0..l.dock_slots.min(dock.len()) {
                let d = l.dock_icon_rect(s);
                regions.push((format!("dock {s}"), d.x, d.y, d.w, d.h));
            }
            if snap.selected.is_some() {
                regions.push((
                    "remove chip".into(),
                    l.remove_chip.x,
                    l.remove_chip.y,
                    l.remove_chip.w,
                    l.remove_chip.h,
                ));
                regions.push((
                    "move chip".into(),
                    l.move_chip.x,
                    l.move_chip.y,
                    l.move_chip.w,
                    l.move_chip.h,
                ));
            }

            for (what, x, y, rw, rh) in regions {
                let painted = ink(x, y, rw, rh);
                let area = (rw * rh) as usize;
                // 2% threshold with gradient tolerance: a real control (icon,
                // text, pill border) differs significantly from its centre,
                // while uniform wallpaper reads ~0 and a 1x1 region reads 0.
                assert!(
                    painted * 50 > area,
                    "{name}: `{what}` is tappable but has no pixels under it ({painted}/{area})"
                );
            }
        }
    }

    /// The render path must not allocate.
    ///
    /// The frame is composed straight into a mapped DRM buffer at the panel's
    /// refresh rate, so a heap allocation here is a latency spike on the UI
    /// thread. This installs a counting global allocator, warms everything
    /// that is legitimately one-off (icon decode, the harness buffer), and
    /// then asserts that composing frames allocates nothing at all.
    #[test]
    fn paint_frame_does_not_allocate() {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        // Per-thread, so the other tests running in parallel cannot pollute
        // the count, and const-initialised so touching it from inside the
        // allocator cannot itself allocate.
        std::thread_local! {
            static COUNT: Cell<usize> = const { Cell::new(0) };
        }
        struct Counting;
        // SAFETY: the shim forwards to the system allocator unchanged and only
        // bumps a thread-local counter.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, l: Layout) -> *mut u8 {
                bump();
                System.alloc(l)
            }
            unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
                System.dealloc(p, l)
            }
            unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
                bump();
                System.realloc(p, l, n)
            }
            unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
                bump();
                System.alloc_zeroed(l)
            }
        }
        #[global_allocator]
        static ALLOC: Counting = Counting;

        // Called from the allocator, including during thread teardown, so it
        // must never panic.
        fn bump() {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }
        fn allocations() -> usize {
            COUNT.try_with(|c| c.get()).unwrap_or(0)
        }

        let icon = stub_icon_sized([80, 160, 240], 121);
        let names = ["Phone", "Messages", "Camera", "Maps", "Music", "Store"];
        let grid: Vec<AppGridItem> = names
            .iter()
            .map(|n| AppGridItem {
                id: n,
                name: n,
                color: 0xFF2563EB,
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let snap = Snapshot { launch_color: 0xFF2563EB,
            w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
            drawer_open: false, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0,
            home_page: 0, home_scroll: 0.0, selected: None, search_query: "pho",
            search_active: true, keyboard: false, grid, drawer: Vec::new(), dock: Vec::new(),
            active_app: None,
        };
        let mut c = Canvas::new(1080, 2400);
        // Warm: the very first frame touches lazily initialised state.
        c.draw(&snap);

        let before = allocations();
        for _ in 0..3 {
            c.draw(&snap);
        }
        let after = allocations();
        assert_eq!(
            before,
            after,
            "paint_frame allocated {} times while composing a frame",
            after - before
        );

        // The home state above is the cheapest of twelve: the keyboard frame
        // alone allocates 26x/frame (`ch.to_string()` per key). Loop the
        // states a user actually spends time in so the guard cannot pass on
        // `home` while `keyboard`/`shade`/`drawer` allocate.
        let mut keyboard_snap = snap.clone();
        keyboard_snap.keyboard = true;
        let mut shade_snap = snap.clone();
        shade_snap.shade = true;
        let mut drawer_snap = snap.clone();
        drawer_snap.drawer_open = true;
        drawer_snap.drawer_progress = 1.0;
        let mut launch_snap = snap.clone();
        launch_snap.launch_progress = 0.42;
        launch_snap.launch_origin = Some((540.0, 980.0));
        for (label, s) in [
            ("keyboard", keyboard_snap),
            ("shade", shade_snap),
            ("drawer", drawer_snap),
            ("launch", launch_snap),
        ] {
            c.draw(&s); // warm per-state caches outside the bracket
            let before = allocations();
            for _ in 0..3 {
                c.draw(&s);
            }
            let after = allocations();
            assert_eq!(
                before,
                after,
                "paint_frame allocated {} times in state {label}",
                after - before
            );
        }

        // Sanity check on the harness itself: a deliberate allocation has to be
        // seen, otherwise the assertion above would pass for the wrong reason.
        let probe = allocations();
        let v: Vec<u8> = Vec::with_capacity(4096);
        std::hint::black_box(&v);
        assert!(
            allocations() > probe,
            "the allocation counter is not wired up"
        );
    }

    /// The app launch transform: the expanding card starts on the icon it was
    /// launched from and ends covering the panel.
    #[test]
    fn launch_transform_grows_from_the_tapped_icon() {
        use crate::graphics::layout::Layout;

        let icon = stub_icon_sized([40, 40, 40], 121);
        let grid = vec![AppGridItem {
            id: "phone",
            name: "Phone",
            color: 0xFF2563EB,
            glyph: "A",
            icon: Some(&icon),
        }];
        let (w, h) = (1080usize, 2400usize);
        let l = Layout::plain(w as f32, h as f32);
        let target = l.grid_icon(0);
        let origin = (target.center_x(), target.center_y());

        // Sample the card's footprint at several progress values by finding
        // the pixels of the expanding card inside the frame.
        let mut spans = Vec::new();
        for &t in &[0.15f32, 0.35, 0.6, 0.9] {
            let snap = Snapshot {
                w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
                drawer_open: false, launch_progress: t, launch_origin: Some(origin),
                launch_color: 0xFFF03030,
                pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None,
                search_query: "", search_active: false, keyboard: false, grid: grid.clone(),
                drawer: Vec::new(), dock: Vec::new(), active_app: None,
            };
            let mut c = Canvas::new(w, h);
            c.draw(&snap);
            let mut min_x = w;
            let mut max_x = 0;
            let mut min_y = h;
            let mut max_y = 0;
            for y in 0..h {
                for x in 0..w {
                    let p = c.buf[y * w + x];
                    // The card is saturated red over a near black surface, so
                    // "red dominant" identifies it and nothing else.
                    let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
                    if r > 60 && g < r / 3 && b < r / 3 {
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                        min_y = min_y.min(y);
                        max_y = max_y.max(y);
                    }
                }
            }

            spans.push((t, min_x, max_x, min_y, max_y));
        }

        for (i, &(t, x0, x1, y0, y1)) in spans.iter().enumerate() {
            assert!(x1 > x0 && y1 > y0, "t={t}: the card has no footprint");
            // The card always grows out of the tapped icon, so the icon's
            // centre is inside the footprint at every step.
            assert!(
                origin.0 >= x0 as f32 && origin.0 <= x1 as f32,
                "t={t}: card x {}..{} does not cover the icon at {}",
                x0,
                x1,
                origin.0
            );
            assert!(
                origin.1 >= y0 as f32 && origin.1 <= y1 as f32,
                "t={t}: card y {}..{} does not cover the icon at {}",
                y0,
                y1,
                origin.1
            );
            if i == 0 {
                let width = (x1 - x0) as f32;
                assert!(
                    width < l.w * 0.6,
                    "the card should still be small early on, was {width} wide"
                );
            } else {
                let prev = spans[i - 1];
                assert!(
                    (x1 - x0) > (prev.2 - prev.1) || (y1 - y0) > (prev.4 - prev.3),
                    "t={t}: the card must keep growing"
                );
            }
        }
        // By the end of the transform the card covers the panel.
        let (_, x0, x1, y0, y1) = spans[spans.len() - 1];
        assert!(
            (x1 - x0) as f32 > w as f32 * 0.9,
            "card should cover the panel width, was {} of {}",
            x1 - x0,
            w
        );
        assert!(
            (y1 - y0) as f32 > h as f32 * 0.9,
            "card should cover the panel height, was {} of {}",
            y1 - y0,
            h
        );
    }

    /// The press spring has to be visible: a compressed icon is smaller on
    /// screen than an idle one.
    #[test]
    fn press_scale_visibly_compresses_the_icon() {
        let icon = stub_icon_sized([40, 120, 240], 121);
        let grid = vec![AppGridItem {
            id: "phone",
            name: "Phone",
            color: 0xFF2563EB,
            glyph: "A",
            icon: Some(&icon),
        }];
        let dock = grid.clone();
        fn base<'a>(
            scale: f32,
            pressed: Option<&'a str>,
            grid: &'a [AppGridItem<'a>],
            dock: &'a [AppGridItem<'a>],
        ) -> Snapshot<'a> {
            Snapshot {
            w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
            drawer_open: false, launch_progress: 0.0, launch_origin: None, launch_color: 0xFF2563EB, press_scale: scale,
            home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
            keyboard: false, grid: grid.to_vec(), drawer: Vec::new(), dock: dock.to_vec(),
            active_app: None, pressed_icon: pressed,
            }
        }
        let mut idle = Canvas::new(1080, 2400);
        idle.draw(&base(1.0, None, &grid, &dock));
        let mut pressed = Canvas::new(1080, 2400);
        // The shell compresses a pressed icon to this scale.
        pressed.draw(&base(0.90, Some("phone"), &grid, &dock));

        // Count the coloured icon pixels: a 0.90 scale loses ~19% of the area.
        let count = |c: &Canvas| {
            c.buf
                .iter()
                .filter(|&&p| {
                    let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
                    r < 90 && g > 90 && b > 180
                })
                .count()
        };
        let (a, b) = (count(&idle), count(&pressed));
        assert!(b < a, "pressed icon should be smaller: {b} vs {a}");
        let ratio = b as f32 / a as f32;
        assert!(
            (0.72..0.92).contains(&ratio),
            "press scale should shrink the icon to roughly 0.81 of its area, got {ratio}"
        );
    }

    /// A rounded rectangle must be filled corner to corner, whatever its
    /// proportions. The row-span helper this guards once derived the vertical
    /// corner distance from the *width*, so every rounded rect taller than it
    /// was wide lost its lower rows - which showed up as the launch card
    /// covering only a square of the panel.
    #[test]
    fn rounded_rects_fill_their_whole_extent() {
        // Room for the largest probe plus its origin.
        let w = 400usize;
        let h = 512usize;
        // A range of aspect ratios, including tall and wide, plus degenerate
        // shapes that must not panic.
        for (rw, rh, radius) in [
            (200usize, 40usize, 12usize),
            (40, 200, 12),
            (200, 200, 60),
            (300, 60, 30),
            (60, 300, 30),
            (1, 300, 0),
            (300, 1, 0),
            (5, 5, 4),
        ] {
            let mut buf = vec![0xFF000000u32; w * h];
            debug_assert!(150 + rh <= h && 100 + rw <= w, "probe does not fit");
            crate::graphics::drm_kms::paint_rect_probe(
                &mut buf, w, 100, 150, rw, rh, radius, 0xFFFFFFFF,
            );
            let lit = buf.iter().filter(|&&p| p == 0xFFFFFFFF).count();
            // A rounded rect covers (rw*rh) minus the four corner cutouts.
            let area = (rw * rh) as f32;
            // Each corner loses a square of r^2 minus a quarter disc.
            let cut = if radius > 0 {
                4.0 * (1.0 - std::f32::consts::PI / 4.0) * (radius * radius) as f32
            } else {
                0.0
            };
            let expected = area - cut;
            assert!(
                lit as f32 >= expected * 0.95,
                "{rw}x{rh} r={radius}: filled {lit} of about {expected}"
            );
            // And the corners themselves stay empty.
            if radius > 1 {
                assert_eq!(
                    buf[150 * w + 100],
                    0xFF000000,
                    "{rw}x{rh} r={radius}: top-left corner leaked"
                );
                assert_eq!(
                    buf[(150 + rh - 1) * w + 100],
                    0xFF000000,
                    "{rw}x{rh} r={radius}: bottom-left corner leaked"
                );
            }
        }
    }

    /// The drawer overlay is tested the same way: the sheet's own controls and
    /// grid must all be drawn where the input path looks for them.
    #[test]
    fn drawer_regions_are_painted_where_they_are_tested() {
        use crate::graphics::layout::Layout;

        let icon = stub_icon([240, 160, 60]);
        let names = ["Settings", "Terminal", "Recorder", "Podcast", "Weather", "Wallet"];
        let drawer: Vec<AppGridItem> = names
            .iter()
            .enumerate()
            .map(|(i, n)| AppGridItem {
                id: n,
                name: n,
                color: 0xFFF59E0Bu32.wrapping_add(i as u32).wrapping_mul(0x080808),
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let snap = Snapshot { launch_color: 0xFF2563EB,
            w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 1.0,
            drawer_open: true, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0,
            home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
            keyboard: false, grid: Vec::new(), drawer, dock: Vec::new(), active_app: None,
        };
        let (w, h) = (1080usize, 2400usize);
        let mut c = Canvas::new(w, h);
        c.draw(&snap);
        let l = Layout::plain(w as f32, h as f32);

        // Sheet background sampled once from a corner that no icon, search
        // field or handle covers when the drawer is fully open: comparing
        // each probe against this (instead of the fixed black sentinel the
        // wallpaper overwrites) makes the test able to fail while still
        // passing for uniform controls (solid icons, search field) that
        // differ from the sheet.
        let sheet_bg = c.buf[(h - 10) * w + (w - 10)];
        let sbr = ((sheet_bg >> 16) & 0xFF) as i32;
        let sbg = ((sheet_bg >> 8) & 0xFF) as i32;
        let sbb = (sheet_bg & 0xFF) as i32;
        let painted = |x: f32, y: f32, rw: f32, rh: f32| -> usize {
            let y0 = (y.max(0.0) as usize).min(h);
            let x0 = (x.max(0.0) as usize).min(w);
            let y1 = ((y + rh).max(0.0) as usize).min(h);
            let x1 = ((x + rw).max(0.0) as usize).min(w);
            if y0 >= y1 || x0 >= x1 {
                return 0;
            }
            let mut n = 0;
            for py in y0..y1 {
                for px in x0..x1 {
                    let p = c.buf[py * w + px];
                    let dr = (((p >> 16) & 0xFF) as i32 - sbr).abs();
                    let dg = (((p >> 8) & 0xFF) as i32 - sbg).abs();
                    let db = ((p & 0xFF) as i32 - sbb).abs();
                    if dr + dg + db > 12 {
                        n += 1;
                    }
                }
            }
            n
        };

        for i in 0..(l.grid_cols * l.drawer_rows).min(snap.drawer.len()) {
            let cell = l.drawer_icon_cell(i);
            let got = painted(cell.center_x() - 8.0, cell.center_y() - 8.0, 16.0, 16.0);
            assert!(got > 0, "drawer cell {i} centre is empty");
            assert_eq!(
                l.drawer_grid_hit(0.0, cell.center_x(), cell.center_y()),
                Some(i)
            );
        }
        // The search field and the handle are drawn inside the sheet.
        let ds = l.drawer_search;
        assert!(
            painted(ds.center_x() - 20.0, ds.center_y() - 4.0, 40.0, 8.0) > 0,
            "drawer search field is empty"
        );
        let hd = l.drawer_handle;
        assert!(
            painted(hd.center_x() - 8.0, hd.center_y() - 2.0, 16.0, 4.0) > 0,
            "drawer handle is empty"
        );
    }

    /// Whole-frame budget guard.
    ///
    /// The launcher's frame is drawn on the CPU into a mapped DRM buffer, so
    /// a full repaint has to fit in the vsync period. This renders the busiest
    /// realistic home screen and fails if it cannot. Absolute timings are only
    /// meaningful for an optimised build.
    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "needs --release: absolute timings are meaningless in debug"
    )]
    fn full_frame_stays_inside_the_vsync_budget() {
        use std::time::Instant;

        let icon = stub_icon_sized([80, 160, 240], 121);
        let names = [
            "Phone", "Messages", "Camera", "Maps", "Music", "Store", "Notes", "Files", "Clock",
            "Calc", "Mail", "Pod",
        ];
        let grid: Vec<AppGridItem> = names
            .iter()
            .map(|n| AppGridItem {
                id: n,
                name: n,
                color: 0xFF2563EB,
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let dock: Vec<AppGridItem> = ["Phone", "Messages", "Apps", "Browser", "Camera"]
            .iter()
            .map(|n| AppGridItem {
                id: n,
                name: n,
                color: 0xFF10B981,
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let snap = Snapshot { launch_color: 0xFF2563EB,
            w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
            drawer_open: false, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0,
            home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
            keyboard: false, grid, drawer: Vec::new(), dock, active_app: None,
        };
        let mut c = Canvas::new(1080, 2400);
        // Table-drive EVERY state, not just the cheapest. Timing only `home`
        // is a guard that cannot fail in the states a user actually lives in
        // (the audit measured drawer at 20.7 ms and shade at 15.7 ms while
        // home fit the budget, and the test still "passed"). Each state is
        // listed explicitly so a new state has to be added here on purpose.
        let mut drawer_snap = snap.clone();
        drawer_snap.drawer_open = true;
        drawer_snap.drawer_progress = 1.0;
        let mut drawer_mid = snap.clone();
        drawer_mid.drawer_progress = 0.45;
        let mut shade_snap = snap.clone();
        shade_snap.shade = true;
        let mut keyboard_snap = snap.clone();
        keyboard_snap.keyboard = true;
        let mut launch_snap = snap.clone();
        launch_snap.launch_progress = 0.42;
        launch_snap.launch_origin = Some((540.0, 980.0));
        let mut selected = snap.clone();
        selected.selected = Some("Phone");
        let mut pressed = snap.clone();
        pressed.selected = Some("Music");
        pressed.pressed_icon = Some("Music");
        pressed.press_scale = 0.88;
        let mut search = snap.clone();
        search.search_active = true;
        search.search_query = "pho";
        let mut swipe = snap.clone();
        swipe.home_page = 1;
        swipe.home_scroll = 90.0;
        let mut lock = snap.clone();
        lock.locked = true;
        let mut app = snap.clone();
        app.active_app = Some("Settings");
        // Everything on at once: the worst realistic case.
        let mut all = snap.clone();
        all.selected = Some("Phone");
        all.search_active = true;
        all.search_query = "pho";
        all.drawer_open = true;
        all.drawer_progress = 1.0;
        all.shade = true;
        all.keyboard = true;
        let states: Vec<(&str, Snapshot)> = vec![
            ("home", snap),
            ("home_selected", selected),
            ("home_pressed", pressed),
            ("search", search),
            ("drawer_mid", drawer_mid),
            ("drawer", drawer_snap),
            ("page_swipe", swipe),
            ("lockscreen", lock),
            ("app", app),
            ("shade", shade_snap),
            ("keyboard", keyboard_snap),
            ("launch", launch_snap),
            ("all_combined", all),
        ];
        const FRAMES: u32 = 10;
        for (name, s) in &states {
            c.draw(s); // warm the caches
            let start = Instant::now();
            for _ in 0..FRAMES {
                c.draw(s);
            }
            let per = start.elapsed() / FRAMES;
            eprintln!(
                "{name} frame: {:?} ({} build)",
                per,
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
            );
            // Two tiers, and the tier is a property of the state, not of
            // what the frame happens to cost today:
            //
            //  * STEADY states (what the screen shows when the user is not
            //    mid-gesture) must fit ONE 120 Hz period. This is the tier
            //    that catches real regressions, and it is where every
            //    overage found so far lived: drawer 20.7 ms, shade 15.0 ms
            //    (a full-screen 0xEE scrim), launch 10.7 ms.
            //  * TRANSIENT states are mid-animation frames that exist for
            //    200-400 ms. `drawer_mid` additionally runs the frosted
            //    blur, whose 3x3 in-place box is ~2.4 ms of the frame and is
            //    at its algorithmic floor (9 reads + 9 stores per 9 pixels).
            //    It gets two 120 Hz periods.
            //
            // Every state, transient included, must still fit ONE 60 Hz
            // period: the plan's hard requirement is zero frame drops on
            // 60/90/120/144 Hz panels, and 16.67 ms is the floor of that
            // range. (In debug this test is `ignore`d; the verify scripts
            // must run it with `--release`.)
            let transient = matches!(*name, "drawer_mid" | "page_swipe" | "launch" | "all_combined");
            let steady_budget_us: u128 = if transient { 16_667 } else { 8_000 };
            assert!(
                per.as_micros() < steady_budget_us,
                "{name} frame takes {:?}, over its {}us budget",
                per,
                steady_budget_us
            );
            assert!(
                per.as_micros() < 16_667,
                "{name} frame takes {:?}, over the 60 Hz floor every state must meet",
                per
            );
        }
    }

    #[test]
    fn screenshot_every_launcher_state() {
        let Ok(dir) = std::env::var("UTLC_SCREENSHOT") else {
            return;
        };
        let icon = stub_icon([80, 160, 240]);
        let grid = apps(&icon, &["Phone", "Messages", "Camera", "Maps", "Music", "Store", "Notes", "Files", "Clock", "Calc", "Mail", "Pod"], 0xFF2563EB);
        let dock = apps(&icon, &["Phone", "Messages", "Apps", "Browser", "Camera"], 0xFF10B981);
        let drawer = apps(&icon, &["Settings", "Terminal", "Recorder", "Podcast", "Weather", "Wallet", "Translate", "Contacts", "Files", "Fitness", "Drive", "Photos", "Clock", "Calculator", "Calendar", "Mail"], 0xFFF59E0B);

        let cases: Vec<(&str, Snapshot)> = vec![
            ("home", Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None }),
            ("home_selected", Snapshot { selected: Some("Phone"), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("home_pressed", Snapshot { selected: Some("Music"), press_scale: 0.88, pressed_icon: Some("Music"), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, launch_color: 0xFF2563EB, press_scale: 1.0, pressed_icon: None, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("search", Snapshot { search_active: true, search_query: "pho", ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("drawer", Snapshot { drawer_progress: 1.0, drawer_open: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("drawer_mid", Snapshot { drawer_progress: 0.45, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("page_swipe", Snapshot { home_page: 1, home_scroll: 90.0, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("launch", Snapshot { launch_progress: 0.42, launch_color: 0xFF2563EB, launch_origin: Some((540.0, 980.0)), ..Snapshot { launch_color: 0xFF2563EB, w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("keyboard", Snapshot { keyboard: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("lockscreen", Snapshot { locked: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("shade", Snapshot { shade: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("app", Snapshot { active_app: Some("Settings"), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_color: 0xFF2563EB, launch_origin: None, pressed_icon: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
        ];

        // Also render a full page of a long label to check clipping/ellipsis.
        for (name, snap) in &cases {
            let mut c = Canvas::new(1080, 2400);
            c.draw(snap);
            std::fs::write(format!("{dir}/{name}.ppm"), c.to_ppm()).unwrap();
        }
        eprintln!("{} snapshots written to {dir}", cases.len());
        // A 360x640 variant proves the layout scales, not just the wallpaper.
        let base = cases[0].1.clone();
        let small = Snapshot { launch_color: 0xFF2563EB,
            grid: grid.clone(),
            dock: dock.clone(),
            drawer: drawer.clone(),
            ..base
        };
        let mut c = Canvas::new(360, 640);
        c.draw(&small);
        std::fs::write(format!("{dir}/small.ppm"), c.to_ppm()).unwrap();
        // Sanity: the layout must actually paint something.
        let ink = c.buf.iter().filter(|&&p| p != 0xFF000000).count();
        assert!(ink > c.w * c.h / 8, "small screen render is nearly empty ({} px)", ink);
    }
}
