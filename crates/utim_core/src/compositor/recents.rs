//! Recents / Overview state, and the pure transitions the shell drives it
//! with.
//!
//! This module is *state*, not policy and not geometry. Three separations
//! matter, and each of them exists because the thing it replaces got it wrong:
//!
//! * **No layout.** Card geometry comes from [`RecentsLayout`], which
//!   [`crate::graphics::layout`] already builds from the verified
//!   `quickstep/res/values/dimens.xml` rows. No dp, radius or margin is
//!   re-derived here.
//! * **No syscalls.** [`KillState`] is a state machine. Escalating a card from
//!   [`KillState::Grace`] to [`KillState::Forced`] *emits a
//!   [`KillAction::Force`]* and stops there. Firing a signal from a data
//!   module is untestable and unsafe, and the deleted `RecentsCarousel` was
//!   deleted precisely because the process-lifecycle half of it had no
//!   reachable call site.
//! * **No allocation.** Every list here is a fixed-capacity inline array. The
//!   old carousel used `Vec<RecentsCard>` and `out: &mut Vec<i32>` on the
//!   per-tick kill path; neither is coming back.
//!
//! The other non-negotiable is **spring parking**. Every profile used here is
//! either underdamped (`zeta < 1`) or has a rest threshold an `f32` can stall
//! short of, so a spring left to run converges asymptotically and the shell
//! animates forever. [`SpringSimulation::settle_duration`] is the authority
//! on how long a response takes to enter its rest thresholds;
//! [`Recents::park_expired`] turns that into an absolute deadline against
//! [`Recents::now_ms`] and snaps the spring onto its target when the deadline
//! passes. See [`PARK_UNARMED`] for the arming rule, and
//! `recents_springs_park_within_bounded_time` for the proof.

use crate::graphics::drm_kms::{SpringConfig, SpringSimulation, RECENTS_SCALE_MULTIPLIER};
use crate::graphics::layout::{damped_scroll, Layout, RecentsLayout, Rect};

/// Cards the overview keeps. `QuickstepTransitionManager.MAX_NUM_TASKS`
/// (`QuickstepTransitionManager.java:236`).
pub const MAX_TASKS: usize = 5;

// ===========================================================================
// Dismiss geometry constants
// ===========================================================================

/// Travel that arms a dismiss, as a fraction of the dismiss length.
/// `TaskViewDismissTouchController.DISMISS_THRESHOLD_FRACTION` (`:431`).
pub const DISMISS_THRESHOLD_FRACTION: f32 = 0.5;

/// Fraction at which the overview scale leaves its resting value.
/// `RECENTS_SCALE_FIRST_THRESHOLD_FRACTION` (`:437`).
pub const DISMISS_SCALE_FIRST_THRESHOLD_FRACTION: f32 = 0.2;

/// Fraction at which the dismiss becomes certain and the scale starts moving
/// again. `RECENTS_SCALE_DISMISS_THRESHOLD_FRACTION` (`:438`).
pub const DISMISS_SCALE_DISMISS_THRESHOLD_FRACTION: f32 = 0.5;

/// Fraction at which the scale reaches its committed value.
/// `RECENTS_SCALE_SECOND_THRESHOLD_FRACTION` (`:439`).
pub const DISMISS_SCALE_SECOND_THRESHOLD_FRACTION: f32 = 0.575;

/// Scale of the whole overview while idle. `RECENTS_SCALE_DEFAULT` (`:436`).
pub const DISMISS_SCALE_DEFAULT: f32 = 1.0;
/// Scale once a dismiss is past the first threshold and not yet certain.
/// `RECENTS_SCALE_ON_DISMISS_CANCEL` (`:434`).
pub const DISMISS_SCALE_ON_CANCEL: f32 = 0.9875;
/// Scale once the dismiss is certain. `RECENTS_SCALE_ON_DISMISS_SUCCESS`
/// (`:435`).
pub const DISMISS_SCALE_ON_SUCCESS: f32 = 0.975;

/// Travel either side of the dismiss threshold that counts as "at the
/// threshold" for the haptic, in dp: a drag event does not necessarily land
/// on the exact threshold displacement.
/// `TaskViewDismissTouchController.DISMISS_THRESHOLD_HAPTIC_RANGE` (`:432`).
pub const DISMISS_THRESHOLD_HAPTIC_RANGE: f32 = 10.0;

/// Release speed that counts as a fling, in dp.
/// `quickstep/res/values/dimens.xml:143` (`recents_fast_fling_velocity`).
pub const FAST_FLING_VELOCITY: f32 = 600.0;

/// How far a dismiss may travel back past its origin, 25 dp. The pixel value
/// is read from [`RecentsLayout::dismiss_undershoot`]; this is the dp row it
/// comes from (`task_dismiss_max_undershoot`, `dimens.xml:109`).
pub const DISMISS_UNDERSHOOT_DP: f32 = 25.0;

/// Grace period between asking an app to close and escalating to an
/// uncatchable kill, ms.
///
/// UTLC's own value, carried over from the deleted
/// `RecentsCarousel::update_kill_lifecycle` call sites, which passed
/// `Duration::from_millis(500)`. Lawnchair has no equivalent row: it closes a
/// task and does not escalate. What is preserved is the *shape* -- a graceful
/// request, then a bounded escalation -- not the number.
pub const KILL_GRACE_MS: f32 = 500.0;

/// The piecewise dismiss-scale ladder, plateaus included.
///
/// Port of `TaskViewDismissTouchController.getRecentsScale` (`:352-389`),
/// which is a five-arm `when` over the dismiss fraction:
///
/// ```text
/// f <= 0        -> 1.0
/// 0 < f < 0.2   -> lerp LINEAR 1.0 -> 0.9875
/// 0.2 <= f< 0.5 -> 0.9875            (hold)
/// 0.5 <= f<0.575-> lerp LINEAR 0.9875 -> 0.975
/// f >= 0.575    -> 0.975            (hold)
/// ```
///
/// The plateaus are not a simplification. They are what makes the card hold
/// still between 0.2 and 0.5 of the dismiss distance, so a small overshoot
/// past the first threshold does not read as movement -- the reference holds
/// the scale constant there precisely so the user can see the card has
/// stopped answering small drags before it starts answering large ones.
/// Collapsing this into one `lerp(1.0, 0.975, f)` would shrink the card
/// continuously and lose that distinction.
///
/// A non-finite fraction maps to [`DISMISS_SCALE_DEFAULT`]: unusable input
/// must not read as "certainly dismissing".
#[inline]
pub fn dismiss_recents_scale(fraction: f32) -> f32 {
    if !fraction.is_finite() || fraction <= 0.0 {
        return DISMISS_SCALE_DEFAULT;
    }
    if fraction < DISMISS_SCALE_FIRST_THRESHOLD_FRACTION {
        return lerp(
            DISMISS_SCALE_DEFAULT,
            DISMISS_SCALE_ON_CANCEL,
            fraction / DISMISS_SCALE_FIRST_THRESHOLD_FRACTION,
        );
    }
    if fraction < DISMISS_SCALE_DISMISS_THRESHOLD_FRACTION {
        return DISMISS_SCALE_ON_CANCEL;
    }
    if fraction < DISMISS_SCALE_SECOND_THRESHOLD_FRACTION {
        return lerp(
            DISMISS_SCALE_ON_CANCEL,
            DISMISS_SCALE_ON_SUCCESS,
            (fraction - DISMISS_SCALE_DISMISS_THRESHOLD_FRACTION)
                / (DISMISS_SCALE_SECOND_THRESHOLD_FRACTION
                    - DISMISS_SCALE_DISMISS_THRESHOLD_FRACTION),
        );
    }
    DISMISS_SCALE_ON_SUCCESS
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ===========================================================================
// Cards
// ===========================================================================

/// Where a card is in the close/kill sequence.
///
/// A pure state machine. The transition is performed by the shell, which
/// knows about sockets, `/proc` and `SIGKILL`; this type only records which
/// step of the sequence a card is on and emits the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillState {
    /// Live. The card can still be dismissed and re-launched.
    Idle,
    /// A graceful close has been requested; the grace period is running.
    Grace,
    /// The grace period expired without the app exiting. Uncatchable.
    Forced,
}

/// A dismissal, translated into something the shell has to go do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillAction {
    /// Ask the app to close through the protocol, and start the grace clock.
    Close(i32),
    /// Grace expired; escalate to an uncatchable kill.
    ///
    /// This *describes* the action, it is not the action. Nothing in this
    /// crate signals a process as a result of producing one.
    Force(i32),
}

/// One app in the overview.
///
/// `Copy` and free of pointers, so the whole array lives inline in [`Recents`]
/// and the renderer can read it without touching the heap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskCard {
    /// Index into the app catalogue. Deliberately not a `String`: the
    /// catalogue is the shell's, and a card only ever has to point at it.
    pub app_id: u32,
    /// The app's process, for the close request and the kill escalation.
    pub pid: i32,
    /// The app's toplevel surface.
    pub surface_id: u32,
    /// Index into the shell's snapshot pool, or [`NO_THUMB`].
    pub thumb: u16,
    /// Live vertical drag position, px. Negative is up. Drives the drag only:
    /// the *rendered* offset always comes from `Recents::dismiss[i]`, and the
    /// dismiss *fraction* is always computed from this, which is what the
    /// reference does at `:280`.
    pub dismiss_y: f32,
    /// Release speed of the drag, px/s. Negative is up.
    ///
    /// Written by the caller, not by [`Recents::on_card_drag`]: the reference
    /// takes its release velocity from `TaskViewScrollDetector` (`:308`), i.e.
    /// from the touch stream's own velocity tracking, which only the gesture
    /// layer has. A module that re-derived it from the delta stream would be
    /// running a second, worse velocity tracker.
    pub dismiss_v: f32,
    /// False for a card the shell refuses to kill (a persistent surface, a
    /// protected app). A pinned card cannot be swiped away and is skipped by
    /// [`Recents::clear_all`].
    pub dismissable: bool,
    /// Close/kill progress.
    pub kill: KillState,
    /// When the grace period started, on the clock [`Recents::step`]
    /// advances.
    pub grace_started_ms: f32,
}

/// No snapshot for this card.
pub const NO_THUMB: u16 = u16::MAX;

impl TaskCard {
    /// A live, dismissable card with no snapshot yet.
    pub const fn new(app_id: u32, pid: i32, surface_id: u32) -> Self {
        Self {
            app_id,
            pid,
            surface_id,
            thumb: NO_THUMB,
            dismiss_y: 0.0,
            dismiss_v: 0.0,
            dismissable: true,
            kill: KillState::Idle,
            grace_started_ms: 0.0,
        }
    }
}

/// A bounded batch of [`KillAction`]s, at most one per card.
///
/// [`MAX_TASKS`] is the exact capacity: `clear_all` touches each card once,
/// and the grace escalation fires once per card per lifetime. `push` clamps
/// rather than growing, so a caller that forgets to drain cannot make the
/// shell allocate -- it gets a `false` and can react.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillQueue {
    pub items: [KillAction; MAX_TASKS],
    pub len: u8,
}

impl KillQueue {
    pub const EMPTY: Self = Self {
        items: [KillAction::Close(0); MAX_TASKS],
        len: 0,
    };

    /// Record an action. `false` means the batch was full and this was
    /// dropped.
    pub fn push(&mut self, action: KillAction) -> bool {
        if self.len as usize >= MAX_TASKS {
            return false;
        }
        self.items[self.len as usize] = action;
        self.len += 1;
        true
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &KillAction> + '_ {
        self.items[..self.len as usize].iter()
    }
}

impl Default for KillQueue {
    fn default() -> Self {
        Self::EMPTY
    }
}

// ===========================================================================
// Spring parking
// ===========================================================================

/// Marks a parking slot with no deadline.
///
/// A slot is unarmed while its spring is at rest, and is armed from the state
/// the spring is actually in the first time it is found moving. That single
/// rule is what makes parking robust against direct writes to the public
/// spring fields: a caller that pokes `recents.dismiss[0].target` gets a
/// deadline on the next `step` with no cooperation from this module, measured
/// from the state the caller produced.
const PARK_UNARMED: f32 = f32::NEG_INFINITY;

/// Advance one parking slot. `true` means `spring` must be forced onto its
/// target now.
///
/// `slot` holds an absolute deadline on the caller's millisecond clock.
/// `settle_duration` is evaluated exactly once per animation, here at the arm
/// point, because it walks the response segment by segment and running it per
/// frame would put a bisection search on the hot path.
fn settle_expired(spring: &SpringSimulation, slot: &mut f32, now_ms: f32, frame_ms: f32) -> bool {
    if spring.is_at_rest() {
        *slot = PARK_UNARMED;
        return false;
    }
    if *slot == PARK_UNARMED {
        let settle = spring.settle_duration(frame_ms);
        if !settle.is_finite() {
            // A profile that provably never enters its rest thresholds. Park
            // it on this same frame: the alternative is the "animates
            // forever" failure this whole mechanism exists to prevent, and no
            // profile in `SpringConfig` takes this branch.
            *slot = PARK_UNARMED;
            return true;
        }
        *slot = now_ms + settle * 1000.0;
        return false;
    }
    if now_ms >= *slot {
        *slot = PARK_UNARMED;
        return true;
    }
    false
}

/// [`settle_expired`] for a spring whose rest thresholds live in the x1000
/// domain while its stored value does not.
///
/// [`SpringSimulation::step_scaled`] scales up, integrates, and scales back
/// down, so it evaluates rest against the *scaled* value. Checking rest or a
/// settle duration on the unscaled value would compare a 1000x-looser
/// threshold than the integrator used, and the parking deadline would be up
/// to 1000x too long -- the spring would still be visibly moving when it was
/// declared parked. The temporary is a copy, not a mutation, so the caller's
/// spring keeps its real-unit value.
fn settle_expired_scaled(
    spring: &SpringSimulation,
    slot: &mut f32,
    now_ms: f32,
    frame_ms: f32,
) -> bool {
    let m = RECENTS_SCALE_MULTIPLIER;
    settle_expired(
        &SpringSimulation {
            value: spring.value * m,
            velocity: spring.velocity * m,
            target: spring.target * m,
            config: spring.config,
        },
        slot,
        now_ms,
        frame_ms,
    )
}

/// Force `spring` onto its target and disarm the slot.
#[inline]
fn park_now(spring: &mut SpringSimulation, slot: &mut f32) {
    spring.value = spring.target;
    spring.velocity = 0.0;
    *slot = PARK_UNARMED;
}

// ===========================================================================
// Recents
// ===========================================================================

/// Parking slot of the overview scale.
const P_SCALE: usize = 0;
/// First slot of the per-card dismiss springs.
const P_DISMISS: usize = P_SCALE + 1;
/// First slot of the per-card grid-reflow springs.
const P_REFLOW: usize = P_DISMISS + MAX_TASKS;
/// First slot of the per-card dismiss-effect springs.
const P_EFFECTS: usize = P_REFLOW + MAX_TASKS;
/// Total parking slots, i.e. springs inside a [`Recents`].
pub const PARK_SLOTS: usize = P_EFFECTS + MAX_TASKS;

/// Sentinel for "no card is being dragged".
const NO_CARD: u8 = u8::MAX;

/// The overview.
///
/// # Frame budget
///
/// A card is `0.70 * w` by `0.70 * h`
/// ([`crate::graphics::layout::RECENTS_CARD_SCALE`]). On the 1080 x 2400
/// reference panel that is 756 x 1680 = 1.27 Mpx, and [`MAX_TASKS`] of them
/// is 6.35 Mpx of thumbnail blit per frame if the renderer draws the whole
/// stack. Only the selected card and the one card that can slide in beside it
/// can have a pixel on screen; every other card is either fully occluded by
/// the selected card or off the panel. So [`Self::visible_card`] is the
/// selected card and [`Self::cull_range`] covers at most two, which halves the
/// worst case to 2.54 Mpx.
///
/// That is a *hint*, not an invariant: the range is two entries wide and the
/// renderer is expected to honour it, because a renderer that ignores it is
/// back to the 6.35 Mpx case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recents {
    /// Cards, most recent first. Only the first [`Self::len`] are live.
    pub cards: [TaskCard; MAX_TASKS],
    /// Live cards in [`Self::cards`].
    pub len: u8,
    /// Index of the focused card, always `< len`.
    pub selected: u8,
    /// Overview scale in real units (1.0 = unscaled). Integrated through the
    /// x1000 domain; see [`RECENTS_SCALE_MULTIPLIER`].
    pub scale: SpringSimulation,
    /// Per-card vertical offset, px. During a drag this follows the finger,
    /// linearly below the detach threshold and on a `magnetic_detach` spring
    /// above it; after a release it is a `task_dismiss` spring whose damping
    /// ratio is softened by 0.15 per hop away from the dismissed card, so a
    /// stack of five does not all land on the same frame.
    pub dismiss: [SpringSimulation; MAX_TASKS],
    /// Per-card horizontal offset from its slot, px. `grid_reflow`, and the
    /// thing that animates both a page change and the gap closing after a
    /// removal.
    pub reflow: [SpringSimulation; MAX_TASKS],
    /// Per-card dismiss side-effect progress, 0..1. `dismiss_effects`. 0 at
    /// rest; 1 once the card is fully dismissed.
    pub effects: [SpringSimulation; MAX_TASKS],
    /// Cull hint: the card the renderer must draw. Always
    /// `min(selected, len - 1)`.
    pub visible_card: u8,
    /// Horizontal residue of an in-flight scrub, px, reported by
    /// [`Self::carousel_scroll`]. The renderer applies it through
    /// [`Self::card_rect_centered`]. It is finger tracking, not an animation,
    /// so it is deliberately not a spring.
    pub drag_px: f32,
    /// Card width, px, copied from this panel's [`RecentsLayout`] at
    /// construction. See [`Self::card_box`] for why the box is carried here.
    pub card_w: f32,
    /// Card height, px. See [`Self::card_box`].
    pub card_h: f32,
    /// Card corner radius, px. See [`Self::card_box`].
    pub card_radius: f32,
    /// Index of the card currently under a vertical dismiss drag, or
    /// [`NO_CARD`].
    pub dragging: u8,
    /// True once the current drag has reached the dismiss threshold, so the
    /// haptic fires exactly once per drag.
    pub threshold_haptic_done: bool,
    /// Dismiss travel for this panel, px: the distance from the panel's top
    /// edge to the card's bottom, divided by the on-dismiss scale.
    /// `PortraitPagedViewHandler.getTaskDismissLength` returns
    /// `taskThumbnailBounds.bottom`, and
    /// `TaskViewDismissTouchController.kt:215-222` divides that by
    /// `RECENTS_SCALE_ON_DISMISS_SUCCESS`.
    pub dismiss_length: f32,
    /// How far a dismiss may travel back past its origin, px.
    pub undershoot: f32,
    /// Travel before a card stops tracking the finger 1:1 and starts following
    /// a spring, px. `task_dismiss_detach_threshold`.
    pub detach: f32,
    /// Release speed that counts as a fling, px/s.
    pub fast_fling: f32,
    /// Centre-to-centre card pitch, px: card width plus the 16 dp gap.
    pub pitch: f32,
    /// Travel either side of the dismiss threshold that counts as "at the
    /// threshold" for the haptic, px.
    pub haptic_range: f32,
    /// Animation clock, ms. Advanced only by [`Self::step`].
    now_ms: f32,
    /// Absolute parking deadline per spring, or [`PARK_UNARMED`]. Indexed by
    /// `P_SCALE`, `P_DISMISS + i`, `P_REFLOW + i`, `P_EFFECTS + i`.
    ///
    /// This is the one piece of state the plan's field list does not name, and
    /// it cannot be avoided: a deadline is a function of *when* an animation
    /// started, and [`SpringSimulation`] is a value with no clock. Without it
    /// parking would have to re-derive the deadline every frame, which is the
    /// expensive call the mechanism exists to avoid.
    park: [f32; PARK_SLOTS],
}

