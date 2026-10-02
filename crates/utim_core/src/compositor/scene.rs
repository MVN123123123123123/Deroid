//! Mobile Scene Graph and Multi-Plane Hardware Composer Presentation Engine.
//! Maps visual layers (Wallpaper, Workspace Grid, Hotseat Dock, Active Window,
//! SystemUI Status Bar, Shade, Virtual Keyboard, Lock Screen) directly to
//! HWC 2.x and AIDL composer3 hardware overlay planes with DMA-BUF zero-copy presentation.

use crate::compositor::ime::VirtualKeyboard;
use crate::compositor::lockscreen::LockScreen;
use crate::compositor::systemui::SystemUiShade;
use crate::graphics::composer::{CompositionType, DisplayConfig, HwcComposer, HwcError, Rect};
use crate::graphics::vsync::{VsyncConfig, VsyncPresentationValidator};

/// Lets `MobileScene::{prepare_frame, present_frame}` return the Copy-friendly
/// `HwcError` while `WaylandServer` (which returns `Result<(), String>`)
/// keeps compiling unchanged via `?`.
impl From<HwcError> for String {
    fn from(e: HwcError) -> String {
        e.to_string()
    }
}

/// Scene Graph Visual Plane Order
pub mod plane_z_order {
    pub const WALLPAPER_GRID: u32 = 0;
    pub const HOTSEAT_DOCK: u32 = 10;
    pub const APPLICATION_SURFACE: u32 = 20;
    pub const STATUS_BAR: u32 = 40;
    pub const SYSTEM_UI_SHADE: u32 = 50;
    pub const VIRTUAL_KEYBOARD: u32 = 60;
    pub const LOCK_SCREEN: u32 = 70;
}

/// Active Display Mode of the Mobile Shell
///
/// `Recents` and `SplitScreen` were removed: their only writers were
/// `apply_gesture_action` (dead — no production caller ever dispatched a
/// `GestureAction` here) and each other's match arms, so both were
/// unreachable variants. The overview is owned by the shell in
/// `crates/utlc/src/main.rs`, which has a real task list to select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMode {
    Launcher,
    Application,
    LockScreen,
}

/// The unified Mobile Scene Graph orchestrating all shell components
///
/// This owns only what actually drives the HWC: display geometry, the three
/// planes `prepare_frame` programs, the SystemUI/IME/lock springs that must
/// advance every frame, and the lock-derived mode. The workspace grid, hotseat
/// and app drawer that used to live here were never read by the renderer or
/// the shell, which use `graphics::layout::Layout` and
/// `drm_kms::paint_frame` instead; they were deleted rather than carried.
pub struct MobileScene {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: f64,
    pub mode: ShellMode,
    pub system_ui: SystemUiShade,
    pub keyboard: VirtualKeyboard,
    pub lockscreen: LockScreen,
    pub active_app_surface_id: Option<u32>,
    pub active_app_buffer_fd: Option<i32>,
    pub hwc: HwcComposer,
    pub vsync_validator: VsyncPresentationValidator,
    // HWC layer handles for multi-plane composition. Only the layers
    // prepare_frame programs are created; shade/IME/lock/dock compose via
    // the ClientTarget until explicitly promoted (see try_init_hwc_layers).
    layer_grid: Option<u64>,
    layer_app: Option<u64>,
    layer_status: Option<u64>,
}

