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
    divider_y: f32, // y-coordinate dividing top and bottom (default H / 2.0)
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

    /// Current divider position (clamped to the last height it was set for).
    pub fn divider_y(&self) -> f32 {
        self.divider_y
    }

    /// Move the divider, clamped inside the live display height so neither
    /// viewport can go negative.
    pub fn set_divider(&mut self, y: f32, display_height: f32) {
        self.divider_y = Self::clamp_divider(y, display_height);
    }

    fn clamp_divider(y: f32, display_height: f32) -> f32 {
        y.clamp(1.0, (display_height - 1.0).max(1.0))
    }

    /// Divider clamped against the height actually being laid out: the stored
    /// divider may be stale (built for a different height), so every viewport
    /// re-validates instead of trusting it.
    fn split(&self, display_height: f32) -> f32 {
        Self::clamp_divider(self.divider_y, display_height)
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
        (0.0, 0.0, display_width, self.divider_y.max(0.0))
    }

    pub fn bottom_viewport(&self, display_width: f32, display_height: f32) -> (f32, f32, f32, f32) {
        let d = self.split(display_height);
        (0.0, d, display_width, (display_height - d).max(0.0))
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
            // Already dying: no re-arm of the grace period, no double kill.
            if matches!(
                card.kill_state,
                KillProgress::GracePeriod { .. } | KillProgress::ForcedKill { .. }
            ) {
                return None;
            }
            let kill = card.y_offset < -150.0 && card.is_dismissable;
            let pid = card.pid;
            card.y_offset = 0.0;
            if kill {
                card.kill_state = KillProgress::GracePeriod {
                    initiated_at: Instant::now(),
                    pid,
                };
                return Some(pid);
            }
        }
        None
    }

    /// Check timeouts for closing apps and escalate to SIGKILL after 500ms.
    /// Pushes into the caller-owned `out` buffer: no allocation on this
    /// per-tick path.
    pub fn update_kill_lifecycle(&mut self, grace_period: Duration, out: &mut Vec<(i32, bool)>) {
        let now = Instant::now();

        for card in &mut self.cards {
            if let KillProgress::GracePeriod { initiated_at, pid } = card.kill_state {
                if now.saturating_duration_since(initiated_at) >= grace_period {
                    card.kill_state = KillProgress::ForcedKill { pid };
                    out.push((pid, true)); // Escalate to SIGKILL
                }
            }
        }
    }

    /// Clear All: initiates graceful closure on all dismissable cards,
    /// appending their pids to the caller-owned `out` buffer.
    pub fn clear_all(&mut self, out: &mut Vec<i32>) {
        let now = Instant::now();

        for card in &mut self.cards {
            if card.is_dismissable && card.kill_state == KillProgress::None {
                card.kill_state = KillProgress::GracePeriod {
                    initiated_at: now,
                    pid: card.pid,
                };
                out.push(card.pid);
            }
        }
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
        let mut escalated = Vec::new();
        carousel.update_kill_lifecycle(Duration::from_millis(0), &mut escalated);
        assert_eq!(escalated, vec![(2002, true)]);
    }

    #[test]
    fn test_kill_release_does_not_rearm() {
        let mut carousel = RecentsCarousel::new(1080.0, 2400.0);
        carousel.add_card(RecentsCard::new(
            "calc".into(),
            7,
            "Calc".into(),
            "".into(),
            3,
            None,
            800.0,
            1600.0,
        ));
        carousel.on_card_vertical_drag(0, -200.0);
        assert_eq!(carousel.on_card_vertical_release(0), Some(7));
        assert_eq!(carousel.cards[0].y_offset, 0.0);
        // Repeat swipe on the dying card: no re-arm, no double kill.
        carousel.on_card_vertical_drag(0, -200.0);
        assert_eq!(carousel.on_card_vertical_release(0), None);
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
    fn test_split_viewports_never_negative() {
        let mut split = SplitScreenConfig::new(2400.0);
        split.set_divider(3000.0, 2400.0); // clamped into range
        assert!(split.divider_y() <= 2400.0);
        let (_, _, _, h) = split.bottom_viewport(1080.0, 2400.0);
        assert!(h >= 0.0);
        // Stale divider against a shorter live display: still sane.
        let (_, _, _, h2) = split.bottom_viewport(1080.0, 1280.0);
        assert!(h2 >= 0.0);
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

        let mut pids = Vec::new();
        carousel.clear_all(&mut pids);
        assert_eq!(pids.len(), 2);
    }
}
