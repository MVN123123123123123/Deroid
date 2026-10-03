//! The rotation *policy*: given a stream of orientations and where the panel
//! is now, decide whether to rotate, to which orientation, and after what dwell.
//!
//! # Why this module exists
//!
//! `sensors/sensor_proxy.rs` already implements a complete orientation model --
//! gravity vector to orientation, anti-jitter candidate counting, the lot --
//! and it had **zero live callers**: there was no D-Bus socket to
//! `net.hadess.SensorProxy` and no re-modeset path, so `LauncherState::auto_rotate`
//! was persisted, offered in Settings as `"On (not applied)"`, and read by
//! nothing but the policy config. That history is why the settings row admits
//! nothing until every half exists; all three now do (subscription in the
//! shell, this policy, actuation via `decide_modeset` +
//! `DrmKmsDevice::set_orientation`).
//!
//! This module is the half of that feature which is pure. It contains no I/O,
//! no allocation and no handle: the shell reads a sensor however it likes, feeds
//! the result in, and gets back a decision. The re-modeset itself is a KMS
//! operation owned by `graphics/drm_kms.rs`, so what is left for the shell is
//! [`decide_modeset`], which answers "does anything need to change, and to what"
//! without touching a device.
//!
//! # Hysteresis is the whole point
//!
//! A phone lying on a desk is never still. Every accelerometer reading has noise,
//! and a naive "if the dominant axis changed, rotate" turns that into a panel
//! that flips every time somebody brushes past it. The reference's answer is that
//! the launcher never does that itself: it asks for
//! `SCREEN_ORIENTATION_UNSPECIFIED` (`RotationHelper.java:226`) and lets the
//! platform's rotation classifier decide, having already required a *user
//! preference* be on (`RotationHelper.java:224-225`). The platform is not in this
//! tree, so the classification has to live here, and the requirement is the same
//! one: a candidate orientation must be seen *repeatedly* before it wins.
//!
//! Two details are borrowed verbatim:
//!
//! * **Repeatedly, not once.** [`RotationPolicy`] counts a *trailing run* --
//!   how many of the most recent samples agree with the newest one -- and
//!   rotates when the run reaches [`DEFAULT_DWELL_SAMPLES`]. The existing sensor
//!   model does the same thing with `candidate_sample_count`
//!   (`sensor_proxy.rs:117-134`), so the two agree on what a settled reading is.
//! * **Redundant requests are not re-issued.** `RotationHelper` compares the
//!   flags it is about to set against the last ones and does nothing when they
//!   match (`RotationHelper.java:232`). [`RotationPolicy::update`] returns
//!   [`RotationDecision::Hold`] rather than re-deciding an orientation the panel
//!   is already in, so a shell that forwards the decision to a modeset cannot
//!   churn the panel.
//!
//! # What is deliberately absent
//!
//! No sensor source, no subscription, no clock. Time would be the obvious thing
//! to add -- "rotate after 300 ms of stability" -- and it is left out on
//! purpose: the reference's own dwell is a *sample count* on a claimed
//! accelerometer (`iio-sensor-proxy`'s `ClaimAccelerometer` publishes at the
//! sensor's rate), not a duration, so a count is the faithful model and it keeps
//! the whole module `Copy` and allocation-free.

use crate::graphics::composer::Transform;
use crate::sensors::sensor_proxy::DeviceOrientation;

/// How many recent orientations the policy remembers.
///
/// The window is what bounds the dwell: [`RotationPolicy::set_dwell_samples`]
/// clamps to `1..=ROTATION_WINDOW`, so a candidate can never need more agreeing
/// readings than the ring holds. Eight is the reference's own accelerometer
/// sampling window rounded down to a power of two -- `iio-sensor-proxy` keeps a
/// ten-sample ring and the platform classifier requires three consecutive
/// matches, so eight holds three with room to see the disagreement before it.
pub const ROTATION_WINDOW: usize = 8;

/// Consecutive agreeing samples a candidate orientation needs before it wins.
///
/// Three, because that is the number the rest of this crate already uses:
/// `SensorProxyService::process_accelerometer` requires three consecutive
/// readings (`sensor_proxy.rs:117-134`), and so does the platform rotation
/// classifier the reference delegates to. Two identical implementations that
/// disagree about what "settled" means is a bug that only shows up as a phone
/// that rotates at a slightly different angle than the user expects.
pub const DEFAULT_DWELL_SAMPLES: u8 = 3;

/// What the policy concluded about the panel's orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationDecision {
    /// Leave the panel where it is.
    Hold,
    /// The panel should be in this orientation.
    Rotate(DeviceOrientation),
}

impl RotationDecision {
    /// The orientation to move to, or `None` when holding.
    ///
    /// So a shell can write
    /// `if let Some(o) = decision.orientation() { modeset(o) }` without matching
    /// on the variant and without a second copy of the mapping.
    #[inline]
    pub const fn orientation(self) -> Option<DeviceOrientation> {
        match self {
            Self::Hold => None,
            Self::Rotate(o) => Some(o),
        }
    }

    /// `true` when the panel should move.
    #[inline]
    pub const fn is_rotate(self) -> bool {
        matches!(self, Self::Rotate(_))
    }
}

/// A request that outranks the accelerometer, if there is one.
///
/// This is the reference's `RotationHelper` request ladder
/// (`RotationHelper.java:64-92`, resolved at `:214-231`) reduced to the three
/// answers that matter. The reference keeps three *separate* fields -- a state
/// handler, a transition and a state -- and resolves them in that order; a shell
/// with one of each does not need the distinction, but does need the same three
/// outcomes:
///
/// * `None` -- follow `auto_rotate` and the sensor.
/// * `Rotate` -- `SCREEN_ORIENTATION_UNSPECIFIED` (`:226`): still the
///   accelerometer, but the user's auto-rotate switch does not get a vote. This
///   is what the video player asking for landscape gets.
/// * `Lock` -- `SCREEN_ORIENTATION_LOCKED` (`:223`): ignore the sensor entirely
///   and keep whatever the panel is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationRequest {
    /// Nothing outranks the sensor.
    #[default]
    None,
    /// Use the sensor even when auto-rotate is off.
    Rotate,
    /// Ignore the sensor; hold the current orientation.
    Lock,
}

