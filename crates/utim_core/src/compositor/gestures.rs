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

// ---------------------------------------------------------------------------
// Density reference values (plan §7.3, Task 2.5)
// ---------------------------------------------------------------------------
//
// Every *spatial* threshold below is a dp figure quoted straight out of the
// AOSP resources it came from, and the *speed* ones are the px/s forms of
// those same dp/s figures. The engine consumes all of them as pixels, so a
// config built from raw dp constants is wrong on every panel that is not
// density 1.0: on a 1080x2400 panel (2.5714 px/dp) an 11.3 px gesture slop
// fires on a finger wobble, a 36 px minimum displacement is inside the noise
// floor of a capacitive digitiser, and a 48 px edge zone is 4.4% of the
// width instead of the 11% a `48dp` back gesture is supposed to be.
//
// These are the density-1.0 reference values. `Default` uses them verbatim,
// so the shipped behaviour is unchanged; `GestureConfig::for_density`
// multiplies every length and every speed by the panel density and leaves the
// dimensionless ones alone.

/// `navigation_bar_height` = 48dp.
const DP_BOTTOM_NAV_HEIGHT: f32 = 48.0;
/// Back-gesture inset = 48dp.
const DP_EDGE_ZONE_WIDTH: f32 = 48.0;
/// Status bar height = 48dp.
const DP_TOP_BAR_HEIGHT: f32 = 48.0;
/// `quickstep_home_threshold_y` = 60dp.
const DP_HOME_THRESHOLD_Y: f32 = 60.0;
/// Back-gesture trigger travel = 40dp.
const DP_BACK_THRESHOLD_X: f32 = 40.0;
/// One task-switch step along the nav bar = 80dp.
const DP_SCRUB_THRESHOLD_X: f32 = 80.0;
/// `ViewConfiguration.getScaledTouchSlop()` base = 8dp.
const DP_TOUCH_SLOP: f32 = 8.0;
/// `dimens.xml:151` `quickstep_min_displacement_from_app` = 36dp.
const DP_MIN_DISPLACEMENT: f32 = 36.0;
/// `dimens.xml:152` `quickstep_fling_threshold_speed` = 0.5 px/ms = 500 px/s.
const SPEED_FLING_PX_PER_MS: f32 = 0.5;
const SPEED_FLING_PX_PER_S: f32 = 500.0;
/// `dimens.xml:147` `quickstep_motion_pause_slow`.
const SPEED_MOTION_PAUSE_SLOW: f32 = 0.15;
/// `dimens.xml:146` `quickstep_motion_pause_very_slow`.
const SPEED_MOTION_PAUSE_VERY_SLOW: f32 = 0.0285;
/// `dimens.xml:150` `quickstep_motion_pause_fast`.
const SPEED_MOTION_PAUSE_FAST: f32 = 1.4;

/// `RecentsAnimationDeviceState.java:106`
/// `QUICKSTEP_TOUCH_SLOP_RATIO_GESTURAL`. Dimensionless, so density never
/// touches it.
const GESTURAL_SLOP_RATIO: f32 = 1.414;

/// How much further the horizontal axis must beat the vertical one before a
/// drag along the nav bar is read as a task switch (plan §7.3, Task 2.6).
///
/// This replaces a fixed `|dy| < 30 px` dead zone, which is the wrong shape
/// for the job twice over: it is a raw pixel count, so it silently meant
/// something different on every panel, and it is a *box* rather than a
/// *dominance* rule, so a 45-degree drag that starts almost horizontal still
/// scrubs the task list while the user is plainly reaching for the shade.
const SCRUB_AXIS_DOMINANCE: f32 = 1.5;

/// Fraction of the panel a drag-up has to cover for the release to commit Home
/// even when the finger had already stopped.
///
/// AOSP's `SwipeUpToHomeHandler` compares against a fixed `mUpThresholdY`
/// pixel count, but a fixed pixel count cannot be right on a panel of unknown
/// size, so the commit distance is a fraction of the panel instead. 25% sits
/// below the 50% the MOVE path needs for full progress and far above the
/// gesture slop, so a short nudge cannot reach it and only a deliberate drag
/// or a real fling does.
const HOME_COMMIT_FRACTION: f32 = 0.25;

/// Terminal window pose a committed Home animation lands on: the same
/// scale-down the MOVE path ends at, so the shell runs one curve for a drag
/// release and a fling release alike.
const HOME_COMMIT_SCALE: f32 = 0.6;
const HOME_COMMIT_ALPHA: f32 = 0.7;

/// Gesture Engine Configuration
///
/// Every field here is read by the engine. A constant that only a *future*
/// consumer would read does not belong in this struct: `recents_hold_time`,
/// `overview_min_progress` and `max_swipe_ms` were all removed for exactly
/// that reason -- the first contradicts the motion-pause design outright, and
/// the other two belong to the shell's swipe-to-drawer commit, which does not
/// exist yet. Add them back with the consumer, not before.
///
/// Build this with [`GestureConfig::for_density`], not with a struct literal:
/// the dp figures in the docs below are only meaningful once they have been
/// multiplied by the panel's px/dp.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureConfig {
    /// Panel density in px/dp, as in `DisplayMetrics.density` (density 1.0 =
    /// 160 dpi baseline). Everything spatial or speed-like below is already
    /// multiplied by it, so the shell only has to set this once.
    pub density: f32,
    pub bottom_nav_height: f32, // default 48.0 dp
    pub edge_zone_width: f32,   // default 48.0 dp
    pub top_bar_height: f32,    // default 48.0 dp
    pub home_threshold_y: f32,  // default 60.0 dp
    pub back_threshold_x: f32,  // default 40.0 dp
    pub scrub_threshold_x: f32, // default 80.0 dp

    // --- Quickstep-derived constants (plan §7.3) ---
    /// Fling speed, px/ms. `quickstep/res/values/dimens.xml:152`
    /// `quickstep_fling_threshold_speed`.
    ///
    /// A release faster than this is a fling; slower is a deliberate stop.
    /// Both commit Home, but the branch is explicit so the shell can treat
    /// them differently once it owns the window transform.
    pub fling_threshold: f32,
    /// The same fling speed in px/s, which is the unit
    /// [`GestureAction::Home`]'s commit path works in: velocity there is
    /// `dy / dt` over a release, not a per-millisecond ring delta.
    ///
    /// Kept as its own field rather than a `* 1000.0` on `fling_threshold` so
    /// the two units cannot drift apart when the panel density changes, and
    /// so a shell that already thinks in px/s (a spring, a fling animation)
    /// reads the value it means.
    pub fling_threshold_px: f32,
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
        // Density 1.0: the dp figures are their own pixel values here, which
        // is what every caller that predates `for_density` was written
        // against.
        Self {
            density: 1.0,
            bottom_nav_height: DP_BOTTOM_NAV_HEIGHT,
            edge_zone_width: DP_EDGE_ZONE_WIDTH,
            top_bar_height: DP_TOP_BAR_HEIGHT,
            home_threshold_y: DP_HOME_THRESHOLD_Y,
            back_threshold_x: DP_BACK_THRESHOLD_X,
            scrub_threshold_x: DP_SCRUB_THRESHOLD_X,
            fling_threshold: SPEED_FLING_PX_PER_MS,
            fling_threshold_px: SPEED_FLING_PX_PER_S,
            touch_slop: DP_TOUCH_SLOP,
            touch_slop_ratio: GESTURAL_SLOP_RATIO,
            motion_pause_slow: SPEED_MOTION_PAUSE_SLOW,
            motion_pause_very_slow: SPEED_MOTION_PAUSE_VERY_SLOW,
            motion_pause_fast: SPEED_MOTION_PAUSE_FAST,
            force_pause_ms: 300.0,
            harder_trigger_ms: 400.0,
            rapid_decel_factor: 0.6,
            min_displacement: DP_MIN_DISPLACEMENT,
        }
    }
}

impl GestureConfig {
    /// Thresholds for a panel of `density` px/dp.
    ///
    /// The rule is mechanical and total: every length (px) and every speed
    /// (px/ms, px/s) is multiplied by the density, and every dimensionless
    /// ratio, factor and timeout is left alone. `8dp` becomes 20.6 px at
    /// 2.5714 px/dp, `36dp` becomes 92.6 px, and the gestural slop becomes
    /// 29.1 px -- the point of the whole exercise. Forgetting one field is
    /// exactly the bug this constructor exists to make impossible.
    ///
    /// A non-finite or non-positive density collapses to 1.0 rather than
    /// propagating: a zero gate fires on the first pixel of travel and a NaN
    /// gate never fires at all, and both are far worse than being
    /// unscaled.
    pub fn for_density(density: f32) -> Self {
        let d = if density.is_finite() && density > 0.0 {
            density
        } else {
            1.0
        };
        Self {
            density: d,
            bottom_nav_height: DP_BOTTOM_NAV_HEIGHT * d,
            edge_zone_width: DP_EDGE_ZONE_WIDTH * d,
            top_bar_height: DP_TOP_BAR_HEIGHT * d,
            home_threshold_y: DP_HOME_THRESHOLD_Y * d,
            back_threshold_x: DP_BACK_THRESHOLD_X * d,
            scrub_threshold_x: DP_SCRUB_THRESHOLD_X * d,
            fling_threshold: SPEED_FLING_PX_PER_MS * d,
            fling_threshold_px: SPEED_FLING_PX_PER_S * d,
            touch_slop: DP_TOUCH_SLOP * d,
            min_displacement: DP_MIN_DISPLACEMENT * d,
            motion_pause_slow: SPEED_MOTION_PAUSE_SLOW * d,
            motion_pause_very_slow: SPEED_MOTION_PAUSE_VERY_SLOW * d,
            motion_pause_fast: SPEED_MOTION_PAUSE_FAST * d,
            ..Self::default()
        }
    }