impl MobileScene {
    pub fn new(
        display_id: u32,
        width: u32,
        height: u32,
        refresh_rate: f64,
        hwc_composer: HwcComposer,
    ) -> Self {
        let vsync_cfg =
            VsyncConfig::new(refresh_rate).unwrap_or_else(|_| VsyncConfig::new(60.0).unwrap());
        let vsync_validator = VsyncPresentationValidator::new(vsync_cfg);

        let mut scene = Self {
            display_id,
            width,
            height,
            refresh_rate,
            mode: ShellMode::Launcher,
            system_ui: SystemUiShade::new(),
            keyboard: VirtualKeyboard::new(),
            lockscreen: LockScreen::new(None),
            active_app_surface_id: None,
            active_app_buffer_fd: None,
            hwc: hwc_composer,
            vsync_validator,
            layer_grid: None,
            layer_app: None,
            layer_status: None,
        };

        // Register display with HWC
        let display_cfg = DisplayConfig::standard_mobile(display_id, width, height, refresh_rate);
        scene.hwc.register_display(display_cfg);

        // Allocate HWC hardware composition layers; log and degrade if the
        // composer rejects us rather than silently dropping planes.
        if let Err(e) = scene.try_init_hwc_layers() {
            eprintln!("[-] HWC layer init failed, degrading to client composition: {}", e);
        }
        scene
    }

    /// Create only the layers `prepare_frame` actually programs. Unused
    /// layers still consume z-slots and overlay-plane budget in
    /// `validate_display` (max 4 planes): creating shade/IME/lock planes up
    /// front demoted the status bar to client composition every frame. Those
    /// surfaces compose via the ClientTarget until explicitly promoted.
    fn try_init_hwc_layers(&mut self) -> Result<(), HwcError> {
        self.layer_grid = Some(self.hwc.create_layer(self.display_id)?);
        self.layer_app = Some(self.hwc.create_layer(self.display_id)?);
        self.layer_status = Some(self.hwc.create_layer(self.display_id)?);

        // Assign Z-orders; every failure propagates instead of compositing
        // a plane at the wrong depth for the rest of the session.
        self.hwc.set_layer_z_order(
            self.display_id,
            self.layer_grid.unwrap(),
            plane_z_order::WALLPAPER_GRID,
        )?;
        self.hwc.set_layer_z_order(
            self.display_id,
            self.layer_app.unwrap(),
            plane_z_order::APPLICATION_SURFACE,
        )?;
        self.hwc.set_layer_z_order(
            self.display_id,
            self.layer_status.unwrap(),
            plane_z_order::STATUS_BAR,
        )?;
        Ok(())
    }

    /// Update physics and animations for frame presentation
    pub fn update(&mut self, dt: f32) {
        self.system_ui.update(dt);
        self.keyboard.update(dt);

        if self.lockscreen.is_locked() {
            self.mode = ShellMode::LockScreen;
        } else if self.mode == ShellMode::LockScreen {
            self.mode = ShellMode::Launcher;
        }
    }

    /// Update HWC multi-plane layer attributes and validate composition.
    /// Returns the composer's `HwcError` directly: no heap `String` on the
    /// 60-120 Hz frame path (the success path allocates nothing here).
    pub fn prepare_frame(&mut self) -> Result<(), HwcError> {
        let w = self.width as i32;
        let h = self.height as i32;

        let full_rect = Rect {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };

        // 1. Grid / Wallpaper Layer
        if let Some(l) = self.layer_grid {
            self.hwc
                .set_layer_display_frame(self.display_id, l, full_rect)?;
            self.hwc
                .set_layer_composition_type(self.display_id, l, CompositionType::Device)?;
            self.hwc.set_layer_buffer(self.display_id, l, 100, None)?;
        }

        // 2. Application Layer (with IME viewport push if active)
        if let Some(l) = self.layer_app {
            let push_y = self.keyboard.window_viewport_push_y() as i32;
            let app_rect = Rect {
                left: 0,
                top: -push_y,
                right: w,
                bottom: h - push_y,
            };
            self.hwc
                .set_layer_display_frame(self.display_id, l, app_rect)?;

            // Only `Application` gets a device-composited plane. `Launcher`
            // and `LockScreen` have no client surface of their own, so their
            // grid is painted by the shell into the same primary buffer and
            // must stay client-composited or it would be double-drawn.
            let comp_type = match self.mode {
                ShellMode::Application => CompositionType::Device,
                _ => CompositionType::Client,
            };
            self.hwc
                .set_layer_composition_type(self.display_id, l, comp_type)?;
            if let Some(fd) = self.active_app_buffer_fd {
                self.hwc
                    .set_layer_buffer(self.display_id, l, fd as u64, None)?;
            }
        }

        // 3. Status Bar Layer
        if let Some(l) = self.layer_status {
            let status_rect = Rect {
                left: 0,
                top: 0,
                right: w,
                bottom: self.system_ui.status_bar.height as i32,
            };
            self.hwc
                .set_layer_display_frame(self.display_id, l, status_rect)?;
            self.hwc
                .set_layer_composition_type(self.display_id, l, CompositionType::Device)?;
            self.hwc.set_layer_buffer(self.display_id, l, 200, None)?;
        }

        // 4. Set ClientTarget fallback buffer
        self.hwc.set_client_target(self.display_id, 9999, None)?;

        // 5. Validate HWC composition
        let (changed, has_client) = self.hwc.validate_display(self.display_id)?;

        if has_client {
            // Surface the demotion instead of silently accepting it: some
            // plane fell back to GPU composition this frame.
            eprintln!(
                "[-] HWC demoted layer(s) to client composition on display {}",
                self.display_id
            );
        }
        if changed > 0 {
            self.hwc.accept_display_changes(self.display_id)?;
        }

        Ok(())
    }

