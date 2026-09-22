//! Recents / Overview Multitasking Screen and Process Lifecycle Management.
//! Implements horizontal card stack carousel, swipe-to-kill with graceful xdg_toplevel.close
//! escalating to SIGKILL and UTIM cgroup cleanup, Clear All button, and Split-Screen multitasking.

use std::time::{Duration, Instant};

use crate::compositor::launcher::{SpringConfig, SpringOscillator};

/// State of an application being closed/killed
#[derive(Debug, Clone, PartialEq)]
pub enum KillProgress {
    None,
    GracePeriod { initiated_at: Instant, pid: i32 },
    ForcedKill { pid: i32 },
    Terminated,
}

/// An application card in the Recents carousel
#[derive(Debug, Clone, PartialEq)]
pub struct RecentsCard {
    pub app_id: String,
    pub pid: i32,
    pub title: String,
    pub icon: String,
    pub surface_id: u32,
    pub dmabuf_fd: Option<i32>,
    pub width: f32,
    pub height: f32,
    pub y_offset: f32, // Drag offset during vertical swipe-to-kill
    pub kill_state: KillProgress,
    pub is_dismissable: bool,
}

impl RecentsCard {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app_id: String,
        pid: i32,
        title: String,
        icon: String,
        surface_id: u32,
        dmabuf_fd: Option<i32>,
        width: f32,
        height: f32,
    ) -> Self {
        Self {
            app_id,
            pid,
            title,
            icon,
            surface_id,
            dmabuf_fd,
            width,
            height,
            y_offset: 0.0,
            kill_state: KillProgress::None,
            is_dismissable: true,
        }
    }
}

/// Split-Screen Viewport Configuration (Top/Bottom 50/50)
#[derive(Debug, Clone, PartialEq)]
pub struct SplitScreenConfig {
    pub is_active: bool,
    pub top_app_pid: Option<i32>,
    pub bottom_app_pid: Option<i32>,
    pub divider_y: f32, // y-coordinate dividing top and bottom (default H / 2.0)
}

impl SplitScreenConfig {
    pub fn new(display_height: f32) -> Self {
        Self {
            is_active: false,
            top_app_pid: None,
            bottom_app_pid: None,
            divider_y: display_height / 2.0,
        }
    }

    pub fn enable(&mut self, top_pid: i32, bottom_pid: i32) {
        self.is_active = true;
        self.top_app_pid = Some(top_pid);
        self.bottom_app_pid = Some(bottom_pid);
    }

    pub fn disable(&mut self) {
        self.is_active = false;
        self.top_app_pid = None;
        self.bottom_app_pid = None;
    }

    pub fn top_viewport(&self, display_width: f32) -> (f32, f32, f32, f32) {
        (0.0, 0.0, display_width, self.divider_y)
    }

    pub fn bottom_viewport(&self, display_width: f32, display_height: f32) -> (f32, f32, f32, f32) {
        (
            0.0,
            self.divider_y,
            display_width,
            display_height - self.divider_y,
        )
    }
}

/// Recents / Overview screen carousel
pub struct RecentsCarousel {
    pub display_width: f32,
    pub display_height: f32,
    pub cards: Vec<RecentsCard>,
    pub selected_index: usize,
    pub scroll_spring: SpringOscillator,
    pub split_screen: SplitScreenConfig,
    pub card_width: f32,
    pub card_height: f32,
    pub card_spacing: f32,
}

impl RecentsCarousel {
    pub fn new(display_width: f32, display_height: f32) -> Self {
        let card_width = display_width * 0.75;
        let card_height = display_height * 0.70;
        let card_spacing = display_width * 0.10;

        Self {
            display_width,
            display_height,
            cards: Vec::new(),
            selected_index: 0,
            scroll_spring: SpringOscillator::new(
                0.0,
                SpringConfig {
                    stiffness: 200.0,
                    damping: 22.0,
                    mass: 1.0,
                },
            ),
            split_screen: SplitScreenConfig::new(display_height),
            card_width,
            card_height,
            card_spacing,
        }
    }