impl Recents {
    /// Build the overview for one panel.
    ///
    /// Takes the full [`Layout`] rather than a bare [`RecentsLayout`] because
    /// the dismiss length and the fling threshold are not in the recents
    /// layout: the first is derived from the card's own position against the
    /// panel height, the second from a dp the recents layout does not carry.
    /// Everything geometric still comes from the [`RecentsLayout`] this
    /// produces.
    pub fn new(l: &Layout) -> Self {
        let rl = l.recents();
        let profile = l.profile();
        // The card is centred on the panel, so its bottom edge is the panel
        // midpoint plus half the card height. Read from the layout's own card
        // centre rather than recomputed from the panel, so the two cannot
        // disagree about where the strip sits.
        let card_bottom = rl.card_center.1 + rl.card_h * 0.5;
        Self {
            cards: [TaskCard::new(0, 0, 0); MAX_TASKS],
            len: 0,
            selected: 0,
            scale: SpringSimulation::new(
                DISMISS_SCALE_DEFAULT,
                DISMISS_SCALE_DEFAULT,
                SpringConfig::recents_scale(),
            ),
            dismiss: [SpringSimulation::new(0.0, 0.0, SpringConfig::task_dismiss()); MAX_TASKS],
            reflow: [SpringSimulation::new(0.0, 0.0, SpringConfig::grid_reflow()); MAX_TASKS],
            effects: [SpringSimulation::new(0.0, 0.0, SpringConfig::dismiss_effects()); MAX_TASKS],
            visible_card: 0,
            drag_px: 0.0,
            card_w: rl.card_w,
            card_h: rl.card_h,
            card_radius: rl.corner_r,
            dragging: NO_CARD,
            threshold_haptic_done: false,
            dismiss_length: card_bottom / DISMISS_SCALE_ON_SUCCESS,
            undershoot: rl.dismiss_undershoot,
            detach: rl.detach_dp,
            fast_fling: profile.dp(FAST_FLING_VELOCITY),
            pitch: rl.card_w + rl.spacing,
            haptic_range: profile.dp(DISMISS_THRESHOLD_HAPTIC_RANGE),
            now_ms: 0.0,
            park: [PARK_UNARMED; PARK_SLOTS],
        }
    }

    /// The animation clock, ms.
    pub fn now_ms(&self) -> f32 {
        self.now_ms
    }

    /// Live cards, oldest last.
    pub fn iter(&self) -> impl Iterator<Item = &TaskCard> + '_ {
        self.cards[..self.len as usize].iter()
    }

    // ---------------------------------------------------------------------
    // Mutations
    // ---------------------------------------------------------------------

    /// Insert `card` as the most recent task.
    ///
    /// **Eviction policy: the [`MAX_TASKS`] most recently used tasks survive.**
    /// The stack is most-recent-first, so a full stack drops its *last* entry
    /// -- the oldest -- and hands it back so the shell can release its surface
    /// and its snapshot slot. Launching an app already in the stack promotes
    /// it to the front rather than duplicating it, and hands back the stale
    /// entry for the same reason.
    pub fn push(&mut self, card: TaskCard) -> Option<TaskCard> {
        if let Some(at) = self.index_of(card.pid) {
            // At most one of the two can be `Some`: taking the stale entry
            // frees a slot, so the insert that follows cannot also evict.
            let stale = self.take(at);
            return Some(self.insert_front(card).unwrap_or(stale));
        }
        self.insert_front(card)
    }

    /// Remove the card for `pid`, animating the gap closed.
    pub fn remove_by_pid(&mut self, pid: i32) -> Option<TaskCard> {
        let at = self.index_of(pid)?;
        Some(self.take(at))
    }

    /// Ask every dismissable card to close, and start its grace period.
    ///
    /// Pinned cards and cards already closing are skipped, so a card can
    /// never have its grace clock re-armed and is never asked to close twice.
    /// The cards stay in the stack: the shell removes each one with
    /// [`Self::remove_by_pid`] as its close is acknowledged, exactly as it
    /// does for a swiped card.
    pub fn clear_all(&mut self) -> KillQueue {
        let mut out = KillQueue::EMPTY;
        for i in 0..self.len as usize {
            if self.arm_close(i) {
                out.push(KillAction::Close(self.cards[i].pid));
            }
        }
        out
    }

    /// Drag card `i` by `dy` px. Negative is up.
    ///
    /// Travel is bounded on both sides, as in
    /// `TaskViewDismissTouchController.onDrag` (`:263-279`): to the dismiss
    /// length above the origin, to [`RecentsLayout::dismiss_undershoot`]
    /// below it. Below the origin the reference additionally runs the travel
    /// through a decelerate curve to fake resistance; this module enforces the
    /// bound and leaves the shaping to the renderer, which is the only place
    /// that knows the curve's parameterisation.
    ///
    /// The overview scale follows the finger through
    /// [`dismiss_recents_scale`], as `RECENTS_SCALE_PROPERTY.setValue` does
    /// at `:281`. It is written directly rather than spring-aimed, which is
    /// why [`Self::park_expired`] disarms the scale while a drag is live.
    ///
    /// Release speed is not derived here; see [`TaskCard::dismiss_v`].
    ///
    /// Returns `true` on the single `ACTION_MOVE` that first carries the drag
    /// into the dismiss-threshold haptic band, and `false` on every other
    /// frame and on every no-op. The caller pulses on `true`; see
    /// [`Self::note_threshold_haptic`] for why the return value is the edge and
    /// not [`Self::threshold_haptic_done`].
    pub fn on_card_drag(&mut self, i: usize, dy: f32) -> bool {
        if i >= self.len as usize {
            return false;
        }
        if self.dragging != NO_CARD && self.dragging as usize != i {
            // A second finger is not a second dismiss; the first one owns it.
            return false;
        }
        let live = {
            let card = &self.cards[i];
            card.kill != KillState::Idle || !card.dismissable
        };
        if live {
            // Already dying or pinned: dragging it again must change nothing
            // and, critically, must not take ownership of the drag.
            return false;
        }
        if self.dragging == NO_CARD {
            // A vertical dismiss supersedes any horizontal residue, and starts
            // a fresh haptic latch.
            self.drag_px = 0.0;
            self.threshold_haptic_done = false;
        }
        self.dragging = i as u8;
        self.drag_px = 0.0;

        let travel = (self.cards[i].dismiss_y + dy).clamp(-self.dismiss_length, self.undershoot);
        self.cards[i].dismiss_y = travel;

        // Identity below the detach threshold, spring beyond it: the
        // `generateMotionSpec` breakpoints at `:411-420`, where the spring is
        // `SpringParameters(stiffness = 800f, dampingRatio = 0.95f)`. The
        // spring chases the finger as its target, and is stepped by `step`
        // with parking suppressed while this card is `dragging`.
        if travel.abs() >= self.detach {
            self.dismiss[i].config = SpringConfig::magnetic_detach();
            self.dismiss[i].set_target(travel);
        } else {
            self.dismiss[i].value = travel;
            self.dismiss[i].target = travel;
            self.dismiss[i].velocity = 0.0;
        }

        self.scale.value = dismiss_recents_scale(self.dismiss_fraction(i));
        self.scale.target = self.scale.value;
        self.scale.velocity = 0.0;
        self.note_threshold_haptic(i)
    }

    /// Release card `i` and say what happened.
    ///
    /// The decision is `TaskViewDismissTouchController.onDragEnd` (`:307-338`)
    /// exactly: a fling towards dismissal always wins, a fling back towards
    /// rest always cancels, otherwise it is "past the threshold or not".
    ///
    /// Either way the overview scale springs back to
    /// [`DISMISS_SCALE_DEFAULT`] (`animateRecentsScale(RECENTS_SCALE_DEFAULT)`
    /// at `:339-342`), and the dismiss bank is re-aimed with the `task_dismiss`
    /// profile softened by 0.15 per hop away from this card
    /// (`RecentsDismissUtils.kt:1382`, `ADDITIONAL_DISMISS_DAMPING_RATIO`).
    pub fn on_card_release(&mut self, i: usize) -> DismissOutcome {
        if i >= self.len as usize {
            return DismissOutcome::Ignored;
        }
        let card = self.cards[i];
        if card.kill != KillState::Idle || !card.dismissable {
            return DismissOutcome::Ignored;
        }
        self.dragging = NO_CARD;
        self.drag_px = 0.0;
        self.threshold_haptic_done = false;

        let beyond = self.dismiss_travel(i) >= DISMISS_THRESHOLD_FRACTION;
        let flinging = card.dismiss_v.abs() >= self.fast_fling;
        let fling_up = flinging && card.dismiss_v < 0.0;
        let fling_back = flinging && card.dismiss_v > 0.0;
        let committing = fling_up || (beyond && !fling_back);

        self.scale.set_target(DISMISS_SCALE_DEFAULT);
        self.aim_dismiss_bank(i, committing, card.dismiss_v);
        self.effects[i].set_target(if committing { 1.0 } else { 0.0 });
        self.cards[i].dismiss_y = 0.0;
        self.cards[i].dismiss_v = 0.0;

        if committing {
            self.arm_close(i);
            DismissOutcome::Committed {
                card: i,
                pid: card.pid,
            }
        } else {
            DismissOutcome::Cancelled {
                card: i,
                pid: card.pid,
            }
        }
    }

    /// Focus card `i`, clamped into range.
    ///
    /// The move is animated, not snapped: each card's [`Self::reflow`] spring
    /// is armed at the offset that keeps it where it currently is and aimed at
    /// zero, so a page change slides. An out-of-range index clamps rather than
    /// panicking -- a stale touch event on a stack that has since shrunk is
    /// normal, not exceptional.
    pub fn select(&mut self, i: usize) {
        if self.len == 0 {
            self.selected = 0;
            self.visible_card = 0;
            return;
        }
        let next = (i as u8).min(self.len - 1);
        let prev = self.selected;
        if next == prev {
            return;
        }
        self.selected = next;
        self.arm_reflow_select(prev, next);
        self.visible_card = next;
    }

    /// Feed a horizontal scrub, in px, and page when half a card is crossed.
    ///
    /// Positive is towards older cards. The residue stays in
    /// [`Self::drag_px`] for the renderer to apply directly, so a drag tracks
    /// the finger 1:1 and only the page change is animated. Past either end
    /// the residue is run through [`crate::graphics::layout::damped_scroll`],
    /// the `OverScroll.dampedScroll` curve `PagedView` uses for its own
    /// overscroll, so the ends resist instead of running away.
    pub fn scrub(&mut self, delta: f32) {
        if self.len == 0 || !delta.is_finite() {
            return;
        }
        self.drag_px += delta;
        let half = self.pitch * 0.5;
        while self.drag_px >= half && self.selected + 1 < self.len {
            self.drag_px -= self.pitch;
            let prev = self.selected;
            self.selected += 1;
            self.arm_reflow_select(prev, self.selected);
        }
        while self.drag_px <= -half && self.selected > 0 {
            self.drag_px += self.pitch;
            let prev = self.selected;
            self.selected -= 1;
            self.arm_reflow_select(prev, self.selected);
        }
        if self.selected == 0 && self.drag_px > 0.0 {
            self.drag_px = damped_scroll(self.drag_px, self.pitch);
        } else if self.selected == self.len - 1 && self.drag_px < 0.0 {
            self.drag_px = -damped_scroll(-self.drag_px, self.pitch);
        }
        self.visible_card = self.selected;
    }

    // ---------------------------------------------------------------------
    // Frame
    // ---------------------------------------------------------------------

    /// Advance every spring by `dt` seconds, advance the clock, and escalate
    /// any card whose grace period has expired.
    ///
    /// `frame_ms` is the display's frame period, threaded straight into the
    /// parking deadlines. Returns this frame's kill actions; there is at most
    /// one, because a card escalates once.
    pub fn step(&mut self, dt: f32, frame_ms: f32) -> KillQueue {
        self.now_ms += (dt.max(0.0)) * 1000.0;
        let live = self.dragging;
        // A dragged card's dismiss spring chases the finger, and the scale is
        // finger-driven, so neither may be parked mid-gesture.
        if live == NO_CARD {
            self.scale.step_scaled(dt);
        } else {
            self.scale.step(dt);
        }
        for i in 0..MAX_TASKS {
            self.dismiss[i].step(dt);
            self.reflow[i].step(dt);
            self.effects[i].step(dt);
        }
        self.park_expired(frame_ms);
        let mut out = KillQueue::EMPTY;
        self.escalate(&mut out);
        out
    }

    /// Force every spring past its settle window onto its target.
    ///
    /// A spring is at rest when `|value - target| <= value_threshold` *and*
    /// `|velocity| <= 50 * value_threshold`. An underdamped response only ever
    /// approaches that, and an `f32` value can stall a fraction of an ULP short
    /// of a threshold finer than its own resolution -- both documented on
    /// [`SpringSimulation::settle_duration`]. Either way the shell would keep
    /// animating a card that has visually stopped, which is the bug the deleted
    /// `RecentsCarousel` could not have had because it had no spring at all.
    ///
    /// The deadline comes from `settle_duration`, which searches the closed
    /// form for the first time the response is provably inside both
    /// thresholds, and is measured against [`Self::now_ms`] so it costs one
    /// search per animation rather than one per frame. Arming is lazy and
    /// keyed on "found moving", which keeps the search count at one per
    /// animation even though every spring field is public and callers are
    /// expected to write them.
    ///
    /// Springs belonging to a live drag are exempted: they track a finger that
    /// is still moving, so a settle deadline on either would freeze the card
    /// under the user's finger. Exempt slots are disarmed, so the deadline is
    /// recomputed on the first frame after the release.
    pub fn park_expired(&mut self, frame_ms: f32) {
        let now = self.now_ms;
        let live = self.dragging;
        let mut budget = self.park[P_SCALE];
        if live == NO_CARD && settle_expired_scaled(&self.scale, &mut budget, now, frame_ms) {
            park_now(&mut self.scale, &mut budget);
        } else if live != NO_CARD {
            budget = PARK_UNARMED;
        }
        self.park[P_SCALE] = budget;

        for i in 0..MAX_TASKS {
            if i == live as usize {
                budget = PARK_UNARMED;
            } else {
                budget = self.park[P_DISMISS + i];
                if settle_expired(&self.dismiss[i], &mut budget, now, frame_ms) {
                    park_now(&mut self.dismiss[i], &mut budget);
                }
            }
            self.park[P_DISMISS + i] = budget;

            budget = self.park[P_REFLOW + i];
            if settle_expired(&self.reflow[i], &mut budget, now, frame_ms) {
                park_now(&mut self.reflow[i], &mut budget);
            }
            self.park[P_REFLOW + i] = budget;

            budget = self.park[P_EFFECTS + i];
            if settle_expired(&self.effects[i], &mut budget, now, frame_ms) {
                park_now(&mut self.effects[i], &mut budget);
            }
            self.park[P_EFFECTS + i] = budget;
        }
    }

    // ---------------------------------------------------------------------
    // Queries
    // ---------------------------------------------------------------------

    /// The cards the renderer has to draw: at most two, being the focused card
    /// and the one that can slide in beside it. Empty when there are no cards.
    pub fn cull_range(&self) -> core::ops::Range<u8> {
        if self.len == 0 {
            return 0..0;
        }
        let first = self.visible_card.min(self.len - 1);
        first..(first + 2).min(self.len)
    }

    /// Where card `i` is drawn this frame, including its dismiss and reflow
    /// offsets and the leftover scrub residue. `None` if `i` is not live.
    ///
    /// `l` supplies the card centre. `l` must be that panel's
    /// [`RecentsLayout`], and its card box must match this panel's -- both are
    /// the same values either way, so this is the
    /// [`Self::card_rect_centered`] spelling of the same rect rather than a
    /// second implementation of it. The overview scale is deliberately *not*
    /// applied here -- it is the same for every card, so the renderer applies it
    /// once to the whole stack rather than once per card.
    pub fn card_rect(&self, i: usize, l: &RecentsLayout) -> Option<Rect> {
        self.card_box(i, l.card_center.0, l.card_center.1)
    }

    /// The rect this [`Recents`] draws for card `i`, around the card centre
    /// `(cx, cy)`. `None` if `i` is not live.
    ///
    /// The single source of truth for both [`Self::card_rect`] and
    /// [`Self::card_rect_centered`], which differ only in where they read the
    /// centre from: the layout's own card centre, or half the framebuffer.
    /// Those are the same point on the panel `RecentsLayout` builds, so the
    /// two spellings cannot drift apart horizontally or vertically.
    ///
    /// The card box (`w`, `h`, `radius`) is read from this struct's own copy of
    /// that layout rather than from a caller's, which is what lets the renderer
    /// ask for a rect in framebuffer pixels without being handed a layout at
    /// all. The overview scale is deliberately *not* applied here -- it is the
    /// same for every card, so the renderer applies it once to the whole stack
    /// rather than once per card.
    #[inline]
    fn card_box(&self, i: usize, cx: f32, cy: f32) -> Option<Rect> {
        if i >= self.len as usize {
            return None;
        }
        // The slot is measured from the *selected* card, and the reflow spring
        // carries whatever animation the model is running. `drag_px` is the
        // scrub residue, applied to the whole strip.
        let slot = (i as f32 - self.selected as f32) * self.pitch + self.reflow[i].value;
        Some(Rect {
            x: cx + self.drag_px + slot - self.card_w * 0.5,
            y: cy + self.dismiss[i].value - self.card_h * 0.5,
            w: self.card_w,
            h: self.card_h,
            radius: self.card_radius,
        })
    }

    /// Screen rect of card `i` in the overview carousel, or `None` if `i` is
    /// out of range. Card `self.visible_card` is centered on the viewport.
    ///
    /// `wf`/`hf` are the viewport's pixel size, so the caller does not have to
    /// hold a [`RecentsLayout`] to place a card: the frame loop passes the
    /// framebuffer dimensions straight through. The card is centred by its
    /// *centre* x on `wf * 0.5` -- not its left edge, which is the error the
    /// ad-hoc `first_x = wf*0.5 - scroll*pitch - pitch*0.5` form makes, because
    /// a card whose left edge is at `wf * 0.5` puts the whole card to the right
    /// of centre. Card `i` then sits one [`Self::pitch`] per index away from
    /// the focused card, and the scrub residue ([`Self::carousel_scroll`]) is
    /// honoured so the strip tracks the finger while the user drags.
    ///
    /// Centring is on [`Self::selected`], which every mutation of this struct
    /// keeps equal to [`Self::visible_card`] -- the focused card and the card
    /// the renderer must draw are the same card, so the two names cannot
    /// disagree about which one is in the middle.
    ///
    /// `None` for any `i >= len`, so a stale index from a stack that has since
    /// shrunk is a miss, not a panic.
    pub fn card_rect_centered(&self, i: usize, wf: f32, hf: f32) -> Option<Rect> {
        self.card_box(i, wf * 0.5, hf * 0.5)
    }

    /// The carousel's horizontal scroll offset, px, as already applied by
    /// [`Self::card_rect_centered`].
    ///
    /// This is the live scrub residue ([`Self::drag_px`]): it is finger
    /// tracking, not an animation, so it is zero at rest and non-zero only
    /// mid-drag. It is in **pixels**, not card widths.
    ///
    /// A renderer that places cards itself must add it to the card centre
    /// *unmultiplied*. Scaling it by the pitch again is how a scroll residue
    /// becomes a screen-width jump: `drag_px` already counts pixels, so
    /// `drag_px * pitch` is a square of the distance travelled.
    ///
    /// [`Self::card_rect_centered`] folds this in already. Read this only for a
    /// caller that composes its own offset *on top of* a rect the model
    /// returned, and never alongside `card_rect_centered`'s own output.
    #[inline]
    pub fn carousel_scroll(&self) -> f32 {
        self.drag_px
    }

    /// Signed dismiss fraction of card `i`, -1..=1, positive when dragging up.
    ///
    /// Signed, because the reference's is.
    /// `TaskViewDismissTouchController.kt:280` computes
    /// `displacement / (dismissLength * verticalFactor)`, and in portrait
    /// `verticalFactor` is -1 (`PortraitPagedViewHandler.kt:308`) while "up"
    /// is a negative `displacement` (`:303`). The two negatives cancel, so an
    /// upward drag is a *positive* fraction and a downward one is negative.
    /// That sign is load-bearing: [`dismiss_recents_scale`]'s first arm is
    /// `dismissFraction <= 0 -> RECENTS_SCALE_DEFAULT`, commented "do not
    /// scale recents when dragging below origin" (`:355-357`), so taking
    /// `abs` here would shrink the overview when a card is dragged *down*.
    ///
    /// Use [`Self::dismiss_travel`] for the threshold test, which *is* an
    /// absolute comparison in the reference (`abs(currentDisplacement) >
    /// abs(0.5 * dismissLength)`, `:315`).
    fn dismiss_fraction(&self, i: usize) -> f32 {
        if self.dismiss_length <= 0.0 {
            return 0.0;
        }
        -self.cards[i].dismiss_y / self.dismiss_length
    }

    /// Absolute dismiss travel of card `i`, 0..=1.
    ///
    /// The threshold and the haptic band are absolute in the reference
    /// (`:315`, `:294`), unlike the scale ladder.
    fn dismiss_travel(&self, i: usize) -> f32 {
        self.dismiss_fraction(i).abs()
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn index_of(&self, pid: i32) -> Option<usize> {
        (0..self.len as usize).find(|&i| self.cards[i].pid == pid)
    }

    /// Remove index `at`, compacting cards and their spring banks, and
    /// animating the gap closed.
    fn take(&mut self, at: usize) -> TaskCard {
        let card = self.cards[at];
        let len = self.len as usize;
        for i in at..len - 1 {
            self.cards[i] = self.cards[i + 1];
            self.dismiss[i] = self.dismiss[i + 1];
            self.reflow[i] = self.reflow[i + 1];
            self.effects[i] = self.effects[i + 1];
        }
        // The vacated slot becomes a dead card with at-rest springs. That is
        // not cosmetic: a slot left mid-flight would keep `park_expired`
        // spending settle searches on a card that does not exist.
        self.cards[len - 1] = TaskCard::new(0, 0, 0);
        self.dismiss[len - 1] = SpringSimulation::new(0.0, 0.0, SpringConfig::task_dismiss());
        self.reflow[len - 1] = SpringSimulation::new(0.0, 0.0, SpringConfig::grid_reflow());
        self.effects[len - 1] = SpringSimulation::new(0.0, 0.0, SpringConfig::dismiss_effects());
        self.park[P_DISMISS + len - 1] = PARK_UNARMED;
        self.park[P_REFLOW + len - 1] = PARK_UNARMED;
        self.park[P_EFFECTS + len - 1] = PARK_UNARMED;
        self.len = (len - 1) as u8;

        // Focus follows the card, not the index: a card before the selected
        // one shifts down, and losing the selected card clamps.
        let prev = self.selected;
        let next = if (at as u8) < prev { prev - 1 } else { prev }.min(self.len.saturating_sub(1));
        self.selected = next;
        self.arm_reflow_remove(at, prev, next);
        self.visible_card = next;
        if self.dragging != NO_CARD && at <= self.dragging as usize {
            self.dragging = if at == self.dragging as usize {
                NO_CARD
            } else {
                self.dragging - 1
            };
        }
        card
    }

    /// Insert `card` at the front, evicting the oldest if the stack is full.
    fn insert_front(&mut self, card: TaskCard) -> Option<TaskCard> {
        let n = self.len as usize;
        let evicted = if n == MAX_TASKS {
            Some(self.cards[MAX_TASKS - 1])
        } else {
            None
        };
        let top = n.min(MAX_TASKS - 1);
        for i in (1..=top).rev() {
            self.cards[i] = self.cards[i - 1];
            self.dismiss[i] = self.dismiss[i - 1];
            self.reflow[i] = self.reflow[i - 1];
            self.effects[i] = self.effects[i - 1];
            self.park[P_DISMISS + i] = self.park[P_DISMISS + i - 1];
            self.park[P_REFLOW + i] = self.park[P_REFLOW + i - 1];
            self.park[P_EFFECTS + i] = self.park[P_EFFECTS + i - 1];
        }
        self.cards[0] = card;
        self.dismiss[0] = SpringSimulation::new(0.0, 0.0, SpringConfig::task_dismiss());
        self.reflow[0] = SpringSimulation::new(0.0, 0.0, SpringConfig::grid_reflow());
        self.effects[0] = SpringSimulation::new(0.0, 0.0, SpringConfig::dismiss_effects());
        self.park[P_DISMISS] = PARK_UNARMED;
        self.park[P_REFLOW] = PARK_UNARMED;
        self.park[P_EFFECTS] = PARK_UNARMED;
        let prev = self.selected;
        self.len = (n + 1).min(MAX_TASKS) as u8;
        self.selected = 0;
        self.arm_reflow_insert(prev);
        self.visible_card = 0;
        if self.dragging != NO_CARD {
            self.dragging += 1;
        }
        evicted
    }

    /// Begin the kill grace clock for card `i`. Returns true if the close
    /// request was armed (i.e. the card crossed the dismiss threshold), false
    /// if it snapped back.
    ///
    /// On success the card is [`KillState::Grace`] with
    /// [`TaskCard::grace_started_ms`] set to [`Self::now_ms`], so [`Self::step`]
    /// escalates it to [`KillState::Forced`] and a [`KillAction::Force`] after
    /// [`KILL_GRACE_MS`] whether or not the shell acknowledges the close. The
    /// call is refused -- `false`, no state touched -- for an index past
    /// [`Self::len`], a pinned card ([`TaskCard::dismissable`] is `false`), and
    /// a card that is already closing. Refusing rather than re-arming is what
    /// keeps a card's grace clock monotonic and its close request issued once.
    ///
    /// Arming is deliberately *not* the threshold test. The threshold belongs
    /// to the release ([`Self::on_card_release`], `dismissLength * 0.5` at
    /// `TaskViewDismissTouchController.kt:315`), which is the only place that
    /// knows the finger's travel; [`Self::clear_all`] arms every card with no
    /// drag at all. What lives here is the transition both of those share.
    ///
    /// A card cannot be closing and being dragged, so arming also drops any
    /// live vertical drag and the scrub residue with it.
    pub fn arm_close(&mut self, i: usize) -> bool {
        if i >= self.len as usize {
            return false;
        }
        let card = &mut self.cards[i];
        if !card.dismissable || card.kill != KillState::Idle {
            return false;
        }
        card.kill = KillState::Grace;
        card.grace_started_ms = self.now_ms;
        // A card cannot be closing and being dragged.
        self.dragging = NO_CARD;
        self.drag_px = 0.0;
        true
    }

    /// Promote every card whose grace period has expired and collect the
    /// escalations.
    fn escalate(&mut self, out: &mut KillQueue) {
        for i in 0..self.len as usize {
            let card = &mut self.cards[i];
            if card.kill == KillState::Grace && self.now_ms - card.grace_started_ms >= KILL_GRACE_MS
            {
                card.kill = KillState::Forced;
                out.push(KillAction::Force(card.pid));
            }
        }
    }

    /// Re-aim the whole dismiss bank around a release of card `i`.
    ///
    /// `RecentsDismissUtils.kt:1382` adds 0.15 to the damping ratio of every
    /// task further from the dismissed one, so a five-card stack settles
    /// progressively softer instead of all landing on the same frame. Cards
    /// nearer than `i` have nothing to settle, but they are re-created rather
    /// than left holding a stale profile from an earlier dismissal.
    fn aim_dismiss_bank(&mut self, i: usize, committing: bool, velocity: f32) {
        for j in 0..MAX_TASKS {
            let hops = j.abs_diff(i) as u8;
            let config = SpringConfig::task_dismiss_with_hops(hops);
            let leaving = j == i && committing;
            // Seed from the rendered position, not from zero, or a release
            // half-way through the drag would teleport the card first.
            self.dismiss[j] = SpringSimulation::new(self.dismiss[j].value, 0.0, config)
                .with_velocity(if leaving { velocity } else { 0.0 });
            if leaving {
                self.dismiss[j].set_target(-self.dismiss_length);
            }
            self.park[P_DISMISS + j] = PARK_UNARMED;
        }
    }

    /// Re-aim the reflow bank across a pure selection change.
    ///
    /// `card_rect` places card `j` at `(j - selected) * pitch + reflow[j]`, so
    /// holding its screen position fixed across `prev -> next` means adding
    /// `(next - prev) * pitch` to the spring it already holds. Carrying the
    /// existing value rather than resetting it is what lets an interrupted
    /// page change be redirected instead of restarted.
    fn arm_reflow_select(&mut self, prev: u8, next: u8) {
        let shift = (next as i32 - prev as i32) as f32 * self.pitch;
        for j in 0..MAX_TASKS {
            self.reflow[j].value += shift;
            self.reflow[j].set_target(0.0);
            self.park[P_REFLOW + j] = PARK_UNARMED;
        }
    }

    /// Re-aim the reflow bank across the removal of the card at index `at`.
    ///
    /// As in [`Self::arm_reflow_select`], plus one more pitch for every card
    /// that was *after* `at` and therefore shifted down an index.
    fn arm_reflow_remove(&mut self, at: usize, prev: u8, next: u8) {
        let base = (next as i32 - prev as i32) as f32 * self.pitch;
        for j in 0..MAX_TASKS {
            self.reflow[j].value += if j >= at { base + self.pitch } else { base };
            self.reflow[j].set_target(0.0);
            self.park[P_REFLOW + j] = PARK_UNARMED;
        }
    }

    /// Re-aim the reflow bank after a card was pushed onto the front.
    ///
    /// The inverse of [`Self::arm_reflow_remove`]: every surviving card moved
    /// up an index, so it slides the other way. The new front card has no
    /// previous screen position and is already at rest.
    fn arm_reflow_insert(&mut self, prev: u8) {
        let shift = -(prev as i32 + 1) as f32 * self.pitch;
        for j in 1..MAX_TASKS {
            self.reflow[j].value += shift;
            self.reflow[j].set_target(0.0);
            self.park[P_REFLOW + j] = PARK_UNARMED;
        }
    }

    /// Set `threshold_haptic_done` when the drag is inside the haptic band
    /// around the dismiss threshold and has not fired yet.
    ///
    /// Returns **the rising edge**, not the latch. The latch is what makes the
    /// pulse fire once; the edge is what tells the caller *this call* was the
    /// one that fired it. Conflating the two is how a caller polling the latch
    /// buzzes once per `ACTION_MOVE` -- a 30-frame drag is 30 pulses, which is
    /// both a 30x battery cost and a buzz train no user reads as a threshold.
    fn note_threshold_haptic(&mut self, i: usize) -> bool {
        if self.threshold_haptic_done {
            return false;
        }
        let threshold = DISMISS_THRESHOLD_FRACTION * self.dismiss_length;
        if (self.dismiss_travel(i) * self.dismiss_length - threshold).abs() <= self.haptic_range {
            self.threshold_haptic_done = true;
            return true;
        }
        false
    }
}