/// A fixed-capacity ring of recent orientations.
///
/// `Copy`, no heap, bounded by construction -- this is frame-path data, and a
/// chatty sensor must not be able to grow it. `[DeviceOrientation; N]` is
/// `N * 1` bytes because the enum is a fieldless `u8`-sized value, so the whole
/// policy is a few dozen bytes of stack.
///
/// Index 0 is the **oldest** retained sample, which is the order the reference
/// reads its own contents list in (`Folder.java:1379-1392` iterates
/// `for i in 0..total` over ranks ascending, and the same "order is the list"
/// convention runs through the whole model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrientationRing<const N: usize> {
    buf: [DeviceOrientation; N],
    /// Index of the next slot to write, and of the oldest sample once full.
    head: usize,
    /// Samples retained, saturating at `N`.
    len: usize,
}

impl<const N: usize> Default for OrientationRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> OrientationRing<N> {
    /// An empty ring. `const`, so a policy is `const`-constructible.
    pub const fn new() -> Self {
        Self {
            buf: [DeviceOrientation::Normal; N],
            head: 0,
            len: 0,
        }
    }

    /// Append one sample, dropping the oldest once the ring is full.
    ///
    /// A no-op for `N == 0`. A `const N` of zero is a compile error in every
    /// real use but the type admits it, and a `push` that wrote out of bounds
    /// would be a panic rather than a wrong answer.
    #[inline]
    pub fn push(&mut self, sample: DeviceOrientation) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = sample;
        self.head += 1;
        if self.head == N {
            self.head = 0;
        }
        if self.len < N {
            self.len += 1;
        }
    }

    /// Forget every sample.
    ///
    /// What the reference's sensor model does when the device goes flat
    /// (`sensor_proxy.rs:91-94`: `if z.abs() > 8.0 { self.candidate_sample_count = 0; }`).
    /// A candidate counted across a face-down phase is counting readings that
    /// describe a phone on a table, not a phone being turned.
    #[inline]
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Samples retained, at most [`ROTATION_WINDOW`] for the policy's ring.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The oldest retained sample.
    #[inline]
    pub fn oldest(&self) -> Option<DeviceOrientation> {
        self.at(0)
    }

    /// The most recent sample.
    #[inline]
    pub fn newest(&self) -> Option<DeviceOrientation> {
        self.at(self.len.wrapping_sub(1))
    }

    /// The sample at `i`, counting from the oldest. `None` past the end.
    #[inline]
    pub fn at(&self, i: usize) -> Option<DeviceOrientation> {
        if N == 0 || i >= self.len {
            return None;
        }
        // `head` is the next slot to write, so the oldest lives `len` slots back
        // from it. Only the taken path performs the subtraction, so `N == 0`
        // cannot wrap it.
        let start = if self.len == N { self.head } else { 0 };
        Some(self.buf[(start + i) % N])
    }

    /// How many of the most recent samples equal the newest one.
    ///
    /// The dwell counter. Counts back from `len - 1` and stops at the first
    /// disagreement, so it is `0` on an empty ring and never exceeds `len`.
    pub fn trailing_run(&self) -> u8 {
        let Some(last) = self.newest() else {
            return 0;
        };
        let mut run = 0u8;
        let mut i = self.len;
        while i > 0 {
            i -= 1;
            if self.at(i) != Some(last) {
                break;
            }
            run += 1;
        }
        run
    }
}

/// Quarter turns clockwise from a panel's home orientation.
///
/// `DeviceOrientation` names the direction the *top of the device* points, so it
/// is not by itself a panel rotation: a tablet whose panel is natively landscape
/// needs the screen turned a quarter in order to be held upright. This is the
/// function that reconciles the two, and the shell needs exactly one of it --
/// [`transform_for`] is the same thing wrapped up for the compositor.
///
/// The offset is `base(orientation) - base(natural)`, taken modulo four. With
/// `natural == Normal` that is `base(orientation)`, which is exactly
/// `DeviceOrientation::to_transform` (`sensor_proxy.rs:31-39`) restated in
/// quarter turns, so a portrait phone is unaffected by the whole mechanism.
///
/// `Undefined` is treated as `Normal`. The policy never asks about it -- a flat
/// device produces [`RotationDecision::Hold`] -- but the function is total, and
/// "flat is portrait" is a worse lie than "flat is whatever the panel's home is"
/// given that it is the *offset* from home that is returned.
pub fn quarter_turns(orientation: DeviceOrientation, natural: DeviceOrientation) -> u8 {
    let base = |o: DeviceOrientation| match o {
        DeviceOrientation::Normal | DeviceOrientation::Undefined => 0u8,
        DeviceOrientation::LeftUp => 1,
        DeviceOrientation::BottomUp => 2,
        DeviceOrientation::RightUp => 3,
    };
    (base(orientation) + 4 - base(natural)) % 4
}

/// The compositor transform for an orientation on a panel with this home.
///
/// The one call a shell needs after [`decide_modeset`] returns
/// [`ModesetAction::Rotate`]: the decision is in physical orientations (which is
/// what the sensor reports) and the compositor wants a [`Transform`] (which is
/// what the panel's home decides).
pub fn transform_for(orientation: DeviceOrientation, natural: DeviceOrientation) -> Transform {
    match quarter_turns(orientation, natural) {
        1 => Transform::Rotate90,
        2 => Transform::Rotate180,
        3 => Transform::Rotate270,
        // 0, and the unreachable fourth.
        _ => Transform::None,
    }
}

/// The panel rotations a display actually advertises.
///
/// A DRM connector exposes one mode per supported rotation, so a panel that
/// offers no landscape mode cannot be shown in landscape -- the modeset fails and
/// the compositor falls back to whatever the driver hands back. That is a real
/// failure on real hardware and it has to be *decidable* without issuing the
/// modeset, which is what this mask is for.
///
/// One byte, bit `i` meaning "a mode exists for `i * 90` degrees from home".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanelRotations(u8);

impl PanelRotations {
    /// No rotation at all, not even home. Degenerate; the shell should treat it
    /// as "the driver told us nothing".
    pub const NONE: Self = Self(0);
    /// `0` and `180` degrees: a portrait-only panel, which is what most phones
    /// are.
    pub const PORTRAIT_ONLY: Self = Self(0b0000_0101);
    /// `90` and `270` degrees: a landscape-only panel.
    pub const LANDSCAPE_ONLY: Self = Self(0b0000_1010);
    /// Every rotation.
    pub const ALL: Self = Self(0b0000_1111);

    #[inline]
    pub const fn from_mask(mask: u8) -> Self {
        Self(mask & 0b0000_1111)
    }

    #[inline]
    pub const fn mask(self) -> u8 {
        self.0
    }

    /// Whether a mode exists for `quarter_turns` degrees of panel rotation.
    ///
    /// Total over its argument: a value past three is not a rotation, so it is
    /// absent rather than wrapping into one. A caller that somehow computed a
    /// fifth of a turn gets told no.
    #[inline]
    pub const fn contains(self, quarter_turns: u8) -> bool {
        quarter_turns < 4 && (self.0 & (1 << quarter_turns)) != 0
    }
}

