//! QuickStep Gesture Navigation Engine (Android 10+ Style).
//! Processes raw touch events with sub-8ms latency response, executing
//! Home (swipe-up with scale-down animation), Recents (swipe & hold with haptic trigger),
//! Back (left/right edge swipe), Bottom Bar Scrubbing (rapid app switch),
//! and Notification Shade pull-down.

use std::time::{Duration, Instant};

/// Standard Android animation curves
pub fn cubic_bezier_ease_out(t: f32) -> f32 {
    // Android FastOutSlowIn / EaseOut approximation: (0.2, 0.0, 0.0, 1.0)
    let t = t.clamp(0.0, 1.0);
    // Cubic polynomial ease-out: 1 - (1 - t)^3
    1.0 - (1.0 - t).powi(3)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSide {
    Bottom,
    Left,
    Right,
    Top,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawTouchEvent {
    pub touch_id: i32,
    pub phase: TouchPhase,
    pub x: f32,
    pub y: f32,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GestureAction {
    None,
    /// Return to Home with scale-down animation progress (0.0 to 1.0)
    Home {
        progress: f32,
        scale: f32,
        window_alpha: f32,
    },
    /// Recents / Overview carousel entry with haptic pulse trigger
    Recents {
        progress: f32,
        trigger_haptic: bool,
    },
    /// Back action injection (KEY_BACK / KEY_ESC) with chevron progress
    Back {
        side: EdgeSide,
        progress: f32,
        injected: bool,
    },
    /// Bottom bar scrub for rapid app switching (-1 for prev, +1 for next)
    BottomBarScrub {
        delta_x: f32,
        app_shift: i32,
    },
    /// Notification shade pull-down progress (0.0 to 1.0)
    NotificationShade {
        progress: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum GestureState {
    Idle,
    TrackingBottom {
        start_x: f32,
        start_y: f32,
        start_time: Instant,
        current_x: f32,
        current_y: f32,
        held_recents: bool,
    },
    TrackingEdge {
        side: EdgeSide,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
    },
    TrackingTop {
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
    },
    TrackingCenter,
}

/// Gesture Engine Configuration
#[derive(Debug, Clone, PartialEq)]
pub struct GestureConfig {
    pub bottom_nav_height: f32,      // default 48.0 px
    pub edge_zone_width: f32,        // default 48.0 px
    pub top_bar_height: f32,         // default 48.0 px
    pub home_threshold_y: f32,       // default 60.0 px
    pub recents_hold_time: Duration, // default 180 ms
    pub back_threshold_x: f32,       // default 40.0 px
    pub scrub_threshold_x: f32,      // default 80.0 px
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            bottom_nav_height: 48.0,
            edge_zone_width: 48.0,
            top_bar_height: 48.0,
            home_threshold_y: 60.0,
            recents_hold_time: Duration::from_millis(180),
            back_threshold_x: 40.0,
            scrub_threshold_x: 80.0,
        }
    }
}

pub struct GestureEngine {
    pub display_width: f32,
    pub display_height: f32,
    pub config: GestureConfig,
    state: GestureState,
}

impl GestureEngine {
    pub fn new(display_width: f32, display_height: f32, config: GestureConfig) -> Self {
        Self {
            display_width,
            display_height,
            config,
            state: GestureState::Idle,
        }
    }

    pub fn classify_edge(&self, x: f32, y: f32) -> EdgeSide {
        if y >= self.display_height - self.config.bottom_nav_height {
            EdgeSide::Bottom
        } else if y <= self.config.top_bar_height {
            EdgeSide::Top
        } else if x <= self.config.edge_zone_width {
            EdgeSide::Left
        } else if x >= self.display_width - self.config.edge_zone_width {
            EdgeSide::Right
        } else {
            EdgeSide::Center
        }
    }

    /// Process raw touch event with sub-8ms latency response
    pub fn process_touch(&mut self, event: &RawTouchEvent) -> GestureAction {
        match event.phase {
            TouchPhase::Down => {
                let edge = self.classify_edge(event.x, event.y);
                match edge {
                    EdgeSide::Bottom => {
                        self.state = GestureState::TrackingBottom {
                            start_x: event.x,
                            start_y: event.y,
                            start_time: event.timestamp,
                            current_x: event.x,
                            current_y: event.y,
                            held_recents: false,
                        };
                        GestureAction::None
                    }
                    EdgeSide::Left | EdgeSide::Right => {
                        self.state = GestureState::TrackingEdge {
                            side: edge,
                            start_x: event.x,
                            start_y: event.y,
                            current_x: event.x,
                            current_y: event.y,
                        };
                        GestureAction::None
                    }
                    EdgeSide::Top => {
                        self.state = GestureState::TrackingTop {
                            start_x: event.x,
                            start_y: event.y,
                            current_x: event.x,
                            current_y: event.y,
                        };
                        GestureAction::None
                    }
                    EdgeSide::Center => {
                        self.state = GestureState::TrackingCenter;
                        GestureAction::None
                    }
                }
            }

            TouchPhase::Move => {
                match &mut self.state {
                    GestureState::TrackingBottom {
                        start_x,
                        start_y,
                        start_time,
                        current_x,
                        current_y,
                        held_recents,
                    } => {
                        *current_x = event.x;
                        *current_y = event.y;

                        let dy = *start_y - event.y; // positive upward
                        let dx = event.x - *start_x;
                        let elapsed = event.timestamp.duration_since(*start_time);

                        // Horizontal scrub along bottom bar
                        if dy.abs() < 30.0 && dx.abs() >= self.config.scrub_threshold_x {
                            let shift = if dx > 0.0 { 1 } else { -1 };
                            return GestureAction::BottomBarScrub {
                                delta_x: dx,
                                app_shift: shift,
                            };
                        }

                        // Check swipe up and hold for Recents
                        if dy >= self.config.home_threshold_y {
                            if elapsed >= self.config.recents_hold_time {
                                let trigger_haptic = !*held_recents;
                                *held_recents = true;
                                let progress = (dy / (self.display_height * 0.4)).clamp(0.0, 1.0);
                                return GestureAction::Recents {
                                    progress,
                                    trigger_haptic,
                                };
                            } else {
                                // Dynamic scale-down animation towards home
                                let progress = (dy / (self.display_height * 0.5)).clamp(0.0, 1.0);
                                let scale = 1.0 - (progress * 0.4); // shrinks to 60%
                                return GestureAction::Home {
                                    progress,
                                    scale,
                                    window_alpha: 1.0 - (progress * 0.3),
                                };
                            }
                        }

                        GestureAction::None
                    }

                    GestureState::TrackingEdge {
                        side,
                        start_x,
                        current_x,
                        current_y,
                        ..
                    } => {
                        *current_x = event.x;
                        *current_y = event.y;

                        let dx = match side {
                            EdgeSide::Left => event.x - *start_x,
                            EdgeSide::Right => *start_x - event.x,
                            _ => 0.0,
                        };

                        let progress = (dx / self.config.back_threshold_x).clamp(0.0, 1.0);
                        GestureAction::Back {
                            side: *side,
                            progress,
                            injected: false,
                        }
                    }

                    GestureState::TrackingTop {
                        start_y, current_y, ..
                    } => {
                        *current_y = event.y;
                        let dy = event.y - *start_y; // positive downward
                        let progress = (dy / (self.display_height * 0.5)).clamp(0.0, 1.0);
                        GestureAction::NotificationShade { progress }
                    }

                    _ => GestureAction::None,
                }
            }

            TouchPhase::Up => {
                let action = match &self.state {
                    GestureState::TrackingBottom {
                        start_x,
                        start_y,
                        start_time,
                        held_recents,
                        ..
                    } => {
                        let dy = *start_y - event.y;
                        let dx = event.x - *start_x;
                        let elapsed = event.timestamp.duration_since(*start_time);

                        if *held_recents {
                            GestureAction::Recents {
                                progress: 1.0,
                                trigger_haptic: false,
                            }
                        } else if dy >= self.config.home_threshold_y
                            && elapsed < self.config.recents_hold_time
                        {
                            // Quick swipe up -> Home
                            GestureAction::Home {
                                progress: 1.0,
                                scale: 0.0,
                                window_alpha: 0.0,
                            }
                        } else if dy.abs() < 30.0 && dx.abs() >= self.config.scrub_threshold_x {
                            let shift = if dx > 0.0 { 1 } else { -1 };
                            GestureAction::BottomBarScrub {
                                delta_x: dx,
                                app_shift: shift,
                            }
                        } else {
                            GestureAction::None
                        }
                    }

                    GestureState::TrackingEdge { side, start_x, .. } => {
                        let dx = match side {
                            EdgeSide::Left => event.x - *start_x,
                            EdgeSide::Right => *start_x - event.x,
                            _ => 0.0,
                        };

                        if dx >= self.config.back_threshold_x {
                            // Inward swipe passed threshold -> emit Back
                            GestureAction::Back {
                                side: *side,
                                progress: 1.0,
                                injected: true,
                            }
                        } else {
                            GestureAction::None
                        }
                    }

                    GestureState::TrackingTop { start_y, .. } => {
                        let dy = event.y - *start_y;
                        if dy >= 50.0 {
                            GestureAction::NotificationShade { progress: 1.0 }
                        } else {
                            GestureAction::NotificationShade { progress: 0.0 }
                        }
                    }

                    _ => GestureAction::None,
                };

                self.state = GestureState::Idle;
                action
            }

            TouchPhase::Cancel => {
                self.state = GestureState::Idle;
                GestureAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cubic_bezier_ease_out() {
        assert_eq!(cubic_bezier_ease_out(0.0), 0.0);
        assert_eq!(cubic_bezier_ease_out(1.0), 1.0);
        assert!(cubic_bezier_ease_out(0.5) > 0.5); // Ease-out starts fast
    }

    #[test]
    fn test_home_gesture_detection() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();

        // Touch down at bottom nav bar
        let res_down = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 2380.0,
            timestamp: t0,
        });
        assert_eq!(res_down, GestureAction::None);

        // Quick move up
        let t1 = t0 + Duration::from_millis(50);
        let res_move = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 2200.0, // dy = 180px
            timestamp: t1,
        });
        match res_move {
            GestureAction::Home { progress, .. } => assert!(progress > 0.0),
            _ => panic!("Expected Home action on swipe up"),
        }

        // Release quickly -> Home trigger
        let t2 = t0 + Duration::from_millis(100);
        let res_up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 2200.0,
            timestamp: t2,
        });
        match res_up {
            GestureAction::Home { progress, .. } => assert_eq!(progress, 1.0),
            _ => panic!("Expected completed Home gesture"),
        }
    }

    #[test]
    fn test_recents_hold_gesture() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();

        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 2380.0,
            timestamp: t0,
        });

        // Move up and hold for 200ms (> 180ms threshold)
        let t_held = t0 + Duration::from_millis(200);
        let res_held = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 2200.0,
            timestamp: t_held,
        });

        match res_held {
            GestureAction::Recents { trigger_haptic, .. } => assert!(trigger_haptic),
            _ => panic!("Expected Recents hold gesture with haptic trigger"),
        }
    }

    #[test]
    fn test_back_edge_gesture() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();

        // Touch down at left edge
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 10.0,
            y: 1200.0,
            timestamp: t0,
        });

        // Drag inward
        let t1 = t0 + Duration::from_millis(80);
        let res_move = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 70.0,
            y: 1200.0,
            timestamp: t1,
        });
        match res_move {
            GestureAction::Back { progress, .. } => assert_eq!(progress, 1.0),
            _ => panic!("Expected Back action progress"),
        }

        // Release
        let res_up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 70.0,
            y: 1200.0,
            timestamp: t1 + Duration::from_millis(20),
        });
        match res_up {
            GestureAction::Back { injected, .. } => assert!(injected),
            _ => panic!("Expected Back injection"),
        }
    }
}
