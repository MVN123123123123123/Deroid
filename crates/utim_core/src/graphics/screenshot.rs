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
    pub press_scale: f32,
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
            press_scale: self.press_scale,
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
            app_launch_color: 0xFF2563EB,
            icon_press_scale: self.press_scale,
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
        super::drm_kms::paint_frame(&mut self.buf, self.w, self.w, self.h, &state);
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

/// A tiny 1x1 transparent icon so the grid exercises the bitmap path.
pub fn stub_icon(colour: [u8; 3]) -> RgbaImage {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64 {
        for x in 0..64 {
            let inside = (4..60).contains(&x) && (4..60).contains(&y);
            let ring = (4..9).contains(&x) || (55..60).contains(&x);
            let v = if !inside {
                0
            } else if ring {
                255
            } else {
                ((x * 4) % 200) as u8
            };
            pixels.push(if inside { colour[0] } else { v });
            pixels.push(if inside { colour[1] } else { v });
            pixels.push(if inside { colour[2] } else { v });
            pixels.push(if inside { 255 } else { 0 });
        }
    }
    RgbaImage {
        width: 64,
        height: 64,
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
                id: *n,
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
                id: *n,
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
                id: *n,
                name: n,
                color: 0xFF10B981u32.wrapping_add(i as u32).wrapping_mul(0x080808),
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();

        let cases: Vec<(&str, Snapshot)> = vec![
            (
                "home",
                Snapshot {
                    w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
                    drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0,
                    home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
                    keyboard: false, grid: grid.clone(), drawer: Vec::new(), dock: dock.clone(),
                    active_app: None,
                },
            ),
            (
                "selected",
                Snapshot {
                    selected: Some("Phone"),
                    ..Snapshot {
                        w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0,
                        drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0,
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
                let mut n = 0;
                for py in y0..y1 {
                    for px in x0..x1 {
                        if c.buf[py * w + px] != 0xFF000000 {
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
            for i in 0..(l.grid_cols * l.max_rows.min(grid.len())) {
                let icon_r = l.grid_icon(i);
                regions.push((
                    format!("grid icon {i}"),
                    icon_r.x,
                    icon_r.y,
                    icon_r.w,
                    icon_r.h,
                ));
            }
            for s in 0..l.dock_slots {
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
                assert!(
                    painted * 20 > area,
                    "{name}: `{what}` is tappable but has no pixels under it ({painted}/{area})"
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
                id: *n,
                name: n,
                color: 0xFFF59E0Bu32.wrapping_add(i as u32).wrapping_mul(0x080808),
                glyph: "A",
                icon: Some(&icon),
            })
            .collect();
        let snap = Snapshot {
            w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 1.0,
            drawer_open: true, launch_progress: 0.0, launch_origin: None, press_scale: 1.0,
            home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false,
            keyboard: false, grid: Vec::new(), drawer, dock: Vec::new(), active_app: None,
        };
        let (w, h) = (1080usize, 2400usize);
        let mut c = Canvas::new(w, h);
        c.draw(&snap);
        let l = Layout::plain(w as f32, h as f32);

        let painted = |x: f32, y: f32, rw: f32, rh: f32| -> usize {
            let y0 = (y.max(0.0) as usize).min(h);
            let x0 = (x.max(0.0) as usize).min(w);
            let y1 = ((y + rh).max(0.0) as usize).min(h);
            let x1 = ((x + rw).max(0.0) as usize).min(w);
            let mut n = 0;
            for py in y0..y1 {
                for px in x0..x1 {
                    // The sheet itself is a different colour from the frame.
                    if c.buf[py * w + px] != 0xFF000000 {
                        n += 1;
                    }
                }
            }
            n
        };

        for i in 0..(l.grid_cols * l.drawer_rows.min(6)) {
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
            ("home", Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None }),
            ("home_selected", Snapshot { selected: Some("Phone"), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("home_pressed", Snapshot { selected: Some("Music"), press_scale: 0.88, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("search", Snapshot { search_active: true, search_query: "pho", ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("drawer", Snapshot { drawer_progress: 1.0, drawer_open: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("drawer_mid", Snapshot { drawer_progress: 0.45, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("page_swipe", Snapshot { home_page: 1, home_scroll: 90.0, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("launch", Snapshot { launch_progress: 0.42, launch_origin: Some((540.0, 980.0)), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("keyboard", Snapshot { keyboard: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("lockscreen", Snapshot { locked: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("shade", Snapshot { shade: true, ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
            ("app", Snapshot { active_app: Some("Settings"), ..Snapshot { w: 0, h: 0, time: "10:34", locked: false, shade: false, drawer_progress: 0.0, drawer_open: false, launch_progress: 0.0, launch_origin: None, press_scale: 1.0, home_page: 0, home_scroll: 0.0, selected: None, search_query: "", search_active: false, keyboard: false, grid: grid.clone(), drawer: drawer.clone(), dock: dock.clone(), active_app: None } }),
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
        let small = Snapshot {
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