    /// Present frame to display and verify tear-free VSYNC presentation
    pub fn present_frame(
        &mut self,
        vsync_timestamp_ns: u64,
        present_timestamp_ns: u64,
    ) -> Result<(), HwcError> {
        // Validate tear-free timing
        self.vsync_validator
            .validate_frame_presentation(vsync_timestamp_ns, present_timestamp_ns)
            .map_err(|e| HwcError::ValidationFailed(format!("{:?}", e)))?;

        // Present display via HWC
        let _fences = self
            .hwc
            .present_display(self.display_id)
            .map_err(|e| HwcError::PresentationFailed(format!("{:?}", e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::composer::HwcVersion;

    #[test]
    fn test_mobile_scene_initialization() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut scene = MobileScene::new(0, 1080, 2400, 120.0, hwc);

        assert_eq!(scene.mode, ShellMode::Launcher);
        assert_eq!(scene.width, 1080);
        assert_eq!(scene.height, 2400);

        // Prepare frame
        assert!(scene.prepare_frame().is_ok());

        // Present frame tear-free
        let vsync = 1_000_000_000u64;
        let present = vsync + 20_000;
        assert!(scene.present_frame(vsync, present).is_ok());
    }

    #[test]
    fn test_lock_state_drives_mode_without_a_gesture_dispatcher() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut scene = MobileScene::new(0, 1080, 2400, 90.0, hwc);

        // `LockScreen::new` starts Locked, so the mode is claimed on the
        // first tick -- not seeded by the constructor.
        assert_eq!(scene.mode, ShellMode::Launcher, "mode is set by update()");
        scene.update(0.016);
        assert_eq!(scene.mode, ShellMode::LockScreen);

        // Unlocking hands it back to the launcher.
        scene.lockscreen.unlock();
        scene.update(0.016);
        assert_eq!(scene.mode, ShellMode::Launcher);

        // And re-locking takes it again, idempotently across ticks.
        scene.lockscreen.lock();
        for _ in 0..3 {
            scene.update(0.016);
        }
        assert_eq!(scene.mode, ShellMode::LockScreen);
    }

    #[test]
    fn test_update_ticks_shade_and_keyboard_springs() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut scene = MobileScene::new(0, 1080, 2400, 120.0, hwc);

        scene.system_ui.open();
        scene.keyboard.activate();

        // One tick must actually move both springs, otherwise a dropped
        // `update` would strand the shade/IME mid-transition.
        let shade_before = scene.system_ui.pull_spring.value;
        let kb_before = scene.keyboard.slide_spring.value;
        scene.update(0.016);
        assert!(
            scene.system_ui.pull_spring.value > shade_before,
            "shade pull spring must advance"
        );
        assert!(
            scene.keyboard.slide_spring.value > kb_before,
            "IME slide spring must advance"
        );
    }
}