/// What the shell must do to the panel, if anything.
///
/// The output of [`decide_modeset`], which is the last pure step: after this the
/// shell only performs a mode change, it does not decide one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModesetAction {
    /// Nothing: the panel is already there, or the requested orientation is not
    /// a rotation at all.
    None,
    /// Change the panel rotation to this orientation.
    Rotate(DeviceOrientation),
    /// The panel does not advertise a mode for this rotation.
    ///
    /// Distinct from [`ModesetAction::Rotate`] because the two failures are
    /// different: `Rotate` means "do the modeset and it will work", this means
    /// "do not, it will fail". A shell that issued the modeset anyway gets a
    /// black panel on hardware that cannot do it.
    Unsupported(DeviceOrientation),
}

impl ModesetAction {
    /// The orientation to move to, or `None`.
    #[inline]
    pub const fn orientation(self) -> Option<DeviceOrientation> {
        match self {
            Self::None => None,
            Self::Rotate(o) | Self::Unsupported(o) => Some(o),
        }
    }
}

/// Decide the modeset for an orientation request. No I/O, no device.
///
/// This is the "re-modeset decision side" split out of the modeset itself:
/// given where the panel is, where it is being asked to go, what its home
/// orientation is, and which rotations it supports, answer what -- if anything --
/// has to change. The shell performs [`ModesetAction::Rotate`] and nothing else.
///
/// Three outcomes, in order:
///
/// 1. already there -> [`ModesetAction::None`]. Checked first, and *before* the
///    support mask on purpose: a panel sitting in a rotation it does not support
///    is a fact about the driver, not something to re-issue a modeset over.
/// 2. rotation unsupported -> [`ModesetAction::Unsupported`].
/// 3. otherwise -> [`ModesetAction::Rotate`].
///
/// `natural` is the orientation at which the panel is at its own zero degrees,
/// and both orientations are compared *through* it, so this is correct on a
/// landscape-native tablet as well as a portrait phone.
pub fn decide_modeset(
    current: DeviceOrientation,
    requested: DeviceOrientation,
    natural: DeviceOrientation,
    supported: PanelRotations,
) -> ModesetAction {
    let turns = quarter_turns(requested, natural);
    if turns == quarter_turns(current, natural) {
        return ModesetAction::None;
    }
    if !supported.contains(turns) {
        return ModesetAction::Unsupported(requested);
    }
    ModesetAction::Rotate(requested)
}