    /// Effective touch slop: Quickstep scales the panel slop up for gestural
    /// navigation (`QUICKSTEP_TOUCH_SLOP_RATIO_GESTURAL`).
    #[inline]
    pub fn gesture_slop(&self) -> f32 {
        self.touch_slop * self.touch_slop_ratio
    }

    /// True when a drag is axis-locked to the horizontal and has travelled far
    /// enough to shift a task along the nav bar.
    ///
    /// Both conditions are needed. The dominance ratio is what makes this a
    /// *gesture* rather than a *box*; the distance is what keeps a single
    /// stray sample from switching apps. See [`SCRUB_AXIS_DOMINANCE`].
    #[inline]
    pub fn is_scrub_dominant(&self, dx: f32, dy: f32) -> bool {
        dx.abs() > dy.abs() * SCRUB_AXIS_DOMINANCE && dx.abs() >= self.scrub_threshold_x
    }

    /// Commit distance for a swipe-up released over the app surface, in px.
    #[inline]
    pub fn home_commit_distance(&self, display_height: f32) -> f32 {
        display_height * HOME_COMMIT_FRACTION
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
                        // The axis is claimed by dominance, so a diagonal that
                        // has started pulling towards the shade is not read as
                        // a task switch (plan §7.3, Task 2.6).
                        if self.config.is_scrub_dominant(dx, dy) {
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
                                    scale: HOME_COMMIT_SCALE,
                                    window_alpha: HOME_COMMIT_ALPHA,
                                }
                            } else {
                                GestureAction::None
                            }
                        } else if self.config.is_scrub_dominant(dx, dy) {
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
                        start_x, start_y, ..
                    } => {
                        // Measure from the *release* point, not from the last
                        // MOVE sample. `current_*` only ever advances on a
                        // MOVE, so a fast flick the digitiser reported as
                        // Down-then-Up with no intermediate frame -- which is
                        // what a real fling looks like -- would otherwise be
                        // measured as zero travel and fall through to `None`.
                        //
                        // Raw screen convention: +y is down, so an upward
                        // swipe is a negative dy and a negative velocity.
                        let dx = event.x - *start_x;
                        let dy = event.y - *start_y;
                        let slop = self.config.gesture_slop();

                        // A hard swipe up over the app surface is a Home
                        // commit, not a leftover page scroll. AOSP's
                        // `SwipeUpToHomeHandler` accepts either branch -- a
                        // fling past `fling_threshold_px`, or travel past
                        // `home_commit_distance` for a finger that had
                        // already stopped -- and so does this, so a slow
                        // deliberate drag and a hard flick both land Home.
                        //
                        // Both branches require the vertical axis to be
                        // claimed first: past the slop, and beating the
                        // horizontal by more than the MOVE path's 1:1 rule.
                        // Without that, a diagonal flick would tear the app
                        // down while the user was swiping sideways, and the
                        // release would commit a Home the MOVE path never
                        // reported.
                        //
                        // The velocity is the ring's instantaneous px/ms
                        // across the last two samples -- the frame pair that
                        // actually contains the release -- rather than an
                        // average over the whole gesture: a slow drag that
                        // ends in a flick is a flick, and averaging would
                        // misread it as a stop.
                        let up = -dy; // px travelled upward
                        let axis_claimed = up > slop && up > dx.abs();
                        let (_, vy) = self.motion.velocity();
                        let v_up = -vy * 1000.0; // px/s, positive upward
                        let flung = v_up > self.config.fling_threshold_px;
                        let dragged = up >= self.config.home_commit_distance(self.display_height);
                        // A NaN from a degenerate coordinate or a repeated
                        // timestamp must never reach the shell as a commit,
                        // so the distance branch is guarded on finiteness
                        // even though every comparison above is already
                        // false for it.
                        if axis_claimed && up.is_finite() && (flung || dragged) {
                            // The same terminal pose the drag path ends at, so
                            // the shell animates a flick and a drag with one
                            // curve.
                            GestureAction::Home {
                                progress: 1.0,
                                scale: HOME_COMMIT_SCALE,
                                window_alpha: HOME_COMMIT_ALPHA,
                            }
                        } else if dx.abs() >= slop || dy.abs() >= slop {
                            // Otherwise this is the app's own scroll, handed
                            // back untouched.
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

// ---------------------------------------------------------------------------
// Double-tap detection
// ---------------------------------------------------------------------------
//
// # Why this exists
//
// `InputDispatcher::finish_touch` (`input.rs:237-266`) returns
// `InputDispatchResult::Tap { x, y }` and
// `InputDispatchResult::LongPress { x, y }` with **no timestamp**, so the
// shell has never had a clock for a tap, let alone a history of them.
// `GestureEngine::process_touch` consumes `RawTouchEvent`, which does carry
// `timestamp: Instant` (`gestures.rs:169-176`) but is only fed drags and
// flings: `finish_touch` routes a movement of 25 px or more to `Touch`, and
// everything short of 400 ms to `Tap` (`input.rs:243-254`). So a double-tap
// is not merely unimplemented -- the data to detect one is discarded at the
// point it is produced.
//
// # What the reference does with one
//
// `WorkspaceTouchListener.onDoubleTap` (`WorkspaceTouchListener.java:261-267`)
// sets a flag, claims the following `UP`/`CANCEL` so the tap underneath does
// not also open an app (`:108-113`), and hands off to
// `GestureController.onDoubleTap()` (`GestureController.kt:48-50`), which runs
// whichever `GestureHandlerConfig` the user picked. The **default is Sleep**:
// `PreferenceManager2.kt:813-816` declares
// `doubleTapGestureHandler` with `defaultValue = GestureHandlerConfig.Sleep`
// (`GestureHandlerConfig.kt:76-78`).
//
// # Why the timestamp is a parameter
//
// Nothing here reads a clock. The same lesson as `ScreenTimeout::poll_budget`
// in `crates/utlc/src/main.rs`: when a timeout reads the clock itself, two
// reads inside one event cannot be guaranteed to agree, and the disagreement
// shows up as a gesture that fires a frame late. Feeding the timestamp in --
// the shell's evdev `EV_TIME` stamp, which `input.rs:199-217`
// (`event_timestamp`) already has -- makes every decision here a function of
// the event stream alone, which is also what makes it testable without a
// sleep.

/// `ViewConfiguration.getDoubleTapTimeout()`: the platform's maximum gap
/// between the two `DOWN`s of a double-tap.
///
/// `PipTouchState.java:40` reads it from the platform and
/// `PipTouchState.java:132-133` tests
/// `(mDownTouchTime - mLastDownTouchTime) < DOUBLE_TAP_TIMEOUT`, i.e. the gap
/// is measured **down to down**, not up to up. UTLC's 300 ms is that platform
/// default, which the reference tree does not override anywhere.
pub const DOUBLE_TAP_TIMEOUT_MS: u64 = 300;

/// `ViewConfiguration.getDoubleTapMinTime()`: the platform's *lower* bound on
/// the same gap, in ms.
///
/// A second `DOWN` faster than this is not read as a second tap at all. The
/// reference tree uses it when synthesising a double-tap for a UI test
/// (`SplitScreenUtils.kt:369-371`) but does not set it, so the same 40 ms
/// platform default applies.
pub const DOUBLE_TAP_MIN_TIME_MS: u64 = 40;

/// The reference's own movement tolerance for a pending touch: **twice** the
/// panel slop, in dp.
///
/// `WorkspaceTouchListener.java:92-94`,
/// `mTouchSlop = 2 * ViewConfiguration.get(launcher).getScaledTouchSlop()`,
/// with the comment "Use twice the touch slop as we are looking for long press
/// which is more likely to cause movement", and applied as a radius at
/// `WorkspaceTouchListener.java:175-177`
/// (`PointF.length(...) > mTouchSlop` cancels). The workspace's own
/// double-tap tolerance comes from `GestureDetector`
/// (`WorkspaceTouchListener.java:95`), whose `getDoubleTapSlop()` is framework
/// code and is **not vendored into this tree** -- so 2x slop is the closest
/// movement radius the reference actually states, and it is the same radius
/// that decides "was this a tap at all" in `input.rs:243`.
pub const DOUBLE_TAP_SLOP_DP: f32 = 2.0 * DP_TOUCH_SLOP;

/// Thresholds for double-tap recognition. All three are read on the touch
/// path, so the struct is `Copy` and a caller builds it once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoubleTapConfig {
    /// Movement tolerance in px: the two taps must land within this radius of
    /// each other. See [`DOUBLE_TAP_SLOP_DP`].
    pub slop: f32,
    /// Maximum `DOWN`-to-`DOWN` gap, in ms. See [`DOUBLE_TAP_TIMEOUT_MS`].
    pub window_ms: u64,
    /// Minimum `DOWN`-to-`DOWN` gap, in ms. See [`DOUBLE_TAP_MIN_TIME_MS`].
    pub min_window_ms: u64,
}

impl DoubleTapConfig {
    /// The platform defaults, on a density-1.0 panel.
    pub const DEFAULT: Self = Self {
        slop: DOUBLE_TAP_SLOP_DP,
        window_ms: DOUBLE_TAP_TIMEOUT_MS,
        min_window_ms: DOUBLE_TAP_MIN_TIME_MS,
    };

    /// Thresholds for a panel of `density` px/dp.
    ///
    /// The slop is the only field that scales: the two windows are wall-clock
    /// and a denser panel does not make people tap faster. A non-finite or
    /// non-positive density collapses to 1.0 for the reason spelled out on
    /// [`GestureConfig::for_density`] -- a zero slop pairs only two identical
    /// points, and a NaN one never pairs at all.
    pub fn for_density(density: f32) -> Self {
        let d = if density.is_finite() && density > 0.0 {
            density
        } else {
            1.0
        };
        Self {
            slop: DOUBLE_TAP_SLOP_DP * d,
            ..Self::DEFAULT
        }
    }
}

impl Default for DoubleTapConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One completed tap: when the finger went down, when it came up, and where.
///
/// Both timestamps are needed, not just the up: the pairing window is
/// `DOWN`-to-`DOWN` (`PipTouchState.java:132-133`) while the time the first
/// tap is still *waiting* runs from its `UP`
/// (`PipTouchState.java:356-358`). Collapsing them into one number is what
/// makes a naive implementation pair a first tap that had already expired.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tap {
    /// `DOWN` on the shell's monotonic millisecond clock.
    pub down_ms: u64,
    /// `UP` on the same clock. Never before [`Tap::down_ms`] as far as this
    /// type is concerned; [`Tap::new`] clamps it.
    pub up_ms: u64,
    /// Release x, in panel px.
    pub x: f32,
    /// Release y, in panel px.
    pub y: f32,
}

