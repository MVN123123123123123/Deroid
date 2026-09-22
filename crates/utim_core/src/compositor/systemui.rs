//! SystemUI Status Bar, Quick Settings Panel, and Notification Center.
//! Implements transparent top status bar (time, battery %, cellular RAT 5G/LTE, Wi-Fi),
//! swipe-down Quick Settings panel (tiles & brightness/volume sliders),
//! and org.freedesktop.Notifications D-Bus compliant notification center.

use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::compositor::launcher::{SpringConfig, SpringOscillator};

/// Cellular Radio Access Technology
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellularRat {
    None,
    Gsm2g,
    Umts3g,
    Lte4g,
    Nr5g,
}

impl CellularRat {
    pub fn display_label(&self) -> &'static str {
        match self {
            CellularRat::None => "No SIM",
            CellularRat::Gsm2g => "2G",
            CellularRat::Umts3g => "3G",
            CellularRat::Lte4g => "LTE",
            CellularRat::Nr5g => "5G",
        }
    }
}

/// Transparent Top Status Bar State
#[derive(Debug, Clone, PartialEq)]
pub struct StatusBarState {
    pub height: f32, // e.g. 32.0 dp
    pub time_str: String,
    pub battery_percent: u8,
    pub is_charging: bool,
    pub cellular_bars: u8, // 0 to 4
    pub cellular_rat: CellularRat,
    pub wifi_bars: u8, // 0 to 3
    pub wifi_ssid: Option<String>,
    pub notification_count: usize,
}

impl Default for StatusBarState {
    fn default() -> Self {
        Self {
            height: 32.0,
            time_str: "12:00".into(),
            battery_percent: 85,
            is_charging: false,
            cellular_bars: 4,
            cellular_rat: CellularRat::Nr5g,
            wifi_bars: 3,
            wifi_ssid: Some("UniversalTreble_5G".into()),
            notification_count: 0,
        }
    }
}

/// Quick Settings Tile identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuickTileKind {
    Wifi,
    MobileData,
    Bluetooth,
    Torch,
    AutoRotate,
    AirplaneMode,
    BatterySaver,
    Hotspot,
}