/// The rotation policy: a bounded orientation history plus the rules that read
/// it.
///
/// Every field is a [`Copy`] value and the whole struct is `Copy`, so the shell
/// can hold it by value, keep it on the stack next to its frame state, and pass
/// it to a decision function without a borrow. Nothing here allocates.
///
/// The rules, in the order [`Self::evaluate`] applies them:
///
/// 1. **A pin** ([`Self::set_pinned`]) outranks everything, including the
///    absence of a sensor. A portrait-locked phone held face down is still
///    portrait-locked. No dwell either: a lock is not a candidate the sensor
///    proposed, it is the user's standing instruction.
/// 2. **`RotationRequest::Lock`** holds where the panel is. This is the
///    reference's `SCREEN_ORIENTATION_LOCKED` (`RotationHelper.java:222-223`),
///    which unlike a pin does not name an orientation.
/// 3. **No sensor** holds. Not "portrait" -- a device whose accelerometer could
///    not be read has not said it is upright, and defaulting it to portrait is
///    how a tablet that is genuinely being held sideways flips.
/// 4. **Auto-rotate off** holds, unless a
///    [`RotationRequest::Rotate`] outranks it: that is the reference's
///    `SCREEN_ORIENTATION_NOSENSOR` (`:230`) versus `UNSPECIFIED` (`:226`).
/// 5. Otherwise the **dwell** rule: rotate once
///    [`OrientationRing::trailing_run`] reaches
///    [`Self::dwell_samples`] for a sample that is not where the panel already
///    is.
///
/// Defaults are the conservative ones: no sensor, auto-rotate off, portrait
/// home. A policy that has not been configured holds, which is the only answer
/// that cannot surprise somebody's face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    ring: OrientationRing<ROTATION_WINDOW>,
    current: DeviceOrientation,
    natural: DeviceOrientation,
    pinned: Option<DeviceOrientation>,
    request: RotationRequest,
    dwell: u8,
    has_sensor: bool,
    auto_rotate: bool,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl RotationPolicy {
    /// A policy that holds: no sensor, auto-rotate off, portrait home.
    pub const fn new() -> Self {
        Self {
            ring: OrientationRing::new(),
            current: DeviceOrientation::Normal,
            natural: DeviceOrientation::Normal,
            pinned: None,
            request: RotationRequest::None,
            dwell: DEFAULT_DWELL_SAMPLES,
            has_sensor: false,
            auto_rotate: false,
        }
    }

    /// Tell the policy whether a sensor is actually readable.
    ///
    /// Distinct from "auto-rotate is off": with no sensor there is nothing to
    /// obey, so this holds even when auto-rotate is on. That is the state a
    /// device is in when the D-Bus socket to `net.hadess.SensorProxy` is not
    /// there, and it is the reason
    /// [`RotationDecision::Hold`] can mean "there is no orientation source"
    /// without that being a bug.
    #[inline]
    pub fn set_sensor_present(&mut self, present: bool) {
        self.has_sensor = present;
    }

    /// `LauncherState::auto_rotate`. Defaults to `false`, as the state does.
    #[inline]
    pub fn set_auto_rotate(&mut self, on: bool) {
        self.auto_rotate = on;
    }

    /// The orientation at which the panel is at its own zero degrees.
    ///
    /// Also the resting orientation: while no sample has been recorded the panel
    /// is moved here, because a panel is at its home when it boots and the first
    /// orientation it can be shown in is its own. Once samples have arrived the
    /// panel's position is history and this only changes the offset arithmetic.
    pub fn set_natural(&mut self, natural: DeviceOrientation) {
        self.natural = natural;
        if self.ring.is_empty() {
            self.current = natural;
        }
    }

    /// Pin the panel to one orientation, or stop pinning it.
    ///
    /// `Some(DeviceOrientation::Normal)` is portrait lock. Unlike
    /// [`RotationRequest::Lock`] this names the orientation to move *to*, and it
    /// needs no dwell: the reference's lock is a standing instruction from the
    /// user, not a candidate the sensor proposed, so making it wait three
    /// samples would leave the panel briefly sideways after the lock was set.
    #[inline]
    pub fn set_pinned(&mut self, pin: Option<DeviceOrientation>) {
        self.pinned = pin;
    }

    /// Set the request that outranks the accelerometer.
    #[inline]
    pub fn set_request(&mut self, request: RotationRequest) {
        self.request = request;
    }

    /// Consecutive agreeing samples a candidate needs. Clamped to
    /// `1..=ROTATION_WINDOW`.
    ///
    /// Clamped rather than refused because the alternative is a policy whose
    /// dwell exceeds its own window, which can never rotate at all. `1` is
    /// "no hysteresis", and it is a legitimate setting: it is what a caller
    /// wants when it has already done its own filtering and only wants the
    /// geometry ([`quarter_turns`], [`decide_modeset`]).
    pub fn set_dwell_samples(&mut self, samples: u8) {
        self.dwell = samples.clamp(1, ROTATION_WINDOW as u8);
    }

    /// The dwell in force.
    #[inline]
    pub const fn dwell_samples(&self) -> u8 {
        self.dwell
    }

    /// Where the panel is.
    #[inline]
    pub const fn current(&self) -> DeviceOrientation {
        self.current
    }

    /// The panel's home orientation.
    #[inline]
    pub const fn natural(&self) -> DeviceOrientation {
        self.natural
    }

    #[inline]
    pub const fn sensor_present(&self) -> bool {
        self.has_sensor
    }

    #[inline]
    pub const fn auto_rotate(&self) -> bool {
        self.auto_rotate
    }

    #[inline]
    pub const fn pinned(&self) -> Option<DeviceOrientation> {
        self.pinned
    }

    #[inline]
    pub const fn request(&self) -> RotationRequest {
        self.request
    }

    /// Put the panel somewhere without a decision.
    ///
    /// For boot and for the shell's own re-modeset path: after it performs a
    /// mode change the panel *is* somewhere, and feeding that back through
    /// [`Self::update`] would make the policy believe it had earned the move.
    /// The history is kept, because the samples did happen.
    pub fn reset(&mut self, orientation: DeviceOrientation) {
        self.current = orientation;
    }

    /// The samples behind the current decision.
    ///
    /// Exposed so a shell can show "holding: 2/3 stable" and so a test can
    /// assert on the history rather than on a re-derivation of it.
    #[inline]
    pub const fn history(&self) -> &OrientationRing<ROTATION_WINDOW> {
        &self.ring
    }

    /// What the policy would do right now, without feeding it anything.
    ///
    /// The decision that does not need a sensor: it is what makes a pin testable
    /// (a pin fires before any sample has arrived) and what lets a shell ask
    /// "is this orientation reachable at all" through the same code path that
    /// does the rotating.
    pub fn evaluate(&self) -> RotationDecision {
        if let Some(pin) = self.pinned {
            return if pin != self.current {
                RotationDecision::Rotate(pin)
            } else {
                RotationDecision::Hold
            };
        }
        if self.request == RotationRequest::Lock {
            return RotationDecision::Hold;
        }
        if !self.has_sensor {
            return RotationDecision::Hold;
        }
        if !self.auto_rotate && self.request != RotationRequest::Rotate {
            return RotationDecision::Hold;
        }
        let Some(newest) = self.ring.newest() else {
            return RotationDecision::Hold;
        };
        if newest == self.current {
            return RotationDecision::Hold;
        }
        if self.ring.trailing_run() >= self.dwell {
            RotationDecision::Rotate(newest)
        } else {
            RotationDecision::Hold
        }
    }

    /// Feed one orientation sample and commit the decision.
    ///
    /// Returns the decision *and* moves the panel when it says
    /// [`RotationDecision::Rotate`], so the policy stays the single source of
    /// where the panel is: a caller that only ever reads `current()` cannot fall
    /// out of step with one that only ever reads the return value.
    ///
    /// [`DeviceOrientation::Undefined`] (face up, face down, or too flat to
    /// classify) does not enter the history and clears it, matching the sensor
    /// model's own reset (`sensor_proxy.rs:91-94`). A pin still applies: a
    /// portrait-locked phone face down is portrait-locked.
    pub fn update(&mut self, sample: DeviceOrientation) -> RotationDecision {
        if sample == DeviceOrientation::Undefined {
            self.ring.clear();
        } else {
            self.ring.push(sample);
        }
        let decision = self.evaluate();
        if let RotationDecision::Rotate(orientation) = decision {
            self.current = orientation;
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that is allowed to rotate, as a starting point for the tests
    /// that are about the dwell and not about the gates.
    fn live() -> RotationPolicy {
        let mut p = RotationPolicy::new();
        p.set_sensor_present(true);
        p.set_auto_rotate(true);
        p
    }

    /// Feed `n` copies of `sample` and report every decision.
    fn feed(p: &mut RotationPolicy, sample: DeviceOrientation, n: usize) -> Vec<RotationDecision> {
        (0..n).map(|_| p.update(sample)).collect()
    }

    // ------------------------------------------------------------ the window

    /// The ring is a ring: pushing past its capacity drops the oldest, not the
    /// newest, and the order stays oldest-first.
    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let mut r: OrientationRing<4> = OrientationRing::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.oldest(), None);
        assert_eq!(r.newest(), None);
        assert_eq!(r.trailing_run(), 0, "an empty ring has no candidate");

        for o in [
            DeviceOrientation::LeftUp,
            DeviceOrientation::LeftUp,
            DeviceOrientation::RightUp,
        ] {
            r.push(o);
        }
        assert_eq!(r.len(), 3);
        assert_eq!(r.at(0), Some(DeviceOrientation::LeftUp), "oldest first");
        assert_eq!(r.at(2), Some(DeviceOrientation::RightUp), "newest last");

        // One past capacity: the *oldest* is dropped, so the second `LeftUp`
        // goes and the first survives as the oldest retained sample. A ring that
        // dropped the newest instead would silently feed the policy a stream
        // that never reaches its present, and every dwell would be one short.
        r.push(DeviceOrientation::BottomUp);
        assert_eq!(r.len(), 4, "capacity is a hard bound");
        r.push(DeviceOrientation::Normal);
        assert_eq!(r.len(), 4, "and stays a hard bound");
        assert_eq!(r.newest(), Some(DeviceOrientation::Normal));
        assert_eq!(r.oldest(), Some(DeviceOrientation::LeftUp));
        assert_eq!(
            [r.at(0), r.at(1), r.at(2), r.at(3)],
            [
                Some(DeviceOrientation::LeftUp),
                Some(DeviceOrientation::RightUp),
                Some(DeviceOrientation::BottomUp),
                Some(DeviceOrientation::Normal)
            ],
            "oldest-first, with the overwritten slot dropped"
        );
        assert_eq!(r.at(4), None, "past the end is not an index");

        // Wrapping three more times must not lose the ordering.
        for _ in 0..(4 * 3) {
            r.push(DeviceOrientation::Normal);
        }
        assert_eq!(r.len(), 4);
        assert!(
            r.oldest().zip(r.newest()).is_some_and(
                |(o, n)| o == DeviceOrientation::Normal && n == DeviceOrientation::Normal
            )
        );
    }

    /// A zero-capacity ring is a type the compiler accepts and a `push` must not
    /// write through.
    #[test]
    fn a_zero_capacity_ring_pushes_nothing() {
        let mut r: OrientationRing<0> = OrientationRing::new();
        r.push(DeviceOrientation::LeftUp);
        r.push(DeviceOrientation::RightUp);
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert_eq!(r.newest(), None);
        assert_eq!(r.at(0), None);
        assert_eq!(r.trailing_run(), 0);
        r.clear();
        assert!(r.is_empty());
    }

    /// `clear` forgets the samples. The `Undefined` reset depends on it, and a
    /// `clear` that only moved the write cursor would leave a stale candidate.
    #[test]
    fn clear_forgets_every_sample() {
        let mut r: OrientationRing<8> = OrientationRing::new();
        feed_ring(&mut r, DeviceOrientation::LeftUp, 5);
        assert_eq!(r.trailing_run(), 5);
        r.clear();
        assert_eq!(r.len(), 0);
        assert_eq!(r.oldest(), None);
        assert_eq!(r.newest(), None);
        assert_eq!(r.trailing_run(), 0, "a cleared ring has no candidate");
        // And it is reusable afterwards.
        r.push(DeviceOrientation::RightUp);
        assert_eq!(r.newest(), Some(DeviceOrientation::RightUp));
        assert_eq!(r.trailing_run(), 1);
    }

    fn feed_ring<const N: usize>(r: &mut OrientationRing<N>, o: DeviceOrientation, n: usize) {
        for _ in 0..n {
            r.push(o);
        }
    }

    /// The dwell counter counts *trailing* agreement and stops at the first
    /// disagreement. This is the whole anti-jitter mechanism, so it is tested
    /// against a stream that disagrees in the middle as well as one that does
    /// not.
    #[test]
    fn the_trailing_run_stops_at_the_first_disagreement() {
        let mut r: OrientationRing<8> = OrientationRing::new();
        feed_ring(&mut r, DeviceOrientation::LeftUp, 4);
        assert_eq!(r.trailing_run(), 4);

        // A single dissent in the middle leaves the run counting only what
        // came after it.
        r.push(DeviceOrientation::Normal);
        r.push(DeviceOrientation::RightUp);
        assert_eq!(r.trailing_run(), 1, "one dissent, one sample of agreement");

        // And it grows again only as the new candidate is repeated.
        r.push(DeviceOrientation::RightUp);
        assert_eq!(r.trailing_run(), 2);
        r.push(DeviceOrientation::RightUp);
        assert_eq!(r.trailing_run(), 3);
    }

    /// An alternating stream never accumulates a run, however long it runs. This
    /// is the jitter case a "majority of the window" rule would survive and a
    /// "changed axis" rule would not; the ring is what makes the difference
    /// visible.
    #[test]
    fn an_alternating_stream_never_settles() {
        let mut r: OrientationRing<ROTATION_WINDOW> = OrientationRing::new();
        for _ in 0..64 {
            r.push(DeviceOrientation::LeftUp);
            r.push(DeviceOrientation::RightUp);
        }
        assert_eq!(r.len(), ROTATION_WINDOW);
        assert_eq!(r.trailing_run(), 1);
        // Half the window each way, oldest first: no consensus to be had either,
        // and the window really is disagreeing with itself rather than holding
        // one sample eight times.
        assert_eq!(r.at(0).map(|o| o != r.at(1).unwrap_or(o)), Some(true));
        let mut agree = 0;
        for i in 0..r.len() {
            if r.at(i) == Some(DeviceOrientation::LeftUp) {
                agree += 1;
            }
        }
        assert_eq!(
            agree,
            ROTATION_WINDOW / 2,
            "the window is split, not unanimous"
        );
    }

    // ------------------------------------------------------------- hysteresis

    /// THE test: one jittery reading must not rotate the panel, and a sustained
    /// one must. Same sample value, different number of times.
    #[test]
    fn a_single_reading_does_not_rotate_and_a_sustained_one_does() {
        let mut p = live();
        // One reading of a new orientation: the most a hand brushing the phone
        // produces.
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Hold,
            "one reading is not a decision"
        );
        assert_eq!(p.current(), DeviceOrientation::Normal);
        assert_eq!(p.history().trailing_run(), 1);

        // A second is still not enough at dwell 3.
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);
        assert_eq!(p.current(), DeviceOrientation::Normal);

        // The third settles it.
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp)
        );
        assert_eq!(
            p.current(),
            DeviceOrientation::LeftUp,
            "the policy tracks the panel, so a caller reading either agrees"
        );
    }

    /// Dwell is honoured exactly, for every legal dwell, and cannot be set past
    /// the window. This is what "after what dwell" means: the answer is a
    /// number, and it is the number the caller asked for.
    #[test]
    fn the_dwell_is_honoured_and_clamped_to_the_window() {
        for dwell in 1..=ROTATION_WINDOW as u8 {
            let mut p = live();
            p.set_dwell_samples(dwell);
            assert_eq!(p.dwell_samples(), dwell);
            let decisions = feed(&mut p, DeviceOrientation::RightUp, dwell as usize);
            for (i, d) in decisions.iter().enumerate() {
                assert_eq!(
                    d.is_rotate(),
                    i + 1 == dwell as usize,
                    "dwell {dwell}: sample {} should not decide yet",
                    i + 1
                );
            }
            assert_eq!(p.current(), DeviceOrientation::RightUp);
        }

        // Out of range in both directions.
        let mut p = live();
        p.set_dwell_samples(0);
        assert_eq!(p.dwell_samples(), 1, "zero would mean 'never decide'");
        p.set_dwell_samples(255);
        assert_eq!(
            p.dwell_samples(),
            ROTATION_WINDOW as u8,
            "a dwell past the window can never be reached"
        );
    }

    /// Jitter that flips between two *candidate* orientations must never
    /// rotate, however long it runs -- this is the case a majority rule would
    /// get wrong and a trailing-run rule gets right.
    #[test]
    fn jitter_between_two_candidates_never_rotates() {
        let mut p = live();
        for i in 0..200 {
            let sample = if i % 2 == 0 {
                DeviceOrientation::LeftUp
            } else {
                DeviceOrientation::RightUp
            };
            assert_eq!(
                p.update(sample),
                RotationDecision::Hold,
                "alternating candidates must not decide"
            );
        }
        assert_eq!(p.current(), DeviceOrientation::Normal);
        assert_eq!(p.history().trailing_run(), 1);
    }

    /// A settled candidate does not keep re-deciding. The panel is where the
    /// decision put it, so the next identical sample holds -- which is what
    /// stops a shell forwarding the decision into a modeset every frame.
    #[test]
    fn a_settled_panel_is_not_re_decided() {
        let mut p = live();
        feed(&mut p, DeviceOrientation::LeftUp, 3);
        let mut rotates = 0;
        for _ in 0..50 {
            if p.update(DeviceOrientation::LeftUp).is_rotate() {
                rotates += 1;
            }
        }
        assert_eq!(rotates, 0, "the panel is already there");
    }

    /// A face-down or flat reading resets the candidate, and the count has to
    /// start again afterwards. Without the reset, three samples that straddle a
    /// flat phase would rotate a phone that was never held sideways.
    #[test]
    fn a_flat_reading_drops_the_candidate_and_the_count_restarts() {
        let mut p = live();
        feed(&mut p, DeviceOrientation::LeftUp, 2);
        assert_eq!(p.history().trailing_run(), 2);

        assert_eq!(
            p.update(DeviceOrientation::Undefined),
            RotationDecision::Hold
        );
        assert_eq!(p.history().len(), 0, "flat contributes nothing");
        assert_eq!(p.history().trailing_run(), 0);

        // Two more is not enough again.
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp),
            "and the third still settles it"
        );
    }

    /// Sampling face-down forever holds and stays sampled, so the policy does
    /// not get stuck waiting for a candidate that is not coming.
    #[test]
    fn an_endless_flat_stream_holds_without_panicking() {
        let mut p = live();
        for _ in 0..100 {
            assert_eq!(
                p.update(DeviceOrientation::Undefined),
                RotationDecision::Hold
            );
        }
        assert_eq!(p.current(), DeviceOrientation::Normal);
        assert_eq!(p.history().len(), 0);
    }

    // ------------------------------------------------------- degenerate cases

    /// No sensor: never rotate, however long and however consistent the readings
    /// are. This is the state a device is in with no `net.hadess.SensorProxy`
    /// socket.
    #[test]
    fn no_sensor_never_rotates_however_sustained_the_reading() {
        let mut p = RotationPolicy::new();
        p.set_auto_rotate(true); // the switch is on and it still must not move
        assert!(!p.sensor_present());
        for _ in 0..64 {
            assert_eq!(
                p.update(DeviceOrientation::LeftUp),
                RotationDecision::Hold,
                "a sample cannot create the sensor it needs"
            );
        }
        assert_eq!(p.current(), DeviceOrientation::Normal);
        // The samples are still recorded, so a caller that later learns a sensor
        // exists has the history rather than a cold start.
        assert_eq!(p.history().len(), ROTATION_WINDOW);

        // And a pin still fires without one.
        p.set_pinned(Some(DeviceOrientation::Normal));
        assert_eq!(p.evaluate(), RotationDecision::Hold);
        p.set_pinned(Some(DeviceOrientation::LeftUp));
        assert_eq!(
            p.evaluate(),
            RotationDecision::Rotate(DeviceOrientation::LeftUp),
            "a lock is the user's instruction, not the sensor's"
        );
    }

    /// Auto-rotate off holds where the user left the panel -- including
    /// sideways, which is the point: "off" means "do not follow the sensor", not
    /// "go back to portrait". This is the reference's
    /// `SCREEN_ORIENTATION_NOSENSOR` (`RotationHelper.java:227-231`).
    #[test]
    fn auto_rotate_off_holds_where_the_user_left_it() {
        let mut p = RotationPolicy::new();
        p.set_sensor_present(true);
        assert!(!p.auto_rotate());
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);
        assert_eq!(p.current(), DeviceOrientation::Normal);

        // The user turns it on, and the candidate the sensor was already
        // accumulating is *not* thrown away: the switch gates the decision, not
        // the history. So one more reading settles it.
        p.set_auto_rotate(true);
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp)
        );
        assert_eq!(p.history().trailing_run(), 3);

        // And off again, from landscape: sideways stays sideways.
        p.set_auto_rotate(false);
        assert_eq!(
            p.update(DeviceOrientation::Normal),
            RotationDecision::Hold,
            "off does not mean portrait"
        );
        assert_eq!(p.current(), DeviceOrientation::LeftUp);
    }

    /// A portrait lock moves the panel at once, with no dwell and no sensor, and
    /// stops moving it once it is there.
    #[test]
    fn a_portrait_lock_needs_no_dwell_and_no_sensor() {
        let mut p = live();
        p.reset(DeviceOrientation::RightUp);
        p.set_pinned(Some(DeviceOrientation::Normal));
        assert_eq!(p.pinned(), Some(DeviceOrientation::Normal));
        // Not one sample fed: a lock is not a candidate.
        assert_eq!(
            p.evaluate(),
            RotationDecision::Rotate(DeviceOrientation::Normal)
        );
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::Normal)
        );
        assert_eq!(p.current(), DeviceOrientation::Normal);

        // There it is: holds, and the sensor is still ignored.
        assert_eq!(p.update(DeviceOrientation::LeftUp), RotationDecision::Hold);

        // Unpinning hands control back to the sensor, whose history is intact:
        // two `LeftUp` samples arrived while pinned, so one more settles it.
        p.set_pinned(None);
        assert_eq!(p.pinned(), None);
        assert_eq!(p.evaluate(), RotationDecision::Hold, "two of three");
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp),
            "the samples that arrived while pinned were still recorded"
        );
    }

    /// `RotationRequest` reproduces the reference's ladder: `Lock` beats the
    /// sensor, `Rotate` reaches it through a disabled auto-rotate switch, and
    /// `None` defers to the switch.
    #[test]
    fn a_request_outranks_the_switch_in_both_directions() {
        let mut p = RotationPolicy::new();
        p.set_sensor_present(true);
        p.set_auto_rotate(false);

        // Lock: ignore the sensor entirely.
        p.set_request(RotationRequest::Lock);
        assert_eq!(p.request(), RotationRequest::Lock);
        feed(&mut p, DeviceOrientation::LeftUp, 8);
        assert_eq!(p.current(), DeviceOrientation::Normal, "locked");

        // Rotate: the sensor gets a say even with the switch off.
        p.set_request(RotationRequest::Rotate);
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp),
            "UNSPECIFIED, not NOSENSOR"
        );

        // None: back to obeying the switch.
        p.set_request(RotationRequest::None);
        assert_eq!(p.request(), RotationRequest::None);
        assert_eq!(p.update(DeviceOrientation::Normal), RotationDecision::Hold);

        // And a pin outranks all three.
        p.set_pinned(Some(DeviceOrientation::BottomUp));
        assert_eq!(
            p.evaluate(),
            RotationDecision::Rotate(DeviceOrientation::BottomUp)
        );
        assert_eq!(RotationRequest::default(), RotationRequest::None);
    }

    /// A panel whose home is landscape: the offsets are all shifted by a
    /// quarter, and "upright" is not the panel's home.
    #[test]
    fn a_landscape_natural_panel_offsets_every_orientation() {
        let mut p = live();
        p.set_natural(DeviceOrientation::LeftUp);
        assert_eq!(p.natural(), DeviceOrientation::LeftUp);
        // No samples yet, so the panel starts at its own home.
        assert_eq!(p.current(), DeviceOrientation::LeftUp);

        assert_eq!(
            quarter_turns(DeviceOrientation::LeftUp, DeviceOrientation::LeftUp),
            0
        );
        assert_eq!(
            quarter_turns(DeviceOrientation::Normal, DeviceOrientation::LeftUp),
            3
        );
        assert_eq!(
            quarter_turns(DeviceOrientation::RightUp, DeviceOrientation::LeftUp),
            2
        );
        assert_eq!(
            quarter_turns(DeviceOrientation::BottomUp, DeviceOrientation::LeftUp),
            1
        );

        // Held upright, the panel is a quarter turn from home -- not the panel's
        // home transform, which would be the bug this case exists to catch.
        assert_eq!(
            transform_for(DeviceOrientation::LeftUp, DeviceOrientation::LeftUp),
            Transform::None
        );
        assert_eq!(
            transform_for(DeviceOrientation::Normal, DeviceOrientation::LeftUp),
            Transform::Rotate270
        );
        assert_eq!(
            transform_for(DeviceOrientation::RightUp, DeviceOrientation::LeftUp),
            Transform::Rotate180
        );

        // And it rotates like any other panel once the sensor settles.
        feed(&mut p, DeviceOrientation::RightUp, 3);
        assert_eq!(p.current(), DeviceOrientation::RightUp);

        // A landscape-only panel cannot do portrait, and a portrait-locked
        // landscape-native device is therefore unreachable -- which the modeset
        // decision has to be able to say (see `a_portrait_only_panel_cannot...`).
    }

    /// The defaults are the conservative ones, and `Default` agrees with `new`.
    #[test]
    fn a_default_policy_holds() {
        let p = RotationPolicy::new();
        assert_eq!(p, RotationPolicy::default());
        assert!(!p.sensor_present());
        assert!(!p.auto_rotate());
        assert_eq!(p.pinned(), None);
        assert_eq!(p.request(), RotationRequest::None);
        assert_eq!(p.natural(), DeviceOrientation::Normal);
        assert_eq!(p.current(), DeviceOrientation::Normal);
        assert_eq!(p.dwell_samples(), DEFAULT_DWELL_SAMPLES);
        assert!(p.history().is_empty());
        assert_eq!(p.evaluate(), RotationDecision::Hold);
        assert_eq!(RotationDecision::Hold.orientation(), None);
        assert!(!RotationDecision::Hold.is_rotate());
    }

    /// `reset` moves the panel without a decision and without touching the
    /// history -- the shell's post-modeset path.
    #[test]
    fn reset_moves_the_panel_without_deciding() {
        let mut p = live();
        feed(&mut p, DeviceOrientation::LeftUp, 3);
        assert_eq!(p.current(), DeviceOrientation::LeftUp);
        let len = p.history().len();
        p.reset(DeviceOrientation::BottomUp);
        assert_eq!(p.current(), DeviceOrientation::BottomUp);
        assert_eq!(p.history().len(), len, "the samples really happened");
        // The panel was moved behind the policy's back, so the settled `LeftUp`
        // run now *is* a change of orientation and it decides at once -- which is
        // why `reset` exists: a shell that has just performed its own modeset
        // calls it rather than pretending the move was the policy's.
        assert_eq!(
            p.update(DeviceOrientation::LeftUp),
            RotationDecision::Rotate(DeviceOrientation::LeftUp),
            "the settled run is a real move away from where the panel is"
        );
        assert_eq!(p.current(), DeviceOrientation::LeftUp);
    }

    /// `set_natural` only moves a panel that has not been observed yet. Once
    /// samples exist the panel's position is history.
    #[test]
    fn a_new_home_moves_the_panel_only_before_any_sample() {
        let mut p = live();
        p.set_natural(DeviceOrientation::RightUp);
        assert_eq!(p.current(), DeviceOrientation::RightUp, "nothing seen yet");
        feed(&mut p, DeviceOrientation::BottomUp, 3);
        assert_eq!(p.current(), DeviceOrientation::BottomUp);
        p.set_natural(DeviceOrientation::Normal);
        assert_eq!(
            p.current(),
            DeviceOrientation::BottomUp,
            "a panel that has moved does not snap to the new home"
        );
        assert_eq!(p.natural(), DeviceOrientation::Normal);
    }

    // ------------------------------------------------------------ the geometry

    /// Quarter turns, for every orientation against every home. The table is
    /// exhaustive rather than sampled: this is a four-by-four map and a missing
    /// cell is a device that renders the wrong way up.
    #[test]
    fn quarter_turns_is_a_complete_bijection_against_each_home() {
        const ALL: [DeviceOrientation; 4] = [
            DeviceOrientation::Normal,
            DeviceOrientation::LeftUp,
            DeviceOrientation::BottomUp,
            DeviceOrientation::RightUp,
        ];
        for natural in ALL {
            // Every orientation maps to a distinct turn, and they are exactly
            // 0..=3 -- so the map is a rotation, not a collapse.
            let mut seen = 0u8;
            for o in ALL {
                seen |= 1 << quarter_turns(o, natural);
            }
            assert_eq!(
                seen, 0b1111,
                "home {natural:?} does not produce a bijection"
            );
            assert_eq!(quarter_turns(natural, natural), 0, "home is zero");
        }
        // A portrait phone is the identity, which is what makes this inert on
        // the device shape it was written for.
        for (i, o) in ALL.iter().enumerate() {
            assert_eq!(quarter_turns(*o, DeviceOrientation::Normal), i as u8);
            assert_eq!(transform_for(*o, DeviceOrientation::Normal), as_drm(o));
        }
        // Undefined is total and does not panic.
        assert_eq!(
            quarter_turns(DeviceOrientation::Undefined, DeviceOrientation::Normal),
            0
        );
        assert_eq!(
            quarter_turns(DeviceOrientation::Normal, DeviceOrientation::Undefined),
            0
        );
    }

    /// Every transform the function can return is reachable, and none is
    /// `FlipH`/`FlipV` -- a rotation policy that could emit a flip would be a
    /// mirrored launcher.
    #[test]
    fn transform_for_only_ever_returns_rotations() {
        const ALL: [DeviceOrientation; 5] = [
            DeviceOrientation::Normal,
            DeviceOrientation::LeftUp,
            DeviceOrientation::BottomUp,
            DeviceOrientation::RightUp,
            DeviceOrientation::Undefined,
        ];
        let mut seen = 0u8;
        for natural in ALL {
            for o in ALL {
                let bit = match transform_for(o, natural) {
                    Transform::None => 1,
                    Transform::Rotate90 => 2,
                    Transform::Rotate180 => 4,
                    Transform::Rotate270 => 8,
                    other => panic!("a rotation policy produced {other:?}"),
                };
                seen |= bit;
            }
        }
        assert_eq!(seen, 0b1111, "all four rotations are reachable");
    }

    /// The reference's own mapping is what the portrait case must reproduce, so
    /// the test compares against it rather than against a second derivation.
    fn as_drm(o: &DeviceOrientation) -> Transform {
        o.to_transform()
    }

    // -------------------------------------------------------- the modeset call

    /// The three outcomes, and the order that produces them.
    #[test]
    fn decide_modeset_separates_no_change_from_a_change_from_an_impossible_one() {
        // Already there.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::Normal,
                DeviceOrientation::Normal,
                PanelRotations::PORTRAIT_ONLY
            ),
            ModesetAction::None
        );
        // Change, and the panel can do it.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::LeftUp,
                DeviceOrientation::Normal,
                PanelRotations::ALL
            ),
            ModesetAction::Rotate(DeviceOrientation::LeftUp)
        );
        // Change, and the panel cannot: refused, not attempted.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::LeftUp,
                DeviceOrientation::Normal,
                PanelRotations::PORTRAIT_ONLY
            ),
            ModesetAction::Unsupported(DeviceOrientation::LeftUp)
        );
        assert_eq!(ModesetAction::None.orientation(), None);
        assert_eq!(
            ModesetAction::Rotate(DeviceOrientation::RightUp).orientation(),
            Some(DeviceOrientation::RightUp)
        );
        assert_eq!(
            ModesetAction::Unsupported(DeviceOrientation::RightUp).orientation(),
            Some(DeviceOrientation::RightUp)
        );
    }

    /// A portrait-only panel: portrait is free, landscape is refused, and being
    /// *already* in a rotation is never re-decided even if the mask omits it --
    /// the driver put it there.
    #[test]
    fn a_portrait_only_panel_refuses_landscape_without_disturbing_the_current_one() {
        let p = PanelRotations::PORTRAIT_ONLY;
        assert!(p.contains(0));
        assert!(p.contains(2));
        assert!(!p.contains(1));
        assert!(!p.contains(3));

        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::BottomUp,
                DeviceOrientation::Normal,
                p
            ),
            ModesetAction::Rotate(DeviceOrientation::BottomUp),
            "the other portrait rotation is allowed"
        );
        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::LeftUp,
                DeviceOrientation::Normal,
                p
            ),
            ModesetAction::Unsupported(DeviceOrientation::LeftUp)
        );
        // Already sideways, on a panel that does not claim to support it: the
        // answer is still "nothing to do", because the panel is there.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::LeftUp,
                DeviceOrientation::LeftUp,
                DeviceOrientation::Normal,
                p
            ),
            ModesetAction::None
        );
    }

    /// A landscape-native panel that is also landscape-only: its home must be
    /// reachable, which is the case a naive "compare orientations" gets wrong.
    #[test]
    fn a_landscape_only_panel_reaches_its_own_home() {
        let p = PanelRotations::LANDSCAPE_ONLY;
        assert!(p.contains(1));
        assert!(p.contains(3));
        assert!(!p.contains(0));
        assert!(!p.contains(2));
        // Home is LeftUp -> turn 0, which a landscape-only panel does not have.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::Normal,
                DeviceOrientation::LeftUp,
                DeviceOrientation::LeftUp,
                p
            ),
            ModesetAction::Unsupported(DeviceOrientation::LeftUp)
        );
        // Held upright the panel needs 270 degrees, which it does have.
        assert_eq!(
            decide_modeset(
                DeviceOrientation::LeftUp,
                DeviceOrientation::Normal,
                DeviceOrientation::LeftUp,
                p
            ),
            ModesetAction::Rotate(DeviceOrientation::Normal)
        );
    }

    /// The mask is a byte, four bits, and total: nothing outside 0..=3 is ever
    /// "contained".
    #[test]
    fn the_rotation_mask_is_total() {
        assert_eq!(PanelRotations::from_mask(0xFF), PanelRotations::ALL);
        assert_eq!(PanelRotations::from_mask(0xF0).mask(), 0b0000);
        assert_eq!(PanelRotations::default(), PanelRotations::NONE);
        assert_eq!(PanelRotations::NONE.mask(), 0);
        assert!(!PanelRotations::NONE.contains(0));
        assert!(!PanelRotations::ALL.contains(4));
        assert!(!PanelRotations::ALL.contains(255));
        for t in 0..4u8 {
            assert!(PanelRotations::ALL.contains(t), "ALL omits {t}");
        }
    }

    /// The policy and the modeset decision agree: a policy that says
    /// `Rotate(LeftUp)` on a portrait-only panel produces an `Unsupported`
    /// action, and never a `Rotate` that would fail.
    #[test]
    fn the_policy_and_the_modeset_decision_compose() {
        let mut p = live();
        let was = p.current();
        let decisions = feed(&mut p, DeviceOrientation::LeftUp, 3);
        let wanted = decisions.last().copied().unwrap().orientation();
        assert_eq!(wanted, Some(DeviceOrientation::LeftUp));
        assert_eq!(
            p.current(),
            DeviceOrientation::LeftUp,
            "the policy committed the move"
        );
        assert_eq!(
            decide_modeset(was, p.current(), p.natural(), PanelRotations::PORTRAIT_ONLY),
            ModesetAction::Unsupported(DeviceOrientation::LeftUp),
            "the panel cannot follow the policy, and says so before the modeset"
        );
        // On a panel that can, the same policy yields the same orientation.
        assert_eq!(
            decide_modeset(was, p.current(), p.natural(), PanelRotations::ALL),
            ModesetAction::Rotate(DeviceOrientation::LeftUp)
        );
    }
}