impl Tap {
    /// A tap that went down at `down_ms` and came up at `up_ms`.
    ///
    /// An `up_ms` before `down_ms` is clamped rather than trusted: a wrapped
    /// or out-of-order evdev stamp must not become a negative duration that
    /// silently widens every window.
    #[inline]
    pub const fn new(down_ms: u64, up_ms: u64, x: f32, y: f32) -> Self {
        Self {
            down_ms,
            up_ms: if up_ms < down_ms { down_ms } else { up_ms },
            x,
            y,
        }
    }

    /// How long the finger was down, in ms.
    #[inline]
    pub const fn duration_ms(&self) -> u64 {
        self.up_ms - self.down_ms
    }

    /// Centre-to-centre distance to `other`, in px.
    #[inline]
    pub fn distance_to(&self, other: Tap) -> f32 {
        let (dx, dy) = (other.x - self.x, other.y - self.y);
        (dx * dx + dy * dy).sqrt()
    }

    /// `DOWN`-to-`DOWN` gap to `other`, in ms; `0` if the clock went backwards.
    #[inline]
    pub const fn gap_to(&self, other: Tap) -> u64 {
        other.down_ms.saturating_sub(self.down_ms)
    }
}

/// What one touch sequence turned out to be, at the moment it ended.
///
/// This enum is the whole reason the pair-breaking rule is structural. A
/// caller cannot report a drag as anything *other* than [`TouchEnd::Other`],
/// and reporting anything as [`TouchEnd::Other`] unconditionally discards the
/// pending first half. Splitting "tap" and "not a tap" into a sum type means
/// there is no third path on which a drag silently leaves a stale first tap
/// armed -- which is exactly the bug
/// `PipTouchState.java:132` guards against with `!mPreviouslyDragging`
/// (set at `:219`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchEnd {
    /// A tap: short, and within the dispatcher's own movement box
    /// (`input.rs:243-254`).
    Tap {
        /// `DOWN` on the shell's monotonic millisecond clock.
        down_ms: u64,
        /// `UP` on the same clock.
        up_ms: u64,
        /// Release x, in panel px.
        x: f32,
        /// Release y, in panel px.
        y: f32,
    },
    /// Anything else: a drag, a fling, a long press, a cancel. Breaks the pair.
    Other,
}

impl TouchEnd {
    /// The [`Tap`] this sequence produced, if it was a tap.
    #[inline]
    pub const fn as_tap(self) -> Option<Tap> {
        match self {
            TouchEnd::Tap {
                down_ms,
                up_ms,
                x,
                y,
            } => Some(Tap::new(down_ms, up_ms, x, y)),
            TouchEnd::Other => None,
        }
    }
}

/// What [`TapHistory::on_touch_end`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapOutcome {
    /// A tap with nothing to pair with. It is now pending as the first half of
    /// a possible double-tap, and it will be dropped after
    /// [`DoubleTapConfig::window_ms`] minus its own duration.
    FirstTap,
    /// Two taps that met the slop and the window. The pending first half is
    /// consumed, so a third tap starts a fresh pair rather than a triple.
    DoubleTap,
    /// A tap that arrived while a first half was pending but did not pair --
    /// too far away, outside the window, or faster than the minimum. The old
    /// first half is discarded and **this** tap becomes the new pending one,
    /// so a run of misses can never chain two unrelated taps together.
    Unpaired,
    /// The sequence was not a tap, so any pending first half was discarded.
    Interrupted,
}

/// A pure predicate: do these two taps form a double-tap?
///
/// Three conditions, all necessary:
///
/// * the `DOWN`-to-`DOWN` gap is at least [`DoubleTapConfig::min_window_ms`]
///   and strictly below [`DoubleTapConfig::window_ms`] -- the two halves of
///   `PipTouchState.java:132-133`, which has the upper bound only because the
///   platform's own detector applies the lower one;
/// * the releases are within [`DoubleTapConfig::slop`] of each other, the
///   radius of [`DOUBLE_TAP_SLOP_DP`];
/// * the second `DOWN` is not before the first. This is stated separately
///   rather than left to the gap comparison because a backwards gap reads as
///   zero, which passes a `min_window_ms` of zero -- so with the minimum
///   opened up, only this check refuses a wrapped clock.
///
/// Free function on purpose: it holds no state, so it can be pinned against
/// hand-computed cases without driving a state machine.
pub fn forms_double_tap(first: Tap, second: Tap, config: DoubleTapConfig) -> bool {
    if second.down_ms < first.down_ms {
        return false;
    }
    let gap = first.gap_to(second);
    if gap < config.min_window_ms || gap >= config.window_ms {
        return false;
    }
    // `slop` is a distance, so compare squared: one `sqrt` per tap saved. A
    // non-positive *or* non-finite slop pairs nothing, so a misconfigured
    // tolerance can never widen the rule into "always".
    let (dx, dy) = (second.x - first.x, second.y - first.y);
    let slop = config.slop;
    if slop <= 0.0 || !slop.is_finite() {
        return false;
    }
    dx * dx + dy * dy <= slop * slop
}

/// The last tap, and whether it is still waiting for a partner.
///
/// 24 bytes, `Copy`, no allocation, no clock. The one piece of mutable state
/// on the tap path is a single optional [`Tap`].
///
/// Construct with [`TapHistory::new`]; a plain `Default` is
/// [`DoubleTapConfig::DEFAULT`] and a density-scaled panel wants
/// [`TapHistory::with_config`] fed [`DoubleTapConfig::for_density`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapHistory {
    /// The first half of a possible pair, or `None`.
    pending: Option<Tap>,
    config: DoubleTapConfig,
}

impl TapHistory {
    /// An empty history with the given thresholds.
    #[inline]
    pub const fn new(config: DoubleTapConfig) -> Self {
        Self {
            pending: None,
            config,
        }
    }

    /// The thresholds in force.
    #[inline]
    pub const fn config(&self) -> DoubleTapConfig {
        self.config
    }

    /// The pending first half, if one is armed. Does **not** check expiry --
    /// use [`TapHistory::pending_within`] for that, or
    /// [`TapHistory::on_touch_end`], which expires before it pairs.
    #[inline]
    pub const fn pending(&self) -> Option<Tap> {
        self.pending
    }

    /// Forget any pending first half. A full reset.
    #[inline]
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Milliseconds the pending first half is still good for, from `now_ms`.
    ///
    /// This is the timeout the shell needs so a first tap does not linger
    /// forever: it is the window minus the first tap's own duration minus
    /// however long it has already been waiting, floored at zero. The
    /// reference computes the same quantity as
    /// `PipTouchState.getDoubleTapTimeoutCallbackDelay` (`:355-360`),
    /// `Math.max(0, DOUBLE_TAP_TIMEOUT - (mUpTouchTime - mDownTouchTime))`,
    /// which is where the "minus the tap's own duration" comes from. Returns
    /// `0` when nothing is pending or the budget is spent, so a shell can
    /// poll it without a branch of its own.
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        let Some(first) = self.pending else {
            return 0;
        };
        // All three `saturating_sub`s floor at zero, and a `u64` cannot go
        // negative, so a `now_ms` behind the tap's own `UP` -- or a tap longer
        // than the whole window -- reads as "spent" rather than wrapping to an
        // enormous budget.
        self.config
            .window_ms
            .saturating_sub(first.duration_ms())
            .saturating_sub(now_ms.saturating_sub(first.up_ms))
    }

    /// Whether a first half is pending and still inside its budget at
    /// `now_ms`.
    #[inline]
    pub fn is_armed(&self, now_ms: u64) -> bool {
        self.pending.is_some() && self.remaining_ms(now_ms) > 0
    }

    /// The pending first half, but only if `now_ms` is still inside its
    /// budget. This is the reading a shell wants for a "waiting for the
    /// second tap" affordance.
    pub fn pending_within(&self, now_ms: u64) -> Option<Tap> {
        self.is_armed(now_ms).then_some(self.pending)?
    }

    /// The one entry point: report how a touch sequence ended.
    ///
    /// This is the function the shell calls from wherever it currently turns
    /// an [`InputDispatchResult`] into a row, and it is deliberately the only
    /// way to change the state -- there is no `push_tap` that a drag could
    /// skip past, so the pair-breaking rule cannot be forgotten.
    pub fn on_touch_end(&mut self, end: TouchEnd) -> TapOutcome {
        let Some(tap) = end.as_tap() else {
            // `WorkspaceTouchListener.java:102-104` clears the flag on every
            // `DOWN`; `PipTouchState.java:132` refuses to pair when the
            // previous sequence dragged (`:219`).
            self.pending = None;
            return TapOutcome::Interrupted;
        };
        // An expired first half is not a first half. Expiring here rather
        // than trusting the caller is what makes `remaining_ms` advisory
        // rather than load-bearing.
        let paired = self
            .pending
            .filter(|first| self.remaining_ms(first.up_ms) > 0)
            .is_some_and(|first| forms_double_tap(first, tap, self.config));
        if paired {
            self.pending = None;
            TapOutcome::DoubleTap
        } else if self.pending.is_some() {
            // Keep only the newer tap: a chain of misses must not be able to
            // pair tap 1 with tap 3.
            self.pending = Some(tap);
            TapOutcome::Unpaired
        } else {
            self.pending = Some(tap);
            TapOutcome::FirstTap
        }
    }

    /// Convenience for a shell that has a timestamp in hand and wants the
    /// boolean directly. Same semantics as [`TapHistory::on_touch_end`].
    #[inline]
    pub fn tap(&mut self, tap: Tap) -> bool {
        self.on_touch_end(TouchEnd::Tap {
            down_ms: tap.down_ms,
            up_ms: tap.up_ms,
            x: tap.x,
            y: tap.y,
        }) == TapOutcome::DoubleTap
    }
}