    pub fn add_card(&mut self, card: RecentsCard) {
        // If card exists, update it and bring to front (index 0)
        if let Some(pos) = self.cards.iter().position(|c| c.pid == card.pid) {
            self.cards.remove(pos);
        }
        self.cards.insert(0, card);
        self.snap_to_index(0);
    }

    pub fn remove_card_by_pid(&mut self, pid: i32) -> Option<RecentsCard> {
        if let Some(pos) = self.cards.iter().position(|c| c.pid == pid) {
            let card = self.cards.remove(pos);
            if self.selected_index >= self.cards.len() && !self.cards.is_empty() {
                self.selected_index = self.cards.len() - 1;
            }
            self.snap_to_index(self.selected_index);
            Some(card)
        } else {
            None
        }
    }

    pub fn snap_to_index(&mut self, index: usize) {
        if self.cards.is_empty() {
            self.selected_index = 0;
            self.scroll_spring.target = 0.0;
            return;
        }

        let idx = index.min(self.cards.len() - 1);
        self.selected_index = idx;
        let step = self.card_width + self.card_spacing;
        self.scroll_spring.target = -(idx as f32) * step;
    }

    /// Horizontal drag scroll
    pub fn on_horizontal_drag(&mut self, delta_x: f32) {
        self.scroll_spring.current += delta_x;
        self.scroll_spring.target = self.scroll_spring.current;
        self.scroll_spring.velocity = delta_x * 20.0;
    }

    /// Horizontal release with velocity
    pub fn on_horizontal_release(&mut self, velocity_x: f32) {
        let step = self.card_width + self.card_spacing;
        let approx_idx = (-self.scroll_spring.current / step).round() as i32;

        let target_idx = if velocity_x < -200.0 {
            (self.selected_index as i32 + 1).min(self.cards.len() as i32 - 1)
        } else if velocity_x > 200.0 {
            (self.selected_index as i32 - 1).max(0)
        } else {
            approx_idx.clamp(0, self.cards.len().saturating_sub(1) as i32)
        };

        self.snap_to_index(target_idx as usize);
        self.scroll_spring.velocity = velocity_x;
    }

    /// Vertical swipe-to-kill drag on card
    pub fn on_card_vertical_drag(&mut self, card_index: usize, delta_y: f32) {
        if let Some(card) = self.cards.get_mut(card_index) {
            // Negative delta_y is upward swipe
            card.y_offset += delta_y;
        }
    }

    /// Vertical swipe-to-kill release: if dragged upward > 150px, initiate kill
    pub fn on_card_vertical_release(&mut self, card_index: usize) -> Option<i32> {
        if let Some(card) = self.cards.get_mut(card_index) {
            if card.y_offset < -150.0 && card.is_dismissable {
                let pid = card.pid;
                card.kill_state = KillProgress::GracePeriod {
                    initiated_at: Instant::now(),
                    pid,
                };
                return Some(pid);
            } else {
                card.y_offset = 0.0; // Reset
            }
        }
        None
    }

    /// Check timeouts for closing apps and escalate to SIGKILL after 500ms
    pub fn update_kill_lifecycle(&mut self, grace_period: Duration) -> Vec<(i32, bool)> {
        // Returns list of (pid, is_force_kill)
        let mut actions = Vec::new();
        let now = Instant::now();

        for card in &mut self.cards {
            if let KillProgress::GracePeriod { initiated_at, pid } = card.kill_state {
                if now.duration_since(initiated_at) >= grace_period {
                    card.kill_state = KillProgress::ForcedKill { pid };
                    actions.push((pid, true)); // Escalate to SIGKILL
                }
            }
        }

        actions
    }