/// What releasing a card decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DismissOutcome {
    /// The card springs home. The springs are already re-aimed; nothing else
    /// is required of the caller.
    Cancelled { card: usize, pid: i32 },
    /// The card is leaving. Its kill state is [`KillState::Grace`], so it
    /// escalates on its own; the shell should release the surface when the
    /// close is acknowledged.
    Committed { card: usize, pid: i32 },
    /// The index is not live, the card is pinned, or it is already closing.
    Ignored,
}

// ===========================================================================
// Folder open
// ===========================================================================

/// How long the folder footer waits before it starts fading in, ms.
/// `FolderSpringAnimatorSet.FOLDER_NAME_ALPHA_DURATION` (`:52`).
pub const FOLDER_TITLE_DELAY_MS: f32 = 32.0;
/// Dark-theme scrim alpha behind an open folder.
/// `FolderSpringAnimatorSet.kt:337` (`finalScrimAlpha`), dark branch.
pub const FOLDER_SCRIM_ALPHA_DARK: f32 = 0.32;
/// Scale the workspace and hotseat settle at behind an open folder.
/// `FolderSpringAnimatorSet.LAUNCHER_SCALE` (`:51`).
pub const FOLDER_LAUNCHER_SCALE: f32 = 0.975;

/// Items an open folder can hold.
///
/// Ours, and for the reason every capacity in this file is ours: a `Vec` on
/// the touch path is what the plan forbids. The reference holds its folder
/// contents in views it inflates from the model
/// (`FolderPagedView.inflateChildren`, driven by `mOrganizer`'s
/// `getMaxItemsPerPage()` = `cols * rows`, `FolderGridOrganizer.java:92`) and
/// pages them without an upper bound; the pager is what makes an unbounded
/// count reachable rather than being a cap in disguise. 64 is the same bound
/// `launcher_state::MAX_FOLDER_ITEMS` uses, and the two must agree -- a folder
/// that the model can hold but the state file cannot is a folder whose last
/// item vanishes on the next boot.
pub const FOLDER_ITEMS: usize = 64;

/// Bytes of an inline app id.
///
/// The longest thing that reaches this is a catalogue id or a component key
/// (`com.android.providers.calendar/…`), and 31 covers those with room to
/// spare. An id that does not fit is **refused**, not truncated: a truncated
/// id would be a different app, and a folder holding a different app under a
/// name that looks right is worse than a folder that declined the add.
pub const FOLDER_ID_BYTES: usize = 32;

/// One folder member, inline.
///
/// A `[u8; 32]` rather than a `&str` because the container must stay `Copy`
/// with no lifetime and no allocation: `main.rs` holds the `FolderOpen`
/// across the whole daemon loop and hands `DrmInteractiveState::folder_apps`
/// a borrowed slice every frame, so a self-referential or owning string would
/// either pin a borrow into the render path or allocate per mutation.
///
/// The loss against `&str` is one thing only -- a caller that wants the
/// catalogue entry for item `i` has to look it up by id. That lookup is what
/// the shell already does per frame to build `folder_apps`, so it is not new
/// work.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FolderItem {
    buf: [u8; FOLDER_ID_BYTES],
    len: u8,
}

impl FolderItem {
    /// An empty slot. `folder_idx`-style: what a pre-filled array holds.
    pub const EMPTY: Self = Self {
        buf: [0; FOLDER_ID_BYTES],
        len: 0,
    };

    /// Borrow the id. `""` for an [`Self::EMPTY`] slot.
    #[inline]
    pub fn id(&self) -> &str {
        // Written only through [`Self::new`], which takes a `&str` and so
        // always leaves a whole number of whole characters behind, and by
        // [`Self::from_raw`] which is fed bytes that came from one. Safe
        // without a UTF-8 validation pass on every frame read.
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    /// Copy `id` in, or refuse it if it is empty or too long.
    #[inline]
    pub fn new(id: &str) -> Option<Self> {
        let b = id.as_bytes();
        if b.is_empty() || b.len() > FOLDER_ID_BYTES {
            return None;
        }
        let mut buf = [0u8; FOLDER_ID_BYTES];
        buf[..b.len()].copy_from_slice(b);
        Some(Self {
            buf,
            len: b.len() as u8,
        })
    }
}

impl core::fmt::Debug for FolderItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FolderItem({:?})", self.id())
    }
}

/// What dropping to `items.len() <= 1` means for the folder, as a value.
///
/// The reference makes this a side effect of three separate call sites --
/// `Folder.java:1129` (after an update), `:1331` (after a drop completes) and
/// `:1757` (after a removal) -- each of which calls
/// `replaceFolderWithFinalItem()` when `getItemCount() <= 1`. Returning the
/// decision instead of performing it keeps the mutation in the shell, where
/// the page cell and the folder record both live; a model that deleted the
/// folder itself could not express "and put the survivor *in that cell*",
/// which is half of what the reference's call does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderCollapse {
    /// The folder becomes its one remaining item. The shell must write
    /// `app_id` into the cell the folder occupied and delete the record.
    ///
    /// An **empty** folder cannot produce this: there is no survivor to put
    /// in the cell, and the reference's own
    /// `LauncherDelegate.replaceFolderWithFinalItem` bails on an empty
    /// folder rather than inventing an item.
    IntoApp { app_id: FolderItem },
    /// More than one item, or none at all: leave the folder alone.
    Keep,
}