impl Default for TapHistory {
    fn default() -> Self {
        Self::new(DoubleTapConfig::DEFAULT)
    }
}

/// What a double-tap runs.
///
/// The ten `GestureHandlerConfig.Simple` variants, which are the ten fixed
/// entries a settings dropdown can hold
/// (`GestureHandlerConfig.kt:74-130`). `GestureHandlerConfig.OpenApp`
/// (`GestureHandlerConfig.kt:133-184`) is deliberately absent: it is a
/// `data class` carrying an `OpenAppTarget`, so it is "open the app the user
/// picked" rather than a fixed menu entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum DoubleTapAction {
    /// `GestureHandlerConfig.NoOp` (`GestureHandlerConfig.kt:72-74`).
    NoOp = 0,
    /// `GestureHandlerConfig.Sleep` (`:76-78`). **The reference default**:
    /// `PreferenceManager2.kt:813-816`.
    Sleep = 1,
    /// `GestureHandlerConfig.Recents` (`:80-84`).
    Recents = 2,
    /// `GestureHandlerConfig.OpenNotifications` (`:86-91`).
    OpenNotifications = 3,
    /// `GestureHandlerConfig.OpenQuickSettings` (`:93-98`).
    OpenQuickSettings = 4,
    /// `GestureHandlerConfig.OpenAppDrawer` (`:100-107`).
    OpenAppDrawer = 5,
    /// `GestureHandlerConfig.OpenAppSearch` (`:109-116`).
    OpenAppSearch = 6,
    /// `GestureHandlerConfig.OpenSearch` (`:118-123`).
    OpenSearch = 7,
    /// `GestureHandlerConfig.OpenAssistant` (`:125-130`).
    OpenAssistant = 8,
}

/// `PreferenceManager2.kt:815`: `defaultValue = GestureHandlerConfig.Sleep`.
pub const DOUBLE_TAP_DEFAULT: DoubleTapAction = DoubleTapAction::Sleep;

impl DoubleTapAction {
    /// Every action, in `GestureHandlerConfig.kt`'s declaration order.
    pub const ALL: [DoubleTapAction; 9] = [
        DoubleTapAction::NoOp,
        DoubleTapAction::Sleep,
        DoubleTapAction::Recents,
        DoubleTapAction::OpenNotifications,
        DoubleTapAction::OpenQuickSettings,
        DoubleTapAction::OpenAppDrawer,
        DoubleTapAction::OpenAppSearch,
        DoubleTapAction::OpenSearch,
        DoubleTapAction::OpenAssistant,
    ];

