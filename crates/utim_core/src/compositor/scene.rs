//! Mobile Scene Graph and Multi-Plane Hardware Composer Presentation Engine.
//! Maps visual layers (Wallpaper, Workspace Grid, Hotseat Dock, Active Window,
//! SystemUI Status Bar, Shade, Virtual Keyboard, Lock Screen) directly to
//! HWC 2.x and AIDL composer3 hardware overlay planes with DMA-BUF zero-copy presentation.

use crate::compositor::gestures::GestureAction;
use crate::compositor::ime::VirtualKeyboard;
use crate::compositor::launcher::{AppDrawer, HotseatDock, WorkspaceGrid};
use crate::compositor::lockscreen::LockScreen;
use crate::compositor::recents::RecentsCarousel;
use crate::compositor::systemui::SystemUiShade;
use crate::graphics::composer::{CompositionType, DisplayConfig, HwcComposer, Rect};
use crate::graphics::vsync::{VsyncConfig, VsyncPresentationValidator};

/// Scene Graph Visual Plane Order
pub mod plane_z_order {
    pub const WALLPAPER_GRID: u32 = 0;
    pub const HOTSEAT_DOCK: u32 = 10;
    pub const APPLICATION_SURFACE: u32 = 20;
    pub const RECENTS_CAROUSEL: u32 = 30;
    pub const STATUS_BAR: u32 = 40;
    pub const SYSTEM_UI_SHADE: u32 = 50;
    pub const VIRTUAL_KEYBOARD: u32 = 60;
    pub const LOCK_SCREEN: u32 = 70;
}

/// Active Display Mode of the Mobile Shell
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMode {
    Launcher,
    Application,
    Recents,
    SplitScreen,
    LockScreen,
}

/// The unified Mobile Scene Graph orchestrating all shell components
pub struct MobileScene {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: f64,
    pub mode: ShellMode,
    pub workspace: WorkspaceGrid,
    pub dock: HotseatDock,
    pub drawer: AppDrawer,
    pub recents: RecentsCarousel,
    pub system_ui: SystemUiShade,
    pub keyboard: VirtualKeyboard,
    pub lockscreen: LockScreen,
    pub active_app_surface_id: Option<u32>,
    pub active_app_buffer_fd: Option<i32>,
    pub hwc: HwcComposer,
    pub vsync_validator: VsyncPresentationValidator,
    // HWC layer handles for multi-plane composition
    layer_grid: Option<u64>,
    layer_dock: Option<u64>,
    layer_app: Option<u64>,
    layer_status: Option<u64>,
    layer_shade: Option<u64>,
    layer_ime: Option<u64>,
    layer_lock: Option<u64>,
}

impl MobileScene {
    pub fn new(
        display_id: u32,
        width: u32,
        height: u32,
        refresh_rate: f64,
        hwc_composer: HwcComposer,
    ) -> Self {
        let vsync_cfg = VsyncConfig::new(refresh_rate).unwrap_or_else(|_| VsyncConfig::new(60.0).unwrap());
        let vsync_validator = VsyncPresentationValidator::new(vsync_cfg);

        let mut scene = Self {
            display_id,
            width,
            height,
            refresh_rate,
            mode: ShellMode::Launcher,
            workspace: WorkspaceGrid::new(4, 5, 3, width as f32),
            dock: HotseatDock::default_mobile(height as f32),
            drawer: AppDrawer::new(),
            recents: RecentsCarousel::new(width as f32, height as f32),
            system_ui: SystemUiShade::new(width as f32, height as f32),
            keyboard: VirtualKeyboard::new(width as f32, height as f32),
            lockscreen: LockScreen::new(None),
            active_app_surface_id: None,
            active_app_buffer_fd: None,
            hwc: hwc_composer,
            vsync_validator,
            layer_grid: None,
            layer_dock: None,
            layer_app: None,
            layer_status: None,
            layer_shade: None,
            layer_ime: None,
            layer_lock: None,
        };

        // Register display with HWC
        let display_cfg = DisplayConfig::standard_mobile(display_id, width, height, refresh_rate);
        scene.hwc.register_display(display_cfg);

        // Allocate HWC hardware composition layers
        scene.init_hwc_layers();
        scene
    }

