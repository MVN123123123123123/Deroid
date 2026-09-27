//! QuickStep Gesture Navigation Engine (Android 10+ Style).
//! Processes raw touch events with sub-8ms latency response, executing
//! Home (swipe-up with scale-down animation), Recents (swipe & hold with haptic trigger),
//! Back (left/right edge swipe), Bottom Bar Scrubbing (rapid app switch),
//! and Notification Shade pull-down.
//!
//! Everything here is allocation-free POD: the whole engine is a flat struct
//! whose only mutable state besides the gesture state machine is a fixed
//! 8-sample motion ring buffer and a motion-pause accumulator. No `String`,
//! no `Vec`, no `format!` on the touch hot path.

use std::time::Instant;

// ---------------------------------------------------------------------------
// Material 3 path interpolators (plan §5.1)
// ---------------------------------------------------------------------------

/// Bisection steps for the parameter solve. Bounds the recovered bezier
/// parameter to within 2^-20 ~= 9.5e-7. The *output* error is that times the
/// curve's slope: `EMPHASIZED` peaks near 1.9 around t = 0.15, so worst case
/// is ~1.8e-6, which is under 0.005 px across a 2400 px panel.
const BISECTION_STEPS: u32 = 20;

/// Evaluate a cubic bezier easing the way Android's `PathInterpolator` does:
/// `cp` is `[c1x, c1y, c2x, c2y]` (control points 1 and 2; the start point is
/// `(0, 0)` and the end point is `(1, 1)`), `t_in` is the animation clock in
/// `0..=1`, and the return value is the eased fraction.
///
/// The bezier parameter is recovered by bisecting `x(t) == t_in` (the x control
/// points of every Material curve are monotone, so bisection always converges)
/// and then evaluating `y` at that parameter. Table-free: no LUT, no
/// allocation, no Newton iteration with a derivative guard.
#[inline]
pub fn cubic_bezier(cp: [f32; 4], t_in: f32) -> f32 {
    let t = t_in.clamp(0.0, 1.0);
    if t <= 0.0 || t >= 1.0 {
        return t; // both endpoints are exact for any control points
    }
    let (c1x, c1y, c2x, c2y) = (cp[0], cp[1], cp[2], cp[3]);

    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut i = 0;
    while i < BISECTION_STEPS {
        let m = 0.5 * (lo + hi);
        let w = 1.0 - m;
        let bx = 3.0 * w * w * m * c1x + 3.0 * w * m * m * c2x + m * m * m;
        if bx < t {
            lo = m;
        } else {
            hi = m;
        }
        i += 1;
    }
    let m = 0.5 * (lo + hi);
    let w = 1.0 - m;
    3.0 * w * w * m * c1y + 3.0 * w * m * m * c2y + m * m * m
}

/// Android `FastOutSlowInInterpolator` == `cubicTo(0.4, 0.0, 0.2, 1.0)`.
///
/// Kept as the public name because `crates/utlc` and the integration tests
/// import it.
#[inline]
pub fn fast_out_slow_in(t: f32) -> f32 {
    cubic_bezier([0.4, 0.0, 0.2, 1.0], t)
}

/// Material 3 `emphasized_decelerate` == `cubicTo(0.05, 0.7, 0.1, 1.0)`.
#[inline]
pub fn emphasized_decelerate(t: f32) -> f32 {
    cubic_bezier([0.05, 0.7, 0.1, 1.0], t)
}

/// Material 3 `emphasized_accelerate` == `cubicTo(0.3, 0.0, 0.8, 0.15)`.
#[inline]
pub fn emphasized_accelerate(t: f32) -> f32 {
    cubic_bezier([0.3, 0.0, 0.8, 0.15], t)
}

/// Material 3 `standard_decelerate` == `cubicTo(0.0, 0.0, 0.0, 1.0)`.
#[inline]
pub fn standard_decelerate(t: f32) -> f32 {
    cubic_bezier([0.0, 0.0, 0.0, 1.0], t)
}

/// Material 3 `touch_response` == `cubicTo(0.3, 0.0, 0.1, 1.0)`.
#[inline]
pub fn touch_response(t: f32) -> f32 {
    cubic_bezier([0.3, 0.0, 0.1, 1.0], t)
}

/// Material 3 `linear_out_slow_in` == `cubicTo(0.0, 0.0, 0.2, 1.0)`.
#[inline]
pub fn linear_out_slow_in(t: f32) -> f32 {
    cubic_bezier([0.0, 0.0, 0.2, 1.0], t)
}

/// Material 3 `fast_out_linear_in` == `cubicTo(0.4, 0.0, 1.0, 1.0)`.
#[inline]
pub fn fast_out_linear_in(t: f32) -> f32 {
    cubic_bezier([0.4, 0.0, 1.0, 1.0], t)
}

/// Split point of the Material 3 `emphasized` path: `(0.1667, 0.4)`.
const EMPHASIZED_SPLIT: f32 = 0.1667;
/// Split point of the Material 3 `emphasized` path: `(0.1667, 0.4)`.
const EMPHASIZED_SPLIT_Y: f32 = 0.4;

/// Segment 1 of `emphasized`, normalised to its own sub-range: absolute
/// control points `(0.05, 0)` and `(0.1333, 0.06)` over `[0, 0.1667] x [0, 0.4]`.
const EMPHASIZED_SEG1: [f32; 4] = [
    0.05 / EMPHASIZED_SPLIT,
    0.0,
    0.1333 / EMPHASIZED_SPLIT,
    0.06 / EMPHASIZED_SPLIT_Y,
];

/// Segment 2 of `emphasized`, normalised to its own sub-range: absolute
/// control points `(0.2083, 0.82)` and `(0.25, 1.0)` over
/// `[0.1667, 1.0] x [0.4, 1.0]`.
const EMPHASIZED_SEG2: [f32; 4] = [
    (0.2083 - EMPHASIZED_SPLIT) / (1.0 - EMPHASIZED_SPLIT),
    (0.82 - EMPHASIZED_SPLIT_Y) / (1.0 - EMPHASIZED_SPLIT_Y),
    (0.25 - EMPHASIZED_SPLIT) / (1.0 - EMPHASIZED_SPLIT),
    // (1.0 - 0.4) / (1.0 - 0.4): the second control point already sits on the
    // segment's end point, so its normalised y is exactly 1.
    1.0,
];

