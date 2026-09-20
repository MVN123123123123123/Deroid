//! Android-style Home Screen Workspace Grid, Hotseat Dock, and App Drawer.
//! Implements paged grid layout, spring-physics horizontal scrolling,
//! persistent bottom dock, and smooth drawer transitions.


/// Spring physics parameters for smooth workspace grid and recents scrolling
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringConfig {
    pub stiffness: f32, // Tension k (default 180.0)
    pub damping: f32,   // Friction c (default 18.0)
    pub mass: f32,      // Mass m (default 1.0)
}

impl Default for SpringConfig {
    fn default() -> Self {
        Self {
            stiffness: 180.0,
            damping: 18.0,
            mass: 1.0,
        }
    }
}

/// Spring-physics 1D harmonic oscillator
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringOscillator {
    pub current: f32,
    pub target: f32,
    pub velocity: f32,
    pub config: SpringConfig,
}

impl SpringOscillator {
    pub fn new(initial: f32, config: SpringConfig) -> Self {
        Self {
            current: initial,
            target: initial,
            velocity: 0.0,
            config,
        }
    }

    /// Step simulation by dt (seconds)
    pub fn step(&mut self, dt: f32) {
        if (self.current - self.target).abs() < 0.05 && self.velocity.abs() < 0.05 {
            self.current = self.target;
            self.velocity = 0.0;
            return;
        }

        // F = -k * (x - target) - c * v
        let displacement = self.current - self.target;
        let spring_force = -self.config.stiffness * displacement;
        let damping_force = -self.config.damping * self.velocity;
        let total_force = spring_force + damping_force;

        let acceleration = total_force / self.config.mass;
        self.velocity += acceleration * dt;
        self.current += self.velocity * dt;
    }

    pub fn is_settled(&self) -> bool {
        (self.current - self.target).abs() < 0.05 && self.velocity.abs() < 0.05
    }
}

/// A cell on the workspace grid
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridItem {
    pub page: usize,
    pub col: usize,
    pub row: usize,
    pub app_id: String,
}

/// Paged Home Workspace Grid
pub struct WorkspaceGrid {
    pub cols: usize,        // e.g. 4
    pub rows: usize,        // e.g. 5
    pub num_pages: usize,   // e.g. 3
    pub page_width: f32,    // Display width in pixels
    pub current_page: usize,
    pub scroll_spring: SpringOscillator,
    pub items: Vec<GridItem>,
}

impl WorkspaceGrid {
    pub fn new(cols: usize, rows: usize, num_pages: usize, page_width: f32) -> Self {
        Self {
            cols,
            rows,
            num_pages,
            page_width,
            current_page: 0,
            scroll_spring: SpringOscillator::new(0.0, SpringConfig::default()),
            items: Vec::new(),
        }
    }

    pub fn add_item(&mut self, page: usize, col: usize, row: usize, app_id: String) -> bool {
        if page >= self.num_pages || col >= self.cols || row >= self.rows {
            return false;
        }
        // Check slot collision
        if self.items.iter().any(|i| i.page == page && i.col == col && i.row == row) {
            return false;
        }
        self.items.push(GridItem {
            page,
            col,
            row,
            app_id,
        });
        true
    }

    /// Snap to a specific page
    pub fn set_page(&mut self, page: usize) {
        if page < self.num_pages {
            self.current_page = page;
            self.scroll_spring.target = -(page as f32) * self.page_width;
        }
    }

    /// Handle drag scroll delta (dragging with touch)
    pub fn on_drag(&mut self, delta_x: f32) {
        // Apply rubber-band resistance when overscrolling boundaries
        let min_offset = -((self.num_pages - 1) as f32) * self.page_width;
        let max_offset = 0.0;

        let cur = self.scroll_spring.current;
        let resistance = if cur > max_offset {
            1.0 / (1.0 + (cur - max_offset) / 100.0)
        } else if cur < min_offset {
            1.0 / (1.0 + (min_offset - cur) / 100.0)
        } else {
            1.0
        };

        self.scroll_spring.current += delta_x * resistance;
        self.scroll_spring.target = self.scroll_spring.current;
        self.scroll_spring.velocity = delta_x * 30.0; // estimate velocity
    }

    /// On drag release: snap to nearest page or flick to next/prev page
    pub fn on_release(&mut self, velocity_x: f32) {
        let current_offset = self.scroll_spring.current;
        let approx_page = (-current_offset / self.page_width).round() as i32;

        let target_page = if velocity_x < -300.0 {
            // Flick left (next page)
            (self.current_page as i32 + 1).min(self.num_pages as i32 - 1)
        } else if velocity_x > 300.0 {
            // Flick right (previous page)
            (self.current_page as i32 - 1).max(0)
        } else {
            // Snap to nearest page
            approx_page.clamp(0, self.num_pages as i32 - 1)
        };

        self.set_page(target_page as usize);
        self.scroll_spring.velocity = velocity_x;
    }

    pub fn update(&mut self, dt: f32) {
        self.scroll_spring.step(dt);
        let progress = -self.scroll_spring.current / self.page_width;
        self.current_page = progress.round().clamp(0.0, (self.num_pages - 1) as f32) as usize;
    }