    fn init_hwc_layers(&mut self) {
        self.layer_grid = self.hwc.create_layer(self.display_id).ok();
        self.layer_dock = self.hwc.create_layer(self.display_id).ok();
        self.layer_app = self.hwc.create_layer(self.display_id).ok();
        self.layer_status = self.hwc.create_layer(self.display_id).ok();
        self.layer_shade = self.hwc.create_layer(self.display_id).ok();
        self.layer_ime = self.hwc.create_layer(self.display_id).ok();
        self.layer_lock = self.hwc.create_layer(self.display_id).ok();

        // Assign Z-orders
        if let Some(l) = self.layer_grid {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::WALLPAPER_GRID);
        }
        if let Some(l) = self.layer_dock {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::HOTSEAT_DOCK);
        }
        if let Some(l) = self.layer_app {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::APPLICATION_SURFACE);
        }
        if let Some(l) = self.layer_status {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::STATUS_BAR);
        }
        if let Some(l) = self.layer_shade {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::SYSTEM_UI_SHADE);
        }
        if let Some(l) = self.layer_ime {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::VIRTUAL_KEYBOARD);
        }
        if let Some(l) = self.layer_lock {
            let _ = self.hwc.set_layer_z_order(self.display_id, l, plane_z_order::LOCK_SCREEN);
        }
    }

    /// Dispatch gesture event to scene state machine
    pub fn apply_gesture_action(&mut self, action: GestureAction) {
        match action {
            GestureAction::Home { progress, .. } => {
                if progress >= 1.0 {
                    self.mode = ShellMode::Launcher;
                }
            }
            GestureAction::Recents { .. } => {
                self.mode = ShellMode::Recents;
            }
            GestureAction::Back { injected, .. } => {
                if injected {
                    if self.system_ui.is_open() {
                        self.system_ui.close();
                    } else if self.keyboard.is_active {
                        self.keyboard.deactivate();
                    } else if self.mode == ShellMode::Recents {
                        self.mode = ShellMode::Launcher;
                    }
                }
            }
            GestureAction::NotificationShade { progress } => {
                self.system_ui.set_pull_progress(progress);
            }
            GestureAction::BottomBarScrub { app_shift, .. } => {
                if !self.recents.cards.is_empty() {
                    let new_idx = (self.recents.selected_index as i32 + app_shift)
                        .clamp(0, self.recents.cards.len() as i32 - 1) as usize;
                    self.recents.snap_to_index(new_idx);
                }
            }
            GestureAction::None => {}
        }
    }

    /// Update physics and animations for frame presentation
    pub fn update(&mut self, dt: f32) {
        self.workspace.update(dt);
        self.drawer.update(dt);
        self.recents.update(dt);
        self.system_ui.update(dt);
        self.keyboard.update(dt);

        if self.lockscreen.is_locked() {
            self.mode = ShellMode::LockScreen;
        } else if self.mode == ShellMode::LockScreen {
            self.mode = ShellMode::Launcher;
        }
    }

    /// Update HWC multi-plane layer attributes and validate composition
    pub fn prepare_frame(&mut self) -> Result<(), String> {
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
            let _ = self.hwc.set_layer_display_frame(self.display_id, l, full_rect);
            let _ = self.hwc.set_layer_composition_type(self.display_id, l, CompositionType::Device);
            let _ = self.hwc.set_layer_buffer(self.display_id, l, 100, None);
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
            let _ = self.hwc.set_layer_display_frame(self.display_id, l, app_rect);

            let comp_type = match self.mode {
                ShellMode::Application | ShellMode::SplitScreen => CompositionType::Device,
                _ => CompositionType::Client,
            };
            let _ = self.hwc.set_layer_composition_type(self.display_id, l, comp_type);
            if let Some(fd) = self.active_app_buffer_fd {
                let _ = self.hwc.set_layer_buffer(self.display_id, l, fd as u64, None);
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
            let _ = self.hwc.set_layer_display_frame(self.display_id, l, status_rect);
            let _ = self.hwc.set_layer_composition_type(self.display_id, l, CompositionType::Device);
            let _ = self.hwc.set_layer_buffer(self.display_id, l, 200, None);
        }

        // 4. Set ClientTarget fallback buffer
        let _ = self.hwc.set_client_target(self.display_id, 9999, None);

        // 5. Validate HWC composition
        let (changed, _has_client) = self.hwc
            .validate_display(self.display_id)
            .map_err(|e| format!("HWC validation failed: {:?}", e))?;

        if changed > 0 {
            let _ = self.hwc.accept_display_changes(self.display_id);
        }

        Ok(())
    }

    /// Present frame to display and verify tear-free VSYNC presentation
    pub fn present_frame(&mut self, vsync_timestamp_ns: u64, present_timestamp_ns: u64) -> Result<(), String> {
        // Validate tear-free timing
        self.vsync_validator
            .validate_frame_presentation(vsync_timestamp_ns, present_timestamp_ns)
            .map_err(|e| format!("VSYNC tear validation error: {:?}", e))?;

        // Present display via HWC
        let _fences = self.hwc
            .present_display(self.display_id)
            .map_err(|e| format!("HWC presentation failed: {:?}", e))?;

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
    fn test_scene_gesture_mode_transitions() {
        let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut scene = MobileScene::new(0, 1080, 2400, 90.0, hwc);

        scene.mode = ShellMode::Application;

        // Home gesture returns to Launcher
        scene.apply_gesture_action(GestureAction::Home {
            progress: 1.0,
            scale: 0.0,
            window_alpha: 0.0,
        });
        assert_eq!(scene.mode, ShellMode::Launcher);

        // Recents gesture enters Recents
        scene.apply_gesture_action(GestureAction::Recents {
            progress: 1.0,
            trigger_haptic: false,
        });
        assert_eq!(scene.mode, ShellMode::Recents);
    }
}