    /// Clear All: initiates graceful closure on all dismissable cards
    pub fn clear_all(&mut self) -> Vec<i32> {
        let mut pids = Vec::new();
        let now = Instant::now();

        for card in &mut self.cards {
            if card.is_dismissable && card.kill_state == KillProgress::None {
                card.kill_state = KillProgress::GracePeriod {
                    initiated_at: now,
                    pid: card.pid,
                };
                pids.push(card.pid);
            }
        }

        pids
    }

    pub fn update(&mut self, dt: f32) {
        self.scroll_spring.step(dt);
    }

    /// Get screen rect for a card in the carousel
    pub fn card_rect(&self, index: usize) -> Option<(f32, f32, f32, f32)> {
        let card = self.cards.get(index)?;
        let step = self.card_width + self.card_spacing;
        let base_x = (self.display_width - self.card_width) / 2.0;
        let base_y = (self.display_height - self.card_height) / 2.0;

        let card_x = base_x + (index as f32) * step + self.scroll_spring.current;
        let card_y = base_y + card.y_offset;

        Some((card_x, card_y, self.card_width, self.card_height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recents_carousel_add_and_snap() {
        let mut carousel = RecentsCarousel::new(1080.0, 2400.0);
        let card1 = RecentsCard::new(
            "term".into(),
            1001,
            "Term".into(),
            "".into(),
            1,
            None,
            800.0,
            1600.0,
        );
        let card2 = RecentsCard::new(
            "web".into(),
            1002,
            "Web".into(),
            "".into(),
            2,
            None,
            800.0,
            1600.0,
        );

        carousel.add_card(card1);
        carousel.add_card(card2);

        assert_eq!(carousel.cards.len(), 2);
        assert_eq!(carousel.cards[0].pid, 1002); // most recent is at index 0

        carousel.snap_to_index(1);
        for _ in 0..60 {
            carousel.update(0.016);
        }
        assert_eq!(carousel.selected_index, 1);
        assert!(carousel.scroll_spring.current < -500.0);
    }

    #[test]
    fn test_swipe_to_kill_lifecycle() {
        let mut carousel = RecentsCarousel::new(1080.0, 2400.0);
        let card = RecentsCard::new(
            "calc".into(),
            2002,
            "Calc".into(),
            "".into(),
            3,
            None,
            800.0,
            1600.0,
        );
        carousel.add_card(card);

        // Simulate swipe up by -200px
        carousel.on_card_vertical_drag(0, -200.0);
        let kill_pid = carousel.on_card_vertical_release(0);
        assert_eq!(kill_pid, Some(2002));

        match &carousel.cards[0].kill_state {
            KillProgress::GracePeriod { pid, .. } => assert_eq!(*pid, 2002),
            _ => panic!("Expected GracePeriod state"),
        }

        // Simulate grace period expiry (e.g. 500ms)
        let escalated = carousel.update_kill_lifecycle(Duration::from_millis(0));
        assert_eq!(escalated, vec![(2002, true)]);
    }

    #[test]
    fn test_split_screen_viewports() {
        let mut split = SplitScreenConfig::new(2400.0);
        split.enable(1001, 1002);
        assert!(split.is_active);

        let top = split.top_viewport(1080.0);
        assert_eq!(top, (0.0, 0.0, 1080.0, 1200.0));

        let bottom = split.bottom_viewport(1080.0, 2400.0);
        assert_eq!(bottom, (0.0, 1200.0, 1080.0, 1200.0));
    }

    #[test]
    fn test_clear_all_action() {
        let mut carousel = RecentsCarousel::new(1080.0, 2400.0);
        carousel.add_card(RecentsCard::new(
            "c1".into(),
            1,
            "C1".into(),
            "".into(),
            1,
            None,
            800.0,
            1600.0,
        ));
        carousel.add_card(RecentsCard::new(
            "c2".into(),
            2,
            "C2".into(),
            "".into(),
            2,
            None,
            800.0,
            1600.0,
        ));

        let pids = carousel.clear_all();
        assert_eq!(pids.len(), 2);
    }
}