    /// Calculate bounding box for an item on the grid
    pub fn item_rect(
        &self,
        item: &GridItem,
        top_offset: f32,
        grid_height: f32,
    ) -> (f32, f32, f32, f32) {
        let cell_w = self.page_width / self.cols as f32;
        let cell_h = grid_height / self.rows as f32;

        let page_base_x = (item.page as f32) * self.page_width + self.scroll_spring.current;
        let x = page_base_x + (item.col as f32) * cell_w;
        let y = top_offset + (item.row as f32) * cell_h;

        (x, y, cell_w, cell_h)
    }
}

/// Persistent bottom Hotseat Dock
pub struct HotseatDock {
    pub slots: [Option<String>; 5], // Pinned App IDs (Phone, SMS, Browser, Camera, Terminal)
    pub height: f32,
    pub y_position: f32,
}

impl HotseatDock {
    pub fn default_mobile(display_height: f32) -> Self {
        let height = 96.0;
        let y_position = display_height - height - 48.0; // 48px above bottom edge gesture bar
        Self {
            slots: [
                Some("org.mobian.dialer".into()),
                Some("chatty".into()),
                Some("firefox".into()),
                Some("org.gnome.Snapshot".into()),
                Some("alacritty".into()),
            ],
            height,
            y_position,
        }
    }

    pub fn set_slot(&mut self, index: usize, app_id: Option<String>) {
        if index < 5 {
            self.slots[index] = app_id;
        }
    }

    pub fn slot_rect(&self, index: usize, display_width: f32) -> (f32, f32, f32, f32) {
        let slot_w = display_width / 5.0;
        let x = (index as f32) * slot_w;
        (x, self.y_position, slot_w, self.height)
    }
}

/// App Drawer State Machine
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrawerState {
    Closed,
    Dragging { progress: f32 }, // 0.0 = Closed, 1.0 = Fully open
    Open,
}

pub struct AppDrawer {
    pub state: DrawerState,
    pub search_query: String,
    pub spring: SpringOscillator,
}

impl Default for AppDrawer {
    fn default() -> Self {
        Self::new()
    }
}

impl AppDrawer {
    pub fn new() -> Self {
        Self {
            state: DrawerState::Closed,
            search_query: String::new(),
            spring: SpringOscillator::new(0.0, SpringConfig {
                stiffness: 220.0,
                damping: 20.0,
                mass: 1.0,
            }),
        }
    }

    pub fn open(&mut self) {
        self.state = DrawerState::Open;
        self.spring.target = 1.0;
    }

    pub fn close(&mut self) {
        self.state = DrawerState::Closed;
        self.spring.target = 0.0;
        self.search_query.clear();
    }

    pub fn set_drag_progress(&mut self, progress: f32) {
        let clamped = progress.clamp(0.0, 1.0);
        self.state = DrawerState::Dragging { progress: clamped };
        self.spring.current = clamped;
        self.spring.target = clamped;
    }

    pub fn on_release(&mut self, velocity_y: f32) {
        if velocity_y < -300.0 || self.spring.current > 0.4 {
            self.open();
        } else {
            self.close();
        }
        self.spring.velocity = velocity_y / 1000.0;
    }

    pub fn update(&mut self, dt: f32) {
        self.spring.step(dt);
        if self.spring.is_settled() {
            if self.spring.target == 1.0 {
                self.state = DrawerState::Open;
            } else if self.spring.target == 0.0 {
                self.state = DrawerState::Closed;
            }
        }
    }

    pub fn progress(&self) -> f32 {
        self.spring.current.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spring_oscillator_convergence() {
        let mut spring = SpringOscillator::new(0.0, SpringConfig::default());
        spring.target = 100.0;

        for _ in 0..120 {
            spring.step(0.016); // 60Hz step (16.6ms)
        }

        assert!(spring.is_settled());
        assert!((spring.current - 100.0).abs() < 0.1);
    }

    #[test]
    fn test_workspace_grid_paging() {
        let mut grid = WorkspaceGrid::new(4, 5, 3, 1080.0);
        assert_eq!(grid.current_page, 0);

        grid.add_item(0, 0, 0, "phone".into());
        grid.add_item(1, 1, 1, "browser".into());

        grid.set_page(1);
        for _ in 0..60 {
            grid.update(0.016);
        }

        assert_eq!(grid.current_page, 1);
        assert!((grid.scroll_spring.current - (-1080.0)).abs() < 1.0);
    }

    #[test]
    fn test_hotseat_dock_layout() {
        let dock = HotseatDock::default_mobile(2400.0);
        assert_eq!(dock.slots.len(), 5);
        let rect0 = dock.slot_rect(0, 1080.0);
        assert_eq!(rect0.0, 0.0);
        assert_eq!(rect0.2, 1080.0 / 5.0);
    }

    #[test]
    fn test_app_drawer_open_close() {
        let mut drawer = AppDrawer::new();
        assert_eq!(drawer.state, DrawerState::Closed);

        drawer.open();
        for _ in 0..60 {
            drawer.update(0.016);
        }
        assert_eq!(drawer.state, DrawerState::Open);
        assert_eq!(drawer.progress(), 1.0);

        drawer.close();
        for _ in 0..60 {
            drawer.update(0.016);
        }
        assert_eq!(drawer.state, DrawerState::Closed);
        assert_eq!(drawer.progress(), 0.0);
    }
}
