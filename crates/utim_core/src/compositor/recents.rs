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

use crate::graphics::drm_kms::{RECENTS_SCALE_MULTIPLIER, SpringConfig, SpringSimulation};
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
fn settle_expired(
    spring: &SpringSimulation,
    slot: &mut f32,
    now_ms: f32,
    frame_ms: f32,
) -> bool {
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
    /// Horizontal residue of an in-flight scrub, px. The renderer adds this
    /// on top of [`Self::card_rect`]. It is finger tracking, not an animation,
    /// so it is deliberately not a spring.
    pub drag_px: f32,
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
        // midpoint plus half the card height. Recovered from the action chip,
        // which `RecentsLayout` documents as centred on the card.
        let card_bottom = rl.chip.center_y() + rl.card_h * 0.5;
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
    pub fn on_card_drag(&mut self, i: usize, dy: f32) {
        if i >= self.len as usize {
            return;
        }
        if self.dragging != NO_CARD && self.dragging as usize != i {
            // A second finger is not a second dismiss; the first one owns it.
            return;
        }
        let live = {
            let card = &self.cards[i];
            card.kill != KillState::Idle || !card.dismissable
        };
        if live {
            // Already dying or pinned: dragging it again must change nothing
            // and, critically, must not take ownership of the drag.
            return;
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
        self.note_threshold_haptic(i);
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
    /// `l` supplies the card box and radius; the pitch comes from the panel
    /// this [`Recents`] was built for, so `l` must be that panel's
    /// [`RecentsLayout`]. The overview scale is deliberately *not* applied
    /// here -- it is the same for every card, so the renderer applies it once
    /// to the whole stack rather than once per card.
    pub fn card_rect(&self, i: usize, l: &RecentsLayout) -> Option<Rect> {
        if i >= self.len as usize {
            return None;
        }
        // The action chip is documented as centred on the card, so the card
        // centre is the chip centre. No dp is re-derived here.
        let slot = (i as f32 - self.selected as f32) * self.pitch + self.reflow[i].value;
        Some(Rect {
            x: l.chip.center_x() + self.drag_px + slot - l.card_w * 0.5,
            y: l.chip.center_y() + self.dismiss[i].value - l.card_h * 0.5,
            w: l.card_w,
            h: l.card_h,
            radius: l.corner_r,
        })
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
        let next = if (at as u8) < prev {
            prev - 1
        } else {
            prev
        }
        .min(self.len.saturating_sub(1));
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

    /// Start -- or refuse to start -- a graceful close on card `i`. `true` if
    /// the card moved to [`KillState::Grace`] because of this call.
    fn arm_close(&mut self, i: usize) -> bool {
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
            if card.kill == KillState::Grace
                && self.now_ms - card.grace_started_ms >= KILL_GRACE_MS
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
    fn note_threshold_haptic(&mut self, i: usize) {
        if self.threshold_haptic_done {
            return;
        }
        let threshold = DISMISS_THRESHOLD_FRACTION * self.dismiss_length;
        if (self.dismiss_travel(i) * self.dismiss_length - threshold).abs() <= self.haptic_range {
            self.threshold_haptic_done = true;
        }
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

/// An opening, or closing, folder.
///
/// Pure state: three springs and the index they belong to. The scrim spring
/// doubles as the workspace-scale spring because the reference runs both with
/// the *same* `STIFFNESS_LAUNCHER_SCRIM` / `DAMPING_LAUNCHER_SCRIM` and the
/// same zero start delay (`FolderSpringAnimatorSet.kt:340-375`) -- only the
/// endpoints differ, which makes one spring plus a normalisation exact rather
/// than approximate.
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
        }
    }

    /// Start opening folder `folder_idx`.
    ///
    /// The title's start delay is armed here and nowhere else; the reference
    /// delays it only when *opening* (`:278`), and closing is immediate.
    pub fn open(&mut self, folder_idx: u8) {
        self.folder_idx = folder_idx;
        self.morph.set_target(1.0);
        self.scrim.set_target(FOLDER_SCRIM_ALPHA_DARK);
        self.title_alpha.set_target(1.0);
        self.title_delay_ms = FOLDER_TITLE_DELAY_MS;
        self.park = [PARK_UNARMED; 3];
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
        assert!(near(
            dismiss_recents_scale(0.5375),
            (0.9875 + 0.975) * 0.5
        ));

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
        assert_eq!(r.on_card_release(0), DismissOutcome::Cancelled { card: 0, pid: 1001 });
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
                near(SpringConfig::task_dismiss_with_hops(hops as u8).damping_ratio, *zeta),
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
        assert!(matches!(r.on_card_release(0), DismissOutcome::Committed { .. }));

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
        assert_eq!(r.on_card_release(0), DismissOutcome::Cancelled { card: 0, pid: 1001 });
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
        assert!(near(r.reflow[0].value, r.pitch), "one pitch of slide expected");
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
        let evicted = r2.push(TaskCard::new(4, 1004, 4)).expect("a full stack evicts");
        assert_eq!(evicted.pid, 1000, "the oldest is the one that goes");
        assert_eq!(r2.len as usize, MAX_TASKS);

        // Re-pushing a live pid promotes it rather than duplicating it, and
        // hands back the stale entry. Exactly one of promotion and eviction
        // can happen at a time, because taking the stale entry frees a slot.
        let mut r3 = recents(MAX_TASKS as u32);
        let stale = r3.push(TaskCard::new(99, 1001, 77)).expect("stale entry");
        assert_eq!(stale.pid, 1001);
        assert_eq!(r3.len as usize, MAX_TASKS, "a promotion is not an insertion");
        assert_eq!(r3.cards[0].pid, 1001, "most recent first");
        assert_eq!(r3.selected, 0, "the promoted card takes focus");
        assert_eq!(r3.iter().filter(|c| c.pid == 1001).count(), 1, "no duplicate");

        // Removal frees exactly one slot and keeps the rest in order.
        let mut r4 = recents(5);
        let gone = r4.remove_by_pid(1002).expect("present");
        assert_eq!(gone.pid, 1002);
        assert_eq!(r4.len, 4);
        assert_eq!(
            r4.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![1004, 1003, 1001, 1000]
        );
        assert!(r4.remove_by_pid(4242).is_none(), "an absent pid is not an error");

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
        assert!(matches!(r.on_card_release(2), DismissOutcome::Committed { .. }));
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
        assert!(near(r.scale.value, r.scale.target), "scale not on its target");
        assert!(near(r.scale.value, DISMISS_SCALE_DEFAULT), "scale must rest at 1.0");
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
        assert!(r.park.iter().all(|d| d.is_infinite()), "a slot stayed armed");

        // A nonsensical frame time must not be able to disable parking: that
        // is the failure this mechanism exists to prevent.
        let mut r2 = recents(4);
        drag(&mut r2, 0, DISMISS_THRESHOLD_FRACTION + 0.02);
        let _ = r2.on_card_release(0);
        for _ in 0..240 {
            r2.step(FRAME_MS / 1000.0, f32::NAN);
        }
        for i in 0..MAX_TASKS {
            assert!(r2.dismiss[i].is_at_rest(), "dismiss[{i}] with a bad frame_ms");
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
        assert!(near(r3.dismiss[0].value, 0.0), "a poked spring must still park");

        // A live drag is exempt. Past the detach threshold the card follows the
        // finger on a `magnetic_detach` spring, so it *lags* the finger by
        // construction; if parking applied, the card would be snapped onto the
        // finger and the magnetic give would disappear. The finger keeps
        // moving well past any settle duration, and stays inside the dismiss
        // clamp so the target genuinely advances every frame.
        let mut r4 = recents(2);
        assert!(r4.detach < r4.dismiss_length, "the panel must have a drag band");
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
        assert!(r4.dismiss[0].is_at_rest(), "the release must re-arm parking");
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
        assert_eq!(r.cards[0].kill, KillState::Grace, "a commit starts the clock");

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
        assert_eq!(r.on_card_drag(7, -10.0), (), "out-of-range drag is a no-op");
    }

    #[test]
    fn dismiss_outcome_cancels_below() {
        // `recents(3)` is [1002, 1001, 1000]: most recent first.
        let mut r = recents(3);
        assert_eq!(r.cards[0].pid, 1002);
        drag(&mut r, 0, DISMISS_THRESHOLD_FRACTION - 0.01);
        assert_eq!(r.on_card_release(0), DismissOutcome::Cancelled { card: 0, pid: 1002 });
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
        assert!(matches!(r.on_card_release(0), DismissOutcome::Cancelled { .. }));
        assert!(near(r.scale.target, dismiss_recents_scale(0.0)));
        run(&mut r, 1.0);
        assert!(near(r.scale.value, DISMISS_SCALE_DEFAULT), "release restores 1.0");

        // The overview scale tracked the ladder during the drag, plateaus and
        // all. Each `drag` is a delta, so the return leg has to travel all the
        // way back to the origin and past it.
        let mut r = recents(1);
        drag(&mut r, 0, 0.35);
        assert!(near(r.scale.value, DISMISS_SCALE_ON_CANCEL), "held plateau");
        drag(&mut r, 0, 0.6);
        assert!(near(r.scale.value, DISMISS_SCALE_ON_SUCCESS), "past the threshold");
        // Back to 0.2 of upward travel: on the first ramp's plateau edge.
        drag(&mut r, 0, -0.75);
        assert!(near(r.scale.value, DISMISS_SCALE_ON_CANCEL), "0.2 of travel");
        // Past the origin and below it: not a dismiss at all, so the overview
        // does not shrink. This is the sign that matters and the reason
        // `dismiss_fraction` is signed.
        drag(&mut r, 0, -0.5);
        assert!(r.cards[0].dismiss_y > 0.0, "the card is now below the origin");
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
        // Drag far past the origin: the travel stops at 25 dp.
        r.on_card_drag(0, 5000.0);
        assert!(near(r.cards[0].dismiss_y, expect), "got {}", r.cards[0].dismiss_y);
        r.on_card_drag(0, 5000.0);
        assert!(near(r.cards[0].dismiss_y, expect), "a second drag must not creep");

        // Under the cap it tracks exactly.
        r.on_card_drag(0, -expect);
        assert!(near(r.cards[0].dismiss_y, 0.0));

        // Upward travel is bounded by the dismiss length, and the two bounds
        // are not the same number.
        let mut r = recents(2);
        r.on_card_drag(0, -5000.0);
        assert!(near(r.cards[0].dismiss_y, -r.dismiss_length), "upward clamp");
        assert!(r.dismiss_length > 2.0 * expect, "the bounds should differ");

        // The detach threshold is the other side of the 25 dp bound and is
        // also layout-derived.
        assert!(near(rl.detach_dp, 72.0 * l.profile().dp(1.0)), "72 dp");
        assert!(r.detach > expect, "a card detaches before it undershoots 25 dp");

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
        assert!(near(r.cards[0].dismiss_y, 0.0), "a dying card must not move");
        assert_eq!(r.dragging, NO_CARD, "and must not take the drag");
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
        assert!(!m.push_shortcut(1), "the 8-slot buffer is the binding limit");
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
        assert!(damped_scroll(1000.0, 100.0) < 0.1 * 100.0, "overdrag is resisted");
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
        assert!(f.is_closed(), "a fresh folder is at rest, not animating out");
        assert!(near(f.workspace_scale(), 1.0), "a closed workspace is unscaled");

        f.open(4);
        assert_eq!(f.folder_idx, 4);
        assert!(near(f.morph.target, 1.0));
        assert!(near(f.scrim.target, FOLDER_SCRIM_ALPHA_DARK));
        assert!(near(f.title_alpha.target, 1.0));
        assert!(near(f.title_delay_ms, FOLDER_TITLE_DELAY_MS));

        // The title waits out its start delay before it moves at all; nothing
        // else does.
        f.step(0.010, FRAME_MS);
        assert!(near(f.title_alpha.value, 0.0), "the title moved during its delay");
        assert!(f.morph.value > 0.0, "the container must not wait on the title");
        f.step(0.010, FRAME_MS);
        assert!(near(f.title_delay_ms, 12.0), "the delay counts down in ms");
        f.step(0.012, FRAME_MS);
        assert!(near(f.title_delay_ms, 0.0), "the delay is 32 ms of clock, no more");

        // Open fully and check the endpoints the reference animates to.
        for _ in 0..240 {
            f.step(1.0 / 120.0, FRAME_MS);
        }
        assert!(near(f.morph.value, 1.0), "the morph did not park on 1.0");
        assert!(near(f.scrim.value, FOLDER_SCRIM_ALPHA_DARK), "the scrim alpha");
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
}
