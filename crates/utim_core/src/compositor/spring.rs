//! Sub-stepped 1D damped-harmonic oscillator shared by the SystemUI shade and
//! the IME slide.
//!
//! # Why this lives in its own module
//!
//! It used to live in `compositor::launcher` next to `WorkspaceGrid`,
//! `HotseatDock` and `AppDrawer`. Those three types turned out to have no
//! production call site (the shell reads `graphics::layout::Layout` and
//! `drm_kms::paint_frame` directly, and drives its own state in
//! `crates/utlc/src/main.rs`), so the whole file was deleted. The oscillator
//! itself *is* live — `SystemUiShade::pull_spring` and
//! `VirtualKeyboard::slide_spring` both step it every frame — so it was moved
//! here rather than deleted with its former neighbours.
//!
//! # Why this is not `graphics::drm_kms::SpringSimulation`
//!
//! Two different integrators with two different parameterisations, and the
//! distinction is deliberate:
//!
//! * This one is *numeric* (semi-implicit Euler, sub-stepped) and takes an
//!   explicit damping coefficient `c` alongside `k` and `m`.
//! * `SpringSimulation` is *closed-form* and takes a damping **ratio** `ζ`,
//!   matching Android's `SpringForce` (`k = ω²`, `c = 2ζω`).
//!
//! Both are kept. Replacing either with the other would silently change the
//! feel of a shipped animation.

/// Spring physics parameters for smooth SystemUI/IME transitions.
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

/// Spring-physics 1D harmonic oscillator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringOscillator {
    pub current: f32,
    pub target: f32,
    pub velocity: f32,
    pub config: SpringConfig,
}

impl SpringOscillator {
    /// Hard ceiling for one integration sub-step: 240 Hz keeps |lambda| < 1
    /// for every spring config in the crate (measured divergence thresholds
    /// are 0.060..0.081 s, so a single 1/60 s step is already marginal and
    /// any stalled frame would explode without sub-stepping).
    const MAX_STEP: f32 = 1.0 / 240.0;
    /// Hard ceiling for the total simulated time per call: a suspend/resume
    /// must not teleport the UI.
    const MAX_TOTAL: f32 = 0.25;
    /// Both value and velocity must sit inside this to count as settled.
    pub const SETTLE_EPSILON: f32 = 0.05;

    pub fn new(initial: f32, config: SpringConfig) -> Self {
        Self {
            current: initial,
            target: initial,
            velocity: 0.0,
            config,
        }
    }

    /// Step simulation by dt (seconds). Clamped and sub-stepped so no
    /// caller can destabilise the spring: non-finite/non-positive dt is
    /// ignored, dt is capped at MAX_TOTAL, and integration runs in MAX_STEP
    /// slices with a NaN watchdog that parks the spring on its target.
    pub fn step(&mut self, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        if (self.current - self.target).abs() < Self::SETTLE_EPSILON
            && self.velocity.abs() < Self::SETTLE_EPSILON
        {
            self.current = self.target;
            self.velocity = 0.0;
            return;
        }

        // F = -k * (x - target) - c * v (semi-implicit Euler, sub-stepped)
        let mut remaining = dt.min(Self::MAX_TOTAL);
        while remaining > 0.0 {
            let h = remaining.min(Self::MAX_STEP);
            remaining -= h;
            let displacement = self.current - self.target;
            let spring_force = -self.config.stiffness * displacement;
            let damping_force = -self.config.damping * self.velocity;
            let total_force = spring_force + damping_force;

            let acceleration = total_force / self.config.mass;
            self.velocity += acceleration * h;
            self.current += self.velocity * h;
            if !self.current.is_finite() || !self.velocity.is_finite() {
                // Never let NaN/inf into geometry: park on target.
                self.current = self.target;
                self.velocity = 0.0;
                break;
            }
        }
    }

    pub fn is_settled(&self) -> bool {
        (self.current - self.target).abs() < Self::SETTLE_EPSILON
            && self.velocity.abs() < Self::SETTLE_EPSILON
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
    fn test_step_rejects_hostile_dt_without_exploding() {
        let mut spring = SpringOscillator::new(0.0, SpringConfig::default());
        spring.target = 1.0;

        // Non-finite and non-positive dt is ignored outright; a huge dt is
        // clamped to MAX_TOTAL. None may poison the state with NaN/inf.
        for bad in [f32::NAN, f32::INFINITY, 0.0, -1.0, 1.0e9] {
            spring.step(bad);
            assert!(spring.current.is_finite(), "dt {bad} poisoned current");
            assert!(spring.velocity.is_finite(), "dt {bad} poisoned velocity");
            // Clamped to 0.25 s of simulation against a k=180, zeta=0.67
            // spring: a real overshoot, but a bounded one.
            assert!(
                spring.current > -1.0 && spring.current < 10.0,
                "dt {bad} let the spring run away to {}",
                spring.current
            );
        }
    }

    #[test]
    fn test_overshoot_stays_inside_critical_damping_for_default_config() {
        // default is k=180, c=18, m=1 -> zeta = c / (2*sqrt(k*m)) = 0.67
        let cfg = SpringConfig::default();
        let zeta = cfg.damping / (2.0 * (cfg.stiffness * cfg.mass).sqrt());
        assert!(
            (0.0..1.0).contains(&zeta),
            "default config must be underdamped, got zeta={zeta}"
        );

        let mut spring = SpringOscillator::new(0.0, cfg);
        spring.target = 100.0;
        let mut peak = 0.0f32;
        for _ in 0..240 {
            spring.step(1.0 / 120.0);
            peak = peak.max(spring.current);
        }
        // Underdamped but well-damped: overshoot stays well under 20%.
        assert!(peak > 100.0, "underdamped spring must overshoot, got {peak}");
        assert!(peak < 120.0, "overshoot must stay bounded, got {peak}");
    }
}
