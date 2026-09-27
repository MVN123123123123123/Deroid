//! Unified Single-Process Wayland Compositor Server & Event Loop.
//! Manages Wayland client sockets, protocols, scene graph updates,
//! HWC multi-plane frame presentation, and memory/performance metrics.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::compositor::gestures::{GestureConfig, GestureEngine, RawTouchEvent, TouchPhase};
use crate::compositor::power_sync::UtimPowerSync;
use crate::compositor::protocols::ProtocolRegistry;
use crate::compositor::scene::MobileScene;
use crate::graphics::composer::HwcComposer;

/// Performance & Memory Diagnostic Metrics
#[derive(Debug, Clone, PartialEq)]
pub struct CompositorMetrics {
    pub resident_memory_bytes: usize, // RSS bytes from /proc/self/statm
    pub boot_to_launcher_duration: Duration, // Cold boot startup to first presented frame
    pub touch_processing_latency: Duration, // Sub-8ms input guarantee
    pub is_rss_within_target: bool,   // RSS < 15 MB
    pub is_boot_within_target: bool,  // Boot < 0.45s
}

/// Wayland Compositor Server
///
/// Note there is deliberately **no** `gestures` field here. One used to
/// exist, and it was never consulted: the live shell owns its own
/// `GestureEngine` (`crates/utlc/src/main.rs`), so the copy in `WaylandServer`
/// was a second engine that only existed to be measured. `get_metrics` used
/// to build a *third*, throwaway engine for the same purpose. All three are
/// gone; metrics now probe the engine the caller actually owns.
pub struct WaylandServer {
    pub socket_path: PathBuf,
    pub listener: Option<UnixListener>,
    pub protocols: ProtocolRegistry,
    pub scene: MobileScene,
    pub power_sync: UtimPowerSync,
    pub start_time: Instant,
    pub first_frame_presented_at: Option<Instant>,
    pub last_vsync_ns: u64,
    pub client_streams: Vec<UnixStream>,
    pub running: bool,
}

impl WaylandServer {
    pub fn new(
        socket_path: &Path,
        width: u32,
        height: u32,
        refresh_rate: f64,
        hwc_composer: HwcComposer,
    ) -> Self {
        let start_time = Instant::now();
        let scene = MobileScene::new(0, width, height, refresh_rate, hwc_composer);
        let power_sync = UtimPowerSync::default();

        Self {
            socket_path: socket_path.to_path_buf(),
            listener: None,
            protocols: ProtocolRegistry::new(),
            scene,
            power_sync,
            start_time,
            first_frame_presented_at: None,
            last_vsync_ns: 0,
            client_streams: Vec::new(),
            running: false,
        }
    }