/// An opening, or closing, folder.
///
/// Pure state: three springs, the index they belong to, and the folder's
/// contents. The scrim spring doubles as the workspace-scale spring because
/// the reference runs both with the *same* `STIFFNESS_LAUNCHER_SCRIM` /
/// `DAMPING_LAUNCHER_SCRIM` and the same zero start delay
/// (`FolderSpringAnimatorSet.kt:340-375`) -- only the endpoints differ, which
/// makes one spring plus a normalisation exact rather than approximate.
///
/// Fixed capacity and `Copy`, like everything else in this module: the
/// contents are a `[FolderItem; FOLDER_ITEMS]` and a length, not a `Vec`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderOpen {
    /// Which folder of the workspace is open.
    pub folder_idx: u8,
    /// Container transform progress, 0..1. `folder_morph`, 380 / 0.8.
    pub morph: SpringSimulation,
    /// Scrim alpha, 0..[`FOLDER_SCRIM_ALPHA_DARK`]. `folder_scrim`,
    /// 380 / 0.98.
    pub scrim: SpringSimulation,
    /// Footer alpha, 0..1. `folder_alpha`, 1600 / 0.9. Held at 0 until
    /// [`Self::title_delay_ms`] has run out, which is the reference's
    /// `FOLDER_NAME_ALPHA_DURATION` start delay (`:52`, applied at `:274-278`).
    pub title_alpha: SpringSimulation,
    /// Remaining start delay on [`Self::title_alpha`], ms.
    pub title_delay_ms: f32,
    /// Absolute parking deadline per spring, or [`PARK_UNARMED`].
    pub park: [f32; 3],
    /// Animation clock, ms. Advanced only by [`Self::step`].
    now_ms: f32,
    /// Members, rank order, live in `[0, item_count)`. Beyond that the array is
    /// stale and must not be read.
    items: [FolderItem; FOLDER_ITEMS],
    /// Live prefix length of [`Self::items`].
    item_count: u8,
    /// Which page of `items` the grid is showing. The reference's
    /// `FolderPagedView.getCurrentPage()`; `open` starts at page 0 and only
    /// `Folder.java:823`, `animateOpen(items, rank / itemsPerPage())`, ever
    /// opens onto another one.
    pub page: u8,
    /// Horizontal drag offset within the page, px, 0..=page width. Signed,
    /// because the reference's pager drags both ways before it commits.
    pub page_offset_x: f32,
    /// `cols * rows` of the grid showing this folder, from
    /// `FolderGridOrganizer.getMaxItemsPerPage()` (`FolderGridOrganizer.java:92`).
    /// Zero until the shell tells the model how big the grid is, which is why
    /// every paging accessor below treats it as at least 1.
    pub items_per_page: u8,
}

const F_MORPH: usize = 0;
const F_SCRIM: usize = 1;
const F_TITLE: usize = 2;

impl FolderOpen {
    /// A closed folder. `folder_idx` is kept so closing and reopening the
    /// same folder does not lose it.
    pub fn closed(folder_idx: u8) -> Self {
        Self {
            folder_idx,
            morph: SpringSimulation::new(0.0, 0.0, SpringConfig::folder_morph()),
            scrim: SpringSimulation::new(0.0, 0.0, SpringConfig::folder_scrim()),
            title_alpha: SpringSimulation::new(0.0, 0.0, SpringConfig::folder_alpha()),
            title_delay_ms: 0.0,
            park: [PARK_UNARMED; 3],
            now_ms: 0.0,
            items: [FolderItem::EMPTY; FOLDER_ITEMS],
            item_count: 0,
            page: 0,
            page_offset_x: 0.0,
            items_per_page: 1,
        }
    }

    /// Start opening folder `folder_idx`.
    ///
    /// The title's start delay is armed here and nowhere else; the reference
    /// delays it only when *opening* (`:278`), and closing is immediate.
    ///
    /// Contents are **not** cleared: [`Self::set_contents`] is how the shell
    /// fills a folder before opening it, and clearing here would throw that
    /// away. What *is* reset is the page state, because the reference's
    /// `FolderPagedView` calls `setCurrentPage(0)` whenever it drops its
    /// children (`FolderPagedView.java:483-490`) and a folder re-opened onto a
    /// stale page is a folder whose first tap hits nothing.
    pub fn open(&mut self, folder_idx: u8) {
        self.folder_idx = folder_idx;
        self.morph.set_target(1.0);
        self.scrim.set_target(FOLDER_SCRIM_ALPHA_DARK);
        self.title_alpha.set_target(1.0);
        self.title_delay_ms = FOLDER_TITLE_DELAY_MS;
        self.park = [PARK_UNARMED; 3];
        self.page = 0;
        self.page_offset_x = 0.0;
    }

    /// Start closing. No title delay: the footer goes with the container.
    pub fn close(&mut self) {
        self.morph.set_target(0.0);
        self.scrim.set_target(0.0);
        self.title_alpha.set_target(0.0);
        self.title_delay_ms = 0.0;
        self.park = [PARK_UNARMED; 3];
    }

    /// True once every spring is parked, so the shell can drop the folder.
    pub fn is_closed(&self) -> bool {
        self.morph.is_at_rest() && self.scrim.is_at_rest() && self.title_alpha.is_at_rest()
    }

    /// Advance the three springs by `dt`, honouring the title's start delay.
    ///
    /// The title spring is neither stepped nor armed while its delay is
    /// running, so the delay costs one comparison per frame instead of an
    /// integration, and the deadline is measured from the first frame *after*
    /// the delay expires rather than from before it.
    pub fn step(&mut self, dt: f32, frame_ms: f32) {
        self.now_ms += dt.max(0.0) * 1000.0;
        let now = self.now_ms;
        if self.title_delay_ms > 0.0 {
            self.title_delay_ms = (self.title_delay_ms - dt * 1000.0).max(0.0);
            self.park[F_TITLE] = PARK_UNARMED;
        } else {
            self.title_alpha.step(dt);
            let mut slot = self.park[F_TITLE];
            if settle_expired(&self.title_alpha, &mut slot, now, frame_ms) {
                park_now(&mut self.title_alpha, &mut slot);
            }
            self.park[F_TITLE] = slot;
        }
        self.morph.step(dt);
        let mut slot = self.park[F_MORPH];
        if settle_expired(&self.morph, &mut slot, now, frame_ms) {
            park_now(&mut self.morph, &mut slot);
        }
        self.park[F_MORPH] = slot;
        self.scrim.step(dt);
        let mut slot = self.park[F_SCRIM];
        if settle_expired(&self.scrim, &mut slot, now, frame_ms) {
            park_now(&mut self.scrim, &mut slot);
        }
        self.park[F_SCRIM] = slot;
    }

    /// Scale the workspace and hotseat are at behind this folder.
    ///
    /// The reference plays the scrim and the workspace/hotseat scale as two
    /// animations with identical spring constants and identical zero start
    /// delay, differing only in their endpoints
    /// (`FolderSpringAnimatorSet.kt:340-375`). Two runs of one spring with two
    /// endpoints are one run plus a linear map, so this is exact.
    pub fn workspace_scale(&self) -> f32 {
        let p = (self.scrim.value / FOLDER_SCRIM_ALPHA_DARK).clamp(0.0, 1.0);
        FOLDER_LAUNCHER_SCALE + (1.0 - FOLDER_LAUNCHER_SCALE) * (1.0 - p)
    }

    // -------------------------------------------------------------- contents
    //
    // Uncalled outside their tests in this pass. In `main.rs`:
    // `set_contents` where `folder_items` is built today (`main.rs:2228`)
    // from `state.folder(idx).items`; `add_item` / `remove_item` in the
    // drop-into-folder and drag-out-of-folder handlers; `collapse` at the end
    // of both, to decide whether to `folder_delete` and write the survivor
    // into the cell; `item_count` / `items` to fill
    // `DrmInteractiveState::folder_apps`; `items_per_page` from
    // `FolderLayout::items_per_page()` on open; and the paging methods from
    // the folder's touch handler.

    /// Replace the contents with `ids`, in rank order.
    ///
    /// The whole-list setter rather than only an appender, because that is
    /// what the reference does: `FolderService.updateFolderWithItems`
    /// (`data/folder/service/FolderService.kt:38-46`) *replaces* membership --
    /// `replaceFolderItems` deletes every row for the folder and re-inserts --
    /// and `FolderPagedView`'s `bindItems` (`FolderPagedView.java:460-492`)
    /// tears its children down and rebuilds them. A drag is therefore a
    /// read-modify-write of the list, not an edit, and a model that only
    /// appended could not express a reorder at all.
    ///
    /// Overflow is *dropped*, not grown: `ids` past [`FOLDER_ITEMS`] are
    /// ignored. `launcher_state::MAX_FOLDER_ITEMS` is the same number, so a
    /// folder the state file can hold is a folder this can hold.
    pub fn set_contents(&mut self, ids: &[&str]) {
        // Compacted into the front of the array rather than written at
        // `enumerate`'s index: an id that cannot be represented is *skipped*,
        // and leaving its slot holding the previous call's item would make a
        // stale app appear in the live prefix. Rank must stay dense for the
        // same reason `remove_item` shifts -- the pager divides it.
        let mut w = 0usize;
        for id in ids {
            // An id this model cannot represent is skipped rather than stored
            // truncated: see `FOLDER_ID_BYTES`.
            if w == FOLDER_ITEMS {
                break;
            }
            if let Some(item) = FolderItem::new(id) {
                self.items[w] = item;
                w += 1;
            }
        }
        for item in &mut self.items[w..] {
            *item = FolderItem::EMPTY;
        }
        self.item_count = w as u8;
        self.page = 0;
        self.page_offset_x = 0.0;
    }

    /// Append one app id. `false` if the folder is full or `id` cannot be
    /// represented.
    ///
    /// Appended, which is `rank = index`
    /// (`data/folder/service/FolderService.kt:39-43`) and is why a drop lands
    /// at the end of the visible grid rather than at the touch point.
    pub fn add_item(&mut self, id: &str) -> bool {
        let n = self.item_count as usize;
        if n >= FOLDER_ITEMS {
            return false;
        }
        let Some(item) = FolderItem::new(id) else {
            return false;
        };
        self.items[n] = item;
        self.item_count = n as u8 + 1;
        true
    }

    /// Remove the member at `index`, shifting the tail down.
    ///
    /// Shifting rather than blanking keeps rank dense, because rank *is* the
    /// index: the pager computes the page from
    /// `rank / mOrganizer.getMaxItemsPerPage()` (`FolderPagedView.java:327`)
    /// and a hole would put the wrong items on the wrong page.
    ///
    /// `false` if `index` is past the end -- an empty or full-width drag can
    /// produce one and it must not index the array.
    pub fn remove_item(&mut self, index: usize) -> bool {
        let n = self.item_count as usize;
        if index >= n {
            return false;
        }
        for i in index..n - 1 {
            self.items[i] = self.items[i + 1];
        }
        self.items[n - 1] = FolderItem::EMPTY;
        self.item_count = n as u8 - 1;
        // The last page can vanish, and then the current page is past the end.
        let last = self.page_count();
        if self.page as usize >= last {
            self.page = last.saturating_sub(1) as u8;
            self.page_offset_x = 0.0;
        }
        true
    }

    /// How many items the folder holds.
    #[inline]
    pub fn item_count(&self) -> usize {
        self.item_count as usize
    }

    /// The live members, in rank order. Never longer than
    /// [`Self::item_count`], and free of allocation.
    #[inline]
    pub fn items(&self) -> &[FolderItem] {
        &self.items[..self.item_count as usize]
    }

    /// The item at `index`, if it exists.
    #[inline]
    pub fn item(&self, index: usize) -> Option<FolderItem> {
        self.items().get(index).copied()
    }

    /// Whether the folder should collapse into its last item.
    ///
    /// The reference's rule, `getItemCount() <= 1`, at all three of its call
    /// sites (`Folder.java:1129`, `:1331`, `:1757`). It is `<=` and not `==`
    /// because a folder can reach one item by removal *or* by a completed
    /// drag, and both paths check the count rather than a flag.
    ///
    /// An empty folder returns [`FolderCollapse::Keep`]: the rule fires, but
    /// `replaceFolderWithFinalItem` has no "final item" to substitute, and
    /// `LauncherDelegate.replaceFolderWithFinalItem`
    /// (`folder/LauncherDelegate.java:160-162`) returns without doing
    /// anything for a folder it cannot reduce. Guessing a survivor here would
    /// put an app in a cell the user never chose.
    pub fn collapse(&self) -> FolderCollapse {
        match self.item_count {
            1 => FolderCollapse::IntoApp {
                app_id: self.items[0],
            },
            _ => FolderCollapse::Keep,
        }
    }

    // ---------------------------------------------------------------- paging
    //
    // Uncalled outside their tests in this pass. In `main.rs`: `page_count` /
    // `shows_page_indicator` to decide whether `drm_kms` draws the pager band,
    // `begin_page_drag` / `page_drag` / `end_page_drag` from the folder's
    // horizontal touch handler, `clamp_page` right after `items_per_page` is
    // assigned, and `page` is read by `drm_kms` to pick which items the grid
    // shows.

    /// `cols * rows`, floored at 1.
    ///
    /// Floored because the shell sets it after constructing the model and a
    /// zero would make every division by it a NaN -- the same guard
    /// `Layout::new_scaled` applies to `font_scale`.
    #[inline]
    pub fn items_per_page(&self) -> usize {
        (self.items_per_page as usize).max(1)
    }

    /// How many pages the contents occupy. `FolderPagedView.getPageCount()`'s
    /// content form, `ceil(n / (cols * rows))`.
    ///
    /// Empty is **one** page, not zero. The reference agrees:
    /// `getPageCount() > 0 ? ... : 0` (`FolderPagedView.java:507`) is the
    /// *desired width* branch, while `getDesiredHeight` returns 0 for zero
    /// pages (`:511-514`) -- and a `FolderOpen` always exists while a folder
    /// is being driven, so the shell needs "how many page positions are legal"
    /// and that is at least 1.
    #[inline]
    pub fn page_count(&self) -> usize {
        let n = self.item_count();
        n.div_ceil(self.items_per_page()).max(1)
    }

    /// Whether the footer shows page dots.
    ///
    /// The reference's exact rule, `getPageCount() > 1`
    /// (`FolderPagedView.java:496`, and `Folder.java:908` for the footer
    /// swap). A single page gets a centred title and no dots: drawing one dot
    /// for "page 1 of 1" is the single most common way a pager announces
    /// that it exists when it does not need to.
    #[inline]
    pub fn shows_page_indicator(&self) -> bool {
        self.page_count() > 1
    }

    /// Clamp the page state after the contents or the grid size changed.
    ///
    /// Separate from the setters because `items_per_page` is a `pub` field:
    /// the shell sets it directly when the profile's folder grid is
    /// reconfigured, and a folder that was on page 3 of a 3x3 grid is then on
    /// page 27 of a 2x2 one unless something clamps it. One call, at the
    /// configuration site, is cheaper than making the field private and
    /// adding a setter that does half the job.
    pub fn clamp_page(&mut self) {
        let last = self.page_count().saturating_sub(1);
        if self.page as usize > last {
            self.page = last as u8;
        }
        self.page_offset_x = 0.0;
    }

    /// Start a horizontal page drag `x` px from the current page's left edge.
    ///
    /// The page does not move yet. The reference's pager holds the content
    /// under the finger for the whole drag and only commits on release
    /// (`snapToPage` after `setCurrentPage`), and splitting that into two
    /// steps is what stops a single flick from advancing two pages -- which is
    /// what advancing in *both* `begin` and `end` would do.
    pub fn begin_page_drag(&mut self, x: f32) {
        self.page_offset_x = if x.is_finite() { x } else { 0.0 };
    }

    /// A page drag in progress: the drag offset, px.
    #[inline]
    pub fn page_drag(&self) -> f32 {
        self.page_offset_x
    }

    /// Commit a page drag: the page advances once if the offset passed half a
    /// page width, and the offset is cleared.
    ///
    /// `page_w` is one page's width in px, which the caller gets from
    /// `FolderLayout::grid`. Half is the reference's threshold
    /// (`PagedView` snaps when the drag passes `mFlingThreshold`, i.e. half the
    /// viewport), and evaluating it once on the *final* offset rather than on
    /// the peak is what makes a flick that was briefly past the threshold
    /// still land.
    pub fn end_page_drag(&mut self, page_w: f32) {
        let half = page_w.max(1.0) * 0.5;
        // Dragging *left* reveals the next page and dragging *right* goes
        // back, so the more negative offset advances and the more positive one
        // rewinds. Getting this pair the wrong way round is the most likely
        // mistake in a pager, and it feels right until the user tries it.
        if self.page_offset_x <= -half {
            self.page += 1;
        } else if self.page_offset_x >= half {
            self.page = self.page.saturating_sub(1);
        }
        self.page_offset_x = 0.0;
        self.clamp_page();
    }
}

// ===========================================================================
// Popup menu
// ===========================================================================

/// Deep shortcuts shown before the rest are dropped.
/// `PopupPopulator.MAX_SHORTCUTS` (`PopupPopulator.java:49`).
pub const MAX_DEEP_SHORTCUTS: u8 = 4;

/// Entries the reference popups can contain.
///
/// A flat `Copy` enum rather than a menu tree: the popups are one level deep,
/// and the deep shortcuts that would want a second level are flattened into
/// [`PopupItem::DeepShortcut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupItem {
    // Workspace long-press menu (`LauncherOptionsPopup.getLauncherOptions`).
    Wallpapers,
    Widgets,
    AllApps,
    HomeSettings,
    HomeScreenLock,
    EditMode,
    SystemSettings,
    DefaultPageForWorkspace,
    // Icon long-press menu (`SystemShortcut`, `LawnchairShortcut`).
    AppInfo,
    Install,
    Remove,
    Uninstall,
    Customize,
    OpenInStore,
    PauseApps,
    /// The `n`th deep shortcut of the icon.
    ///
    /// The index is bounded by [`PopupItems::push_shortcut`], so a
    /// `DeepShortcut` that reached the list by any other route is still
    /// bounded: there is no way to obtain one with `n >= 4` through the API
    /// this module offers.
    DeepShortcut(u8),
}

/// Fixed-capacity popup item list. A `Vec` here would allocate on the touch
/// path; the plan forbids it.
///
/// The capacity is 8 because that is the largest single popup the reference
/// can produce: the workspace long-press menu
/// (`LauncherOptionsPopup.DEFAULT_ORDER`) has exactly eight entries once
/// `carousel` -- a metadata-only option, filtered out of the built list at
/// `LauncherOptionsPopup.kt:145` -- is removed.
///
/// The icon long-press menu is 7 system entries plus up to
/// [`MAX_DEEP_SHORTCUTS`] deep shortcuts, so at that end the *capacity* binds
/// before the shortcut cap does. `push` reports the drop rather than growing
/// and `push_shortcut` reports the shortcut cap, so the shell can tell which
/// of the two it hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupItems {
    pub items: [PopupItem; 8],
    pub len: u8,
}

impl PopupItems {
    pub const EMPTY: Self = Self {
        // The filler is never read past `len`; `Wallpapers` is a real variant
        // so the array is a valid `PopupItems` at every point.
        items: [PopupItem::Wallpapers; 8],
        len: 0,
    };

    /// Append an item. `false` if the list is full.
    pub fn push(&mut self, item: PopupItem) -> bool {
        if self.len as usize >= self.items.len() {
            return false;
        }
        self.items[self.len as usize] = item;
        self.len += 1;
        true
    }

    /// Append the `n`th deep shortcut.
    ///
    /// `false` if `n` is out of range, if [`Self::push`] would overflow, or if
    /// [`MAX_DEEP_SHORTCUTS`] shortcuts are already listed. The count is a
    /// scan of at most 8 entries, which is cheaper than allocating a `Vec` to
    /// hold them and runs once per long-press rather than per frame.
    pub fn push_shortcut(&mut self, n: u8) -> bool {
        if n >= MAX_DEEP_SHORTCUTS {
            return false;
        }
        let listed = (0..self.len as usize)
            .filter(|&i| matches!(self.items[i], PopupItem::DeepShortcut(_)))
            .count() as u8;
        if listed >= MAX_DEEP_SHORTCUTS {
            return false;
        }
        self.push(PopupItem::DeepShortcut(n))
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Item `i`, or `None`.
    pub fn get(&self, i: usize) -> Option<PopupItem> {
        (i < self.len as usize).then(|| self.items[i])
    }

    pub fn iter(&self) -> impl Iterator<Item = &PopupItem> + '_ {
        self.items[..self.len as usize].iter()
    }
}