/// Material 3 `emphasized` == `PathInterpolator(0.05, 0, 0.1333, 0.06, 0.1667, 0.4,
/// 0.2083, 0.82, 0.25, 1.0, 1.0, 1.0)`.
///
/// This is a TWO-SEGMENT interpolator, so it is *not* expressible as one
/// `cubic_bezier` call. Each segment is normalised to its own sub-range and
/// run through the same bisection solver, so the join is C0-continuous: both
/// segments return exactly `EMPHASIZED_SPLIT_Y` (= 0.4) at the boundary.
#[inline]
pub fn emphasized(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    if x < EMPHASIZED_SPLIT {
        cubic_bezier(EMPHASIZED_SEG1, x / EMPHASIZED_SPLIT) * EMPHASIZED_SPLIT_Y
    } else {
        cubic_bezier(
            EMPHASIZED_SEG2,
            (x - EMPHASIZED_SPLIT) / (1.0 - EMPHASIZED_SPLIT),
        ) * (1.0 - EMPHASIZED_SPLIT_Y)
            + EMPHASIZED_SPLIT_Y
    }
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
    // TODO(plan §7.3): `Home` should also carry
    // `workspace_origin: Option<(f32, f32)>` so the shell can drop the app into
    // the workspace card the swipe started over. Adding the field is a
    // breaking change to this public enum and `crates/utlc/src/main.rs`
    // destructures `Home { progress, .. }` today, so it waits for the
    // shell-side migration.
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
    /// Center screen swipe (e.g., page switch or app drawer pull)
    Swipe {
        delta_x: f32,
        delta_y: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum GestureState {
    Idle,
    TrackingBottom {
        owner_id: i32,
        start_x: f32,
        start_y: f32,
        #[allow(dead_code)]
        start_time: Instant,
        current_x: f32,
        current_y: f32,
        held_recents: bool,
        last_scrub_x: f32,
    },
    TrackingEdge {
        owner_id: i32,
        side: EdgeSide,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
    },
    TrackingTop {
        owner_id: i32,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
    },
    TrackingCenter {
        owner_id: i32,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
    },
}

impl GestureState {
    /// Pointer that owns the in-flight gesture, if any.
    fn owner_id(&self) -> Option<i32> {
        match self {
            GestureState::Idle => None,
            GestureState::TrackingBottom { owner_id, .. }
            | GestureState::TrackingEdge { owner_id, .. }
            | GestureState::TrackingTop { owner_id, .. }
            | GestureState::TrackingCenter { owner_id, .. } => Some(*owner_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Motion history (plan §7.3, Task 3)
// ---------------------------------------------------------------------------

/// Number of samples kept in the motion ring buffer. At 120 Hz a touch frame is
/// ~8.3 ms, so 8 samples cover ~66 ms: enough to see a fling and a stop, and
/// small enough that the whole buffer is 192 bytes.
pub const MOTION_SAMPLES: usize = 8;

/// Fixed-size ring buffer of the last [`MOTION_SAMPLES`] touch samples.
///
/// `Instant` is not `Default`, so the timestamps are initialised with a single
/// captured sentinel; `n` gates every read, so the sentinel is never
/// interpreted as a real sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionHistory {
    t: [Instant; MOTION_SAMPLES],
    x: [f32; MOTION_SAMPLES],
    y: [f32; MOTION_SAMPLES],
    /// Number of valid samples, saturating at `MOTION_SAMPLES`.
    n: u8,
    /// Index of the *next* write slot; the newest sample is `head - 1`.
    head: u8,
}

impl MotionHistory {
    pub fn new() -> Self {
        let sentinel = Instant::now();
        Self {
            t: [sentinel; MOTION_SAMPLES],
            x: [0.0; MOTION_SAMPLES],
            y: [0.0; MOTION_SAMPLES],
            n: 0,
            head: 0,
        }
    }

    /// Drop all samples. O(1): the stale payloads stay in the arrays but `n`
    /// gates every read, so nothing can observe them.
    #[inline]
    pub fn clear(&mut self) {
        self.n = 0;
        self.head = 0;
    }

    /// Number of live samples.
    #[inline]
    pub fn len(&self) -> u8 {
        self.n
    }

    /// True once [`MotionHistory::clear`] has run and nothing has been pushed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Append one sample, overwriting the oldest once the buffer is full.
    #[inline]
    pub fn push(&mut self, t: Instant, x: f32, y: f32) {
        let i = self.head as usize;
        self.t[i] = t;
        self.x[i] = x;
        self.y[i] = y;
        self.head = ((i + 1) % MOTION_SAMPLES) as u8;
        if self.n < MOTION_SAMPLES as u8 {
            self.n += 1;
        }
    }

    /// Index of the `back`th newest sample (`back == 0` is the newest).
    #[inline]
    fn idx(&self, back: usize) -> usize {
        (self.head as usize + MOTION_SAMPLES - 1 - back) % MOTION_SAMPLES
    }

    /// Elapsed time between the two newest samples, in milliseconds.
    /// 0.0 when fewer than two samples exist or the clock did not advance.
    #[inline]
    pub fn dt_ms(&self) -> f32 {
        if self.n < 2 {
            return 0.0;
        }
        let dt = self.t[self.idx(0)].saturating_duration_since(self.t[self.idx(1)]);
        if dt.is_zero() {
            0.0
        } else {
            dt.as_secs_f32() * 1000.0
        }
    }

    /// Pointer velocity in px/ms, positive x right and positive y **down**
    /// (raw screen convention, straight out of the sample ring).
    ///
    /// Computed over the two newest samples only: O(1), zero allocation.
    /// Returns `(0.0, 0.0)` when fewer than two samples exist or `dt <= 0`, so
    /// a duplicate timestamp can never divide by zero.
    #[inline]
    pub fn velocity(&self) -> (f32, f32) {
        if self.n < 2 {
            return (0.0, 0.0);
        }
        let new = self.idx(0);
        let old = self.idx(1);
        let dt = self.t[new].saturating_duration_since(self.t[old]);
        if dt.is_zero() {
            return (0.0, 0.0);
        }
        let dt_ms = dt.as_secs_f32() * 1000.0;
        (
            (self.x[new] - self.x[old]) / dt_ms,
            (self.y[new] - self.y[old]) / dt_ms,
        )
    }

    /// Net displacement magnitude in px from the oldest to the newest live
    /// sample. Windowed, not monotonic: once the finger stops, old travel
    /// ages out of the ring. Callers that need a sticky "did this gesture move
    /// far enough" gate must latch it (see [`MotionPause`]).
    #[inline]
    pub fn travel(&self) -> f32 {
        if self.n < 2 {
            return 0.0;
        }
        let new = self.idx(0);
        let old = self.idx(self.n as usize - 1);
        (self.x[new] - self.x[old]).hypot(self.y[new] - self.y[old])
    }

    /// Magnitude of [`MotionHistory::velocity`], in px/ms.
    #[inline]
    pub fn speed(&self) -> f32 {
        let (vx, vy) = self.velocity();
        vx.hypot(vy)
    }
}

impl Default for MotionHistory {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Motion pause detection (plan §7.3, Task 4)
// ---------------------------------------------------------------------------

/// Recents "hold" detector.
///
/// A motion pause is *not* a duration hold: it is the finger slowing down and
/// staying slow, which is what AOSP's `MotionPauseDetector` measures. The
/// trigger is the finger being
///
/// * slow (`speed <= motion_pause_slow`) for `force_pause_ms`, or very slow
///   (`speed <= motion_pause_very_slow`) for `harder_trigger_ms`,
/// * *and* having rapidly decelerated: `decel = speed / peak_speed <= 0.6`
///   (`rapid_decel_factor`), and
/// * *and* having travelled at least `min_displacement` at some point.
///
/// Conventions used here (matching `MotionPauseDetector.java`):
///
/// * `decel` is the ratio `speed / peak_speed`, so a *small* `decel` means the
///   finger has decelerated a lot. It is produced by [`MotionPause::observe`].
/// * The displacement gate is latched (`mHasHitMinDisplacement`) because
///   [`MotionHistory::travel`] is windowed and would fall back to ~0 once the
///   finger stops.
pub struct MotionPause {
    /// Consecutive time spent below `motion_pause_slow` (ms).
    slow_ms: f32,
    /// Consecutive time spent below `motion_pause_very_slow` (ms).
    very_slow_ms: f32,
    /// Fastest speed seen so far in this gesture (px/ms).
    peak_speed: f32,
    /// Latched `travel >= min_displacement`.
    min_displacement_hit: bool,
    /// Latched trigger.
    paused: bool,
}

impl MotionPause {
    pub const fn new() -> Self {
        Self {
            slow_ms: 0.0,
            very_slow_ms: 0.0,
            peak_speed: 0.0,
            min_displacement_hit: false,
            paused: false,
        }
    }

    /// Forget everything about the current gesture (call on touch DOWN).
    #[inline]
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Track the gesture peak speed and return the deceleration ratio
    /// `speed / peak_speed` in `0.0..=1.0` to hand back to
    /// [`MotionPause::is_paused`].
    #[inline]
    pub fn observe(&mut self, speed: f32) -> f32 {
        if speed > self.peak_speed {
            self.peak_speed = speed;
        }
        if self.peak_speed > 0.0 {
            (speed / self.peak_speed).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// True once the finger has been slow for `force_pause_ms`, or very slow
    /// for `harder_trigger_ms`, after rapidly decelerating and having covered
    /// `min_displacement`. Latches once fired, like AOSP's `mIsPaused`.
    pub fn is_paused(
        &mut self,
        speed: f32,
        decel: f32,
        travel: f32,
        dt_ms: f32,
        cfg: &GestureConfig,
    ) -> bool {
        if self.paused {
            return true;
        }
        self.min_displacement_hit |= travel >= cfg.min_displacement;
        if !self.min_displacement_hit {
            // Too little travel to be a deliberate stop; keep the peak
            // tracking but do not accumulate pause time.
            return false;
        }

        // A finger moving linearly is unambiguously a drag
        // (`LINEAR_VELOCITY` in MotionPauseDetector): drop both clocks. A
        // non-finite speed (NaN from a degenerate timestamp) can never count as
        // slowness either.
        if !speed.is_finite() || speed > cfg.motion_pause_fast {
            self.slow_ms = 0.0;
            self.very_slow_ms = 0.0;
            return false;
        }

        let slow = speed <= cfg.motion_pause_slow;
        let decelerated = decel.is_finite() && decel <= cfg.rapid_decel_factor;
        // A duplicated timestamp yields dt == 0: that must add no pause time
        // rather than poison the accumulator with a NaN.
        let dt = if dt_ms.is_finite() && dt_ms > 0.0 {
            dt_ms
        } else {
            0.0
        };
        if slow && decelerated {
            self.slow_ms += dt;
            if speed <= cfg.motion_pause_very_slow {
                self.very_slow_ms += dt;
            } else {
                self.very_slow_ms = 0.0;
            }
        } else {
            // Not slow, or slow but still tracking the peak: this is a drag.
            self.slow_ms = 0.0;
            self.very_slow_ms = 0.0;
        }

        if self.slow_ms >= cfg.force_pause_ms || self.very_slow_ms >= cfg.harder_trigger_ms {
            self.paused = true;
        }
        self.paused
    }
}

impl Default for MotionPause {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Gesture Engine Configuration
///
/// Every field here is read by the engine. A constant that only a *future*
/// consumer would read does not belong in this struct: `recents_hold_time`,
/// `overview_min_progress` and `max_swipe_ms` were all removed for exactly
/// that reason -- the first contradicts the motion-pause design outright, and
/// the other two belong to the shell's swipe-to-drawer commit, which does not
/// exist yet. Add them back with the consumer, not before.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureConfig {
    pub bottom_nav_height: f32, // default 48.0 px
    pub edge_zone_width: f32,   // default 48.0 px
    pub top_bar_height: f32,    // default 48.0 px
    pub home_threshold_y: f32,  // default 60.0 px
    pub back_threshold_x: f32,  // default 40.0 px
    pub scrub_threshold_x: f32, // default 80.0 px

    // --- Quickstep-derived constants (plan §7.3) ---
    /// Fling speed, px/ms. `quickstep/res/values/dimens.xml:152`
    /// `quickstep_fling_threshold_speed`.
    ///
    /// A release faster than this is a fling; slower is a deliberate stop.
    /// Both commit Home, but the branch is explicit so the shell can treat
    /// them differently once it owns the window transform.
    pub fling_threshold: f32,
    /// Panel touch slop, px. `ViewConfiguration.getScaledTouchSlop()` on a
    /// 1080 px / ~420 dp panel.
    pub touch_slop: f32,
    /// `RecentsAnimationDeviceState.java:106`
    /// `QUICKSTEP_TOUCH_SLOP_RATIO_GESTURAL`.
    pub touch_slop_ratio: f32,
    /// Slow-motion speed threshold, px/ms. `dimens.xml:147`
    /// `quickstep_motion_pause_slow`.
    pub motion_pause_slow: f32,
    /// Very-slow speed threshold, px/ms. `dimens.xml:146`
    /// `quickstep_motion_pause_very_slow`.
    pub motion_pause_very_slow: f32,
    /// Linear (fast) speed threshold, px/ms. `dimens.xml:150`
    /// `quickstep_motion_pause_fast`. A finger above this is moving linearly:
    /// a drag, never a pause.
    pub motion_pause_fast: f32,
    /// `MotionPauseDetector.java:45` `FORCE_PAUSE_TIMEOUT` (ms).
    pub force_pause_ms: f32,
    /// `MotionPauseDetector.java:50` `HARDER_TRIGGER_TIMEOUT` (ms).
    pub harder_trigger_ms: f32,
    /// `MotionPauseDetector.java:41` `RAPID_DECELERATION_FACTOR`.
    pub rapid_decel_factor: f32,
    /// `dimens.xml:151` `quickstep_min_displacement_from_app` (px).
    pub min_displacement: f32,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            bottom_nav_height: 48.0,
            edge_zone_width: 48.0,
            top_bar_height: 48.0,
            home_threshold_y: 60.0,
            back_threshold_x: 40.0,
            scrub_threshold_x: 80.0,
            fling_threshold: 0.5,
            touch_slop: 8.0,
            touch_slop_ratio: 1.414,
            motion_pause_slow: 0.15,
            motion_pause_very_slow: 0.0285,
            motion_pause_fast: 1.4,
            force_pause_ms: 300.0,
            harder_trigger_ms: 400.0,
            rapid_decel_factor: 0.6,
            min_displacement: 36.0,
        }
    }
}

impl GestureConfig {
    /// Effective touch slop: Quickstep scales the panel slop up for gestural
    /// navigation (`QUICKSTEP_TOUCH_SLOP_RATIO_GESTURAL`).
    #[inline]
    pub fn gesture_slop(&self) -> f32 {
        self.touch_slop * self.touch_slop_ratio
    }
}

pub struct GestureEngine {
    pub display_width: f32,
    pub display_height: f32,
    pub config: GestureConfig,
    state: GestureState,
    /// Ring of the last [`MOTION_SAMPLES`] touch samples for this gesture.
    motion: MotionHistory,
    /// Recents motion-pause accumulator for this gesture.
    pause: MotionPause,
}

impl GestureEngine {
    pub fn new(display_width: f32, display_height: f32, config: GestureConfig) -> Self {
        Self {
            display_width,
            display_height,
            config,
            state: GestureState::Idle,
            motion: MotionHistory::new(),
            pause: MotionPause::new(),
        }
    }

    pub fn classify_edge(&self, x: f32, y: f32) -> EdgeSide {
        // Edges win in the corners (Android's rule: corner beats bar), so a
        // back gesture stays reachable from the bottom/top corners.
        if x <= self.config.edge_zone_width {
            EdgeSide::Left
        } else if x >= self.display_width - self.config.edge_zone_width {
            EdgeSide::Right
        } else if y >= self.display_height - self.config.bottom_nav_height {
            EdgeSide::Bottom
        } else if y <= self.config.top_bar_height {
            EdgeSide::Top
        } else {
            EdgeSide::Center
        }
    }

    /// Process raw touch event with sub-8ms latency response
    pub fn process_touch(&mut self, event: &RawTouchEvent) -> GestureAction {
        // Single-pointer engine: Move/Up from a foreign pointer are ignored,
        // and a second finger's Down never steals the in-flight gesture.
        match (&self.state, event.phase) {
            (GestureState::Idle, _) | (_, TouchPhase::Cancel) => {}
            (_, TouchPhase::Down) => {
                if let Some(owner) = self.state.owner_id() {
                    if owner != event.touch_id {
                        return GestureAction::None;
                    }
                }
            }
            (state, TouchPhase::Move | TouchPhase::Up)
                if state.owner_id() != Some(event.touch_id) =>
            {
                return GestureAction::None; // not our finger
            }
            _ => {}
        }

        // Every sample the owning pointer produces feeds the motion ring,
        // including the release: the Up arm needs a real velocity to tell a
        // deliberate stop from a fling.
        match event.phase {
            TouchPhase::Down => {
                self.motion.clear();
                self.pause.reset();
                self.motion.push(event.timestamp, event.x, event.y);
            }
            TouchPhase::Move | TouchPhase::Up => {
                self.motion.push(event.timestamp, event.x, event.y);
            }
            TouchPhase::Cancel => {}
        }

        match event.phase {
            TouchPhase::Down => {
                let edge = self.classify_edge(event.x, event.y);
                match edge {
                    EdgeSide::Bottom => {
                        self.state = GestureState::TrackingBottom {
                            owner_id: event.touch_id,
                            start_x: event.x,
                            start_y: event.y,
                            start_time: event.timestamp,
                            current_x: event.x,
                            current_y: event.y,
                            held_recents: false,
                            last_scrub_x: event.x,
                        };
                        GestureAction::None
                    }
                    EdgeSide::Left | EdgeSide::Right => {
                        self.state = GestureState::TrackingEdge {
                            owner_id: event.touch_id,
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
                            owner_id: event.touch_id,
                            start_x: event.x,
                            start_y: event.y,
                            current_x: event.x,
                            current_y: event.y,
                        };
                        GestureAction::None
                    }
                    EdgeSide::Center => {
                        self.state = GestureState::TrackingCenter {
                            owner_id: event.touch_id,
                            start_x: event.x,
                            start_y: event.y,
                            current_x: event.x,
                            current_y: event.y,
                        };
                        GestureAction::None
                    }
                }
            }

            TouchPhase::Move => {
                match &mut self.state {
                    GestureState::TrackingBottom {
                        start_x,
                        start_y,
                        current_x,
                        current_y,
                        held_recents,
                        last_scrub_x,
                        ..
                    } => {
                        *current_x = event.x;
                        *current_y = event.y;

                        let dy = *start_y - event.y; // positive upward
                        let dx = event.x - *start_x;

                        // Horizontal scrub along bottom bar: one app shift per
                        // scrub_threshold_x of additional travel (re-armed).
                        if dy.abs() < 30.0 && dx.abs() >= self.config.scrub_threshold_x {
                            let steps = (dx.abs() / self.config.scrub_threshold_x) as i32;
                            let last = ((*last_scrub_x - *start_x).abs()
                                / self.config.scrub_threshold_x)
                                as i32;
                            if steps > last {
                                *last_scrub_x = event.x;
                                let shift = if dx > 0.0 { 1 } else { -1 };
                                return GestureAction::BottomBarScrub {
                                    delta_x: dx,
                                    app_shift: shift,
                                };
                            }
                            return GestureAction::None;
                        }

                        // Swiping back down to the nav bar cancels Recents.
                        if dy < self.config.home_threshold_y {
                            *held_recents = false;
                        }

                        if dy >= self.config.home_threshold_y {
                            // Recents is a *motion pause*, not a duration hold:
                            // the finger must have travelled, decelerated, and
                            // then stayed slow. A 60 px crawl held for 180 ms
                            // is still a Home drag.
                            let speed = self.motion.speed();
                            let decel = self.pause.observe(speed);
                            let paused = self.pause.is_paused(
                                speed,
                                decel,
                                self.motion.travel(),
                                self.motion.dt_ms(),
                                &self.config,
                            );
                            if paused {
                                // Rising edge only: the latch keeps the haptic
                                // from re-pulsing on every subsequent MOVE.
                                let trigger_haptic = !*held_recents;
                                *held_recents = true;
                                let progress = (dy / (self.display_height * 0.4)).clamp(0.0, 1.0);
                                return GestureAction::Recents {
                                    progress,
                                    trigger_haptic,
                                };
                            }
                            // Dynamic scale-down animation towards home
                            let progress = (dy / (self.display_height * 0.5)).clamp(0.0, 1.0);
                            let scale = 1.0 - (progress * 0.4); // shrinks to 60%
                            return GestureAction::Home {
                                progress,
                                scale,
                                window_alpha: 1.0 - (progress * 0.3),
                            };
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

                        // Outward jitter is not a back gesture.
                        if dx <= 0.0 {
                            return GestureAction::None;
                        }
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

                    GestureState::TrackingCenter {
                        start_x,
                        start_y,
                        current_x,
                        current_y,
                        ..
                    } => {
                        *current_x = event.x;
                        *current_y = event.y;
                        // Swipe up from inside an app towards Home. The axis is
                        // claimed by dominance (|dy| > |dx|), not by a fixed
                        // dead zone, and only past the Quickstep-scaled slop, so
                        // a horizontal drag stays available to the app.
                        let dy = *start_y - event.y; // positive upward
                        let dx = event.x - *start_x;
                        let slop = self.config.gesture_slop();
                        if dy > slop && dy.abs() > dx.abs() {
                            let progress = (dy / (self.display_height * 0.5)).clamp(0.0, 1.0);
                            GestureAction::Home {
                                progress,
                                scale: 1.0 - (progress * 0.4),
                                window_alpha: 1.0 - (progress * 0.3),
                            }
                        } else {
                            GestureAction::None
                        }
                    }
                    _ => GestureAction::None,
                }
            }

            TouchPhase::Up => {
                let action = match &mut self.state {
                    GestureState::TrackingBottom {
                        start_x,
                        start_y,
                        held_recents,
                        last_scrub_x,
                        ..
                    } => {
                        let dy = *start_y - event.y;
                        let dx = event.x - *start_x;

                        // Swiping back below the threshold releases the Recents
                        // latch, so the release cannot commit an overview the
                        // user already abandoned.
                        if dy < self.config.home_threshold_y {
                            *held_recents = false;
                        }
                        if *held_recents {
                            GestureAction::Recents {
                                progress: 1.0,
                                trigger_haptic: false,
                            }
                        } else if dy >= self.config.home_threshold_y {
                            // Commit Home only for a deliberate stop or a real
                            // upward fling; a slow drag that is still crawling
                            // downwards is a cancel. Terminates on the same
                            // curve the drag used (scale 0.6, alpha 0.7).
                            //
                            // Velocity is the raw screen convention (positive y
                            // down), so negate for the gesture's own dy.
                            let (_, vy) = self.motion.velocity();
                            let v_up = -vy; // px/ms, positive upward
                            if v_up.abs() < self.config.fling_threshold
                                || v_up > self.config.fling_threshold
                            {
                                GestureAction::Home {
                                    progress: 1.0,
                                    scale: 0.6,
                                    window_alpha: 0.7,
                                }
                            } else {
                                GestureAction::None
                            }
                        } else if dy.abs() < 30.0 && dx.abs() >= self.config.scrub_threshold_x {
                            // Commit only a step the MOVE path has not emitted.
                            let steps = (dx.abs() / self.config.scrub_threshold_x) as i32;
                            let last = ((*last_scrub_x - *start_x).abs()
                                / self.config.scrub_threshold_x)
                                as i32;
                            if steps > last {
                                let shift = if dx > 0.0 { 1 } else { -1 };
                                GestureAction::BottomBarScrub {
                                    delta_x: dx,
                                    app_shift: shift,
                                }
                            } else {
                                GestureAction::None
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
                            // A tap is not a drag: report nothing instead of
                            // slamming an opening shade to 0.0.
                            GestureAction::None
                        }
                    }

                    GestureState::TrackingCenter {
                        start_x,
                        start_y,
                        current_x,
                        current_y,
                        ..
                    } => {
                        let dx = *current_x - *start_x;
                        let dy = *current_y - *start_y;
                        let slop = self.config.gesture_slop();
                        if dx.abs() >= slop || dy.abs() >= slop {
                            GestureAction::Swipe {
                                delta_x: dx,
                                delta_y: dy,
                            }
                        } else {
                            GestureAction::None
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
    use std::time::Duration;

    /// Drive a gesture from the bottom nav bar.
    fn ev(t: Instant, phase: TouchPhase, y: f32) -> RawTouchEvent {
        RawTouchEvent {
            touch_id: 1,
            phase,
            x: 540.0,
            y,
            timestamp: t,
        }
    }

    /// The engine is owned by the shell across threads and the motion ring
    /// holds `Instant`s: neither may make it un-sendable.
    #[test]
    fn gesture_engine_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<GestureEngine>();
        assert_send::<MotionHistory>();
        assert_send::<MotionPause>();
    }

    // -- §5.1 interpolators -------------------------------------------------

    /// Reference values were derived offline from the control points
    /// (200-step float64 bisection on `x(t) == probe`, error < 2^-200), not by
    /// calling the solver under test. 6 decimals is ~30x the solver's own
    /// error, so the 1e-3 bound below is a real check, not a tautology.
    #[test]
    fn interpolators_match_android_control_points() {
        struct Curve {
            name: &'static str,
            f: fn(f32) -> f32,
            f25: f32,
            f50: f32,
            f75: f32,
            /// True when the curve is expected to sit below the y == x
            /// diagonal at t = 0.25 (an accelerate-style curve, or any curve
            /// whose first control point x is far right of its y).
            below_diagonal_at_25: bool,
        }
        const CURVES: [Curve; 7] = [
            Curve {
                name: "fast_out_slow_in(0.4,0,0.2,1.0)",
                f: fast_out_slow_in,
                f25: 0.236587,
                f50: 0.775561,
                f75: 0.959368,
                // c1 = (0.4, 0): the first 25% of the timeline is still
                // accelerating, so it lags the diagonal slightly.
                below_diagonal_at_25: true,
            },
            Curve {
                name: "emphasized_decelerate(0.05,0.7,0.1,1.0)",
                f: emphasized_decelerate,
                f25: 0.831530,
                f50: 0.950247,
                f75: 0.990511,
                below_diagonal_at_25: false,
            },
            Curve {
                name: "emphasized_accelerate(0.3,0,0.8,0.15)",
                f: emphasized_accelerate,
                f25: 0.035343,
                f50: 0.153998,
                f75: 0.405586,
                below_diagonal_at_25: true,
            },
            Curve {
                name: "standard_decelerate(0,0,0,1)",
                f: standard_decelerate,
                f25: 0.690551,
                f50: 0.889882,
                f75: 0.976445,
                below_diagonal_at_25: false,
            },
            Curve {
                name: "touch_response(0.3,0,0.1,1)",
                f: touch_response,
                f25: 0.433591,
                f50: 0.838471,
                f75: 0.968957,
                below_diagonal_at_25: false,
            },
            Curve {
                name: "linear_out_slow_in(0,0,0.2,1)",
                f: linear_out_slow_in,
                f25: 0.577573,
                f50: 0.839245,
                f75: 0.964216,
                below_diagonal_at_25: false,
            },
            Curve {
                name: "fast_out_linear_in(0.4,0,1,1)",
                f: fast_out_linear_in,
                f25: 0.098627,
                f50: 0.324815,
                f75: 0.630085,
                below_diagonal_at_25: true,
            },
        ];

        for c in &CURVES {
            assert!((c.f)(0.0).abs() < 1e-6, "{}: f(0) != 0", c.name);
            assert!(((c.f)(1.0) - 1.0).abs() < 1e-6, "{}: f(1) != 1", c.name);
            assert!(
                ((c.f)(0.25) - c.f25).abs() < 1e-3,
                "{}: f(0.25) = {} want {}",
                c.name,
                (c.f)(0.25),
                c.f25
            );
            assert!(
                ((c.f)(0.5) - c.f50).abs() < 1e-3,
                "{}: f(0.5) = {} want {}",
                c.name,
                (c.f)(0.5),
                c.f50
            );
            assert!(
                ((c.f)(0.75) - c.f75).abs() < 1e-3,
                "{}: f(0.75) = {} want {}",
                c.name,
                (c.f)(0.75),
                c.f75
            );
            // Monotone, in range, no NaN over a dense grid. None of the
            // Material curves overshoot 0 or 1.
            let mut prev = 0.0f32;
            let mut i = 0;
            while i <= 1000 {
                let x = i as f32 / 1000.0;
                let y = (c.f)(x);
                assert!(y.is_finite(), "{}: f({}) not finite", c.name, x);
                assert!(
                    (0.0..=1.0).contains(&y),
                    "{}: f({}) = {} out of range",
                    c.name,
                    x,
                    y
                );
                assert!(y >= prev, "{}: f({}) = {} not monotone", c.name, x, y);
                prev = y;
                i += 1;
            }
            // Accelerate-style curves are allowed to sit below the diagonal
            // early; decelerate-style ones must be above it.
            if c.below_diagonal_at_25 {
                assert!(
                    (c.f)(0.25) < 0.25,
                    "{}: f(0.25) = {} should lag the diagonal",
                    c.name,
                    (c.f)(0.25)
                );
            } else {
                assert!(
                    (c.f)(0.25) > 0.25,
                    "{}: f(0.25) = {} should lead the diagonal",
                    c.name,
                    (c.f)(0.25)
                );
            }
        }

        // emphasized_accelerate is the extreme case: it is 7x below the
        // diagonal at a quarter of the timeline and is still below it at 0.9
        // (0.683301), which is what makes it an *accelerate* curve.
        assert!(emphasized_accelerate(0.5) < 0.5);
        assert!(emphasized_accelerate(0.9) < 0.9);

        // Out-of-domain clocks clamp instead of extrapolating.
        assert_eq!(fast_out_slow_in(-5.0), 0.0);
        assert_eq!(fast_out_slow_in(9.0), 1.0);
    }

    #[test]
    fn emphasized_is_two_segment_and_continuous() {
        assert_eq!(emphasized(0.0), 0.0);
        assert_eq!(emphasized(1.0), 1.0);
        // Both segments return exactly 0.4 at the join, so the path is
        // C0-continuous across the split at (0.1667, 0.4): segment 1 hits its
        // own end point and segment 2 hits its own start point, both of which
        // the solver returns exactly.
        assert_eq!(cubic_bezier(EMPHASIZED_SEG1, 1.0), 1.0);
        assert_eq!(cubic_bezier(EMPHASIZED_SEG2, 0.0), 0.0);
        assert_eq!(
            emphasized(EMPHASIZED_SPLIT),
            EMPHASIZED_SPLIT_Y,
            "join value must be exactly the split y"
        );
        // The left limit approaches it linearly (the local slope there is
        // ~6.1, not infinite): the jump must vanish with the step, which a
        // real discontinuity could not do.
        let mut h = 1e-3f32;
        while h >= 1e-6 {
            let jump = (emphasized(EMPHASIZED_SPLIT) - emphasized(EMPHASIZED_SPLIT - h)).abs();
            assert!(
                jump < 20.0 * h,
                "step of {} at h = {} is not a continuous join",
                jump,
                h
            );
            h *= 0.1;
        }
        // Reference values, same offline derivation as above:
        //   (0.05, 0.020613) (0.0833, 0.061558) (0.2, 0.635686)
        //   (0.25, 0.772842) (0.5, 0.950614)  (0.75, 0.991401)
        for (x, want) in [
            (0.05, 0.020613),
            (0.0833, 0.061558),
            (0.2, 0.635686),
            (0.25, 0.772842),
            (0.5, 0.950614),
            (0.75, 0.991401),
        ] {
            assert!(
                (emphasized(x) - want).abs() < 1e-3,
                "emphasized({}) = {} want {}",
                x,
                emphasized(x),
                want
            );
        }
        // Monotone, finite, no overshoot of either endpoint.
        let mut prev = 0.0f32;
        let mut i = 0;
        while i <= 1000 {
            let x = i as f32 / 1000.0;
            let y = emphasized(x);
            assert!(y.is_finite(), "emphasized({}) not finite", x);
            assert!(y >= prev, "emphasized({}) = {} not monotone", x, y);
            assert!(y <= 1.0, "emphasized({}) = {} overshoots 1.0", x, y);
            prev = y;
            i += 1;
        }
        // A single cubic bezier cannot reproduce it: the emphasized path is
        // genuinely two-segment, so the first (accelerate) segment alone is
        // far slower than the joined curve at the split.
        assert!(emphasized(EMPHASIZED_SPLIT) > emphasized_accelerate(EMPHASIZED_SPLIT));
    }

    // -- §7.3 motion history -----------------------------------------------

    #[test]
    fn motion_history_velocity_matches_known_slope() {
        let t0 = Instant::now();
        let mut h = MotionHistory::new();
        assert!(h.is_empty());
        // Fewer than two samples: no velocity, no travel.
        assert_eq!(h.velocity(), (0.0, 0.0));
        assert_eq!(h.speed(), 0.0);
        assert_eq!(h.travel(), 0.0);
        assert_eq!(h.dt_ms(), 0.0);

        h.push(t0, 100.0, 200.0);
        assert_eq!(h.len(), 1);
        assert_eq!(h.velocity(), (0.0, 0.0));

        // 1 px/ms ramp: 5 px right, 3 px down, every 5 ms.
        for i in 1i32..=6i32 {
            h.push(
                t0 + Duration::from_millis(5 * i as u64),
                100.0 + 5.0 * i as f32,
                200.0 - 3.0 * i as f32,
            );
        }
        let (vx, vy) = h.velocity();
        assert!((vx - 1.0).abs() < 1e-4, "vx = {}", vx);
        assert!((vy + 0.6).abs() < 1e-4, "vy = {}", vy);
        assert!((h.speed() - (1.0f32 * 1.0 + 0.6 * 0.6).sqrt()).abs() < 1e-4);
        assert!((h.dt_ms() - 5.0).abs() < 1e-3);
        // 7 live samples fit in the ring, so the oldest live one is the very
        // first push: (30, -18) px from the newest.
        assert!((h.travel() - (30.0f32 * 30.0 + 18.0 * 18.0).sqrt()).abs() < 1e-3);

        // Duplicate timestamps must not divide by zero.
        h.push(t0 + Duration::from_millis(30), 500.0, 500.0);
        assert_eq!(h.velocity(), (0.0, 0.0));
        assert_eq!(h.speed(), 0.0);
        assert_eq!(h.dt_ms(), 0.0);

        // Ring wrap: 20 pushes must not panic and must keep only 8 samples.
        let mut h2 = MotionHistory::new();
        for i in 0..20i32 {
            h2.push(t0 + Duration::from_millis(i as u64), i as f32, 0.0);
        }
        assert_eq!(h2.len() as usize, MOTION_SAMPLES);
        h2.clear();
        assert!(h2.is_empty());
        assert_eq!(h2.velocity(), (0.0, 0.0));
        assert_eq!(h2.travel(), 0.0);
    }

    #[test]
    fn motion_pause_detector_fires_on_deceleration() {
        let cfg = GestureConfig::default();
        let mut p = MotionPause::new();

        // Phase 1: a fast rise. 2.0 px/ms for 100 ms never accumulates pause.
        let mut t = 0.0f32;
        while t < 100.0 {
            let decel = p.observe(2.0);
            assert!(
                !p.is_paused(2.0, decel, 100.0, 10.0, &cfg),
                "paused while fast"
            );
            t += 10.0;
        }
        assert!(p.peak_speed >= 2.0);

        // Phase 2: drop to 0.1 px/ms (below motion_pause_slow) and hold.
        // Must not fire before force_pause_ms of slowness.
        let step = 10.0;
        let mut slow_for = 0.0f32;
        let mut fired_at = -1.0f32;
        while slow_for < 400.0 {
            slow_for += step;
            let decel = p.observe(0.1);
            let fired = p.is_paused(0.1, decel, 100.0, step, &cfg);
            if fired && fired_at < 0.0 {
                fired_at = slow_for;
            }
            if slow_for < cfg.force_pause_ms {
                assert!(!fired, "paused after only {}ms of slowness", slow_for);
            }
        }
        assert!(
            (fired_at - cfg.force_pause_ms).abs() < step,
            "fired at {}ms, want {}ms",
            fired_at,
            cfg.force_pause_ms
        );

        // Phase 3: a fresh detector that is slow but has NOT decelerated
        // (still at its peak speed) must never fire.
        let mut p2 = MotionPause::new();
        p2.observe(0.1);
        t = 0.0;
        while t < 1000.0 {
            let decel = p2.observe(0.1);
            assert!(!p2.is_paused(0.1, decel, 100.0, 10.0, &cfg));
            t += 10.0;
        }

        // Phase 4: a fresh detector that decelerated but never travelled far
        // enough must not fire either.
        let mut p3 = MotionPause::new();
        p3.observe(2.0);
        t = 0.0;
        while t < 1000.0 {
            let decel = p3.observe(0.0);
            assert!(!p3.is_paused(0.0, decel, 1.0, 10.0, &cfg));
            t += 10.0;
        }

        // Phase 5: very slow motion. The trigger is an OR
        // (`slow_ms >= force_pause_ms` OR `very_slow_ms >= harder_trigger_ms`)
        // and a very slow finger is also a slow finger, so with the shipped
        // defaults the *force* trigger wins at 300 ms; `harder_trigger_ms`
        // only dominates when it is the shorter of the two, which is what the
        // next phase checks.
        let mut p4 = MotionPause::new();
        p4.observe(2.0);
        let very = cfg.motion_pause_very_slow;
        let mut held = 0.0f32;
        let mut fired = -1.0f32;
        while held < 500.0 {
            held += 10.0;
            let decel = p4.observe(very);
            if p4.is_paused(very, decel, 100.0, 10.0, &cfg) && fired < 0.0 {
                fired = held;
            }
        }
        assert!(
            (fired - cfg.force_pause_ms).abs() < 10.0,
            "very slow fired at {}ms, want the force trigger at {}ms",
            fired,
            cfg.force_pause_ms
        );

        // Phase 6: a config where the harder trigger is the shorter one.
        // Slow-but-not-very-slow motion must not fire at all; very slow motion
        // must fire exactly on `harder_trigger_ms`.
        let hard = GestureConfig {
            force_pause_ms: 10_000.0,
            harder_trigger_ms: 120.0,
            ..GestureConfig::default()
        };
        let mut p5 = MotionPause::new();
        p5.observe(2.0);
        let mut held = 0.0f32;
        let mut fired_slow = -1.0f32;
        while held < 300.0 {
            held += 10.0;
            let decel = p5.observe(0.1);
            if p5.is_paused(0.1, decel, 100.0, 10.0, &hard) && fired_slow < 0.0 {
                fired_slow = held;
            }
        }
        assert_eq!(
            fired_slow, -1.0,
            "0.1 px/ms is not very slow, the harder trigger must not fire"
        );
        let mut p6 = MotionPause::new();
        p6.observe(2.0);
        let mut held = 0.0f32;
        let mut fired_very = -1.0f32;
        while held < 300.0 {
            held += 10.0;
            let decel = p6.observe(very);
            if p6.is_paused(very, decel, 100.0, 10.0, &hard) && fired_very < 0.0 {
                fired_very = held;
            }
        }
        assert!(
            (fired_very - hard.harder_trigger_ms).abs() < 10.0,
            "very slow fired at {}ms, want the harder trigger at {}ms",
            fired_very,
            hard.harder_trigger_ms
        );
    }

    // -- §5 / §7 gesture behaviour -----------------------------------------

    #[test]
    fn test_fast_out_slow_in_matches_android_curve() {
        assert_eq!(fast_out_slow_in(0.0), 0.0);
        assert_eq!(fast_out_slow_in(1.0), 1.0);
        // cubicTo(0.4, 0.0, 0.2, 1.0) reference point.
        assert!((fast_out_slow_in(0.25) - 0.2366).abs() < 0.005);
        // Monotone, no overshoot, no NaN.
        let mut prev = 0.0f32;
        let mut t = 0.0f32;
        while t <= 1.0 {
            let y = fast_out_slow_in(t);
            assert!(y.is_finite() && (0.0..=1.0).contains(&y));
            assert!(y >= prev);
            prev = y;
            t += 0.05;
        }
    }

    #[test]
    fn test_home_gesture_detection() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();

        // Touch down at bottom nav bar
        let res_down = engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        assert_eq!(res_down, GestureAction::None);

        // Quick move up
        let t1 = t0 + Duration::from_millis(50);
        let res_move = engine.process_touch(&ev(t1, TouchPhase::Move, 2200.0)); // dy = 180px
        match res_move {
            GestureAction::Home { progress, .. } => assert!(progress > 0.0),
            _ => panic!("Expected Home action on swipe up"),
        }

        // Release after stopping -> deliberate stop, commits Home
        let t2 = t0 + Duration::from_millis(100);
        let res_up = engine.process_touch(&ev(t2, TouchPhase::Up, 2200.0));
        match res_up {
            GestureAction::Home { progress, .. } => assert_eq!(progress, 1.0),
            _ => panic!("Expected completed Home gesture"),
        }
    }

    /// A genuine upward flick commits Home; a drag that is still travelling
    /// downwards past the threshold is a cancel.
    #[test]
    fn bottom_release_needs_a_stop_or_an_upward_fling() {
        // Flick: 180 px in 30 ms = 6 px/ms upward.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let up = engine.process_touch(&ev(t0 + Duration::from_millis(30), TouchPhase::Up, 2200.0));
        assert!(
            matches!(up, GestureAction::Home { progress: 1.0, .. }),
            "quick flick must commit Home, got {:?}",
            up
        );

        // Drag past the threshold but travelling downwards at 6 px/ms: cancel.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2000.0));
        let down =
            engine.process_touch(&ev(t0 + Duration::from_millis(30), TouchPhase::Up, 2180.0));
        assert_eq!(down, GestureAction::None);
    }

    #[test]
    fn test_recents_hold_gesture() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();

        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));

        // Fast rise so the gesture has a peak to decelerate from: 30 px every
        // 8 ms = 3.75 px/ms, well past motion_pause_fast. The first frames are
        // still under home_threshold_y, so they are not claimed at all.
        let mut t = t0;
        let mut y = 2380.0f32;
        for i in 0..4 {
            t += Duration::from_millis(8);
            y -= 30.0;
            let a = engine.process_touch(&ev(t, TouchPhase::Move, y));
            if i == 0 {
                // 30 px is still under home_threshold_y (60 px).
                assert_eq!(
                    a,
                    GestureAction::None,
                    "under the threshold nothing is claimed"
                );
            } else {
                assert!(
                    matches!(a, GestureAction::Home { .. }),
                    "fast rise is still a Home drag, got {:?}",
                    a
                );
            }
        }

        // Then creep: 0.5 px every 8 ms = 0.0625 px/ms, below motion_pause_slow,
        // decel ~0.017. Recents must fire once force_pause_ms has accumulated.
        let mut haptics = 0;
        let mut recents_seen = 0;
        for _ in 0..60 {
            t += Duration::from_millis(8);
            y -= 0.5;
            match engine.process_touch(&ev(t, TouchPhase::Move, y)) {
                GestureAction::Recents { trigger_haptic, .. } => {
                    recents_seen += 1;
                    if trigger_haptic {
                        haptics += 1;
                    }
                }
                GestureAction::Home { .. } | GestureAction::None => {}
                other => panic!("unexpected action {:?}", other),
            }
        }
        assert_eq!(haptics, 1, "haptic must pulse exactly once");
        assert!(recents_seen > 0);
    }

    #[test]
    fn recents_requires_a_motion_pause_not_just_a_hold() {
        // 1. A crawl of hundreds of pixels held for 400 ms while still moving
        //    fast is a Home drag, never Recents: the finger has not paused.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let mut t = t0;
        let mut y = 2380.0f32;
        let mut saw_home = false;
        for _ in 0..50 {
            // 12 px every 8 ms = 1.5 px/ms upward: past motion_pause_fast the
            // whole time, so the pause accumulators keep resetting.
            t += Duration::from_millis(8);
            y -= 12.0;
            let a = engine.process_touch(&ev(t, TouchPhase::Move, y));
            match a {
                GestureAction::Home { .. } => saw_home = true,
                GestureAction::None => {}
                other => panic!("fast crawl must stay Home or None, got {:?}", other),
            }
        }
        assert!(saw_home);
        let up = engine.process_touch(&ev(t, TouchPhase::Up, y));
        assert!(
            !matches!(up, GestureAction::Recents { .. }),
            "a fast crawl must never commit Recents, got {:?}",
            up
        );

        // 2. A drag that decelerates to near-zero and stays there must latch
        //    Recents, and the haptic must be a rising edge.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let mut t = t0;
        let mut y = 2380.0f32;
        // Rise at 3.75 px/ms (30 px per 8 ms frame), then a hard brake to
        // 0.375 px/ms, then near-zero. The brake alone is not a pause: it has
        // to be *held*.
        for i in 0..4 {
            t += Duration::from_millis(8);
            y -= 30.0;
            let a = engine.process_touch(&ev(t, TouchPhase::Move, y));
            if i == 0 {
                // 30 px is still under home_threshold_y (60 px).
                assert_eq!(
                    a,
                    GestureAction::None,
                    "under the threshold nothing is claimed"
                );
            } else {
                assert!(
                    matches!(a, GestureAction::Home { .. }),
                    "deceleration in progress is still Home, got {:?}",
                    a
                );
            }
        }
        // Now hold still enough to pause: 0.4 px every 8 ms = 0.05 px/ms.
        let mut haptics = 0;
        let mut recents_seen = 0;
        for _ in 0..80 {
            t += Duration::from_millis(8);
            y -= 0.4;
            match engine.process_touch(&ev(t, TouchPhase::Move, y)) {
                GestureAction::Recents { trigger_haptic, .. } => {
                    if recents_seen == 0 {
                        assert!(trigger_haptic, "the first Recents must pulse the haptic");
                    } else {
                        assert!(!trigger_haptic, "the haptic must be a rising edge only");
                    }
                    recents_seen += 1;
                    if trigger_haptic {
                        haptics += 1;
                    }
                }
                GestureAction::Home { .. } | GestureAction::None => {}
                other => panic!("unexpected action {:?}", other),
            }
        }
        assert_eq!(haptics, 1, "haptic must pulse exactly once");
        assert!(recents_seen > 0, "Recents never fired");
        let up = engine.process_touch(&ev(t, TouchPhase::Up, y));
        assert!(
            matches!(
                up,
                GestureAction::Recents {
                    progress: 1.0,
                    trigger_haptic: false
                }
            ),
            "latched release must commit Recents, got {:?}",
            up
        );
    }

    #[test]
    fn recents_latch_clears_when_the_finger_drops_back() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let mut t = t0;
        let mut y = 2380.0f32;
        for _ in 0..4 {
            t += Duration::from_millis(8);
            y -= 30.0;
            engine.process_touch(&ev(t, TouchPhase::Move, y));
        }
        let mut paused = false;
        for _ in 0..80 {
            t += Duration::from_millis(8);
            y -= 0.4;
            if let GestureAction::Recents { .. } = engine.process_touch(&ev(t, TouchPhase::Move, y))
            {
                paused = true;
            }
        }
        assert!(paused, "expected a Recents pause first");
        // Drop back under the threshold: the latch must release.
        t += Duration::from_millis(16);
        y = 2370.0;
        let a = engine.process_touch(&ev(t, TouchPhase::Move, y));
        assert_eq!(a, GestureAction::None);
        let up = engine.process_touch(&ev(t + Duration::from_millis(8), TouchPhase::Up, y));
        assert_eq!(up, GestureAction::None, "cancelled hold must not commit");
    }

    #[test]
    fn tracking_center_emits_home_on_vertical_drag() {
        let cfg = GestureConfig::default();
        let slop = cfg.gesture_slop();
        assert!((slop - 11.312).abs() < 1e-3, "slop = {}", slop);
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg);
        let t0 = Instant::now();

        // Down in the middle of the panel, then drag up.
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });

        // Under slop: nothing yet.
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1200.0 - slop + 1.0,
            timestamp: t0 + Duration::from_millis(8),
        });
        assert_eq!(
            a,
            GestureAction::None,
            "below slop must not claim the gesture"
        );

        // Past slop: Home, and the progress follows the drag.
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 600.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        match a {
            GestureAction::Home {
                progress,
                scale,
                window_alpha,
            } => {
                let dy = 600.0f32; // 1200 - 600
                let want = (dy / (2400.0 * 0.5)).clamp(0.0, 1.0);
                assert!((progress - want).abs() < 1e-4, "progress = {}", progress);
                assert!((scale - (1.0 - 0.4 * want)).abs() < 1e-4);
                assert!((window_alpha - (1.0 - 0.3 * want)).abs() < 1e-4);
            }
            other => panic!("expected Home on an in-app swipe up, got {:?}", other),
        }
    }

    #[test]
    fn tracking_center_ignores_horizontal() {
        let cfg = GestureConfig::default();
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg);
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        // A long horizontal drag: Home must never appear, but the release must
        // still report the Swipe once past slop.
        for i in 1..=8i32 {
            let a = engine.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Move,
                x: 540.0 + 60.0 * i as f32,
                y: 1200.0,
                timestamp: t0 + Duration::from_millis(8 * i as u64),
            });
            assert!(
                !matches!(a, GestureAction::Home { .. }),
                "horizontal drag must not claim Home, got {:?}",
                a
            );
        }
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0 + 480.0,
            y: 1200.0,
            timestamp: t0 + Duration::from_millis(72),
        });
        assert!(
            matches!(up, GestureAction::Swipe { delta_x, .. } if (delta_x - 480.0).abs() < 1e-3),
            "expected Swipe on release, got {:?}",
            up
        );
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
