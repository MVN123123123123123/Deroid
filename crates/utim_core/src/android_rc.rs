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
    /// Directives that configure the service but have no dedicated field
    /// (`socket`, `writepid`, `task`, `priority`, `nice`,
    /// `oom_score_adjust`, `exec_start` continuations, ...), kept verbatim
    /// so no configuration is silently lost.
    pub extra: Vec<String>,
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
            extra: Vec::new(),
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

/// Parsed `.rc` plus the imports that were not expanded, so a caller can
/// never mistake a partial discovery for a complete one.
pub struct RcParse {
    pub services: Vec<AndroidService>,
    pub unresolved_imports: Vec<String>,
}

/// Parse Android .rc init files, recording (not dropping) `import`
/// directives, `exec_start`/`socket` blocks and command-less services.
///
/// Expanding an `import` needs the caller's filesystem, so imports are
/// returned in [`RcParse::unresolved_imports`] for the caller to recurse
/// into rather than silently skipped.
pub fn parse_android_rc_with_imports(content: &str) -> RcParse {
    let mut services = Vec::new();
    let mut imports = Vec::new();
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

        if parts[0] == "import" {
            // Recorded, not silently dropped. Expanding it needs the caller's
            // filesystem, so the caller decides whether to recurse.
            // Flush any open service first: an import is a top-level
            // directive, not part of the preceding service block.
            if let Some(svc) = current_service.take() {
                services.push(svc);
            }
            if parts.len() < 2 {
                eprintln!("android_rc: ignoring import with no path");
            } else {
                for p in &parts[1..] {
                    imports.push(p.to_string());
                }
            }
        } else if parts[0] == "service" {
            if let Some(svc) = current_service.take() {
                services.push(svc);
            }
            if parts.len() >= 3 {
                let name = parts[1].to_string();
                let command = parts[2..].iter().map(|s| s.to_string()).collect();
                current_service = Some(AndroidService::new(name, command));
            } else {
                eprintln!(
                    "android_rc: ignoring command-less service line: {trimmed:?}"
                );
            }
        } else if let Some(ref mut svc) = current_service {
            match parts[0] {                "class" => {
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
                "exec_start" => {
                    for p in &parts[1..] {
                        if *p != "--" {
                            svc.command.push(p.to_string());
                        }
                    }
                    svc.extra.push(parts.join(" "));
                }
                "socket" | "writepid" | "file" | "task" | "priority" | "nice"
                | "oom_score_adjust" | "rlimit" | "seclabel" | "write" | "mkdir"
                | "exec" => {
                    svc.extra.push(parts.join(" "));
                }
                other => {
                    eprintln!("android_rc: ignoring directive {other:?} for {}", svc.name);
                }
            }
        }
    }

    if let Some(svc) = current_service {
        services.push(svc);
    }

    RcParse {
        services,
        unresolved_imports: imports,
    }
}

/// Parse Android .rc init files.
pub fn parse_android_rc(content: &str) -> Vec<AndroidService> {
    parse_android_rc_with_imports(content).services
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

    #[test]
    fn test_import_recorded_not_dropped() {
        // S19: an import must never vanish into the previous service block;
        // it is reported so the caller can recurse.
        let rc = "service vendor.hwcomposer-2-1 /vendor/bin/hw/composer@2.1-service\n    user system\nimport /vendor/etc/init/hw/init.treble-qsi.rc\nservice vendor.audio-hal /vendor/bin/hw/audio.service\n    user audioserver\n";
        let parsed = parse_android_rc_with_imports(rc);
        assert_eq!(parsed.services.len(), 2);
        assert_eq!(
            parsed.unresolved_imports,
            vec!["/vendor/etc/init/hw/init.treble-qsi.rc"]
        );
        // The import line left no residue in either service.
        assert_eq!(parsed.services[0].command.len(), 1);
    }

    #[test]
    fn test_exec_start_and_socket_recorded() {
        // S19: exec_start extends the command (minus `--`), socket blocks are
        // kept verbatim instead of discarded.
        let rc = "service rild /vendor/bin/hw/rild\n    user radio\n    exec_start -- /system/bin/logwrapper /vendor/bin/hw/rild\n    socket rild stream 660 radio radio\n";
        let parsed = parse_android_rc_with_imports(rc);
        assert_eq!(parsed.services.len(), 1);
        let svc = &parsed.services[0];
        assert!(svc.command.contains(&"/system/bin/logwrapper".to_string()));
        assert!(!svc.command.iter().any(|a| a == "--"));
        assert!(svc.extra.iter().any(|e| e.starts_with("socket ")));
    }

    #[test]
    fn test_command_less_service_is_dropped_with_diagnostic() {
        // S19: a service line with no command yields no service (nothing to
        // exec), but the drop is explicit rather than silent.
        let rc = "service broken_no_command\nservice good /bin/true\n";
        let parsed = parse_android_rc_with_imports(rc);
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services[0].name, "good");
    }

    #[test]
    fn test_legacy_entry_still_returns_services_only() {
        // The Vec-returning wrapper keeps its contract for existing callers.
        let rc = "service vendor.audio-hal /vendor/bin/hw/audio.service\n    user audioserver\n";
        let services = parse_android_rc(rc);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].user, "audioserver");
    }
}
