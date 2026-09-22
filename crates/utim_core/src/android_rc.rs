//! Android .rc init file parser for discovering vendor HAL services.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidService {
    pub name: String,
    pub command: Vec<String>,
    pub class: Vec<String>,
    pub user: String,
    pub group: String,
    pub supplementary_groups: Vec<String>,
    pub capabilities: Vec<String>,
    pub disabled: bool,
    pub oneshot: bool,
    pub critical: bool,
    pub onrestart: Vec<String>,
}

impl AndroidService {
    pub fn new(name: String, command: Vec<String>) -> Self {
        Self {
            name,
            command,
            class: Vec::new(),
            user: "root".to_string(),
            group: "root".to_string(),
            supplementary_groups: Vec::new(),
            capabilities: Vec::new(),
            disabled: false,
            oneshot: false,
            critical: false,
            onrestart: Vec::new(),
        }
    }

    /// Check if this service matches a specific HAL subsystem (e.g., composer, audio, radio/ril).
    pub fn matches_subsystem(&self, subsystem: &str) -> bool {
        let name_lower = self.name.to_lowercase();
        let cmd_lower = self
            .command
            .first()
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let user_lower = self.user.to_lowercase();
        let class_lower = self.class.join(" ").to_lowercase();

        let target = subsystem.to_lowercase();

        let matches_direct = name_lower.contains(&target)
            || cmd_lower.contains(&target)
            || user_lower.contains(&target)
            || class_lower.contains(&target);

        if matches_direct {
            return true;
        }

        // Handle standard Android HAL aliases
        match target.as_str() {
            "radio" | "telephony" | "ril" => {
                name_lower.contains("ril")
                    || cmd_lower.contains("ril")
                    || name_lower.contains("radio")
                    || user_lower == "radio"
            }
            "composer" | "hwcomposer" | "display" => {
                name_lower.contains("composer")
                    || cmd_lower.contains("composer")
                    || name_lower.contains("hwc")
            }
            "audio" => name_lower.contains("audio") || user_lower == "audioserver",
            "camera" => name_lower.contains("camera") || user_lower == "cameraserver",
            "sensor" | "sensors" => name_lower.contains("sensor"),
            _ => false,
        }
    }
}

/// Parse Android .rc init files.
pub fn parse_android_rc(content: &str) -> Vec<AndroidService> {
    let mut services = Vec::new();
    let mut current_service: Option<AndroidService> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        if parts[0] == "service" {
            if let Some(svc) = current_service.take() {
                services.push(svc);
            }
            if parts.len() >= 3 {
                let name = parts[1].to_string();
                let command = parts[2..].iter().map(|s| s.to_string()).collect();
                current_service = Some(AndroidService::new(name, command));
            }
        } else if let Some(ref mut svc) = current_service {
            match parts[0] {
                "class" => {
                    svc.class = parts[1..].iter().map(|s| s.to_string()).collect();
                }
                "user" => {
                    if parts.len() >= 2 {
                        svc.user = parts[1].to_string();
                    }
                }
                "group" => {
                    if parts.len() >= 2 {
                        svc.group = parts[1].to_string();
                        if parts.len() > 2 {
                            svc.supplementary_groups =
                                parts[2..].iter().map(|s| s.to_string()).collect();
                        }
                    }
                }
                "capabilities" => {
                    svc.capabilities = parts[1..].iter().map(|s| s.to_string()).collect();
                }
                "disabled" => {
                    svc.disabled = true;
                }
                "oneshot" => {
                    svc.oneshot = true;
                }
                "critical" => {
                    svc.critical = true;
                }
                "onrestart" if parts.len() >= 2 => {
                    svc.onrestart.push(parts[1..].join(" "));
                }
                _ => {
                    // Ignore other rc directives like writepid, file, etc.
                }
            }
        }
    }

    if let Some(svc) = current_service {
        services.push(svc);
    }

    services
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_android_rc() {
        let rc_content = r#"
# Sample vendor RC
service vendor.hwcomposer-2-1 /vendor/bin/hw/android.hardware.graphics.composer@2.1-service
    class hal animation
    user system
    group graphics drmrpc
    capabilities SYS_NICE
    onrestart restart surfaceflinger

service vendor.audio-hal /vendor/bin/hw/android.hardware.audio.service
    class hal
    user audioserver
    group audio camera drmrpc
    capabilities SYS_NICE
    disabled
"#;

        let services = parse_android_rc(rc_content);
        assert_eq!(services.len(), 2);

        assert_eq!(services[0].name, "vendor.hwcomposer-2-1");
        assert_eq!(
            services[0].command[0],
            "/vendor/bin/hw/android.hardware.graphics.composer@2.1-service"
        );
        assert_eq!(services[0].user, "system");
        assert_eq!(services[0].group, "graphics");
        assert_eq!(services[0].supplementary_groups, vec!["drmrpc"]);
        assert!(services[0].matches_subsystem("composer"));
        assert!(!services[0].disabled);

        assert_eq!(services[1].name, "vendor.audio-hal");
        assert!(services[1].matches_subsystem("audio"));
        assert!(services[1].disabled);
    }
}