impl QuickTileKind {
    pub fn name(&self) -> &'static str {
        match self {
            QuickTileKind::Wifi => "Wi-Fi",
            QuickTileKind::MobileData => "Mobile Data",
            QuickTileKind::Bluetooth => "Bluetooth",
            QuickTileKind::Torch => "Flashlight",
            QuickTileKind::AutoRotate => "Auto-rotate",
            QuickTileKind::AirplaneMode => "Airplane mode",
            QuickTileKind::BatterySaver => "Battery Saver",
            QuickTileKind::Hotspot => "Hotspot",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuickTile {
    pub kind: QuickTileKind,
    pub is_active: bool,
    pub subtitle: String,
}

/// org.freedesktop.Notifications Card
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationCard {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>, // (action_key, label) e.g. ("reply", "Reply")
    pub created_at: Instant,
    pub x_offset: f32, // Swipe-to-dismiss drag offset
}

/// Quick Settings Panel & Notification Shade
pub struct SystemUiShade {
    pub display_width: f32,
    pub display_height: f32,
    pub status_bar: StatusBarState,
    pub pull_spring: SpringOscillator, // 0.0 = Hidden, 1.0 = Fully pulled down
    pub tiles: Vec<QuickTile>,
    pub brightness_percent: u8, // 0 to 100
    pub volume_percent: u8,     // 0 to 100
    pub notifications: Vec<NotificationCard>,
    pub next_notification_id: u32,
    pub torch_sysfs_path: String,
    pub backlight_sysfs_path: String,
}

impl SystemUiShade {
    pub fn new(display_width: f32, display_height: f32) -> Self {
        let tiles = vec![
            QuickTile {
                kind: QuickTileKind::Wifi,
                is_active: true,
                subtitle: "Connected".into(),
            },
            QuickTile {
                kind: QuickTileKind::MobileData,
                is_active: true,
                subtitle: "5G Active".into(),
            },
            QuickTile {
                kind: QuickTileKind::Bluetooth,
                is_active: false,
                subtitle: "Off".into(),
            },
            QuickTile {
                kind: QuickTileKind::Torch,
                is_active: false,
                subtitle: "Off".into(),
            },
            QuickTile {
                kind: QuickTileKind::AutoRotate,
                is_active: true,
                subtitle: "On".into(),
            },
            QuickTile {
                kind: QuickTileKind::AirplaneMode,
                is_active: false,
                subtitle: "Off".into(),
            },
            QuickTile {
                kind: QuickTileKind::BatterySaver,
                is_active: false,
                subtitle: "Off".into(),
            },
            QuickTile {
                kind: QuickTileKind::Hotspot,
                is_active: false,
                subtitle: "Off".into(),
            },
        ];

        Self {
            display_width,
            display_height,
            status_bar: StatusBarState::default(),
            pull_spring: SpringOscillator::new(
                0.0,
                SpringConfig {
                    stiffness: 240.0,
                    damping: 24.0,
                    mass: 1.0,
                },
            ),
            tiles,
            brightness_percent: 75,
            volume_percent: 60,
            notifications: Vec::new(),
            next_notification_id: 1,
            torch_sysfs_path: "/sys/class/leds/torch-light/brightness".into(),
            backlight_sysfs_path: "/sys/class/backlight/panel0-backlight/brightness".into(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.pull_spring.target == 1.0 || self.pull_spring.current > 0.5
    }

    pub fn open(&mut self) {
        self.pull_spring.target = 1.0;
    }

    pub fn close(&mut self) {
        self.pull_spring.target = 0.0;
    }

    pub fn set_pull_progress(&mut self, progress: f32) {
        let clamped = progress.clamp(0.0, 1.0);
        self.pull_spring.current = clamped;
        self.pull_spring.target = clamped;
    }

    pub fn toggle_tile(&mut self, kind: QuickTileKind) -> bool {
        let mut activated = false;
        let mut found = false;
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.kind == kind) {
            tile.is_active = !tile.is_active;
            tile.subtitle = if tile.is_active {
                "On".into()
            } else {
                "Off".into()
            };
            activated = tile.is_active;
            found = true;
        }
        if found {
            if kind == QuickTileKind::Torch {
                self.sync_torch_sysfs(activated);
            }
            return activated;
        }
        false
    }

    pub fn set_brightness(&mut self, percent: u8) {
        self.brightness_percent = percent.min(100);
        self.sync_brightness_sysfs(self.brightness_percent);
    }

    pub fn set_volume(&mut self, percent: u8) {
        self.volume_percent = percent.min(100);
    }

    fn sync_torch_sysfs(&self, active: bool) {
        let path = Path::new(&self.torch_sysfs_path);
        if path.exists() {
            let val = if active { "255\n" } else { "0\n" };
            let _ = fs::write(path, val);
        }
    }

    fn sync_brightness_sysfs(&self, percent: u8) {
        let path = Path::new(&self.backlight_sysfs_path);
        if path.exists() {
            let val = format!("{}\n", (percent as u32 * 255) / 100);
            let _ = fs::write(path, val);
        }
    }

    // --- org.freedesktop.Notifications implementation ---

    pub fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<(String, String)>,
    ) -> u32 {
        let id = if replaces_id != 0 && self.notifications.iter().any(|n| n.id == replaces_id) {
            replaces_id
        } else {
            let new_id = self.next_notification_id;
            self.next_notification_id += 1;
            new_id
        };

        let card = NotificationCard {
            id,
            app_name,
            app_icon,
            summary,
            body,
            actions,
            created_at: Instant::now(),
            x_offset: 0.0,
        };

        if let Some(pos) = self.notifications.iter().position(|n| n.id == id) {
            self.notifications[pos] = card;
        } else {
            self.notifications.insert(0, card);
        }

        self.status_bar.notification_count = self.notifications.len();
        id
    }

    pub fn close_notification(&mut self, id: u32) -> bool {
        if let Some(pos) = self.notifications.iter().position(|n| n.id == id) {
            self.notifications.remove(pos);
            self.status_bar.notification_count = self.notifications.len();
            true
        } else {
            false
        }
    }

    pub fn on_notification_swipe(&mut self, id: u32, delta_x: f32) {
        if let Some(card) = self.notifications.iter_mut().find(|n| n.id == id) {
            card.x_offset += delta_x;
        }
    }

    pub fn on_notification_release(&mut self, id: u32) -> bool {
        // Dismiss if swiped more than 100px horizontally
        if let Some(pos) = self.notifications.iter().position(|n| n.id == id) {
            if self.notifications[pos].x_offset.abs() > 100.0 {
                self.notifications.remove(pos);
                self.status_bar.notification_count = self.notifications.len();
                return true; // dismissed
            } else {
                self.notifications[pos].x_offset = 0.0;
            }
        }
        false
    }

    pub fn update(&mut self, dt: f32) {
        self.pull_spring.step(dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_bar_defaults() {
        let bar = StatusBarState::default();
        assert_eq!(bar.cellular_rat.display_label(), "5G");
        assert_eq!(bar.battery_percent, 85);
        assert_eq!(bar.cellular_bars, 4);
    }

    #[test]
    fn test_quick_settings_tile_toggles() {
        let mut shade = SystemUiShade::new(1080.0, 2400.0);
        assert!(!shade.is_open());

        shade.open();
        for _ in 0..60 {
            shade.update(0.016);
        }
        assert!(shade.is_open());

        let torch_state = shade.toggle_tile(QuickTileKind::Torch);
        assert!(torch_state);
        let torch_tile = shade
            .tiles
            .iter()
            .find(|t| t.kind == QuickTileKind::Torch)
            .unwrap();
        assert!(torch_tile.is_active);

        shade.set_brightness(90);
        assert_eq!(shade.brightness_percent, 90);
    }

    #[test]
    fn test_notification_center_lifecycle_and_swipe_dismiss() {
        let mut shade = SystemUiShade::new(1080.0, 2400.0);

        let id = shade.notify(
            "Messaging".into(),
            0,
            "chatty".into(),
            "New Message".into(),
            "Hello from Linux Treble GSI!".into(),
            vec![("reply".into(), "Reply".into())],
        );

        assert_eq!(shade.notifications.len(), 1);
        assert_eq!(shade.status_bar.notification_count, 1);
        assert_eq!(id, 1);

        // Swipe card horizontally by 120px (> 100px threshold)
        shade.on_notification_swipe(id, 120.0);
        let dismissed = shade.on_notification_release(id);
        assert!(dismissed);
        assert_eq!(shade.notifications.len(), 0);
        assert_eq!(shade.status_bar.notification_count, 0);
    }
}