impl Default for PopupItems {
    fn default() -> Self {
        Self::EMPTY
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::layout::FolderLayout;

    const FRAME_MS: f32 = 1000.0 / 120.0;
    const PANEL: (f32, f32) = (1080.0, 2400.0);

    fn layout() -> Layout {
        Layout::new(PANEL.0, PANEL.1, false)
    }

    fn recents(n: u32) -> Recents {
        let mut r = Recents::new(&layout());
        for i in 0..n {
            r.push(TaskCard::new(i, 1000 + i as i32, 10 + i));
        }
        r
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-4
    }

    /// Step `r` for `seconds` at 120 Hz, collecting every kill action.
    fn run(r: &mut Recents, seconds: f32) -> KillQueue {
        let mut kills = KillQueue::EMPTY;
        for _ in 0..(seconds * 120.0).round() as u32 {
            for a in r.step(FRAME_MS / 1000.0, FRAME_MS).iter() {
                assert!(
                    kills.push(*a),
                    "the per-tick batch is per card and cannot overflow"
                );
            }
        }
        kills
    }

    /// Drag card `i` by `fraction` of the dismiss length, with no fling.
    fn drag(r: &mut Recents, i: usize, fraction: f32) {
        r.on_card_drag(i, -fraction * r.dismiss_length);
        r.cards[i].dismiss_v = 0.0;
    }

    /// Centre x of a rect, in the same px the rect is in.
    fn center_x(r: Rect) -> f32 {
        r.x + r.w * 0.5
    }

    /// Coordinate comparison at the scale the panel actually uses.
    ///
    /// `near` is a 1e-4 *tolerance*, which is right for a spring value and too
    /// tight for an absolute pixel coordinate: an `f32` near 10^3 has a ULP of
    /// about 6e-5, so two algebraically equal centre derivations
    /// (`(h - card_h) * 0.5 + card_h * 0.5` and `h * 0.5`) can land two ULP
    /// apart. 1e-3 px is ~16 ULP and four orders of magnitude below a pixel, so
    /// it cannot mask a real layout change while tolerating the rounding.
    fn near_px(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-3
    }

    // ---------------------------------------------------------------------

    #[test]
    fn card_rect_centered_puts_the_selected_card_on_the_viewport_centre() {
        let l = layout();
        let (wf, hf) = PANEL;
        let mut r = recents(3);

        // Selected 0: card 0's *centre* is at wf * 0.5, not its left edge. The
        // ad-hoc `first_x = wf * 0.5 - scroll * pitch - pitch * 0.5` form this
        // replaces put the left edge there, so the whole card sat half a card
        // to the right of centre.
        assert_eq!(r.selected, 0);
        let c0 = r.card_rect_centered(0, wf, hf).expect("live");
        assert!(
            near_px(center_x(c0), wf * 0.5),
            "centre was {}",
            center_x(c0)
        );
        assert!(near(c0.w, l.recents().card_w), "card width is layout's");

        // Selected 1: card 1 takes the centre. *Immediately* after the page
        // change, though, both cards are still where they were -- the reflow
        // spring is holding them there, which is the slide rather than a snap.
        // So the pitch is an assertion about the settled carousel, and the
        // in-between is asserted separately below.
        r.select(1);
        assert_eq!(r.selected, 1);
        assert_eq!(r.visible_card, 1, "focus and the cull hint move together");
        run(&mut r, 1.0);

        let c1 = r.card_rect_centered(1, wf, hf).expect("live");
        assert!(
            near_px(center_x(c1), wf * 0.5),
            "centre was {}",
            center_x(c1)
        );
        // Card 0 is exactly one pitch to its left. `pitch` is the
        // centre-to-centre step, so this is the same pitch the reflow springs
        // are armed with.
        let c0 = r.card_rect_centered(0, wf, hf).expect("live");
        assert!(
            near_px(center_x(c0), wf * 0.5 - r.pitch),
            "card 0 is at {} expected {}",
            center_x(c0),
            wf * 0.5 - r.pitch
        );
        // And card 2 is symmetric on the other side, so the pitch really is
        // the step and not an accident of the subtraction.
        let c2 = r.card_rect_centered(2, wf, hf).expect("live");
        assert!(near_px(center_x(c2), wf * 0.5 + r.pitch));
        // The settled neighbours are also exactly one pitch from each other,
        // i.e. the gap between cards is `spacing`, not zero.
        assert!(near_px(center_x(c2) - center_x(c1), r.pitch));

        // A page change slides rather than snaps, so the invariant has to hold
        // mid-animation too, or the selected card visibly jumps.
        let mut r = recents(3);
        r.select(1);
        for _ in 0..8 {
            r.step(FRAME_MS / 1000.0, FRAME_MS);
            let sel = r
                .card_rect_centered(usize::from(r.selected), wf, hf)
                .expect("live");
            assert!(
                (center_x(sel) - wf * 0.5).abs() < r.pitch,
                "selected card drifted off centre: {}",
                center_x(sel)
            );
        }
        // Settled, it is exactly centred again.
        run(&mut r, 1.0);
        let sel = r
            .card_rect_centered(usize::from(r.selected), wf, hf)
            .expect("live");
        assert!(near_px(center_x(sel), wf * 0.5), "did not settle on centre");

        // The scrub residue moves the whole strip, and it is reported in px.
        // `card_rect_centered` already applies it, so it must NOT be applied a
        // second time by the caller. The scrub is taken from a middle page so
        // it is not run through the end-of-list overscroll curve, which would
        // make the residue a damped fraction of the input.
        let mut r = recents(3);
        r.select(1);
        r.scrub(r.pitch * 0.25);
        assert_eq!(r.selected, 1, "quarter pitch does not page");
        assert!(
            near_px(r.carousel_scroll(), r.pitch * 0.25),
            "scroll is px, got {}",
            r.carousel_scroll()
        );
        assert!(
            near(r.carousel_scroll(), r.drag_px),
            "scroll is the residue"
        );
        // Every card shifts by the residue and by nothing else, so the strip
        // tracks the finger as a rigid body. Compared against the same rect
        // with the residue zeroed, which is the only difference between them.
        let mut unscrubbed = r;
        unscrubbed.drag_px = 0.0;
        for i in 0..r.len as usize {
            let off = center_x(r.card_rect_centered(i, wf, hf).expect("live"))
                - center_x(unscrubbed.card_rect_centered(i, wf, hf).expect("live"));
            assert!(near_px(off, r.carousel_scroll()), "card {i} residue: {off}");
        }
        // And it is reported raw, not pre-divided by the pitch. Dividing it
        // again is the exact error the old `scroll * pitch` form made.
        assert!(
            !near_px(r.carousel_scroll(), r.pitch * 0.25 / r.pitch),
            "the scroll offset must not be in card units"
        );

        // Out of range is a miss, not a panic -- a stale index from a stack
        // that has since shrunk is normal.
        let r = recents(3);
        assert!(r.card_rect_centered(3, wf, hf).is_none(), "past the end");
        assert!(
            r.card_rect_centered(usize::MAX, wf, hf).is_none(),
            "far past"
        );
        let empty = Recents::new(&l);
        assert!(empty.card_rect_centered(0, wf, hf).is_none(), "no cards");
        // At rest the residue is exactly zero, so a settled overview adds
        // nothing, and an empty one has nothing to scroll.
        let r = recents(0);
        assert!(near(r.carousel_scroll(), 0.0), "nothing to scroll");
    }

    #[test]
    fn the_focused_card_and_the_drawn_card_are_the_same_card() {
        // `card_rect_centered` centres `selected` and the renderer draws
        // `visible_card`. Those are two names for one thing, and if any
        // mutation of the stack ever moves one without the other the selected
        // card silently stops being the one in the middle.
        let mut r = recents(MAX_TASKS as u32);
        let check = |r: &Recents, what: &str| {
            assert_eq!(
                r.visible_card, r.selected,
                "{what}: focus {} but cull hint {}",
                r.selected, r.visible_card
            );
            assert!(r.selected < r.len.max(1), "{what}: focus past the end");
        };
        check(&r, "fresh");
        for i in 0..r.len as usize {
            r.select(i);
            check(&r, "select");
        }
        // Past the end clamps rather than leaving the cull hint behind.
        r.select(99);
        check(&r, "over-select");
        r.select(0);
        // A scrub pages, and pages both.
        r.scrub(r.pitch);
        check(&r, "scrub forward");
        r.scrub(-r.pitch);
        check(&r, "scrub back");
        // A removal follows the card the focus was on.
        r.remove_by_pid(r.cards[1].pid);
        check(&r, "remove below the focus");
        r.select(0);
        r.remove_by_pid(r.cards[0].pid);
        check(&r, "remove the focused card");
        // And so does an insertion.
        r.push(TaskCard::new(99, 4242, 1));
        check(&r, "push");
        // An empty stack still has a live cull hint of zero and draws nothing.
        let e = Recents::new(&layout());
        check(&e, "empty");
        assert!(e.cull_range().is_empty());
    }

    #[test]
    fn card_rect_centered_agrees_with_card_rect_on_y() {
        let l = layout();
        let rl = l.recents();
        // Every card, every selection, and the same y from both spellings. If
        // these drift, the overview jumps vertically the frame the renderer
        // switches helper, which is exactly the regression this pins.
        let mut r = recents(3);
        for sel in 0..3usize {
            r.select(sel);
            for i in 0..r.len as usize {
                let a = r.card_rect(i, &rl).expect("live");
                let b = r.card_rect_centered(i, l.w, l.h).expect("live");
                assert!(near_px(a.y, b.y), "card {i} sel {sel}: {} vs {}", a.y, b.y);
                assert!(near(a.h, b.h), "card {i} sel {sel}: height");
                assert!(near(a.w, b.w), "card {i} sel {sel}: width");
                assert!(near(a.radius, b.radius), "card {i} sel {sel}: radius");
                // x agrees too: `card_rect` reads the card centre off the
                // layout, which is the same viewport centre.
                assert!(near_px(a.x, b.x), "card {i} sel {sel}: {} vs {}", a.x, b.x);
            }
            // Out of range matches too: both are `None`.
            assert!(r.card_rect(3, &rl).is_none());
            assert!(r.card_rect_centered(3, l.w, l.h).is_none());
        }

        // The vertical placement really is the layout's, and a dismiss moves
        // it and nothing else.
        let mut r = recents(2);
        let base = r.card_rect_centered(0, l.w, l.h).expect("live");
        assert!(
            near_px(base.y, rl.card_center.1 - rl.card_h * 0.5),
            "the card is centred on the layout's card centre"
        );
        // A drag moves it up, and only vertically. The rendered y is the
        // dismiss *spring*, which tracks the finger 1:1 below the detach
        // threshold and follows it on a `magnetic_detach` spring above, so the
        // sub-threshold case is the exact one to pin here.
        let small = r.detach / r.dismiss_length * 0.5;
        drag(&mut r, 0, small);
        let dragged = r.card_rect_centered(0, l.w, l.h).expect("live");
        assert!(
            near_px(dragged.y, base.y - small * r.dismiss_length),
            "a sub-detach drag is 1:1"
        );
        assert!(near(dragged.w, base.w), "a drag does not resize the card");
        assert!(
            near_px(center_x(dragged), center_x(base)),
            "nor move it across"
        );
        // Past the detach threshold the same rect is spring-driven, so it
        // tracks the finger's *target* rather than the finger itself, and the
        // two differ -- which is the magnetic give the reference is after.
        let back = -r.dismiss_length * 0.5;
        drag(&mut r, 0, back);
        let sprung = r.card_rect_centered(0, l.w, l.h).expect("live");
        assert!(
            near(r.dismiss[0].target, r.cards[0].dismiss_y),
            "aimed at finger"
        );
        // y is the spring *value*, not the target, which is why the card lags
        // the finger once it detaches.
        assert!(
            near_px(sprung.y, l.h * 0.5 + r.dismiss[0].value - r.card_h * 0.5),
            "y is the rendered spring value"
        );
        assert!(sprung.y > base.y, "and the card is above where it started");
    }

    #[test]
    fn the_card_box_the_model_carries_is_the_layouts() {
        // `card_rect_centered` is handed framebuffer dimensions, not a
        // `RecentsLayout`, so the model carries its own copy of the card box.
        // If that copy ever drifted from the layout the renderer would draw
        // cards at the wrong size while every other layout-driven element
        // stayed correct -- a bug with no other symptom.
        let l = layout();
        let rl = l.recents();
        let r = recents(1);
        assert!(
            near(r.card_w, rl.card_w),
            "card width: {} vs {}",
            r.card_w,
            rl.card_w
        );
        assert!(near(r.card_h, rl.card_h), "card height");
        assert!(near(r.card_radius, rl.corner_r), "corner radius");
        // And the pitch the carousel steps by is the width plus the gap, from
        // the same layout, so a card is never overlapping its neighbour.
        assert!(near(r.pitch, rl.card_w + rl.spacing), "pitch");
        assert!(near(r.pitch, r.card_w + rl.spacing), "pitch uses the copy");
        assert!(near(r.pitch, 797.142_9), "756 px card + 16 dp gap on 1080");

        // The whole struct stays inline: no `Vec`, no `String`, no pointer to
        // grow. `Recents` is `Copy`, so the renderer can read it without
        // touching the heap, which is the property that made it worth keeping.
        assert!(
            core::mem::size_of::<Recents>() < 1024,
            "Recents grew to {} bytes",
            core::mem::size_of::<Recents>()
        );
        let copy = r; // `Copy`, not a clone: this compiles only if it is Copy.
        assert_eq!(copy, r);
    }

    // ---------------------------------------------------------------------

    #[test]
    fn recents_dismiss_scale_ladder() {
        // The three anchors, exactly.
        assert!(near(dismiss_recents_scale(0.0), 1.0));
        assert!(near(dismiss_recents_scale(0.2), 0.9875));
        assert!(near(dismiss_recents_scale(0.575), 0.975));

        // Below the origin is not a dismiss at all (`:355-357`).
        assert!(near(dismiss_recents_scale(-0.5), 1.0));

        // The two ramps are linear and hit the plateau values exactly at the
        // plateau edges rather than asymptotically.
        assert!(near(dismiss_recents_scale(0.1), (1.0 + 0.9875) * 0.5));
        assert!(near(dismiss_recents_scale(0.5), 0.9875));
        assert!(near(dismiss_recents_scale(0.5375), (0.9875 + 0.975) * 0.5));

        // The plateaus are FLAT, which is the whole reason they exist: a small
        // overshoot past 0.2 must not read as movement.
        for &(a, b) in &[(0.21_f32, 0.34_f32), (0.22, 0.49), (0.25, 0.4)] {
            assert_eq!(
                dismiss_recents_scale(a),
                dismiss_recents_scale(b),
                "plateau {a}..{b} is not flat"
            );
        }
        for &(a, b) in &[(0.6_f32, 0.9_f32), (0.58, 1.0), (0.575, 2.0)] {
            assert_eq!(
                dismiss_recents_scale(a),
                dismiss_recents_scale(b),
                "post-threshold hold {a}..{b} is not flat"
            );
        }

        // A release restores 1.0: the resting end of the ladder is the
        // overview's unscaled state, and `Recents` aims the scale spring
        // there rather than at a private constant.
        assert!(near(dismiss_recents_scale(0.0), DISMISS_SCALE_DEFAULT));
        let mut r = recents(2);
        drag(&mut r, 0, 0.1);
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { card: 0, pid: 1001 }
        );
        assert!(near(r.scale.target, dismiss_recents_scale(0.0)));

        // Unusable input is not "certainly dismissing".
        assert!(near(dismiss_recents_scale(f32::NAN), 1.0));
        assert!(near(dismiss_recents_scale(f32::INFINITY), 1.0));

        // Monotone non-increasing: the overview never grows as a card is
        // dragged away.
        let mut prev = dismiss_recents_scale(0.0);
        for step in 0..=600 {
            let v = dismiss_recents_scale(step as f32 / 1000.0);
            assert!(v <= prev + 1e-6, "ladder rose at fraction {step}/1000");
            prev = v;
        }
    }