    /// The persisted key, verbatim from the reference's `@SerialName`.
    pub const fn id(self) -> &'static str {
        match self {
            DoubleTapAction::NoOp => "noOp",
            DoubleTapAction::Sleep => "sleep",
            DoubleTapAction::Recents => "recents",
            DoubleTapAction::OpenNotifications => "openNotificationdata",
            DoubleTapAction::OpenQuickSettings => "openQuickSettings",
            DoubleTapAction::OpenAppDrawer => "openAppDrawer",
            DoubleTapAction::OpenAppSearch => "openAppSearch",
            DoubleTapAction::OpenSearch => "openSearch",
            DoubleTapAction::OpenAssistant => "openAssistant",
        }
    }

    /// The settings-row label, from the `@StringRes` each variant names
    /// (`GestureHandlerConfig.kt:74-130`). ASCII only: this tree has no font
    /// package, so a non-ASCII label renders as tofu.
    pub const fn label(self) -> &'static str {
        match self {
            DoubleTapAction::NoOp => "No action",
            DoubleTapAction::Sleep => "Sleep",
            DoubleTapAction::Recents => "Recents",
            DoubleTapAction::OpenNotifications => "Open notifications",
            DoubleTapAction::OpenQuickSettings => "Open quick settings",
            DoubleTapAction::OpenAppDrawer => "Open app drawer",
            DoubleTapAction::OpenAppSearch => "Open app search",
            DoubleTapAction::OpenSearch => "Open search",
            DoubleTapAction::OpenAssistant => "Open assistant",
        }
    }

    /// The action whose [`DoubleTapAction::id`] is `s`, if any.
    pub fn from_id(s: &str) -> Option<DoubleTapAction> {
        DoubleTapAction::ALL.into_iter().find(|a| a.id() == s)
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
        // This is the *nav bar* version of that: rise 80 px (past
        // `home_threshold_y`, so the MOVE path claims Home), fall back to 60 px,
        // and lift while still crawling downwards. Nothing may commit.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let claimed = engine.process_touch(&ev(
            t0 + Duration::from_millis(10),
            TouchPhase::Move,
            2300.0,
        ));
        assert!(
            matches!(claimed, GestureAction::Home { progress, .. } if progress > 0.0),
            "the rise must have claimed Home first, got {:?}",
            claimed
        );
        engine.process_touch(&ev(
            t0 + Duration::from_millis(20),
            TouchPhase::Move,
            2310.0,
        ));
        let down =
            engine.process_touch(&ev(t0 + Duration::from_millis(30), TouchPhase::Up, 2320.0));
        assert_eq!(
            down,
            GestureAction::None,
            "a nav-bar drag released while falling back is a cancel, got {:?}",
            down
        );

        // The same "downwards must not exit" rule over the app surface. This
        // case is a *centre* gesture, not a nav-bar one: y = 2000 is well
        // above the 2352 px nav bar, so it is measured by `TrackingCenter`.
        // It used to assert `None`, and that only held because the release
        // arm measured travel from the last MOVE sample -- a Down/Up pair
        // with no MOVE between them reported zero travel for a real 180 px
        // drag. Measuring from the release point (Task 2.1.3) turns that
        // phantom cancel into the real thing: the app's own scroll, handed
        // back with its true displacement and, crucially, not a Home.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2000.0));
        let down =
            engine.process_touch(&ev(t0 + Duration::from_millis(30), TouchPhase::Up, 2180.0));
        assert!(
            !matches!(down, GestureAction::Home { .. }),
            "a 6 px/ms downward drag must never commit Home, got {:?}",
            down
        );
        assert!(
            matches!(down, GestureAction::Swipe { delta_x, delta_y }
                if delta_x.abs() < 1e-3 && (delta_y - 180.0).abs() < 1e-3),
            "a downward drag is the app scrolling back: got {:?}",
            down
        );
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

    /// 1080 px / 420 dp is the panel every other test in this file uses, and
    /// 2.5714 px/dp is what its dp thresholds have to be scaled by.
    const DENSITY_1080P: f32 = 1080.0 / 420.0;

    #[test]
    fn tracking_center_emits_home_on_vertical_drag() {
        let cfg = GestureConfig::default();
        let slop = cfg.gesture_slop();
        // Density 1.0: 8dp * 1.414.
        assert!((slop - 11.312).abs() < 1e-3, "slop = {}", slop);
        // The same 8dp slop on the panel this file actually drives, which is
        // 2.5714 px/dp: 8 * 2.5714 = 20.57 px, * 1.414 = 29.09 px. Asserting
        // this here is the point of `for_density` -- an engine shipped on
        // 1080p with the unscaled 11.3 px slop fires on a finger wobble.
        let dslop = GestureConfig::for_density(DENSITY_1080P).gesture_slop();
        assert!(
            (dslop - 29.088).abs() < 5e-3,
            "density-scaled slop = {}",
            dslop
        );
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

    // -- Task 2.5: density-scaled thresholds ---------------------------------

    /// Every threshold is a dp (or dp/s) figure that the engine reads as
    /// pixels. `for_density` has to scale *all* of them, not just the two
    /// that happened to be named, or the engine is half-correct on a 1080p
    /// panel: a 2.57x-correct gesture slop next to a still-48px edge zone and
    /// a still-60px home threshold.
    #[test]
    fn for_density_scales_every_px_denominated_threshold() {
        let d = DENSITY_1080P; // 2.5714 px/dp
        let base = GestureConfig::default();
        let cfg = GestureConfig::for_density(d);

        assert!((cfg.density - d).abs() < 1e-6, "density = {}", cfg.density);

        // The two the plan calls out by name.
        assert!(
            (cfg.touch_slop - 20.5714).abs() < 5e-3,
            "{}",
            cfg.touch_slop
        );
        assert!(
            (cfg.min_displacement - 92.5714).abs() < 5e-3,
            "{}",
            cfg.min_displacement
        );
        assert!(
            (cfg.fling_threshold_px - 1285.714).abs() < 1e-2,
            "{}",
            cfg.fling_threshold_px
        );
        assert!((cfg.touch_slop_ratio - 1.414).abs() < 1e-6);

        // Every other length the engine consumes: the nav bar, the back edge,
        // the status bar, the home drag, the back drag and the scrub step are
        // all dp in the AOSP resources, so all of them are wrong unscaled.
        for (got, want_dp, name) in [
            (
                cfg.bottom_nav_height,
                DP_BOTTOM_NAV_HEIGHT,
                "bottom_nav_height",
            ),
            (cfg.edge_zone_width, DP_EDGE_ZONE_WIDTH, "edge_zone_width"),
            (cfg.top_bar_height, DP_TOP_BAR_HEIGHT, "top_bar_height"),
            (
                cfg.home_threshold_y,
                DP_HOME_THRESHOLD_Y,
                "home_threshold_y",
            ),
            (
                cfg.back_threshold_x,
                DP_BACK_THRESHOLD_X,
                "back_threshold_x",
            ),
            (
                cfg.scrub_threshold_x,
                DP_SCRUB_THRESHOLD_X,
                "scrub_threshold_x",
            ),
        ] {
            assert!(
                (got - want_dp * d).abs() < 5e-3,
                "{} = {} want {}",
                name,
                got,
                want_dp * d
            );
        }
        // And the speeds, which are px/s in disguise: a physical gesture maps
        // to proportionally more pixels on a denser panel, so the threshold
        // has to move with it or every fling reads as a stop.
        assert!(
            (cfg.fling_threshold - SPEED_FLING_PX_PER_MS * d).abs() < 5e-4,
            "{}",
            cfg.fling_threshold
        );
        assert!(
            (cfg.motion_pause_slow - SPEED_MOTION_PAUSE_SLOW * d).abs() < 5e-4,
            "{}",
            cfg.motion_pause_slow
        );
        assert!((cfg.motion_pause_very_slow - SPEED_MOTION_PAUSE_VERY_SLOW * d).abs() < 5e-4);
        assert!((cfg.motion_pause_fast - SPEED_MOTION_PAUSE_FAST * d).abs() < 5e-4);

        // The two speed fields must not drift: same threshold, two units.
        assert!(
            (cfg.fling_threshold_px - cfg.fling_threshold * 1000.0).abs() < 1e-2,
            "px/s and px/ms disagree: {} vs {}",
            cfg.fling_threshold_px,
            cfg.fling_threshold
        );

        // Dimensionless inputs are untouched.
        assert_eq!(cfg.force_pause_ms, base.force_pause_ms);
        assert_eq!(cfg.harder_trigger_ms, base.harder_trigger_ms);
        assert_eq!(cfg.rapid_decel_factor, base.rapid_decel_factor);
        assert_eq!(cfg.touch_slop_ratio, base.touch_slop_ratio);

        // `Default` is density 1.0, so every caller written before
        // `for_density` existed keeps byte-identical thresholds.
        assert_eq!(base, GestureConfig::for_density(1.0));
        assert!((base.gesture_slop() - 11.312).abs() < 1e-3);

        // A degenerate density must not produce a config where every gate is
        // either always-true (0 px) or never-true (NaN).
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let c = GestureConfig::for_density(bad);
            assert_eq!(c.density, 1.0, "bad density {:?} must collapse", bad);
            assert_eq!(c, base, "bad density {:?} must not change a gate", bad);
        }
    }

    /// A finger that wobbles inside the slop must not commit anything. On
    /// 1080p the unscaled slop was 11.3 px, well inside the noise floor of a
    /// capacitive panel; scaled, it is 20.6 px, and a 19 px wobble is
    /// nothing at all.
    #[test]
    fn a_sub_slop_wobble_does_not_commit_a_gesture() {
        let cfg = GestureConfig::for_density(DENSITY_1080P);
        let slop = cfg.gesture_slop();
        assert!((slop - 20.5714 * 1.414).abs() < 5e-3, "slop = {}", slop);

        // 1. Over the app surface: 19 px of noise in both axes, in every
        //    direction, and nothing is claimed at any point.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let mut t = t0;
        for i in 0..12i32 {
            t += Duration::from_millis(8);
            let swing = if i % 2 == 0 { 19.0 } else { -19.0 };
            let a = engine.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Move,
                x: 540.0 + swing,
                y: 1200.0 - swing,
                timestamp: t,
            });
            assert_eq!(a, GestureAction::None, "frame {} claimed {:?}", i, a);
        }
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1200.0,
            timestamp: t + Duration::from_millis(8),
        });
        assert_eq!(up, GestureAction::None, "a wobble is a tap, got {:?}", up);

        // 2. From the nav bar, where the same wobble would otherwise be
        //    measured against home_threshold_y (60dp = 154 px scaled).
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        let t0 = Instant::now();
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let mut t = t0;
        let mut y = 2380.0f32;
        for i in 0..12i32 {
            t += Duration::from_millis(8);
            y += if i % 2 == 0 { -19.0 } else { 19.0 };
            let a = engine.process_touch(&ev(t, TouchPhase::Move, y));
            assert!(
                matches!(a, GestureAction::None),
                "a 19 px wobble must not claim the nav-bar gesture, got {:?}",
                a
            );
        }
        assert_eq!(
            engine.process_touch(&ev(t, TouchPhase::Up, y)),
            GestureAction::None
        );

        // 3. The control: the slop is a live gate, not an inert number. The
        //    same 19 px would have been over half the *unscaled* 11.3 px slop,
        //    and a genuinely vertical 40 px drag is a gesture again.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1200.0 - 40.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            matches!(a, GestureAction::Home { progress, .. } if progress > 0.0),
            "a 40 px vertical drag is past the 20.6 px slop and must claim Home, got {:?}",
            a
        );

        // 4. And just under the scaled slop the MOVE path stays silent, so
        //    the slop boundary itself is pinned from both sides.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg);
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1200.0 - slop + 1.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert_eq!(
            a,
            GestureAction::None,
            "one px under the slop claims nothing"
        );
    }

    // -- Task 2.1.3: swipe-up-to-home on release ----------------------------

    /// A hard upward flick over the app surface must commit Home on release.
    /// Before this, the `TrackingCenter` release arm had exactly one exit --
    /// `Swipe` -- so a swipe the MOVE path never got to sample (or one the
    /// shell had already been morphing) resolved to a page scroll and the
    /// app never closed.
    #[test]
    fn center_release_commits_home_on_an_upward_fling() {
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        // 60 px per 8 ms frame = 7.5 px/ms = 7500 px/s upward, which is 15x
        // `fling_threshold_px` (500 px/s at density 1.0).
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1140.0,
            timestamp: t0 + Duration::from_millis(8),
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1080.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        match up {
            GestureAction::Home {
                progress,
                scale,
                window_alpha,
            } => {
                assert_eq!(progress, 1.0, "a commit is a commit, not a partial");
                // The same terminal pose the drag path lands on, so the shell
                // runs one curve for a flick and a drag alike.
                assert_eq!(scale, HOME_COMMIT_SCALE);
                assert_eq!(window_alpha, HOME_COMMIT_ALPHA);
            }
            other => panic!("an upward fling must commit Home, got {:?}", other),
        }

        // The same fling reported as a single Down/Up pair -- no MOVE frame at
        // all -- must also commit. This is the case the old
        // `current_x/current_y` measurement silently dropped on the floor,
        // because those only advance on a MOVE.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1080.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            matches!(up, GestureAction::Home { progress: 1.0, .. }),
            "a frame-less fling must still commit, got {:?}",
            up
        );

        // And the distance branch: a slow drag past a quarter of the panel
        // commits too, so a deliberate drag is not penalised for lacking a
        // flick at the end. 700 px of 2400 is 29% (the gate is 25% = 600 px),
        // taken over 2 s so the release speed is 350 px/s -- under the 500 px/s
        // fling gate, which is what makes this the *distance* branch rather
        // than the fling branch arriving again by the back door.
        let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 500.0,
            timestamp: t0 + Duration::from_millis(2000),
        });
        assert!(
            matches!(up, GestureAction::Home { progress: 1.0, .. }),
            "a long slow drag past a quarter of the panel must commit, got {:?}",
            up
        );
    }

    /// The negative cases for the same gate. A nudge is not a gesture, and a
    /// downward drag is the app scrolling back, never an exit.
    #[test]
    fn center_release_only_commits_home_for_a_real_upward_throw() {
        let cfg = GestureConfig::default();
        let slop = cfg.gesture_slop();
        let t0 = Instant::now();

        // 1. A small upward nudge, slow. 40 px over 400 ms = 0.1 px/ms = 100
        //    px/s, an order of magnitude under the 500 px/s fling gate, and
        //    40 px is nowhere near the 600 px distance gate. The app's own
        //    scroll gets it back verbatim.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1160.0,
            timestamp: t0 + Duration::from_millis(50),
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1160.0,
            timestamp: t0 + Duration::from_millis(400),
        });
        match up {
            GestureAction::Swipe { delta_x, delta_y } => {
                assert!(delta_x.abs() < 1e-3, "delta_x = {}", delta_x);
                assert!((delta_y - (-40.0)).abs() < 1e-3, "delta_y = {}", delta_y);
            }
            other => panic!("a nudge must stay a Swipe, got {:?}", other),
        }

        // 2. A downward drag: positive dy, no Home under any circumstances.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1300.0,
            timestamp: t0 + Duration::from_millis(8),
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1400.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        match up {
            GestureAction::Swipe { delta_x, delta_y } => {
                assert!(delta_x.abs() < 1e-3, "delta_x = {}", delta_x);
                assert!(
                    (delta_y - 200.0).abs() < 1e-3,
                    "a downward drag has positive dy, got {}",
                    delta_y
                );
            }
            other => panic!("a downward drag must be a Swipe, got {:?}", other),
        }

        // 3. Fast, but sideways. A horizontal flick is the pager's, and the
        //    axis must be claimed before the release can tear the app down
        //    even though the velocity is far past the fling gate.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 300.0,
            y: 1200.0,
            timestamp: t0,
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 900.0,
            y: 1180.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            !matches!(up, GestureAction::Home { .. }),
            "a horizontal flick must not commit Home, got {:?}",
            up
        );

        // 4. A tap: no travel at all, so there is nothing to hand back and
        //    nothing to commit.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let up = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 540.0,
            y: 1200.0,
            timestamp: t0 + Duration::from_millis(120),
        });
        assert_eq!(
            up,
            GestureAction::None,
            "a tap is not a gesture, got {:?}",
            up
        );

        // 5. The distance gate is exactly a quarter of the panel, pinned from
        //    both sides. 601 px is just over the 600 px gate; 599 px is just
        //    under. Both are taken over 3 s (~200 px/s) so the fling branch is
        //    definitively closed and the distance branch is the only thing
        //    that can decide.
        for (travel, want_home) in [(601.0f32, true), (599.0, false)] {
            let mut e = GestureEngine::new(1080.0, 2400.0, cfg.clone());
            e.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Down,
                x: 540.0,
                y: 1200.0,
                timestamp: t0,
            });
            let a = e.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Up,
                x: 540.0,
                y: 1200.0 - travel,
                timestamp: t0 + Duration::from_millis(3000),
            });
            assert_eq!(
                matches!(a, GestureAction::Home { .. }),
                want_home,
                "{} px of a 2400 px panel: {:?}",
                travel,
                a
            );
        }

        // ...and the gate follows the panel rather than a hard-coded pixel
        // count: the same 300 px is 12.5% of a 2400 px panel (no commit) but
        // 27.8% of a 1080 px one (commit).
        for (height, want_home) in [(2400.0f32, false), (1080.0, true)] {
            let mut e = GestureEngine::new(540.0, height, cfg.clone());
            e.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Down,
                x: 270.0,
                y: 540.0,
                timestamp: t0,
            });
            let a = e.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Up,
                x: 270.0,
                y: 240.0,
                timestamp: t0 + Duration::from_millis(3000),
            });
            assert_eq!(
                matches!(a, GestureAction::Home { .. }),
                want_home,
                "300 px of a {} px panel: {:?}",
                height,
                a
            );
        }

        // 6. The MOVE path's slop rule is unchanged by any of this: a drag
        //    inside the slop still reports nothing.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg);
        let t0 = Instant::now();
        engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Down,
            x: 540.0,
            y: 1200.0,
            timestamp: t0,
        });
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 1200.0 - slop + 1.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert_eq!(a, GestureAction::None);
    }

    // -- Task 2.6: scrub axis dominance --------------------------------------

    /// The nav-bar scrub used to be gated on a fixed `|dy| < 30 px` box,
    /// which is a dead zone in the wrong units and the wrong shape. The
    /// horizontal axis has to *dominate* instead.
    #[test]
    fn bottom_scrub_needs_a_horizontally_dominant_drag() {
        let cfg = GestureConfig::default();
        let t0 = Instant::now();

        // 1. Horizontal-dominant: 120 px right against 40 px up is a 3:1
        //    ratio, so it is unambiguously a task switch.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 660.0,
            y: 2340.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            matches!(
                a,
                GestureAction::BottomBarScrub {
                    delta_x,
                    app_shift: 1
                } if (delta_x - 120.0).abs() < 1e-3
            ),
            "a 3:1 horizontal drag must scrub, got {:?}",
            a
        );

        // ...and back the other way.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 420.0,
            y: 2340.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            matches!(a, GestureAction::BottomBarScrub { app_shift: -1, .. }),
            "a leftward scrub must shift back, got {:?}",
            a
        );

        // 2. Vertical-dominant: 100 px right against 80 px up is only 1.25:1,
        //    which is not a clear win for the horizontal axis. The old dead
        //    zone let this through (|dy| = 80 was over its 30 px box) and
        //    switched apps while the user was reaching for the shade. Now it
        //    belongs to the Home drag instead.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 640.0,
            y: 2300.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            !matches!(a, GestureAction::BottomBarScrub { .. }),
            "a 1.25:1 drag is not a scrub, got {:?}",
            a
        );
        assert!(
            matches!(a, GestureAction::Home { progress, .. } if progress > 0.0),
            "it is a Home drag instead, got {:?}",
            a
        );

        // 3. A purely vertical drag is never a scrub, on either path.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 540.0,
            y: 2300.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            !matches!(a, GestureAction::BottomBarScrub { .. }),
            "a vertical drag must not scrub, got {:?}",
            a
        );

        // 4. The dominance ratio is 1.5, pinned from both sides. dx = 120
        //    against dy = 79 is 1.519:1 and scrubs; against dy = 81 it is
        //    1.481:1 and does not. (Both are under the old 30 px dead zone
        //    only for the second, which is exactly the asymmetry the
        //    dominance rule removes.)
        for (dy, want_scrub) in [(79.0f32, true), (81.0, false)] {
            let mut e = GestureEngine::new(1080.0, 2400.0, cfg.clone());
            e.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
            let a = e.process_touch(&RawTouchEvent {
                touch_id: 1,
                phase: TouchPhase::Move,
                x: 660.0,
                y: 2380.0 - dy,
                timestamp: t0 + Duration::from_millis(16),
            });
            assert_eq!(
                matches!(a, GestureAction::BottomBarScrub { .. }),
                want_scrub,
                "dx 120 vs dy {} must be a scrub = {}, got {:?}",
                dy,
                want_scrub,
                a
            );
        }

        // 5. Dominance is not enough on its own: the drag still has to travel
        //    a full step (80 px by default). 79 px right is short of it, so
        //    this is not a scrub even though 79/0 is infinitely dominant.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Move,
            x: 619.0,
            y: 2380.0,
            timestamp: t0 + Duration::from_millis(16),
        });
        assert!(
            !matches!(a, GestureAction::BottomBarScrub { .. }),
            "a sub-threshold drag must not scrub, got {:?}",
            a
        );

        // 6. The release path shares the same rule: a vertical-dominant
        //    release commits no step, a horizontal-dominant one does.
        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 640.0,
            y: 2300.0,
            timestamp: t0 + Duration::from_millis(120),
        });
        assert!(
            !matches!(a, GestureAction::BottomBarScrub { .. }),
            "a vertical-dominant release must not scrub, got {:?}",
            a
        );

        let mut engine = GestureEngine::new(1080.0, 2400.0, cfg);
        engine.process_touch(&ev(t0, TouchPhase::Down, 2380.0));
        let a = engine.process_touch(&RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 700.0,
            y: 2360.0,
            timestamp: t0 + Duration::from_millis(120),
        });
        assert!(
            matches!(
                a,
                GestureAction::BottomBarScrub {
                    delta_x,
                    app_shift: 1
                } if (delta_x - 160.0).abs() < 1e-3
            ),
            "a horizontal-dominant release commits the step, got {:?}",
            a
        );
    }

    /// The dominance rule is a property of the config, so it is unit-tested
    /// away from the state machine too: the sign of the drag must not matter,
    /// and a NaN must never come out dominant.
    #[test]
    fn scrub_dominance_is_sign_symmetric_and_nan_safe() {
        let cfg = GestureConfig::default();
        let t = cfg.scrub_threshold_x;
        assert!(cfg.is_scrub_dominant(t, 0.0));
        assert!(cfg.is_scrub_dominant(-t, 0.0));
        assert!(cfg.is_scrub_dominant(t, -t / SCRUB_AXIS_DOMINANCE * 0.99));
        assert!(!cfg.is_scrub_dominant(t, -t / SCRUB_AXIS_DOMINANCE * 1.01));
        assert!(!cfg.is_scrub_dominant(0.0, 0.0));
        assert!(!cfg.is_scrub_dominant(t * 0.5, 0.0), "short of one step");
        // Every comparison with NaN is false, so a degenerate sample cannot
        // fire a task switch.
        assert!(!cfg.is_scrub_dominant(f32::NAN, 0.0));
        assert!(!cfg.is_scrub_dominant(t, f32::NAN));
        assert!(!cfg.is_scrub_dominant(f32::INFINITY, f32::INFINITY));

        // A density-scaled config scales the step but not the ratio, so the
        // same physical drag scrubs on any panel.
        let d = GestureConfig::for_density(DENSITY_1080P);
        assert!(d.is_scrub_dominant(d.scrub_threshold_x, 0.0));
        assert!(!d.is_scrub_dominant(d.scrub_threshold_x * 0.99, 0.0));
    }

    // -- double-tap detection ---------------------------------------------
    //
    // Section note: every case below is a hand-computed (gap, distance) pair,
    // not a value read back out of the implementation. The thresholds are the
    // platform's, stated at the top of the section, so the bounds are
    // arithmetic on those constants rather than on anything the code produced.

    /// A tap that went down and up inside `dur_ms`, released at `(x, y)`.
    fn tap(down_ms: u64, dur_ms: u64, x: f32, y: f32) -> Tap {
        Tap::new(down_ms, down_ms + dur_ms, x, y)
    }

    /// The four numbers the recognition rule is stated in: the window's lower
    /// and upper bound, and the slop.
    #[test]
    fn double_tap_thresholds_are_the_platform_defaults() {
        let c = DoubleTapConfig::DEFAULT;
        assert_eq!(c.window_ms, 300, "ViewConfiguration.getDoubleTapTimeout");
        assert_eq!(c.min_window_ms, 40, "ViewConfiguration.getDoubleTapMinTime");
        // `WorkspaceTouchListener.java:94`: twice the panel slop, 8 dp each.
        assert_eq!(c.slop, 16.0);
        assert_eq!(c.slop, 2.0 * DP_TOUCH_SLOP);
        // Density is the only thing that scales, and it scales the slop alone:
        // a denser panel does not make people tap faster.
        let d = DoubleTapConfig::for_density(DENSITY_1080P);
        assert!((d.slop - 16.0 * DENSITY_1080P).abs() < 1.0e-4);
        assert_eq!(d.window_ms, c.window_ms);
        assert_eq!(d.min_window_ms, c.min_window_ms);
        // A degenerate density collapses to 1.0 rather than to a zero slop,
        // which would pair only two identical points.
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let b = DoubleTapConfig::for_density(bad);
            assert_eq!(b.slop, c.slop, "density {bad}");
            assert_eq!(b.window_ms, c.window_ms);
        }
    }

    /// The happy path, at both ends of the window. The gap is measured
    /// `DOWN`-to-`DOWN` (`PipTouchState.java:132-133`), so the *up* of the
    /// second tap is `dur` ms after its own `DOWN` and plays no part.
    #[test]
    fn two_taps_inside_the_window_and_the_slop_are_a_double_tap() {
        let c = DoubleTapConfig::DEFAULT;
        // 40 ms apart: the shortest pairing the platform will accept.
        assert!(forms_double_tap(
            tap(1000, 20, 500.0, 900.0),
            tap(1040, 20, 500.0, 900.0),
            c
        ));
        // 299 ms apart: the longest, since the bound is strict (`:133`).
        assert!(forms_double_tap(
            tap(1000, 20, 500.0, 900.0),
            tap(1299, 20, 500.0, 900.0),
            c
        ));
        // Diagonally at exactly the slop radius: (12, 12) is 16.97, so use
        // (0, 16) which is exactly 16.
        assert!(forms_double_tap(
            tap(1000, 20, 500.0, 900.0),
            tap(1100, 20, 500.0, 916.0),
            c
        ));
    }

    /// Each of the three conditions, knocked out one at a time. These are the
    /// perturbations of the happy path, so a rule that is accidentally
    /// missing fails here rather than passing the happy path.
    #[test]
    fn each_condition_knocked_out_alone_breaks_the_pair() {
        let c = DoubleTapConfig::DEFAULT;
        let a = tap(1000, 20, 500.0, 900.0);
        // Too soon: 39 ms, one below the minimum.
        assert!(!forms_double_tap(a, tap(1039, 20, 500.0, 900.0), c));
        // Too late: exactly the window, which is excluded (`:133` is `<`).
        assert!(!forms_double_tap(a, tap(1300, 20, 500.0, 900.0), c));
        assert!(!forms_double_tap(a, tap(1400, 20, 500.0, 900.0), c));
        // Too far: 17 px down, one past the 16 px slop.
        assert!(!forms_double_tap(a, tap(1100, 20, 500.0, 917.0), c));
        // Too far on x only, to catch a check that only looks at y.
        assert!(!forms_double_tap(a, tap(1100, 20, 517.0, 900.0), c));
        // A zero slop pairs nothing at all, rather than everything.
        let zero = DoubleTapConfig { slop: 0.0, ..c };
        assert!(!forms_double_tap(a, tap(1100, 20, 500.0, 900.0), zero));
        assert!(forms_double_tap(
            a,
            tap(1100, 20, 500.0, 900.0),
            DoubleTapConfig { slop: 1.0e-6, ..c }
        ));
    }

    /// A wrapped or out-of-order stamp must not pair. A negative gap that
    /// wrapped to a huge positive one would otherwise look like a slow first
    /// tap and *fail* the window -- so the dangerous direction is the one that
    /// wraps small, which is what `Tap::gap_to` returning 0 rules out via the
    /// minimum. Checked at both 0 and a non-zero minimum.
    #[test]
    fn a_backwards_clock_never_pairs() {
        let c = DoubleTapConfig::DEFAULT;
        let a = tap(1000, 20, 500.0, 900.0);
        let b = tap(1100, 20, 500.0, 900.0);
        // In order, 100 ms apart at the same point: pairs.
        assert!(forms_double_tap(a, b, c));
        // Reversed, the gap is negative. `Tap::gap_to` clamps it to 0, which
        // the 40 ms minimum already refuses -- so the guard that matters is
        // the ordering one, not the clamp.
        assert_eq!(b.gap_to(a), 0, "a backwards gap reads as zero");
        assert!(!forms_double_tap(b, a, c));
        // With the minimum opened up there is nothing left to refuse it but
        // the ordering check itself.
        let no_min = DoubleTapConfig {
            min_window_ms: 0,
            ..c
        };
        assert!(!forms_double_tap(b, a, no_min));
        assert!(forms_double_tap(a, b, no_min), "in order it still pairs");
    }

    /// The first tap's own duration eats into its budget. This is the whole
    /// reason `Tap` carries both timestamps: a 250 ms press leaves only 50 ms
    /// for a partner, and an implementation that measured up-to-up would give
    /// it 300. Mirrors `PipTouchState.getDoubleTapTimeoutCallbackDelay`
    /// (`:355-360`), `DOUBLE_TAP_TIMEOUT - (mUpTouchTime - mDownTouchTime)`.
    #[test]
    fn a_long_first_tap_leaves_a_shorter_budget() {
        let c = DoubleTapConfig::DEFAULT;
        let mut h = TapHistory::new(c);
        // A 250 ms press.
        h.on_touch_end(TouchEnd::Tap {
            down_ms: 1000,
            up_ms: 1250,
            x: 500.0,
            y: 900.0,
        });
        assert_eq!(h.remaining_ms(1250), 50, "300 - 250");
        // A partner 60 ms after the up is 310 ms after the down: outside.
        assert_eq!(
            h.on_touch_end(TouchEnd::Tap {
                down_ms: 1310,
                up_ms: 1330,
                x: 500.0,
                y: 900.0,
            }),
            TapOutcome::Unpaired
        );
        // 49 ms after the up is 299 ms after the down: inside, with 1 ms to
        // spare. The bound is on the `DOWN` gap, so the second tap's own
        // duration is irrelevant.
        let mut h = TapHistory::new(c);
        h.on_touch_end(TouchEnd::Tap {
            down_ms: 1000,
            up_ms: 1250,
            x: 500.0,
            y: 900.0,
        });
        assert_eq!(h.remaining_ms(1299), 1, "299 ms of the 300 ms window used");
        assert!(h.is_armed(1299));
        assert_eq!(h.pending_within(1299), Some(tap(1000, 250, 500.0, 900.0)));
        assert_eq!(h.remaining_ms(1300), 0, "the window bound is strict");
        assert!(!h.is_armed(1300));
        assert_eq!(h.pending_within(1300), None);
        assert_eq!(
            h.on_touch_end(TouchEnd::Tap {
                down_ms: 1299,
                up_ms: 1500,
                x: 500.0,
                y: 900.0,
            }),
            TapOutcome::DoubleTap,
            "a second tap that itself lasts 201 ms still pairs"
        );
    }

    /// The timeout: the first tap does not linger forever. `remaining_ms`
    /// counts down from the window and floors at zero, and the shell's
    /// `is_armed` / `pending_within` go false exactly when it reaches zero.
    #[test]
    fn the_first_tap_does_not_linger_past_its_budget() {
        let c = DoubleTapConfig::DEFAULT;
        let mut h = TapHistory::new(c);
        assert_eq!(h.remaining_ms(0), 0, "nothing pending");
        assert!(!h.is_armed(0));
        assert_eq!(h.pending(), None);
        assert_eq!(
            h.on_touch_end(TouchEnd::Tap {
                down_ms: 1000,
                up_ms: 1010,
                x: 500.0,
                y: 900.0,
            }),
            TapOutcome::FirstTap
        );
        assert_eq!(h.remaining_ms(1010), 290, "300 - a 10 ms press");
        assert!(h.is_armed(1010));
        // The `DOWN` gap from 1000 is what the window is measured on, so the
        // budget runs out at 1300 -- not at 1310 (up-to-up) and not at 1200
        // (window minus nothing).
        assert!(h.is_armed(1299), "one ms of the 300 ms window left");
        assert_eq!(h.remaining_ms(1299), 1);
        assert!(!h.is_armed(1300), "the bound is strict (`:133` is `<`)");
        assert_eq!(h.remaining_ms(1300), 0, "floored, not negative");
        assert_eq!(h.pending_within(1300), None);
        // The stored tap is still there -- `pending` is the raw reading, and
        // `pending_within` is the one that expires. Mixing them up is a bug
        // this pins.
        assert_eq!(h.pending(), Some(tap(1000, 10, 500.0, 900.0)));
        // And a backwards clock cannot inflate the budget.
        assert_eq!(h.remaining_ms(0), 290, "saturating, not wrapping");
    }

    /// A double-tap consumes the pending first half, so a third tap starts a
    /// fresh pair instead of firing again -- the reference's
    /// `mDoubleTapPending` claim of the following `UP`/`CANCEL`
    /// (`WorkspaceTouchListener.java:108-113`) exists for the same reason:
    /// one user intent, one action.
    #[test]
    fn a_double_tap_consumes_the_pair_and_the_third_tap_starts_over() {
        let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
        fn t(h: &mut TapHistory, ms: u64) -> TapOutcome {
            h.on_touch_end(TouchEnd::Tap {
                down_ms: ms,
                up_ms: ms + 10,
                x: 500.0,
                y: 900.0,
            })
        }
        assert_eq!(t(&mut h, 1000), TapOutcome::FirstTap);
        assert_eq!(t(&mut h, 1100), TapOutcome::DoubleTap);
        assert_eq!(h.pending(), None, "the pair was consumed");
        assert_eq!(
            t(&mut h, 1200),
            TapOutcome::FirstTap,
            "not a second double-tap"
        );
        assert_eq!(t(&mut h, 1300), TapOutcome::DoubleTap);
    }

    /// **The rule the brief is really about: a drag in between breaks the
    /// pair.** `PipTouchState.java:132` refuses to pair when the previous
    /// sequence dragged (`!mPreviouslyDragging`, set at `:219`), and
    /// `WorkspaceTouchListener.java:102-104` clears the pending flag on every
    /// `DOWN`. Feeding a long press between the two taps is the same event from
    /// the detector's point of view: a non-tap.
    #[test]
    fn a_drag_in_between_breaks_the_pair() {
        // A drag is `TouchEnd::Other`: past the dispatcher's own 25 px box, so
        // `finish_touch` never even calls it a tap (`input.rs:243-256`).
        let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
        let t = |h: &mut TapHistory, ms: u64| {
            h.on_touch_end(TouchEnd::Tap {
                down_ms: ms,
                up_ms: ms + 10,
                x: 500.0,
                y: 900.0,
            })
        };
        assert_eq!(t(&mut h, 1000), TapOutcome::FirstTap);
        assert_eq!(h.on_touch_end(TouchEnd::Other), TapOutcome::Interrupted);
        assert_eq!(h.pending(), None, "the drag left a tap armed");
        // Two taps 100 ms apart, 50 ms after the drag, which is inside every
        // threshold -- so the only thing that can refuse this pair is the
        // drag.
        assert_eq!(t(&mut h, 1050), TapOutcome::FirstTap);
        assert_eq!(
            t(&mut h, 1100),
            TapOutcome::DoubleTap,
            "only after re-arming"
        );
    }

    /// A run of misses must not chain two unrelated taps together. Tap 1, a
    /// miss, then tap 3 near tap *1*: the answer has to be no, because tap 2
    /// is the pending half after the miss. Without this, a slow double-tap
    /// followed by a fast one fires.
    #[test]
    fn a_run_of_misses_never_chains_tap_one_onto_tap_three() {
        let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
        let t = |h: &mut TapHistory, ms: u64, x: f32| {
            h.on_touch_end(TouchEnd::Tap {
                down_ms: ms,
                up_ms: ms + 10,
                x,
                y: 900.0,
            })
        };
        assert_eq!(t(&mut h, 1000, 500.0), TapOutcome::FirstTap);
        // Tap 2 is 500 ms later -- a miss, and it re-arms as the new first half.
        assert_eq!(t(&mut h, 1500, 500.0), TapOutcome::Unpaired);
        assert_eq!(h.pending().map(|p| p.down_ms), Some(1500));
        // Tap 3 is 100 ms after tap 2 but 200 ms after tap 1: it pairs with
        // tap 2, not with tap 1, and tap 1 is gone for good.
        assert_eq!(t(&mut h, 1600, 500.0), TapOutcome::DoubleTap);
        // The chain does not restart into a phantom.
        assert_eq!(h.pending(), None);
        assert_eq!(t(&mut h, 1700, 500.0), TapOutcome::FirstTap);
    }

    /// An expired first half is not a first half. This is the difference
    /// between `remaining_ms` being advisory and load-bearing: a caller that
    /// polls it and a caller that ignores it must get the same answer.
    #[test]
    fn an_expired_first_half_does_not_pair_even_without_a_poll() {
        let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
        h.on_touch_end(TouchEnd::Tap {
            down_ms: 1000,
            up_ms: 1010,
            x: 500.0,
            y: 900.0,
        });
        // Never polled. The tap arrives 5000 ms later.
        assert_eq!(
            h.on_touch_end(TouchEnd::Tap {
                down_ms: 5000,
                up_ms: 5010,
                x: 500.0,
                y: 900.0,
            }),
            TapOutcome::Unpaired,
            "an expired first half still reports Unpaired, not DoubleTap"
        );
        // And the newcomer is the new first half, so the very next tap pairs.
        assert_eq!(
            h.on_touch_end(TouchEnd::Tap {
                down_ms: 5100,
                up_ms: 5110,
                x: 500.0,
                y: 900.0,
            }),
            TapOutcome::DoubleTap
        );
    }

    /// The whole thing is a single optional tap. No `Vec`, no `String`, no
    /// clock, and `Send` so the compositor can own it across threads.
    #[test]
    fn the_tap_history_is_one_optional_tap_and_no_clock() {
        assert!(!core::mem::needs_drop::<TapHistory>());
        // `Option<Tap>` is 32 (a 24-byte `Tap` plus a discriminant; none of
        // its four scalar fields has a niche) and `DoubleTapConfig` is 24
        // (f32, 4 bytes of padding, two u64). Fixed, so it can live in the
        // compositor's own state.
        assert_eq!(core::mem::size_of::<TapHistory>(), 56);
        fn assert_send<T: Send>() {}
        assert_send::<TapHistory>();
        assert_send::<DoubleTapConfig>();
        // And it is deterministic: the same event stream gives the same
        // answers on every run, because nothing in it reads time.
        let run = || {
            let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
            let mut out = [false; 4];
            for (i, ms) in [1000u64, 1100, 5000, 5100].into_iter().enumerate() {
                out[i] = h.tap(tap(ms, 10, 500.0, 900.0));
            }
            out
        };
        assert_eq!(run(), run());
        assert_eq!(run(), [false, true, false, true]);
    }

    /// A cancelled sequence is a non-tap, so it breaks the pair.
    /// `WorkspaceTouchListener.java:109` treats `CANCEL` exactly like `UP`
    /// for this purpose.
    ///
    /// The dispatcher reports a long press as its own variant
    /// (`input.rs:245-248`), so the mapping from `InputDispatchResult` to
    /// [`TouchEnd`] is pinned here rather than left as a suggestion: it is
    /// exhaustively matched, so a future variant cannot silently become a tap.
    #[test]
    fn cancel_and_long_press_break_the_pair() {
        use crate::compositor::input::InputDispatchResult;

        /// The mapping the shell has to write. Exhaustive on purpose.
        fn end_of(r: &InputDispatchResult, down_ms: u64, up_ms: u64) -> TouchEnd {
            match *r {
                InputDispatchResult::Tap { x, y } => TouchEnd::Tap {
                    down_ms,
                    up_ms,
                    x,
                    y,
                },
                // A drag, a long press, a cancel, an injected key: all break
                // the pair. `InputDispatchResult::Touch(RawTouchEvent)` is a
                // completed gesture here by construction, because
                // `finish_touch` only routes movement past the 25 px box
                // there (`input.rs:243-256`).
                _ => TouchEnd::Other,
            }
        }

        let tap_result = InputDispatchResult::Tap { x: 500.0, y: 900.0 };
        let long_press = InputDispatchResult::LongPress { x: 500.0, y: 900.0 };
        let drag = InputDispatchResult::Touch(RawTouchEvent {
            touch_id: 1,
            phase: TouchPhase::Up,
            x: 500.0,
            y: 900.0,
            timestamp: std::time::Instant::now(),
        });
        let none = InputDispatchResult::None;

        // Only `Tap` maps to a tap; the three others are all `Other`.
        assert_eq!(
            end_of(&tap_result, 1000, 1010),
            TouchEnd::Tap {
                down_ms: 1000,
                up_ms: 1010,
                x: 500.0,
                y: 900.0
            }
        );
        for r in [&long_press, &drag, &none] {
            assert_eq!(end_of(r, 1000, 1010), TouchEnd::Other, "{r:?}");
        }

        // And the consequence: a tap, a long press, a tap -- no double-tap,
        // where the reference clears the flag on every `DOWN`
        // (`WorkspaceTouchListener.java:102-104`).
        let mut h = TapHistory::new(DoubleTapConfig::DEFAULT);
        assert_eq!(
            h.on_touch_end(end_of(&tap_result, 1000, 1010)),
            TapOutcome::FirstTap
        );
        assert_eq!(
            h.on_touch_end(end_of(&long_press, 1020, 1400)),
            TapOutcome::Interrupted
        );
        assert_eq!(h.pending(), None);
        assert_eq!(
            h.on_touch_end(end_of(&tap_result, 1100, 1110)),
            TapOutcome::FirstTap,
            "the tap after the long press starts a new pair"
        );
    }

    /// `Tap::new` clamps an out-of-order stamp rather than producing a negative
    /// duration, and `forms_double_tap` is a pure function of its arguments.
    #[test]
    fn a_tap_clamps_an_out_of_order_stamp() {
        let t = Tap::new(1000, 900, 1.0, 2.0);
        assert_eq!(t.up_ms, 1000);
        assert_eq!(t.duration_ms(), 0);
        assert_eq!(t.distance_to(Tap::new(0, 0, 4.0, 6.0)), 5.0);
        // `as_tap` is the only conversion from `TouchEnd`, and it is exact.
        assert_eq!(
            TouchEnd::Other.as_tap(),
            None,
            "a non-tap has no Tap, by construction"
        );
        assert_eq!(
            TouchEnd::Tap {
                down_ms: 5,
                up_ms: 3,
                x: 7.0,
                y: 8.0
            }
            .as_tap(),
            Some(Tap::new(5, 3, 7.0, 8.0))
        );
    }

    /// The default action is **Sleep**: `PreferenceManager2.kt:813-816`
    /// declares `doubleTapGestureHandler` with
    /// `defaultValue = GestureHandlerConfig.Sleep`. If this ever reads
    /// `NoOp`, the launcher quietly stops sleeping on a double-tap and nobody
    /// notices because the rest of the gesture layer still works.
    #[test]
    fn the_default_double_tap_action_is_sleep() {
        assert_eq!(DOUBLE_TAP_DEFAULT, DoubleTapAction::Sleep);
        // The `@SerialName`s as **literals**. An earlier version of this
        // iterated the table asserting `from_id(a.id()) == Some(a)`, which
        // round-trips through the very table under test: a mis-spelt
        // `id()` passed. The perturbation sweep caught it -- changing
        // `OpenQuickSettings`'s key to `open_quick_settings` left the suite
        // green. So the keys are spelled out here instead.
        let want = [
            (DoubleTapAction::NoOp, "noOp"),
            (DoubleTapAction::Sleep, "sleep"),
            (DoubleTapAction::Recents, "recents"),
            (DoubleTapAction::OpenNotifications, "openNotificationdata"),
            (DoubleTapAction::OpenQuickSettings, "openQuickSettings"),
            (DoubleTapAction::OpenAppDrawer, "openAppDrawer"),
            (DoubleTapAction::OpenAppSearch, "openAppSearch"),
            (DoubleTapAction::OpenSearch, "openSearch"),
            (DoubleTapAction::OpenAssistant, "openAssistant"),
        ];
        for (a, id) in want {
            assert_eq!(a.id(), id, "{a:?} key");
            assert_eq!(DoubleTapAction::from_id(id), Some(a), "{id}");
            assert!(a.label().is_ascii(), "{} label is not ASCII", a.id());
        }
        // `OpenApp` is the one `GestureHandlerConfig` variant that is absent,
        // because it is a `data class` carrying an `OpenAppTarget`
        // (`GestureHandlerConfig.kt:133-184`) and cannot be a fixed entry.
        assert_eq!(DoubleTapAction::ALL.len(), 9);
        assert_eq!(DoubleTapAction::from_id("openApp"), None);
        // The two handlers that are also externally invokable upstream
        // (`GestureHandlerConfig.kt:106`, `:115`) are present, so a shell can
        // wire them without a second enum.
        assert_eq!(
            DoubleTapAction::from_id("openAppDrawer"),
            Some(DoubleTapAction::OpenAppDrawer)
        );
    }
}
