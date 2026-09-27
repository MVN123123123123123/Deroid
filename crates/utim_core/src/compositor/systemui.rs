//! SystemUI Status Bar, Quick Settings Panel, and Notification Center.
//! Implements transparent top status bar (time, battery %, cellular RAT 5G/LTE, Wi-Fi),
//! swipe-down Quick Settings panel (tiles & brightness/volume sliders),
//! and org.freedesktop.Notifications D-Bus compliant notification center.

use std::fs;
use std::io::{self, Write};
use std::time::Instant;

use crate::compositor::spring::{SpringConfig, SpringOscillator};

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
    /// Hard cap: any client on org.freedesktop.Notifications could otherwise
    /// grow the list (and its O(n) scans) without limit.
    pub const MAX_NOTIFICATIONS: usize = 64;

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

    pub fn toggle(&mut self) {
        if self.is_open() {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn set_pull_progress(&mut self, progress: f32) {
        let clamped = progress.clamp(0.0, 1.0);
        self.pull_spring.current = clamped;
        self.pull_spring.target = clamped;
        self.pull_spring.velocity = 0.0; // finger took over: drop stale velocity
    }

    pub fn toggle_tile(&mut self, kind: QuickTileKind) -> io::Result<bool> {
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
                // A failed LED write must not masquerade as a working toggle.
                self.sync_torch_sysfs(activated)?;
            }
            return Ok(activated);
        }
        Ok(false)
    }

    pub fn set_brightness(&mut self, percent: u8) -> io::Result<()> {
        self.brightness_percent = percent.min(100);
        self.sync_brightness_sysfs(self.brightness_percent)
    }

    pub fn set_volume(&mut self, percent: u8) {
        self.volume_percent = percent.min(100);
    }

    fn sync_torch_sysfs(&self, active: bool) -> io::Result<()> {
        let val: &[u8] = if active { b"255\n" } else { b"0\n" };
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.torch_sysfs_path)?;
        f.write_all(val)
    }

    fn sync_brightness_sysfs(&self, percent: u8) -> io::Result<()> {
        // Stack-formatted "0..255\n": no format! heap allocation on the
        // per-drag-frame brightness path.
        let v = (percent.min(100) as u32 * 255) / 100;
        let mut buf = [0u8; 5];
        let mut digits = [0u8; 3];
        let mut n = 0usize;
        if v == 0 {
            digits[0] = b'0';
            n = 1;
        } else {
            let mut x = v;
            while x > 0 {
                digits[n] = b'0' + (x % 10) as u8;
                n += 1;
                x /= 10;
            }
        }
        let mut len = 0usize;
        for i in (0..n).rev() {
            buf[len] = digits[i];
            len += 1;
        }
        buf[len] = b'\n';
        len += 1;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.backlight_sysfs_path)?;
        f.write_all(&buf[..len])
    }

    // --- org.freedesktop.Notifications implementation ---

    /// Allocate the next notification id. Never returns 0 (the
    /// `replaces_id == 0` "no replace" sentinel) and never panics at
    /// `u32::MAX`: it wraps to 1 instead of overflowing.
    fn alloc_notification_id(&mut self) -> u32 {
        let id = if self.next_notification_id == 0 {
            1
        } else {
            self.next_notification_id
        };
        self.next_notification_id = id.checked_add(1).filter(|v| *v != 0).unwrap_or(1);
        id
    }

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
            self.alloc_notification_id()
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
            if self.notifications.len() >= Self::MAX_NOTIFICATIONS {
                self.notifications.pop(); // evict the oldest
            }
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
        // Point sysfs at temp files: toggles now report real I/O failures.
        let dir = std::env::temp_dir().join(format!("utim-sysui-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        shade.torch_sysfs_path = dir.join("torch").to_string_lossy().into_owned();
        shade.backlight_sysfs_path = dir.join("backlight").to_string_lossy().into_owned();
        std::fs::write(&shade.torch_sysfs_path, b"0\n").unwrap();
        std::fs::write(&shade.backlight_sysfs_path, b"0\n").unwrap();
        assert!(!shade.is_open());

        shade.open();
        for _ in 0..60 {
            shade.update(0.016);
        }
        assert!(shade.is_open());

        let torch_state = shade.toggle_tile(QuickTileKind::Torch).unwrap();
        assert!(torch_state);
        let torch_tile = shade
            .tiles
            .iter()
            .find(|t| t.kind == QuickTileKind::Torch)
            .unwrap();
        assert!(torch_tile.is_active);
        assert_eq!(std::fs::read(&shade.torch_sysfs_path).unwrap(), b"255\n");

        shade.set_brightness(90).unwrap();
        assert_eq!(shade.brightness_percent, 90);
        assert_eq!(
            std::fs::read(&shade.backlight_sysfs_path).unwrap(),
            b"229\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sysfs_failures_are_reported_not_masked() {
        let mut shade = SystemUiShade::new(1080.0, 2400.0);
        shade.torch_sysfs_path = "/tmp/definitely-absent-utim-torch".into();
        shade.backlight_sysfs_path = "/tmp/definitely-absent-utim-backlight".into();
        assert!(shade.toggle_tile(QuickTileKind::Torch).is_err());
        assert!(shade.set_brightness(100).is_err());
        // Non-torch tiles touch no sysfs: still infallible-OK.
        assert!(shade.toggle_tile(QuickTileKind::Bluetooth).is_ok());
    }

    #[test]
    fn test_notification_cap_and_id_wrap() {
        let mut shade = SystemUiShade::new(1080.0, 2400.0);
        for i in 0..70 {
            shade.notify(
                format!("App{}", i),
                0,
                "icon".into(),
                "S".into(),
                "B".into(),
                vec![],
            );
        }
        assert_eq!(shade.notifications.len(), SystemUiShade::MAX_NOTIFICATIONS);
        assert_eq!(
            shade.status_bar.notification_count,
            SystemUiShade::MAX_NOTIFICATIONS
        );

        // Id wrap: u32::MAX advances to 1, never 0 or panic.
        shade.next_notification_id = u32::MAX;
        let id = shade.notify("W".into(), 0, "i".into(), "S".into(), "B".into(), vec![]);
        assert_eq!(id, u32::MAX);
        assert_eq!(shade.next_notification_id, 1);
        let id2 = shade.notify("W".into(), 0, "i".into(), "S".into(), "B".into(), vec![]);
        assert_eq!(id2, 1);
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