    #[test]
    fn recents_reflow_springs() {
        // `task_dismiss` and its hop table. `RecentsDismissUtils.kt:1382` adds
        // 0.15 per hop; `SpringConfig::task_dismiss_with_hops` clamps at 1.0
        // because Android's `SpringAnimationBuilder` rejects `zeta >= 1`
        // outright (`:107-109`), so a five-card stack reaches the cap at the
        // third neighbour and stays there.
        assert_eq!(SpringConfig::task_dismiss().stiffness, 850.0);
        assert_eq!(SpringConfig::task_dismiss().damping_ratio, 0.65);
        for (hops, zeta) in [0.65_f32, 0.80, 0.95, 1.0, 1.0].iter().enumerate() {
            assert!(
                near(
                    SpringConfig::task_dismiss_with_hops(hops as u8).damping_ratio,
                    *zeta
                ),
                "hop {hops} zeta"
            );
            assert_eq!(
                SpringConfig::task_dismiss_with_hops(hops as u8).stiffness,
                850.0,
                "hop {hops} must not change stiffness"
            );
        }
        assert_eq!(SpringConfig::grid_reflow().stiffness, 2800.0);
        assert_eq!(SpringConfig::grid_reflow().damping_ratio, 0.8);
        assert_eq!(SpringConfig::magnetic_detach().stiffness, 800.0);
        assert_eq!(SpringConfig::magnetic_detach().damping_ratio, 0.95);
        assert_eq!(SpringConfig::dismiss_effects().stiffness, 1600.0);
        assert_eq!(SpringConfig::dismiss_effects().damping_ratio, 1.0);

        // The overview wires the hop table to real cards: commit card 0 of 3
        // and the further cards must carry the softer profiles.
        let mut r = recents(3);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION + 0.01);
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Committed { .. }
        ));

        for (i, zeta) in [0.65_f32, 0.80, 0.95].iter().enumerate() {
            assert!(near(r.dismiss[i].config.damping_ratio, *zeta), "card {i}");
            assert_eq!(r.dismiss[i].config.stiffness, 850.0, "card {i}");
            assert_eq!(r.reflow[i].config.stiffness, 2800.0, "card {i} reflow");
            assert_eq!(r.reflow[i].config.damping_ratio, 0.8, "card {i} reflow");
            assert_eq!(r.effects[i].config.stiffness, 1600.0, "card {i} effects");
        }
        // The dismissed card leaves through the top; its neighbours do not.
        assert!(near(r.dismiss[0].target, -r.dismiss_length));
        assert!(near(r.dismiss[1].target, 0.0));
        assert!(near(r.dismiss[2].target, 0.0));
        assert!(near(r.effects[0].target, 1.0));
        assert!(near(r.effects[1].target, 0.0));

        // A cancelled release aims everything home and springs the scale back
        // to 1.0.
        let mut r = recents(2);
        drag(&mut r, 0, 0.1);
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { card: 0, pid: 1001 }
        );
        assert!(near(r.dismiss[0].target, 0.0));
        assert!(near(r.dismiss[1].target, 0.0));
        assert!(near(r.scale.target, DISMISS_SCALE_DEFAULT));
        assert!(near(r.effects[0].target, 0.0));
        assert_eq!(r.dismiss[0].config.damping_ratio, 0.65);

        // The page change is a real reflow, not a snap: the cards that moved
        // are handed the offset that keeps them where they are and a zero
        // target, so they slide.
        let mut r = recents(3);
        let before = r.card_rect(0, &layout().recents()).expect("live");
        r.select(1);
        let held = r.card_rect(0, &layout().recents()).expect("live");
        assert!(near(held.x, before.x), "card 0 jumped instead of sliding");
        assert!(near(r.reflow[0].target, 0.0));
        assert!(
            near(r.reflow[0].value, r.pitch),
            "one pitch of slide expected"
        );
        // And it lands back in its slot.
        run(&mut r, 1.0);
        let after = r.card_rect(0, &layout().recents()).expect("live");
        assert!(
            (after.x - held.x).abs() > 1.0,
            "the reflow spring never ran, so the slide did not happen"
        );
    }

    #[test]
    fn recents_push_and_remove_never_exceed_max_tasks() {
        let mut r = recents(0);
        for i in 0..8u32 {
            r.push(TaskCard::new(i, 1000 + i as i32, 10 + i));
            assert!(r.len as usize <= MAX_TASKS, "grew past MAX_TASKS");
        }
        assert_eq!(r.len as usize, MAX_TASKS);
        // Policy: the five most recently used survive, most recent first, so
        // the survivors are the last five pushed.
        assert_eq!(
            r.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![1007, 1006, 1005, 1004, 1003]
        );

        // The evicted card comes back so the shell can free its surface.
        let mut r2 = recents(4);
        assert!(r2.push(TaskCard::new(9, 9999, 9)).is_none(), "not full yet");
        let evicted = r2
            .push(TaskCard::new(4, 1004, 4))
            .expect("a full stack evicts");
        assert_eq!(evicted.pid, 1000, "the oldest is the one that goes");
        assert_eq!(r2.len as usize, MAX_TASKS);

        // Re-pushing a live pid promotes it rather than duplicating it, and
        // hands back the stale entry. Exactly one of promotion and eviction
        // can happen at a time, because taking the stale entry frees a slot.
        let mut r3 = recents(MAX_TASKS as u32);
        let stale = r3.push(TaskCard::new(99, 1001, 77)).expect("stale entry");
        assert_eq!(stale.pid, 1001);
        assert_eq!(
            r3.len as usize, MAX_TASKS,
            "a promotion is not an insertion"
        );
        assert_eq!(r3.cards[0].pid, 1001, "most recent first");
        assert_eq!(r3.selected, 0, "the promoted card takes focus");
        assert_eq!(
            r3.iter().filter(|c| c.pid == 1001).count(),
            1,
            "no duplicate"
        );

        // Removal frees exactly one slot and keeps the rest in order.
        let mut r4 = recents(5);
        let gone = r4.remove_by_pid(1002).expect("present");
        assert_eq!(gone.pid, 1002);
        assert_eq!(r4.len, 4);
        assert_eq!(
            r4.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![1004, 1003, 1001, 1000]
        );
        assert!(
            r4.remove_by_pid(4242).is_none(),
            "an absent pid is not an error"
        );

        // The selected index follows the *card* it pointed at, not the slot.
        // `recents(5)` is [1004, 1003, 1002, 1001, 1000], so index 3 is
        // pid 1001.
        let mut r5 = recents(5);
        r5.select(3);
        assert_eq!(r5.selected, 3);
        assert_eq!(r5.cards[3].pid, 1001);
        // Removing a card *before* the focus shifts it down with its card.
        r5.remove_by_pid(1004);
        assert_eq!(r5.selected, 2, "focus follows the card, not the index");
        assert_eq!(r5.cards[2].pid, 1001, "and still points at the same app");
        // Removing a card *after* the focus leaves it alone.
        r5.remove_by_pid(1000);
        assert_eq!(r5.selected, 2, "a card after the focus does not move it");
        assert_eq!(r5.cards[2].pid, 1001);
        // Removing the focused card itself clamps rather than going stale, and
        // lands on the card that slid into its place.
        r5.remove_by_pid(1001);
        assert!(r5.selected < r5.len);
        // Selection is always a live index, even from a stale touch event.
        r5.select(99);
        assert!(r5.selected < r5.len);
        // The vacated slot is at rest, or parking would chase a dead card.
        let last = r5.len as usize;
        assert!(r5.dismiss[last].is_at_rest());
        assert!(r5.reflow[last].is_at_rest());
        assert!(r5.effects[last].is_at_rest());
    }

    #[test]
    fn recents_springs_park_within_bounded_time() {
        let mut r = recents(5);
        // Dirty every bank: a committed dismiss arms the dismiss springs at
        // their hop profiles and the effects spring, and the page change arms
        // the reflow springs and the recents scale.
        r.select(2);
        drag(&mut r, 2, DISMISS_THRESHOLD_FRACTION + 0.02);
        assert!(matches!(
            r.on_card_release(2),
            DismissOutcome::Committed { .. }
        ));
        r.scrub(r.pitch);
        // Sanity: something is genuinely mid-flight right now, so this test is
        // not passing on a struct that never animated.
        assert!(
            !r.scale.is_at_rest() && !r.reflow[0].is_at_rest(),
            "nothing was animating before the parking test ran"
        );

        let kills = run(&mut r, 2.0);
        assert_eq!(kills.len, 1, "one card escalated, exactly once");

        // Every spring in the struct reports at rest, not merely "close".
        assert!(r.scale.is_at_rest(), "scale is {}", r.scale.value);
        assert!(
            near(r.scale.value, r.scale.target),
            "scale not on its target"
        );
        assert!(
            near(r.scale.value, DISMISS_SCALE_DEFAULT),
            "scale must rest at 1.0"
        );
        for i in 0..MAX_TASKS {
            for (name, s) in [
                ("dismiss", &r.dismiss[i]),
                ("reflow", &r.reflow[i]),
                ("effects", &r.effects[i]),
            ] {
                assert!(s.is_at_rest(), "{name}[{i}] still moving: {}", s.value);
                assert!(near(s.value, s.target), "{name}[{i}] not on its target");
                assert!(near(s.velocity, 0.0), "{name}[{i}] still has velocity");
            }
        }

        // Parking is one-way: a further second must not un-park anything.
        let before = r.scale.value;
        run(&mut r, 1.0);
        assert!(near(r.scale.value, before), "a parked spring drifted");
        assert!(
            r.park.iter().all(|d| d.is_infinite()),
            "a slot stayed armed"
        );

        // A nonsensical frame time must not be able to disable parking: that
        // is the failure this mechanism exists to prevent.
        let mut r2 = recents(4);
        drag(&mut r2, 0, DISMISS_THRESHOLD_FRACTION + 0.02);
        let _ = r2.on_card_release(0);
        for _ in 0..240 {
            r2.step(FRAME_MS / 1000.0, f32::NAN);
        }
        for i in 0..MAX_TASKS {
            assert!(
                r2.dismiss[i].is_at_rest(),
                "dismiss[{i}] with a bad frame_ms"
            );
            assert!(near(r2.dismiss[i].value, r2.dismiss[i].target));
        }
        assert!(near(r2.scale.value, DISMISS_SCALE_DEFAULT));

        // A caller that writes a spring field directly still gets a deadline,
        // because arming is keyed on "found moving" rather than on this module
        // having started the animation.
        let mut r3 = recents(2);
        r3.dismiss[0].value = 900.0;
        r3.dismiss[0].target = 0.0;
        for _ in 0..240 {
            r3.step(FRAME_MS / 1000.0, FRAME_MS);
        }
        assert!(
            near(r3.dismiss[0].value, 0.0),
            "a poked spring must still park"
        );

        // A live drag is exempt. Past the detach threshold the card follows the
        // finger on a `magnetic_detach` spring, so it *lags* the finger by
        // construction; if parking applied, the card would be snapped onto the
        // finger and the magnetic give would disappear. The finger keeps
        // moving well past any settle duration, and stays inside the dismiss
        // clamp so the target genuinely advances every frame.
        let mut r4 = recents(2);
        assert!(
            r4.detach < r4.dismiss_length,
            "the panel must have a drag band"
        );
        r4.on_card_drag(0, -(r4.detach * 1.5));
        for i in 0..240 {
            // 3 px/frame at 120 Hz is 360 px/s, so the finger outruns the
            // spring and the lag is permanent rather than a converging tail.
            let travel = r4.detach * 1.5 + 3.0 * i as f32;
            assert!(travel < r4.dismiss_length, "the drag must stay in bounds");
            r4.on_card_drag(0, -3.0);
            r4.step(FRAME_MS / 1000.0, FRAME_MS);
            // `on_card_drag` aims the spring at the finger, so the target *is*
            // the finger. Parking would make the value equal the target.
            assert!(
                near(r4.dismiss[0].target, r4.cards[0].dismiss_y),
                "the spring is aimed at the finger"
            );
            assert!(
                r4.dismiss[0].value - r4.dismiss[0].target > 1.0,
                "a live drag must lag the finger, not be snapped onto it: value {} target {}",
                r4.dismiss[0].value,
                r4.dismiss[0].target
            );
        }
        // The release re-arms the deadline, and the card parks.
        r4.cards[0].dismiss_v = 0.0;
        let _ = r4.on_card_release(0);
        run(&mut r4, 2.0);
        assert!(
            r4.dismiss[0].is_at_rest(),
            "the release must re-arm parking"
        );
    }

    #[test]
    fn recents_culls_to_visible_plus_one() {
        for n in 0..=MAX_TASKS as u8 {
            let mut r = Recents::new(&layout());
            for i in 0..n {
                r.push(TaskCard::new(u32::from(i), 1000 + i32::from(i), 1));
            }
            for sel in 0..MAX_TASKS {
                r.select(sel);
                let range = r.cull_range();
                assert!(
                    range.len() <= 2,
                    "{n} cards, selected {sel}: cull range is {} wide",
                    range.len()
                );
                assert!(range.is_empty() || r.visible_card < n);
                assert!(range.is_empty() || range.contains(&r.visible_card));
                assert!(range.end <= n, "cull range runs past the live cards");
                assert_eq!(range.start, r.visible_card.min(n.saturating_sub(1)));
            }
        }

        // The documented frame-budget arithmetic, so the numbers in the type
        // doc cannot rot silently.
        let card_px = 0.70 * PANEL.0 * 0.70 * PANEL.1;
        let all = MAX_TASKS as f32 * card_px;
        assert!(near(card_px, 1_270_080.0), "card is {card_px} px");
        assert!(near(all, 6_350_400.0), "full stack is {all} px");
        assert!(all > 2.0 * 2.0 * card_px, "culling is meant to halve it");
        assert!(
            near(0.70 * PANEL.0, 756.0) && near(0.70 * PANEL.1, 1680.0),
            "the reference card is 756 x 1680"
        );
    }

    #[test]
    fn dismiss_outcome_commits_above_half_or_on_upward_fling() {
        // Past DISMISS_THRESHOLD_FRACTION with no fling. `recents(2)` is
        // [1001, 1000]: most recent first.
        let mut r = recents(2);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION + 0.01);
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Committed { card: 0, pid: 1001 }
        );
        assert_eq!(
            r.cards[0].kill,
            KillState::Grace,
            "a commit starts the clock"
        );

        // Exactly at the threshold commits. The reference's own test is
        // `abs(displacement) > abs(0.5 * dismissLength)`, so the boundary is
        // the caller's rounding, not ours; landing on it exactly is the
        // documented intent and is asserted rather than left to chance.
        let mut r = recents(1);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION);
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Committed { .. }
        ));

        // An upward fling commits from well under the threshold.
        let mut r = recents(2);
        r.on_card_drag(0, -r.dismiss_length * 0.1);
        r.cards[0].dismiss_v = -(r.fast_fling * 1.5);
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Committed { .. }
        ));

        // A downward fling never commits, whatever the distance.
        let mut r = recents(2);
        r.on_card_drag(0, -r.dismiss_length * 0.95);
        r.cards[0].dismiss_v = r.fast_fling * 2.0;
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { .. }
        ));

        // A pinned card cannot be committed at all.
        let mut r = recents(2);
        r.cards[0].dismissable = false;
        r.on_card_drag(0, -r.dismiss_length);
        r.cards[0].dismiss_v = -r.fast_fling * 2.0;
        assert_eq!(r.on_card_release(0), DismissOutcome::Ignored);
        assert_eq!(r.cards[0].kill, KillState::Idle);
        // And an out-of-range index is ignored, not a panic.
        assert_eq!(r.on_card_release(7), DismissOutcome::Ignored);
        assert!(
            !r.on_card_drag(7, -10.0),
            "an out-of-range drag is a no-op and fires no haptic edge"
        );
    }

    #[test]
    fn dismiss_outcome_cancels_below() {
        // `recents(3)` is [1002, 1001, 1000]: most recent first.
        let mut r = recents(3);
        assert_eq!(r.cards[0].pid, 1002);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION - 0.01);
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { card: 0, pid: 1002 }
        );
        assert_eq!(r.cards[0].kill, KillState::Idle, "a cancel must not close");
        // The springs are aimed home, not left where the finger was.
        assert!(near(r.dismiss[0].target, 0.0));
        assert!(r.dismiss[0].value <= 0.0, "still under the finger");
        assert!(near(r.effects[0].target, 0.0));
        assert_eq!(r.dragging, NO_CARD);

        // A release with no travel at all is also a cancel.
        let mut r = recents(2);
        r.on_card_drag(1, -1.0);
        r.cards[1].dismiss_v = 0.0;
        assert!(matches!(
            r.on_card_release(1),
            DismissOutcome::Cancelled { .. }
        ));

        // A sub-threshold upward fling is still a cancel, and the scale comes
        // back to 1.0 through the ladder's resting value.
        let mut r = recents(2);
        r.on_card_drag(0, -r.dismiss_length * 0.2);
        r.cards[0].dismiss_v = r.fast_fling * 0.5;
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { .. }
        ));
        assert!(near(r.scale.target, dismiss_recents_scale(0.0)));
        run(&mut r, 1.0);
        assert!(
            near(r.scale.value, DISMISS_SCALE_DEFAULT),
            "release restores 1.0"
        );

        // The overview scale tracked the ladder during the drag, plateaus and
        // all. Each `drag` is a delta, so the return leg has to travel all the
        // way back to the origin and past it.
        let mut r = recents(1);
        drag(&mut r, 0, 0.35);
        assert!(near(r.scale.value, DISMISS_SCALE_ON_CANCEL), "held plateau");
        drag(&mut r, 0, 0.6);
        assert!(
            near(r.scale.value, DISMISS_SCALE_ON_SUCCESS),
            "past the threshold"
        );
        // Back to 0.2 of upward travel: on the first ramp's plateau edge.
        drag(&mut r, 0, -0.75);
        assert!(
            near(r.scale.value, DISMISS_SCALE_ON_CANCEL),
            "0.2 of travel"
        );
        // Past the origin and below it: not a dismiss at all, so the overview
        // does not shrink. This is the sign that matters and the reason
        // `dismiss_fraction` is signed.
        drag(&mut r, 0, -0.5);
        assert!(
            r.cards[0].dismiss_y > 0.0,
            "the card is now below the origin"
        );
        assert!(
            near(r.scale.value, DISMISS_SCALE_DEFAULT),
            "dragging below the origin must not shrink the overview"
        );
        // Dragging back up re-enters the ladder on the first ramp. The card is
        // sitting on the 25 dp undershoot clamp, so the return leg has to
        // clear that much before anything else happens.
        r.on_card_drag(0, -r.undershoot);
        assert!(near(r.cards[0].dismiss_y, 0.0), "back to the origin");
        assert!(near(r.scale.value, DISMISS_SCALE_DEFAULT), "at the origin");
        r.on_card_drag(0, -r.dismiss_length * 0.1);
        assert!(r.cards[0].dismiss_y < 0.0, "above the origin again");
        assert!(
            r.scale.value < DISMISS_SCALE_DEFAULT && r.scale.value > DISMISS_SCALE_ON_CANCEL,
            "on the first ramp again: {}",
            r.scale.value
        );
    }

    #[test]
    fn dismiss_undershoot_clamps_to_25dp() {
        let l = layout();
        let rl = l.recents();
        let expect = l.profile().dp(DISMISS_UNDERSHOOT_DP);
        assert!(near(rl.dismiss_undershoot, expect), "the layout row moved");
        // 25 dp on a panel whose density is `w / 420` (DeviceProfile.dp).
        assert!(
            near(expect, l.profile().dp(1.0) * DISMISS_UNDERSHOOT_DP),
            "25 dp on a 1080 px panel"
        );

        let mut r = recents(2);
        // The dismiss-threshold haptic fires on the *rising edge*, once per
        // drag. Reading the `threshold_haptic_done` latch instead made
        // `utlc::overview_touch_move` pulse on every `ACTION_MOVE`, so a
        // 30-frame drag buzzed 30 times -- and the 30th frame of a long drag
        // can sit far past the threshold, where the latch is still true.
        {
            let mut e = recents(2);
            let step = e.dismiss_length / 30.0;
            let mut pulses = 0usize;
            for _ in 0..30 {
                if e.on_card_drag(0, -step) {
                    pulses += 1;
                }
            }
            assert_eq!(
                pulses, 1,
                "a 30-frame dismiss drag must pulse exactly once, not once per frame"
            );
            // And the latch is still set, which is precisely why the shell must
            // not read it: it stays true through the whole rest of the drag.
            assert!(
                e.threshold_haptic_done,
                "the latch is a 'has fired' flag, not an edge"
            );
            // Releasing rearms the latch, so a *cancelled* drag pulses again.
            //
            // The release above committed (the travel was the full dismiss
            // length), which kills the card -- a second drag on a dying card is
            // a no-op by design, so the rearm has to be checked on a release
            // that cancels.
            let mut c = recents(2);
            let short = c.dismiss_length / 60.0;
            let mut below = 0usize;
            for _ in 0..10 {
                if c.on_card_drag(0, -short) {
                    below += 1;
                }
            }
            assert_eq!(below, 0, "a drag that never reaches the band stays quiet");
            assert!(!c.threshold_haptic_done, "and leaves the latch clear");
            assert!(
                matches!(c.on_card_release(0), DismissOutcome::Cancelled { .. }),
                "a release short of the threshold must cancel, not commit"
            );
            let mut after = 0usize;
            for _ in 0..30 {
                if c.on_card_drag(0, -short * 2.0) {
                    after += 1;
                }
            }
            assert_eq!(after, 1, "a rearmed drag pulses once, not once per frame");
        }

        // Drag far past the origin: the travel stops at 25 dp.
        r.on_card_drag(0, 5000.0);
        assert!(
            near(r.cards[0].dismiss_y, expect),
            "got {}",
            r.cards[0].dismiss_y
        );
        r.on_card_drag(0, 5000.0);
        assert!(
            near(r.cards[0].dismiss_y, expect),
            "a second drag must not creep"
        );

        // Under the cap it tracks exactly.
        r.on_card_drag(0, -expect);
        assert!(near(r.cards[0].dismiss_y, 0.0));

        // Upward travel is bounded by the dismiss length, and the two bounds
        // are not the same number.
        let mut r = recents(2);
        r.on_card_drag(0, -5000.0);
        assert!(
            near(r.cards[0].dismiss_y, -r.dismiss_length),
            "upward clamp"
        );
        assert!(r.dismiss_length > 2.0 * expect, "the bounds should differ");

        // The detach threshold is the other side of the 25 dp bound and is
        // also layout-derived.
        assert!(near(rl.detach_dp, 72.0 * l.profile().dp(1.0)), "72 dp");
        assert!(
            r.detach > expect,
            "a card detaches before it undershoots 25 dp"
        );

        // Dragging a card that is already closing changes nothing, and it does
        // not steal the drag either.
        let mut r = recents(2);
        r.on_card_drag(0, -r.dismiss_length * 0.9);
        r.cards[0].dismiss_v = 0.0;
        let _ = r.on_card_release(0);
        assert!(near(r.cards[0].dismiss_y, 0.0));
        assert_eq!(r.dragging, NO_CARD);
        r.on_card_drag(0, 10.0);
        assert_eq!(r.cards[0].kill, KillState::Grace);
        assert!(
            near(r.cards[0].dismiss_y, 0.0),
            "a dying card must not move"
        );
        assert_eq!(r.dragging, NO_CARD, "and must not take the drag");
    }

    // ---------------------------------------------------------------------
    // Task-dismiss lifecycle
    // ---------------------------------------------------------------------

    /// Every pid a run collected as a [`KillAction::Force`], in order.
    fn forced_pids(q: &KillQueue) -> Vec<i32> {
        q.iter()
            .filter_map(|a| match a {
                KillAction::Force(p) => Some(*p),
                KillAction::Close(_) => None,
            })
            .collect()
    }

    #[test]
    fn swiping_a_card_past_the_threshold_closes_it_and_reflows_the_stack() {
        // `recents(3)` is [1002, 1001, 1000], most recent first, and the
        // pushes left the reflow springs mid-slide. Settle them so this test
        // measures the dismiss and not the push.
        let mut r = recents(3);
        run(&mut r, 1.0);
        assert_eq!(r.len, 3);
        assert_eq!(r.selected, 0, "the newest card takes focus");
        assert_eq!(r.visible_card, 0);

        // Drag card 0 up past the threshold, then release.
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION + 0.2);
        assert!(
            r.cards[0].dismiss_y.abs() / r.dismiss_length > DISMISS_THRESHOLD_FRACTION,
            "the drag really did cross the threshold, or the rest proves nothing"
        );
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Committed { card: 0, pid: 1002 }
        );

        // The threshold crossing is what armed the close: the card is in Grace
        // with its clock started, not merely flagged.
        assert_eq!(
            r.cards[0].kill,
            KillState::Grace,
            "a commit starts the clock"
        );
        assert!(
            near(r.cards[0].grace_started_ms, r.now_ms()),
            "the clock starts now, got {} vs {}",
            r.cards[0].grace_started_ms,
            r.now_ms()
        );
        // And `arm_close` is idempotent from here: the card is already closing,
        // so a second request is refused rather than re-arming the clock.
        assert!(!r.arm_close(0), "a closing card must not re-arm");
        assert!(
            near(r.cards[0].grace_started_ms, r.now_ms()),
            "and the clock did not move"
        );

        // The card animates out through the top, and the side effects run to
        // completion, which is what the renderer fades on.
        assert!(
            near(r.dismiss[0].target, -r.dismiss_length),
            "leaves upward"
        );
        assert!(near(r.effects[0].target, 1.0), "and finishes its effects");
        let first = r.step(FRAME_MS / 1000.0, FRAME_MS);
        assert_eq!(first.len, 0, "grace has not expired on the first frame");
        assert!(r.dismiss[0].value < 0.0, "and it is already moving");
        assert!(r.effects[0].value > 0.0, "effects are running");
        assert_eq!(r.len, 3, "the card stays until the shell takes it");

        // The grace clock runs in wall time, not frames, so the escalation
        // lands on the frame the clock expires and not before.
        assert_eq!(
            run(&mut r, KILL_GRACE_MS / 1000.0 - 0.06).len,
            0,
            "too early"
        );
        let late = run(&mut r, 0.1);
        assert_eq!(forced_pids(&late), vec![1002], "the swiped pid escalates");
        assert_eq!(r.cards[0].kill, KillState::Forced);

        // The shell then acknowledges and takes the card out, which is what
        // closes the gap. `arm_reflow_remove` hands every survivor the offset
        // that holds it where it is, so the neighbour slides one pitch inward
        // instead of teleporting.
        let gone = r.remove_by_pid(1002).expect("the swiped card is there");
        assert_eq!(gone.pid, 1002);
        assert_eq!(r.len, 2, "the stack shrank");
        assert_eq!(
            r.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![1001, 1000],
            "and kept its order"
        );
        assert!(
            near(r.reflow[0].value, r.pitch),
            "neighbour is held one pitch out"
        );
        assert!(near(r.reflow[1].value, r.pitch), "so is the one behind it");
        assert!(near(r.reflow[0].target, 0.0), "and both slide home");

        // It really slides: the survivor is drawn holding its old position on
        // the removal frame, then moves inward and lands in the selected slot.
        let held = r.card_rect_centered(0, PANEL.0, PANEL.1).expect("live");
        assert!(
            near(center_x(held), PANEL.0 * 0.5 + r.pitch),
            "held, not moved"
        );
        for _ in 0..6 {
            r.step(FRAME_MS / 1000.0, FRAME_MS);
            let mid = r.card_rect_centered(0, PANEL.0, PANEL.1).expect("live");
            assert!(
                center_x(mid) < center_x(held),
                "the neighbour has to travel inward, not sit still"
            );
        }
        run(&mut r, 1.0);
        let rest = r.card_rect_centered(0, PANEL.0, PANEL.1).expect("live");
        assert!(
            near_px(center_x(rest), PANEL.0 * 0.5),
            "and land on the centre"
        );
        // The vacated slot is at rest, so parking is not chasing a dead card.
        let last = r.len as usize;
        assert!(r.dismiss[last].is_at_rest() && r.reflow[last].is_at_rest());

        // A removed card is not escalated again: the kill fired once, on the
        // clock, and taking it out of the stack cannot queue a second kill.
        assert_eq!(
            run(&mut r, 1.0).len,
            0,
            "a removed card is not killed twice"
        );
    }

    #[test]
    fn a_drag_under_the_threshold_snaps_back_and_kills_nothing() {
        // The negative case, which is the one that matters: a dismiss gesture
        // that the user abandons must leave the stack exactly as it was.
        let mut r = recents(3);
        run(&mut r, 1.0);
        let before = r.iter().map(|c| c.pid).collect::<Vec<_>>();

        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION - 0.05);
        assert_eq!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { card: 0, pid: 1002 }
        );

        // No close was armed, and nothing is queued. `arm_close` is the public
        // entry point `clear_all` and the shell use, and it arms any live
        // dismissable card regardless of travel -- the *threshold* belongs to
        // the release, which is what decided to cancel here.
        assert_eq!(r.cards[0].kill, KillState::Idle, "a cancel must not close");
        assert!(!r.arm_close(99), "an out-of-range index is refused");
        // The springs are aimed home, not left where the finger was.
        assert!(near(r.dismiss[0].target, 0.0), "the card springs home");
        assert!(near(r.effects[0].target, 0.0), "with no side effects");
        assert!(
            near(r.scale.target, DISMISS_SCALE_DEFAULT),
            "and the scale rests"
        );

        // Run it out well past the grace window: a snapped-back card has no
        // clock, so nothing escalates.
        assert_eq!(run(&mut r, 2.0).len, 0, "a cancelled drag must not kill");
        assert_eq!(r.len, 3, "and must not remove anything");
        assert_eq!(
            r.iter().map(|c| c.pid).collect::<Vec<_>>(),
            before,
            "stack intact"
        );
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Idle, "pid {} is closing", c.pid);
        }

        // It really did land back in its slot, and its dismiss spring parked.
        let back = r.card_rect_centered(0, PANEL.0, PANEL.1).expect("live");
        assert!(
            near_px(back.y, PANEL.1 * 0.5 - r.card_h * 0.5),
            "back in the row"
        );
        assert!(r.dismiss[0].is_at_rest(), "dismiss parked");
        assert!(near(r.dismiss[0].value, 0.0), "and landed on the origin");

        // A flick *back* down is a cancel even from past the threshold, which
        // is the other half of `onDragEnd` (`:307-338`).
        let mut r = recents(2);
        r.on_card_drag(0, -r.dismiss_length * 0.95);
        r.cards[0].dismiss_v = r.fast_fling * 2.0;
        assert!(matches!(
            r.on_card_release(0),
            DismissOutcome::Cancelled { .. }
        ));
        assert_eq!(r.cards[0].kill, KillState::Idle);
        assert_eq!(run(&mut r, 2.0).len, 0, "a downward flick kills nothing");
    }

    #[test]
    fn clear_all_closes_every_card_and_is_idempotent() {
        let mut r = recents(3);
        run(&mut r, 1.0);
        let pids: Vec<i32> = r.iter().map(|c| c.pid).collect();

        // Every dismissable card is asked to close, once.
        let batch = r.clear_all();
        assert_eq!(batch.len, 3, "one Close per card");
        let mut closed: Vec<i32> = batch
            .iter()
            .map(|a| match a {
                KillAction::Close(p) => *p,
                KillAction::Force(_) => panic!("a fresh close is not an escalation"),
            })
            .collect();
        closed.sort_unstable();
        let mut want = pids.clone();
        want.sort_unstable();
        assert_eq!(closed, want, "every card, exactly once");
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Grace, "pid {} is closing", c.pid);
        }

        // Calling it again mid-grace is a no-op: no card is asked twice and no
        // clock is re-armed, which is what stops a repeatedly-invoked clear-all
        // from keeping a stuck app alive forever.
        let before: Vec<f32> = r.iter().map(|c| c.grace_started_ms).collect();
        assert!(r.clear_all().is_empty(), "nothing left to close");
        for (i, c) in r.iter().enumerate() {
            assert!(
                near(c.grace_started_ms, before[i]),
                "pid {} clock moved",
                c.pid
            );
        }

        // And every pid is eventually force-killed, on the same clock a swipe
        // uses. `clear_all` is a bulk request, not a different lifecycle.
        assert_eq!(run(&mut r, KILL_GRACE_MS / 1000.0 - 0.05).len, 0, "not yet");
        let mut forced = forced_pids(&run(&mut r, 0.1));
        forced.sort_unstable();
        assert_eq!(forced, want, "every card escalates to a kill");
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Forced);
        }
        // Once each: a further grace period is silent.
        assert_eq!(run(&mut r, 2.0).len, 0, "no card is killed twice");

        // The shell then takes them out, one at a time as each close is
        // acknowledged, and the stack drains to empty.
        for pid in &pids {
            assert!(r.remove_by_pid(*pid).is_some(), "pid {pid} is there");
        }
        assert_eq!(r.len, 0, "the stack is empty");
        assert!(r.cull_range().is_empty(), "and nothing is drawn");
        assert!(
            r.card_rect_centered(0, PANEL.0, PANEL.1).is_none(),
            "no card 0"
        );
        assert!(!r.arm_close(0), "and there is nothing left to arm");

        // Clearing an empty stack is a no-op, not a panic, and stays that way.
        let mut empty = Recents::new(&layout());
        assert!(
            empty.clear_all().is_empty(),
            "an empty stack closes nothing"
        );
        assert!(empty.clear_all().is_empty(), "and again");
        assert_eq!(empty.len, 0);
        // Clearing a stack that has drained is the same call, so a shell that
        // clears on every "clear all" gesture cannot fault.
        assert!(r.clear_all().is_empty(), "a drained stack is still empty");
        assert_eq!(r.len, 0);

        // A pinned card is skipped and never killed, while its neighbours are
        // not held up for it.
        let mut r = recents(3);
        r.cards[1].dismissable = false;
        assert_eq!(r.clear_all().len, 2, "a pinned card is not asked to close");
        let killed = forced_pids(&run(&mut r, 1.0));
        assert_eq!(killed.len(), 2, "its neighbours are");
        assert!(!killed.contains(&r.cards[1].pid), "the pinned one is not");
    }

    #[test]
    fn kill_state_machine_escalates_after_grace() {
        // Pure transition: nothing is signalled here, the escalation comes
        // back out as data.
        let mut r = recents(3);
        let batch = r.clear_all();
        assert_eq!(batch.len, 3, "one Close per dismissable card");
        let mut closed: Vec<i32> = batch
            .iter()
            .map(|a| match a {
                KillAction::Close(p) => *p,
                KillAction::Force(_) => panic!("a fresh close is not an escalation"),
            })
            .collect();
        closed.sort_unstable();
        assert_eq!(closed, vec![1000, 1001, 1002]);
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Grace);
            assert!(near(c.grace_started_ms, 0.0), "the clock starts at 0");
        }

        // Not yet: the grace period has not elapsed.
        let early = run(&mut r, KILL_GRACE_MS / 1000.0 - 0.05);
        assert_eq!(early.len, 0, "escalated before the grace period expired");
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Grace);
        }

        // Now it has.
        let late = run(&mut r, 0.1);
        let mut forced: Vec<i32> = late
            .iter()
            .map(|a| match a {
                KillAction::Force(p) => *p,
                KillAction::Close(_) => panic!("a close is not an escalation"),
            })
            .collect();
        forced.sort_unstable();
        assert_eq!(forced, vec![1000, 1001, 1002]);
        for c in r.iter() {
            assert_eq!(c.kill, KillState::Forced);
        }

        // And it escalates exactly once: another grace period is silent.
        assert_eq!(run(&mut r, 1.0).len, 0, "a card must not be killed twice");

        // `clear_all` on an already-dying stack is silent, so a card is never
        // asked to close twice and its grace clock is never re-armed.
        let mut r = recents(2);
        assert_eq!(r.clear_all().len, 2);
        run(&mut r, 0.1);
        let started = r.cards[0].grace_started_ms;
        assert_eq!(r.clear_all().len, 0, "a closing card must not re-arm");
        assert!(near(r.cards[0].grace_started_ms, started));

        // A pinned card is skipped by clear_all; its neighbours are not.
        let mut r = recents(3);
        r.cards[1].dismissable = false;
        assert_eq!(r.clear_all().len, 2);
        assert_eq!(r.cards[1].kill, KillState::Idle);
        assert_eq!(r.cards[0].kill, KillState::Grace);
        assert_eq!(r.cards[2].kill, KillState::Grace);

        // An empty stack produces nothing.
        assert!(Recents::new(&layout()).clear_all().is_empty());

        // A swiped card escalates on the same clock as a cleared one.
        let mut r = recents(2);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION + 0.02);
        let _ = r.on_card_release(0);
        assert_eq!(run(&mut r, 0.3).len, 0);
        assert_eq!(run(&mut r, 0.3).len, 1, "a swiped card escalates too");

        // The queue is a fixed buffer, not a growth point.
        let mut q = KillQueue::EMPTY;
        for i in 0..20i32 {
            assert_eq!(
                q.push(KillAction::Force(i)),
                (i as usize) < MAX_TASKS,
                "push {i}"
            );
        }
        assert_eq!(q.len as usize, MAX_TASKS);
        assert_eq!(q.iter().count(), MAX_TASKS);
    }

    #[test]
    fn popup_items_are_fixed_capacity_and_never_allocate() {
        // `size_of` is the proof that the list is inline: no `Vec`, no
        // `String`, no pointer to grow.
        assert_eq!(
            core::mem::size_of::<PopupItems>(),
            8 * core::mem::size_of::<PopupItem>() + core::mem::size_of::<u8>()
        );
        let mut p = PopupItems::EMPTY;
        for i in 0..20u8 {
            let _ = p.push(PopupItem::DeepShortcut(i % MAX_DEEP_SHORTCUTS));
        }
        assert_eq!(p.len, 8, "clamped at capacity, did not grow");
        assert_eq!(p.iter().count(), 8);
        assert!(p.get(8).is_none());
        assert_eq!(p.get(0), Some(PopupItem::DeepShortcut(0)));

        // The eight-entry workspace menu fills it exactly.
        let mut w = PopupItems::EMPTY;
        for item in [
            PopupItem::Wallpapers,
            PopupItem::Widgets,
            PopupItem::AllApps,
            PopupItem::HomeSettings,
            PopupItem::HomeScreenLock,
            PopupItem::EditMode,
            PopupItem::SystemSettings,
            PopupItem::DefaultPageForWorkspace,
        ] {
            assert!(w.push(item));
        }
        assert_eq!(w.len, 8);
        assert!(!w.push(PopupItem::AppInfo), "a ninth item is refused");

        // Shortcut cap: 4, and only 4.
        let mut s = PopupItems::EMPTY;
        assert!(s.push(PopupItem::AppInfo));
        for n in 0..MAX_DEEP_SHORTCUTS {
            assert!(s.push_shortcut(n), "shortcut {n}");
        }
        assert!(!s.push_shortcut(MAX_DEEP_SHORTCUTS), "a fifth is refused");
        assert!(!s.push_shortcut(9), "an out-of-range index is refused");
        assert_eq!(
            s.iter()
                .filter(|i| matches!(i, PopupItem::DeepShortcut(_)))
                .count(),
            4
        );

        // The whole icon menu plus its shortcuts, in one fixed buffer: the
        // capacity binds before the shortcut cap, and the drop is reported.
        let mut m = PopupItems::EMPTY;
        for item in [
            PopupItem::AppInfo,
            PopupItem::Install,
            PopupItem::Remove,
            PopupItem::Uninstall,
            PopupItem::Customize,
            PopupItem::OpenInStore,
            PopupItem::PauseApps,
        ] {
            assert!(m.push(item));
        }
        assert!(m.push_shortcut(0), "room for one shortcut");
        assert!(
            !m.push_shortcut(1),
            "the 8-slot buffer is the binding limit"
        );
        assert_eq!(m.len, 8);

        // No variant can hold a `String` or a pointer, so the list is a
        // `u8` discriminant plus at most a `u8` shortcut index. A `Split
        // screen` or `Battery saving mode` variant could not be constructed
        // here at all, because the reference has neither.
        assert_eq!(
            core::mem::size_of::<PopupItem>(),
            2,
            "a PopupItem must stay a bare discriminant plus a u8"
        );

        // Empty list behaviour.
        let e = PopupItems::default();
        assert!(e.is_empty());
        assert_eq!(e.iter().count(), 0);
        assert!(e.get(0).is_none());

        // The scrub's overscroll curve, to confirm the end resistance the
        // scrub reuses is the sourced one and is a function, not something
        // this module has to re-implement.
        assert!(near(damped_scroll(0.0, 100.0), 0.0));
        assert!(near(damped_scroll(100.0, 100.0), 0.07 * 100.0));
        assert!(
            damped_scroll(1000.0, 100.0) < 0.1 * 100.0,
            "overdrag is resisted"
        );
    }

    #[test]
    fn folder_springs_match_lawnchair() {
        // The constants, straight out of the source rows.
        assert!(near(FOLDER_TITLE_DELAY_MS, 32.0));
        assert!(near(FOLDER_SCRIM_ALPHA_DARK, 0.32));
        assert!(near(FOLDER_LAUNCHER_SCALE, 0.975));
        assert_eq!(SpringConfig::folder_morph().stiffness, 380.0);
        assert_eq!(SpringConfig::folder_morph().damping_ratio, 0.8);
        assert_eq!(SpringConfig::folder_scrim().stiffness, 380.0);
        assert_eq!(SpringConfig::folder_scrim().damping_ratio, 0.98);
        assert_eq!(SpringConfig::folder_alpha().stiffness, 1600.0);
        assert_eq!(SpringConfig::folder_alpha().damping_ratio, 0.9);

        let mut f = FolderOpen::closed(3);
        assert_eq!(f.folder_idx, 3);
        assert!(
            f.is_closed(),
            "a fresh folder is at rest, not animating out"
        );
        assert!(
            near(f.workspace_scale(), 1.0),
            "a closed workspace is unscaled"
        );

        f.open(4);
        assert_eq!(f.folder_idx, 4);
        assert!(near(f.morph.target, 1.0));
        assert!(near(f.scrim.target, FOLDER_SCRIM_ALPHA_DARK));
        assert!(near(f.title_alpha.target, 1.0));
        assert!(near(f.title_delay_ms, FOLDER_TITLE_DELAY_MS));

        // The title waits out its start delay before it moves at all; nothing
        // else does.
        f.step(0.010, FRAME_MS);
        assert!(
            near(f.title_alpha.value, 0.0),
            "the title moved during its delay"
        );
        assert!(
            f.morph.value > 0.0,
            "the container must not wait on the title"
        );
        f.step(0.010, FRAME_MS);
        assert!(near(f.title_delay_ms, 12.0), "the delay counts down in ms");
        f.step(0.012, FRAME_MS);
        assert!(
            near(f.title_delay_ms, 0.0),
            "the delay is 32 ms of clock, no more"
        );

        // Open fully and check the endpoints the reference animates to.
        for _ in 0..240 {
            f.step(1.0 / 120.0, FRAME_MS);
        }
        assert!(near(f.morph.value, 1.0), "the morph did not park on 1.0");
        assert!(
            near(f.scrim.value, FOLDER_SCRIM_ALPHA_DARK),
            "the scrim alpha"
        );
        assert!(near(f.title_alpha.value, 1.0), "the footer alpha");
        // The workspace sits behind the open folder, driven by the scrim
        // spring, as in the reference's two runs of one spring.
        assert!(
            near(f.workspace_scale(), FOLDER_LAUNCHER_SCALE),
            "workspace scale is {}",
            f.workspace_scale()
        );

        // Closing drops the title delay: the reference only delays on open.
        f.close();
        assert!(near(f.title_delay_ms, 0.0));
        for _ in 0..240 {
            f.step(1.0 / 120.0, FRAME_MS);
        }
        assert!(f.is_closed(), "the folder never parked closed");
        assert!(near(f.workspace_scale(), 1.0));

        // A folder parked shut does not re-animate on its own.
        let held = f.morph.value;
        f.step(1.0 / 120.0, FRAME_MS);
        assert!(near(f.morph.value, held), "a closed folder drifted");

        // The layout rows the springs are paired with, so the pairing is
        // visible from one side.
        let fl = FolderLayout::new(&layout());
        assert!(near(fl.scrim_alpha, FOLDER_SCRIM_ALPHA_DARK));
        assert!(near(fl.launcher_scale, FOLDER_LAUNCHER_SCALE));
        assert!(near(fl.title_delay_ms as f32, FOLDER_TITLE_DELAY_MS));
    }

    /// A folder holds a fixed-capacity inline list and no `String`, so the
    /// whole of its state is `Copy` and the frame path cannot allocate. This
    /// is the assertion that keeps a future `Vec` from being the easy fix for
    /// something.
    #[test]
    fn a_folder_holds_its_contents_inline() {
        // The inline id is a fixed byte array, not a pointer to one: a
        // `String` would be 3 words and a `&str` 2, and either would drag a
        // borrow or an allocator into the frame path.
        assert_eq!(
            core::mem::size_of::<FolderItem>(),
            FOLDER_ID_BYTES + 1,
            "a byte array and a length, nothing else"
        );
        let mut f = FolderOpen::closed(0);
        // A `FolderOpen` is `Copy`, which is the property the shell relies on
        // when it keeps one across the daemon loop.
        let snapshot = f;
        f.open(1);
        assert_eq!(snapshot.folder_idx, 0, "the copy is independent");
        assert_eq!(f.folder_idx, 1);

        assert_eq!(f.item_count(), 0);
        assert!(f.items().is_empty());
        assert_eq!(f.item(0), None);
        assert_eq!(FOLDER_ITEMS, 64);
        assert_eq!(FOLDER_ID_BYTES, 32);
    }

    /// The inline id is byte-exact and refuses what it cannot hold. A
    /// truncated id would be a *different app*, which is the failure this
    /// bound exists to prevent.
    #[test]
    fn a_folder_item_id_round_trips_and_refuses_what_it_cannot_hold() {
        for id in [
            "a",
            "com.android.providers.calendar",
            "x/y",
            &"z".repeat(31),
        ] {
            let item = FolderItem::new(id).unwrap_or_else(|| panic!("{id} must fit"));
            assert_eq!(item.id(), id);
        }
        assert_eq!(FolderItem::new(""), None, "empty is not an app");
        assert_eq!(
            FolderItem::new(&"z".repeat(FOLDER_ID_BYTES)),
            Some(FolderItem::new(&"z".repeat(FOLDER_ID_BYTES)).unwrap()),
            "exactly the bound fits"
        );
        assert_eq!(
            FolderItem::new(&"z".repeat(FOLDER_ID_BYTES + 1)),
            None,
            "one byte past the bound is refused, not clipped"
        );
        assert_eq!(FolderItem::new(&"z".repeat(1000)), None);
        assert_eq!(FolderItem::EMPTY.id(), "");
        // A multi-byte id is stored whole: no char is cut in half.
        let multi = FolderItem::new("café 日本語").unwrap();
        assert_eq!(multi.id(), "café 日本語");
        assert!(multi.id().len() <= FOLDER_ID_BYTES);
        // Debug is the readable form, so a failing assert names the app.
        assert_eq!(format!("{multi:?}"), "FolderItem(\"café 日本語\")");
    }

    /// The contents replace rather than append, because that is what the
    /// reference does: `FolderService.updateFolderWithItems` deletes every
    /// row for the folder and re-inserts
    /// (`data/folder/service/FolderService.kt:38-46`), and `FolderPagedView`'s
    /// `bindItems` rebuilds its children (`FolderPagedView.java:460-492`).
    /// A model that only appended could not express a reorder at all.
    #[test]
    fn folder_contents_replace_so_a_reorder_is_expressible() {
        let mut f = FolderOpen::closed(0);
        f.set_contents(&["a", "b", "c"]);
        assert_eq!(f.item_count(), 3);
        assert_eq!(
            f.items().iter().map(|i| i.id()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );

        // A reorder is a rewrite in the new order.
        f.set_contents(&["c", "a", "b"]);
        let order: Vec<&str> = f.items().iter().map(FolderItem::id).collect();
        assert_eq!(order, vec!["c", "a", "b"]);

        // And the old tail is dead, not merely unreachable: the count is what
        // bounds `items()`, but a stale non-empty slot past it would show up
        // the moment someone iterated the array directly.
        f.set_contents(&["only"]);
        assert_eq!(f.items().len(), 1);
        assert_eq!(f.items()[0].id(), "only");
        assert_eq!(f.item(1), None);

        // Over capacity the excess is dropped and the rest survives in order.
        let many: Vec<String> = (0..FOLDER_ITEMS + 5).map(|i| format!("a{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        f.set_contents(&refs);
        assert_eq!(f.item_count(), FOLDER_ITEMS, "the bound is the array's");
        assert_eq!(f.items()[0].id(), "a0");
        assert_eq!(
            f.items()[FOLDER_ITEMS - 1].id(),
            format!("a{}", FOLDER_ITEMS - 1)
        );

        // An id too long is skipped, not stored truncated.
        let long = "q".repeat(FOLDER_ID_BYTES + 1);
        f.set_contents(&["keep", &long, "also"]);
        let kept: Vec<&str> = f.items().iter().map(FolderItem::id).collect();
        assert_eq!(
            kept,
            vec!["keep", "also"],
            "the un-representable id is dropped"
        );
    }

    /// `add_item` appends, so a drop lands at the end of the visible grid
    /// rather than at the touch point -- `rank = index`
    /// (`data/folder/service/FolderService.kt:39-43`).
    #[test]
    fn adding_an_item_appends_and_reports_room() {
        let mut f = FolderOpen::closed(0);
        assert!(f.add_item("a"));
        assert!(f.add_item("b"));
        assert_eq!(
            f.items().iter().map(FolderItem::id).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(!f.add_item(""), "an empty id is not an app");
        assert!(!f.add_item(&"q".repeat(100)), "an id that cannot be held");

        for i in 0..(FOLDER_ITEMS - 2) {
            assert!(f.add_item(&format!("f{i}")), "slot {i}");
        }
        assert_eq!(f.item_count(), FOLDER_ITEMS);
        assert!(!f.add_item("one too many"), "a full folder says so");
        assert_eq!(f.item_count(), FOLDER_ITEMS, "and does not grow");
    }

    /// Removal shifts, because rank *is* the index: the pager computes the
    /// page from `rank / maxItemsPerPage` (`FolderPagedView.java:327`), so a
    /// hole would put the wrong items on the wrong page.
    #[test]
    fn removing_an_item_keeps_rank_dense() {
        let mut f = FolderOpen::closed(0);
        f.set_contents(&["a", "b", "c", "d"]);
        assert!(f.remove_item(1));
        let ids: Vec<&str> = f.items().iter().map(FolderItem::id).collect();
        assert_eq!(ids, vec!["a", "c", "d"], "rank 1 now holds c, not a hole");
        assert!(f.remove_item(0));
        assert_eq!(f.items()[0].id(), "c");
        assert_eq!(f.item_count(), 2, "c and d remain");
        assert!(f.remove_item(1), "the last item");
        assert!(f.remove_item(0), "and then the one before it");
        assert_eq!(f.item_count(), 0);
        assert!(f.items().is_empty());
        assert!(!f.remove_item(0), "an empty folder has nothing to remove");
        assert!(
            !f.remove_item(99),
            "an index past the end must not index the array"
        );
    }

    /// The reference's rule is `getItemCount() <= 1` at three call sites
    /// (`Folder.java:1129`, `:1331`, `:1757`) -- `<=`, not `==`, because a
    /// folder reaches one item both by removal and by a completed drag. An
    /// empty folder fires the rule but has no survivor to substitute, and
    /// `LauncherDelegate.replaceFolderWithFinalItem` (`:160-162`) returns
    /// without doing anything for one it cannot reduce.
    #[test]
    fn a_folder_of_one_item_collapses_into_that_item() {
        let mut f = FolderOpen::closed(0);
        assert_eq!(f.collapse(), FolderCollapse::Keep, "empty: no survivor");

        f.set_contents(&["only.app"]);
        assert_eq!(
            f.collapse(),
            FolderCollapse::IntoApp {
                app_id: FolderItem::new("only.app").unwrap()
            }
        );

        f.add_item("second");
        assert_eq!(f.collapse(), FolderCollapse::Keep, "two is a folder");

        // Either removal path that reaches one item reports the survivor, and
        // it is the survivor rather than slot 0.
        assert!(f.remove_item(0));
        assert_eq!(f.item_count(), 1);
        assert_eq!(
            f.collapse(),
            FolderCollapse::IntoApp {
                app_id: FolderItem::new("second").unwrap()
            },
            "removing the first item leaves the second as the survivor"
        );
        // It is a value, not an action: the model did not delete itself.
        assert_eq!(
            f.item_count(),
            1,
            "applying the transition is the shell's job"
        );
        assert!(f.remove_item(0));
        assert_eq!(
            f.collapse(),
            FolderCollapse::Keep,
            "and an empty folder stays"
        );
    }

    /// `itemsPerPage()` is `cols * rows` (`FolderGridOrganizer.java:92`) and
    /// `getPageCount()` is `ceil(n / per_page)`, and the indicator is visible
    /// only past one page (`FolderPagedView.java:496`).
    #[test]
    fn folder_paging_matches_the_reference_pager() {
        let mut f = FolderOpen::closed(0);
        f.items_per_page = 9;

        assert_eq!(f.items_per_page(), 9);
        assert_eq!(f.page_count(), 1, "an empty folder is one page");
        assert!(!f.shows_page_indicator());

        for n in 1..=9 {
            // `vec!`, not `["a"; n]`: a repeat count must be a `const`, so a
            // runtime length needs the allocation even in a test.
            f.set_contents(&vec!["a"; n]);
            assert_eq!(f.page_count(), 1, "{n} items is one page");
            assert!(!f.shows_page_indicator(), "{n}: no dots for page 1 of 1");
        }
        for (n, pages) in [(10usize, 2usize), (18, 2), (19, 3), (64, 8)] {
            f.set_contents(&vec!["a"; n]);
            assert_eq!(f.page_count(), pages, "{n} items");
            assert!(f.shows_page_indicator(), "{n} items needs dots");
        }

        // And it follows the grid rather than a fixed 9.
        f.items_per_page = 25;
        f.set_contents(&["a"; 19]);
        assert_eq!(f.page_count(), 1, "19 items at 25 per page is one page");
        assert!(!f.shows_page_indicator());
        f.set_contents(&["a"; 26]);
        assert_eq!(f.page_count(), 2);
        assert!(f.shows_page_indicator());
    }

    /// Zero `items_per_page` must not produce an infinity, which is the same
    /// guard `Layout::new_scaled` applies to `font_scale`.
    #[test]
    fn a_zero_page_size_degrades_to_one() {
        let mut f = FolderOpen::closed(0);
        f.set_contents(&["a", "b", "c"]);
        f.items_per_page = 0;
        assert_eq!(f.items_per_page(), 1, "never zero: it is a divisor");
        assert_eq!(f.page_count(), 3);
        // And the geometry layer agrees, or the shell would compute one page
        // count and draw another.
        let fl = FolderLayout::new(&layout());
        assert_eq!(fl.items_per_page(), 9);
        assert_eq!(fl.page_count(10), 2);
    }

    /// A drag commits once, on release, and clamps to the last page -- the
    /// reference holds the content under the finger and snaps afterwards.
    /// Committing in both `begin` and `end` would advance two pages per flick,
    /// which is the bug this is here to prevent.
    #[test]
    fn a_page_drag_commits_once_and_clamps() {
        let mut f = FolderOpen::closed(0);
        f.items_per_page = 9;
        // Ten items, so there are two pages to move between. An empty folder
        // is one page and `clamp_page` would pin `page` at 0 -- which is
        // correct, and would make every assertion below vacuous.
        f.set_contents(&["a"; 10]);
        let page_w = 800.0;

        // Short drag: released where it started.
        f.begin_page_drag(100.0);
        assert!(near(f.page_drag(), 100.0), "the content follows the finger");
        assert_eq!(f.page, 0, "the page does not move mid-drag");
        f.end_page_drag(page_w);
        assert_eq!(f.page, 0, "100 px is under half a page");
        assert!(near(f.page_drag(), 0.0), "the offset clears on release");

        // Long drag leftwards: next page. A leftward drag is a *negative* offset, and
        // it advances -- dragging right rewinds.
        f.begin_page_drag(-500.0);
        f.end_page_drag(page_w);
        assert_eq!(f.page, 1, "500 px is over half a page");

        // Rightwards back to the first page.
        f.begin_page_drag(500.0);
        f.end_page_drag(page_w);
        assert_eq!(f.page, 0);

        // At the first page, dragging backwards stays put.
        f.begin_page_drag(500.0);
        f.end_page_drag(page_w);
        assert_eq!(f.page, 0, "cannot page before the first page");

        // At the last page, dragging forwards stays put.
        f.set_contents(&["a"; 10]);
        f.begin_page_drag(-500.0);
        f.end_page_drag(page_w);
        assert_eq!(f.page, 1);
        f.begin_page_drag(-500.0);
        f.end_page_drag(page_w);
        assert_eq!(f.page, 1, "cannot page past the last page");

        // A non-finite offset is refused rather than poisoning the page.
        f.begin_page_drag(f32::NAN);
        assert!(near(f.page_drag(), 0.0));
    }

    /// Losing the last page's worth of items must move the shell off a page
    /// that no longer exists, or the grid shows nothing at all.
    #[test]
    fn removing_the_last_page_moves_the_current_page_back() {
        let mut f = FolderOpen::closed(0);
        f.items_per_page = 9;
        f.set_contents(&["a"; 10]);
        f.begin_page_drag(-500.0);
        f.end_page_drag(800.0);
        assert_eq!(f.page, 1, "on the second page");

        f.remove_item(0);
        assert_eq!(f.page_count(), 1);
        assert_eq!(f.page, 0, "page 1 does not exist any more");
        assert!(near(f.page_drag(), 0.0));

        // Widening the grid does the same: 3x3 to 5x5 turns the last of three
        // pages into the only page. Two leftward drags to reach page 2 of 3.
        f.set_contents(&["a"; 19]);
        for _ in 0..2 {
            f.begin_page_drag(-500.0);
            f.end_page_drag(800.0);
        }
        assert_eq!(f.page, 2, "19 items at 9 per page is three pages");
        f.items_per_page = 25;
        f.clamp_page();
        assert_eq!(f.page, 0, "19 items at 25 per page is one page");
    }

    /// Opening resets the page but keeps the contents: the shell fills a
    /// folder and *then* opens it, and the reference's pager starts at page 0
    /// (`FolderPagedView.java:483-490`, `setCurrentPage(0)`).
    #[test]
    fn opening_resets_the_page_and_keeps_the_contents() {
        let mut f = FolderOpen::closed(0);
        f.items_per_page = 9;
        f.set_contents(&["a"; 10]);
        f.begin_page_drag(-500.0);
        f.end_page_drag(800.0);
        assert_eq!(f.page, 1);

        f.open(3);
        assert_eq!(f.page, 0, "a reopened folder starts at its first page");
        assert!(near(f.page_drag(), 0.0));
        assert_eq!(f.item_count(), 10, "the contents survive the open");
        assert_eq!(f.folder_idx, 3);

        // And so does a re-write of the contents, for the same reason.
        f.set_contents(&["a"; 10]);
        f.begin_page_drag(-500.0);
        f.end_page_drag(800.0);
        f.set_contents(&["a"; 10]);
        assert_eq!(f.page, 0, "re-binding the pager starts it over");
    }
}