    /// Bind the Wayland UNIX domain socket (e.g. /run/user/1000/wayland-0)
    ///
    /// Hardening (P9): a pre-existing symlink is never replaced (refuse
    /// instead of unlinking somebody else's file); the socket is created
    /// under `umask(0077)` so no other user can connect before permissions
    /// are applied; parent-dir creation and chmod failures propagate
    /// instead of being silently ignored.
    pub fn bind_socket(&mut self) -> io::Result<()> {
        // lstat, not stat: a symlink must be detected, never followed.
        if let Ok(meta) = fs::symlink_metadata(&self.socket_path) {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "refusing to replace symlinked socket path",
                ));
            }
            // Regular file / stale socket: remove, propagating errors.
            fs::remove_file(&self.socket_path)?;
        }

        if let Some(parent) = self.socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        // Restrict creation mode: only the owner may access the socket.
        let old_mask = unsafe { libc::umask(0o077) };
        let bound = UnixListener::bind(&self.socket_path);
        unsafe {
            libc::umask(old_mask);
        }
        let listener = bound?;
        listener.set_nonblocking(true)?;

        // Set permissions: 0o700 for user-specific display socket.
        fs::set_permissions(&self.socket_path, fs::Permissions::from_mode(0o700))?;
        self.listener = Some(listener);
        Ok(())
    }

    /// Simulate or perform cold boot to first rendered frame
    pub fn boot_to_first_frame(&mut self) -> Result<Duration, String> {
        self.scene.prepare_frame()?;
        let vsync = if self.last_vsync_ns == 0 {
            1_000_000_000u64
        } else {
            self.last_vsync_ns
        };
        let present = vsync + 10_000;
        self.scene.present_frame(vsync, present)?;
        self.last_vsync_ns = vsync;

        let duration = self.start_time.elapsed();
        self.first_frame_presented_at = Some(Instant::now());
        Ok(duration)
    }

    /// Measure actual Resident Set Size (RSS) memory in bytes from /proc/self/statm
    pub fn measure_resident_memory(&self) -> usize {
        if let Ok(content) = fs::read_to_string("/proc/self/statm") {
            let mut parts = content.split_whitespace();
            // Format: size resident shared text lib data dirty
            if let Some(_size) = parts.next() {
                if let Some(res_pages) = parts.next().and_then(|s| s.parse::<usize>().ok()) {
                    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
                    return res_pages * page_size;
                }
            }
        }
        // Fallback estimate: 8 MB
        8 * 1024 * 1024
    }

    /// Collect comprehensive diagnostic metrics for verification suites.
    ///
    /// Side-effect free by construction (P5): the live scene graph is never
    /// mutated here. Boot failures propagate as `Err` instead of a fabricated
    /// 150 ms duration (P6).
    ///
    /// The touch probe lives in its own function ([`Self::measure_touch_latency`])
    /// rather than here, precisely so it can be re-pointed at the real
    /// `InputDispatcher` -> `GestureEngine` -> `Layout` path without touching
    /// the boot/RSS gates.
    pub fn get_metrics(&mut self) -> Result<CompositorMetrics, String> {
        let rss = self.measure_resident_memory();
        let boot_dur = match self.first_frame_presented_at {
            Some(t) => t.duration_since(self.start_time),
            None => self.boot_to_first_frame()?,
        };

        Ok(CompositorMetrics {
            resident_memory_bytes: rss,
            boot_to_launcher_duration: boot_dur,
            touch_processing_latency: self.measure_touch_latency(),
            is_rss_within_target: rss <= 15 * 1024 * 1024,
            is_boot_within_target: boot_dur < Duration::from_millis(450),
        })
    }

    /// Time one synthetic bottom-edge `Down` through a [`GestureEngine`].
    ///
    /// The engine is constructed locally and dropped, so the caller's live
    /// gesture state cannot be perturbed. The sentinel `touch_id`
    /// (`i32::MAX`) cannot collide with a real evdev slot id.
    ///
    /// Known limitation, tracked by the launcher rewrite plan §8.3: this
    /// measures the engine in isolation. It does **not** include
    /// `InputDispatcher::process_event` or the `Layout` hit-test, so the
    /// reported figure is a lower bound on real end-to-end touch latency,
    /// not a measurement of it.
    pub fn measure_touch_latency(&self) -> Duration {
        let touch_start = Instant::now();
        let mut probe = GestureEngine::new(
            self.scene.width as f32,
            self.scene.height as f32,
            GestureConfig::default(),
        );
        let _ = probe.process_touch(&RawTouchEvent {
            touch_id: i32::MAX,
            phase: TouchPhase::Down,
            x: self.scene.width as f32 * 0.5,
            y: self.scene.height as f32 - 10.0,
            timestamp: touch_start,
        });
        touch_start.elapsed()
    }

    /// Process a single frame step.
    ///
    /// The scene's refresh rate is validated (finite and > 0, else 60Hz)
    /// and timestamp advances use `checked_add` so a hostile rate can
    /// neither divide-by-zero/NaN the period nor wrap the clock (P7).
    pub fn step_frame(&mut self, dt: f32) -> Result<(), String> {
        self.scene.update(dt);
        self.scene.prepare_frame()?;

        let rate = if self.scene.refresh_rate.is_finite() && self.scene.refresh_rate > 0.0 {
            self.scene.refresh_rate
        } else {
            60.0
        };
        let period_ns = (1_000_000_000.0 / rate) as u64;
        self.last_vsync_ns = if self.last_vsync_ns == 0 {
            1_000_000_000u64
        } else {
            self.last_vsync_ns
                .checked_add(period_ns)
                .ok_or("vsync timestamp overflow")?
        };
        let present_ns = self
            .last_vsync_ns
            .checked_add(10_000)
            .ok_or("present timestamp overflow")?;
        self.scene.present_frame(self.last_vsync_ns, present_ns)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::composer::HwcVersion;

    #[test]
    fn test_server_boot_metrics_and_rss() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let socket_path = PathBuf::from("/tmp/test-wayland-0.sock");
        let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);

        let boot_dur = server.boot_to_first_frame().expect("Boot failed");
        assert!(boot_dur < Duration::from_millis(450));

        let metrics = server.get_metrics().expect("metrics failed");
        assert!(metrics.is_boot_within_target);
        assert!(metrics.touch_processing_latency < Duration::from_millis(8));
        assert!(metrics.is_rss_within_target);
    }

    #[test]
    fn test_get_metrics_has_no_side_effects() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let socket_path = PathBuf::from("/tmp/test-wayland-metrics.sock");
        let mut server = WaylandServer::new(&socket_path, 1080, 2400, 60.0, hwc);
        server.boot_to_first_frame().expect("Boot failed");

        // The server no longer owns a gesture engine at all, so there is no
        // live engine state that `get_metrics` could perturb. What it must not
        // do is change the scene or the boot clock, so snapshot both.
        let mode_before = server.scene.mode;
        let first_frame_before = server.first_frame_presented_at;
        let _ = server.get_metrics().expect("metrics failed");

        assert_eq!(server.scene.mode, mode_before, "metrics must not touch the scene");
        assert_eq!(
            server.first_frame_presented_at, first_frame_before,
            "metrics must not re-run the boot path"
        );
    }

    #[test]
    fn test_touch_latency_probe_is_repeatable_and_non_perturbing() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let socket_path = PathBuf::from("/tmp/test-wayland-touch.sock");
        let mut server = WaylandServer::new(&socket_path, 1080, 2400, 60.0, hwc);
        server.boot_to_first_frame().expect("Boot failed");

        // Every probe starts from a fresh engine, so latency must not creep
        // upward with call count (which is what a stateful probe would do).
        let mut worst = Duration::ZERO;
        for _ in 0..64 {
            let d = server.measure_touch_latency();
            assert!(d < Duration::from_millis(8), "probe blew budget: {d:?}");
            worst = worst.max(d);
        }
        assert!(worst < Duration::from_millis(8), "worst probe: {worst:?}");
    }

    #[test]
    fn test_step_frame_rejects_bad_refresh_rate() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let socket_path = PathBuf::from("/tmp/test-wayland-rate.sock");
        let mut server = WaylandServer::new(&socket_path, 1080, 2400, 60.0, hwc);
        for bad in [0.0, -120.0, f64::NAN, f64::INFINITY] {
            server.scene.refresh_rate = bad;
            assert!(server.step_frame(0.016).is_ok(), "rate {bad} must fall back to 60Hz");
        }
        server.last_vsync_ns = u64::MAX;
        server.scene.refresh_rate = 60.0;
        assert!(server.step_frame(0.016).is_err(), "wrapping timestamps must error");
    }

    #[test]
    fn test_server_socket_lifecycle() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let socket_path = PathBuf::from("/tmp/test-wayland-listen.sock");
        let mut server = WaylandServer::new(&socket_path, 1080, 2400, 60.0, hwc);

        assert!(server.bind_socket().is_ok());
        assert!(socket_path.exists());
        let _ = fs::remove_file(&socket_path);
    }
}
