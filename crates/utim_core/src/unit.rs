//! Systemd unit file parser and data structures with zero-copy parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitKind {
    Service,
    Socket,
    Target,
    Timer,
    Mount,
    Unknown,
}

impl UnitKind {
    pub fn from_name(name: &str) -> Self {
        if name.ends_with(".service") {
            UnitKind::Service
        } else if name.ends_with(".socket") {
            UnitKind::Socket
        } else if name.ends_with(".target") {
            UnitKind::Target
        } else if name.ends_with(".timer") {
            UnitKind::Timer
        } else if name.ends_with(".mount") {
            UnitKind::Mount
        } else {
            UnitKind::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[default]
    Simple,
    Exec,
    Forking,
    Oneshot,
    Notify,
    Dbus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnAbort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommand {
    pub ignore_failure: bool, // Prefix '-'
    pub privileged: bool,     // Prefix '+'
    pub no_privileges: bool,  // Prefix '!'
    pub binary: String,
    pub args: Vec<String>,
}

impl ExecCommand {
    pub fn parse(raw: &str) -> Option<Self> {
        let mut trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }

        let mut ignore_failure = false;
        let mut privileged = false;
        let mut no_privileges = false;

        // Parse command prefixes
        loop {
            if let Some(rest) = trimmed.strip_prefix('-') {
                ignore_failure = true;
                trimmed = rest.trim_start();
            } else if let Some(rest) = trimmed.strip_prefix('+') {
                privileged = true;
                trimmed = rest.trim_start();
            } else if let Some(rest) = trimmed.strip_prefix('!') {
                no_privileges = true;
                trimmed = rest.trim_start();
            } else {
                break;
            }
        }

        let parts = parse_words(trimmed);
        if parts.is_empty() {
            return None;
        }

        let binary = parts[0].clone();
        let args = parts[1..].to_vec();

        Some(Self {
            ignore_failure,
            privileged,
            no_privileges,
            binary,
            args,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitSection {
    pub description: String,
    pub documentation: Vec<String>,
    pub requires: Vec<String>,
    pub wants: Vec<String>,
    pub binds_to: Vec<String>,
    pub conflicts: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub condition_path_exists: Vec<String>,
    pub condition_file_not_empty: Vec<String>,
    pub condition_directory_not_empty: Vec<String>,
    pub default_dependencies: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSection {
    pub service_type: ServiceType,
    pub exec_start_pre: Vec<ExecCommand>,
    pub exec_start: Vec<ExecCommand>,
    pub exec_start_post: Vec<ExecCommand>,
    pub exec_stop: Vec<ExecCommand>,
    pub exec_reload: Vec<ExecCommand>,
    pub restart: RestartPolicy,
    pub restart_sec: Duration,
    pub environment: HashMap<String, String>,
    pub environment_files: Vec<(bool, String)>, // (optional, path)
    pub working_directory: Option<String>,
    pub user: Option<String>,
    pub group: Option<String>,
    pub supplementary_groups: Vec<String>,
    pub standard_output: String,
    pub standard_error: String,
    pub limit_nofile: Option<u64>,
    pub limit_memlock: Option<u64>,
    pub limit_nproc: Option<u64>,
    pub oom_score_adjust: Option<i32>,
    pub watchdog_sec: Duration,
    pub pid_file: Option<String>,
    pub bus_name: Option<String>,
}

impl Default for ServiceSection {
    fn default() -> Self {
        Self {
            service_type: ServiceType::Simple,
            exec_start_pre: Vec::new(),
            exec_start: Vec::new(),
            exec_start_post: Vec::new(),
            exec_stop: Vec::new(),
            exec_reload: Vec::new(),
            restart: RestartPolicy::No,
            restart_sec: Duration::from_millis(100),
            environment: HashMap::new(),
            environment_files: Vec::new(),
            working_directory: None,
            user: None,
            group: None,
            supplementary_groups: Vec::new(),
            standard_output: "journal".to_string(),
            standard_error: "journal".to_string(),
            limit_nofile: None,
            limit_memlock: None,
            limit_nproc: None,
            oom_score_adjust: None,
            watchdog_sec: Duration::ZERO,
            pid_file: None,
            bus_name: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SocketSection {
    pub listen_stream: Vec<String>,
    pub listen_datagram: Vec<String>,
    pub socket_mode: Option<u32>,
    pub socket_user: Option<String>,
    pub socket_group: Option<String>,
    pub service: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallSection {
    pub wanted_by: Vec<String>,
    pub required_by: Vec<String>,
    pub also: Vec<String>,
    pub alias: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnit {
    pub name: String,
    pub kind: UnitKind,
    pub path: PathBuf,
    pub unit: UnitSection,
    pub service: Option<ServiceSection>,
    pub socket: Option<SocketSection>,
    pub install: InstallSection,
}

impl SystemdUnit {
    pub fn new(name: String, path: PathBuf) -> Self {
        let kind = UnitKind::from_name(&name);
        Self {
            name,
            kind,
            path,
            unit: UnitSection {
                default_dependencies: true,
                ..Default::default()
            },
            service: if kind == UnitKind::Service {
                Some(ServiceSection::default())
            } else {
                None
            },
            socket: if kind == UnitKind::Socket {
                Some(SocketSection::default())
            } else {
                None
            },
            install: InstallSection::default(),
        }
    }

    /// Check if unit path conditions are met.
    pub fn conditions_met(&self) -> bool {
        for path_str in &self.unit.condition_path_exists {
            let (negated, target) = if let Some(stripped) = path_str.strip_prefix('!') {
                (true, stripped.trim())
            } else {
                (false, path_str.trim())
            };
            let exists = Path::new(target).exists();
            if negated == exists {
                return false;
            }
        }
        for path_str in &self.unit.condition_file_not_empty {
            let (negated, target) = if let Some(stripped) = path_str.strip_prefix('!') {
                (true, stripped.trim())
            } else {
                (false, path_str.trim())
            };
            let not_empty = match std::fs::metadata(target) {
                Ok(meta) => meta.len() > 0,
                Err(_) => false,
            };
            if negated == not_empty {
                return false;
            }
        }
        for path_str in &self.unit.condition_directory_not_empty {
            let (negated, target) = if let Some(stripped) = path_str.strip_prefix('!') {
                (true, stripped.trim())
            } else {
                (false, path_str.trim())
            };
            let not_empty = match std::fs::read_dir(target) {
                Ok(mut dir) => dir.next().is_some(),
                Err(_) => false,
            };
            if negated == not_empty {
                return false;
            }
        }
        true
    }
}

/// Helper function to parse quoted words in command lines.
pub fn parse_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut quote_char = ' ';
    let mut escaped = false;

    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if in_quote {
            if ch == quote_char {
                in_quote = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                words.push(cur);
                cur = String::new();
            }
        } else {
            cur.push(ch);
        }
    }

    if !cur.is_empty() {
        words.push(cur);
    }

    words
}

/// Parse a time duration from systemd format.
/// Supports us/µs, ms, s/sec, min, h, d/day, w/week, bare seconds and
/// fractional values ("1.5s"). "infinity"/"inf" map to Duration::MAX
/// (never-expires); "0"/"no"/"" map to ZERO.
/// Unparseable input returns None (see try_parse_duration); the legacy
/// parse_duration logs a warning and falls back to 100ms so a typo like
/// RestartSec=1ss cannot silently become the C4 fork-bomb ZERO.
pub fn try_parse_duration(s: &str) -> Option<Duration> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("no") {
        return Some(Duration::ZERO);
    }
    if trimmed.eq_ignore_ascii_case("infinity") || trimmed.eq_ignore_ascii_case("inf") {
        return Some(Duration::MAX);
    }

    // Longest suffixes first so "ms"/"min" win over a bare trailing 's'/'m'.
    if let Some(val) = trimmed.strip_suffix("ms") {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v / 1000.0));
            }
        }
    } else if let Some(val) = trimmed
        .strip_suffix("us")
        .or_else(|| trimmed.strip_suffix("µs"))
        .or_else(|| trimmed.strip_suffix("μs"))
    {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v / 1_000_000.0));
            }
        }
    } else if let Some(val) = trimmed.strip_suffix("min") {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v * 60.0));
            }
        }
        // "min" must precede the week/day arms only in suffix length; the
        // bare 's' arm below would claim trailing-"s" plurals ("weeks"),
        // so weeks/days are matched before 's'.
    } else if let Some(val) = trimmed
        .strip_suffix("weeks")
        .or_else(|| trimmed.strip_suffix("week"))
        .or_else(|| trimmed.strip_suffix('w'))
    {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v * 7.0 * 86400.0));
            }
        }
    } else if let Some(val) = trimmed
        .strip_suffix("days")
        .or_else(|| trimmed.strip_suffix("day"))
        .or_else(|| trimmed.strip_suffix('d'))
    {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v * 86400.0));
            }
        }
    } else if let Some(val) = trimmed
        .strip_suffix("seconds")
        .or_else(|| trimmed.strip_suffix("second"))
        .or_else(|| trimmed.strip_suffix("secs"))
        .or_else(|| trimmed.strip_suffix("sec"))
        .or_else(|| trimmed.strip_suffix('s'))
    {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v));
            }
        }
    } else if let Some(val) = trimmed.strip_suffix('h') {
        if let Ok(v) = val.trim().parse::<f64>() {
            if v >= 0.0 {
                return Some(Duration::from_secs_f64(v * 3600.0));
            }
        }
    } else if let Ok(v) = trimmed.parse::<f64>() {
        if v >= 0.0 {
            return Some(Duration::from_secs_f64(v));
        }
    }

    None
}

pub fn parse_duration(s: &str) -> Duration {
    match try_parse_duration(s) {
        Some(d) => d,
        None => {
            eprintln!(
                "[UTIM] Warning: invalid duration {:?}; using 100ms default",
                s
            );
            Duration::from_millis(100)
        }
    }
}

/// Parse a raw systemd unit content into a `SystemdUnit`.
pub fn parse_unit(name: &str, path: &Path, content: &str) -> SystemdUnit {
    let mut unit = SystemdUnit::new(name.to_string(), path.to_path_buf());
    let mut current_section = String::new();

    // Handle line continuations (lines ending with '\')
    let mut raw_lines = Vec::new();
    let mut acc = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if let Some(stripped) = trimmed.strip_suffix('\\') {
            acc.push_str(stripped);
            acc.push(' ');
        } else {
            acc.push_str(trimmed);
            raw_lines.push(acc.clone());
            acc.clear();
        }
    }
    // P13: flush a trailing backslash-continued line so the final directive
    // is not silently dropped.
    if !acc.trim().is_empty() {
        raw_lines.push(acc.clone());
    }

    for line in raw_lines {
        let line_str = line.trim();
        if line_str.starts_with('[') && line_str.ends_with(']') {
            current_section = line_str[1..line_str.len() - 1].to_string();
            if current_section == "Service" && unit.service.is_none() {
                unit.service = Some(ServiceSection::default());
            }
            if current_section == "Socket" && unit.socket.is_none() {
                unit.socket = Some(SocketSection::default());
            }
            continue;
        }

        let Some((key, val)) = line_str.split_once('=') else {
            continue;
        };

        let key = key.trim();
        let val = val.trim();

        match current_section.as_str() {
            "Unit" => match key {
                "Description" => unit.unit.description = val.to_string(),
                "Documentation" => unit.unit.documentation.push(val.to_string()),
                "Requires" => {
                    for w in parse_words(val) {
                        unit.unit.requires.push(w);
                    }
                }
                "Wants" => {
                    for w in parse_words(val) {
                        unit.unit.wants.push(w);
                    }
                }
                "BindsTo" => {
                    for w in parse_words(val) {
                        unit.unit.binds_to.push(w);
                    }
                }
                "Conflicts" => {
                    for w in parse_words(val) {
                        unit.unit.conflicts.push(w);
                    }
                }
                "After" => {
                    for w in parse_words(val) {
                        unit.unit.after.push(w);
                    }
                }
                "Before" => {
                    for w in parse_words(val) {
                        unit.unit.before.push(w);
                    }
                }
                "ConditionPathExists" => unit.unit.condition_path_exists.push(val.to_string()),
                "ConditionFileNotEmpty" => unit.unit.condition_file_not_empty.push(val.to_string()),
                "ConditionDirectoryNotEmpty" => unit
                    .unit
                    .condition_directory_not_empty
                    .push(val.to_string()),
                "DefaultDependencies" => {
                    unit.unit.default_dependencies = val.eq_ignore_ascii_case("yes")
                        || val == "1"
                        || val.eq_ignore_ascii_case("true");
                }
                _ => {}
            },
            "Service" => {
                if let Some(ref mut svc) = unit.service {
                    match key {
                        "Type" => {
                            svc.service_type = match val.to_ascii_lowercase().as_str() {
                                "simple" => ServiceType::Simple,
                                "exec" => ServiceType::Exec,
                                "forking" => ServiceType::Forking,
                                "oneshot" => ServiceType::Oneshot,
                                "notify" => ServiceType::Notify,
                                "dbus" => ServiceType::Dbus,
                                _ => ServiceType::Simple,
                            };
                        }
                        "ExecStart" => {
                            if val.is_empty() {
                                svc.exec_start.clear();
                            } else if let Some(cmd) = ExecCommand::parse(val) {
                                svc.exec_start.push(cmd);
                            }
                        }
                        "ExecStartPre" => {
                            if let Some(cmd) = ExecCommand::parse(val) {
                                svc.exec_start_pre.push(cmd);
                            }
                        }
                        "ExecStartPost" => {
                            if let Some(cmd) = ExecCommand::parse(val) {
                                svc.exec_start_post.push(cmd);
                            }
                        }
                        "ExecStop" => {
                            if let Some(cmd) = ExecCommand::parse(val) {
                                svc.exec_stop.push(cmd);
                            }
                        }
                        "ExecReload" => {
                            if let Some(cmd) = ExecCommand::parse(val) {
                                svc.exec_reload.push(cmd);
                            }
                        }
                        "Restart" => {
                            svc.restart = match val.to_ascii_lowercase().as_str() {
                                "always" => RestartPolicy::Always,
                                "on-success" => RestartPolicy::OnSuccess,
                                "on-failure" => RestartPolicy::OnFailure,
                                "on-abnormal" => RestartPolicy::OnAbnormal,
                                "on-abort" => RestartPolicy::OnAbort,
                                _ => RestartPolicy::No,
                            };
                        }
                        "RestartSec" => svc.restart_sec = parse_duration(val),
                        "Environment" => {
                            for word in parse_words(val) {
                                if let Some((k, v)) = word.split_once('=') {
                                    svc.environment.insert(k.to_string(), v.to_string());
                                }
                            }
                        }
                        "EnvironmentFile" => {
                            let (optional, path_str) = if let Some(stripped) = val.strip_prefix('-')
                            {
                                (true, stripped.trim().to_string())
                            } else {
                                (false, val.to_string())
                            };
                            svc.environment_files.push((optional, path_str));
                        }
                        "WorkingDirectory" => svc.working_directory = Some(val.to_string()),
                        "User" => svc.user = Some(val.to_string()),
                        "Group" => svc.group = Some(val.to_string()),
                        "SupplementaryGroups" => {
                            for w in parse_words(val) {
                                svc.supplementary_groups.push(w);
                            }
                        }
                        "StandardOutput" => svc.standard_output = val.to_string(),
                        "StandardError" => svc.standard_error = val.to_string(),
                        "LimitNOFILE" => svc.limit_nofile = val.parse::<u64>().ok(),
                        "LimitMEMLOCK" => svc.limit_memlock = val.parse::<u64>().ok(),
                        "LimitNPROC" => svc.limit_nproc = val.parse::<u64>().ok(),
                        "OOMScoreAdjust" => svc.oom_score_adjust = val.parse::<i32>().ok(),
                        "WatchdogSec" => svc.watchdog_sec = parse_duration(val),
                        "PIDFile" => svc.pid_file = Some(val.to_string()),
                        "BusName" => svc.bus_name = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
            "Socket" => {
                if let Some(ref mut sock) = unit.socket {
                    match key {
                        "ListenStream" => sock.listen_stream.push(val.to_string()),
                        "ListenDatagram" => sock.listen_datagram.push(val.to_string()),
                        "SocketMode" => {
                            sock.socket_mode = u32::from_str_radix(val, 8).ok();
                        }
                        "SocketUser" => sock.socket_user = Some(val.to_string()),
                        "SocketGroup" => sock.socket_group = Some(val.to_string()),
                        "Service" => sock.service = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
            "Install" => match key {
                "WantedBy" => {
                    for w in parse_words(val) {
                        unit.install.wanted_by.push(w);
                    }
                }
                "RequiredBy" => {
                    for w in parse_words(val) {
                        unit.install.required_by.push(w);
                    }
                }
                "Also" => {
                    for w in parse_words(val) {
                        unit.install.also.push(w);
                    }
                }
                "Alias" => {
                    for w in parse_words(val) {
                        unit.install.alias.push(w);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    unit
}

/// Does a drop-in file explicitly set `key` inside `[section]`?
/// Used to distinguish "not mentioned" from "reset to default" (e.g.
/// `Restart=no`, or bare `ExecStart=` which clears the command list).
fn dropin_has_key(dropin_content: &str, section: &str, key: &str) -> bool {
    let mut current = String::new();
    for line in dropin_content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            current = t[1..t.len() - 1].to_string();
            continue;
        }
        if current == section {
            if let Some((k, _)) = t.split_once('=') {
                if k.trim() == key {
                    return true;
                }
            }
        }
    }
    false
}

/// Apply drop-in configurations (`<unit>.d/*.conf`) on top of an existing unit.
/// Follows systemd reset semantics: a bare `ExecStart=` / `ExecStop=` clears
/// the inherited list, and explicit `Restart=no` / `RestartSec=` override
/// even when they equal the built-in defaults.
pub fn apply_dropin(unit: &mut SystemdUnit, dropin_content: &str) {
    let dummy_path = PathBuf::from("dropin.conf");
    let dropin_unit = parse_unit(&unit.name, &dummy_path, dropin_content);

    // Merge Unit section
    if !dropin_unit.unit.description.is_empty() {
        unit.unit.description = dropin_unit.unit.description;
    }
    if !dropin_unit.unit.documentation.is_empty() {
        unit.unit.documentation = dropin_unit.unit.documentation;
    }
    unit.unit.requires.extend(dropin_unit.unit.requires);
    unit.unit.wants.extend(dropin_unit.unit.wants);
    unit.unit.binds_to.extend(dropin_unit.unit.binds_to);
    unit.unit.conflicts.extend(dropin_unit.unit.conflicts);
    unit.unit.after.extend(dropin_unit.unit.after);
    unit.unit.before.extend(dropin_unit.unit.before);
    unit.unit
        .condition_path_exists
        .extend(dropin_unit.unit.condition_path_exists);
    unit.unit
        .condition_file_not_empty
        .extend(dropin_unit.unit.condition_file_not_empty);
    unit.unit
        .condition_directory_not_empty
        .extend(dropin_unit.unit.condition_directory_not_empty);
    if dropin_has_key(dropin_content, "Unit", "DefaultDependencies") {
        unit.unit.default_dependencies = dropin_unit.unit.default_dependencies;
    }

    // Merge Service section
    if let (Some(ref mut target), Some(source)) = (&mut unit.service, dropin_unit.service) {
        if dropin_has_key(dropin_content, "Service", "Type") {
            target.service_type = source.service_type;
        }
        if dropin_has_key(dropin_content, "Service", "ExecStart") {
            target.exec_start = source.exec_start;
        }
        if dropin_has_key(dropin_content, "Service", "ExecStartPre") {
            target.exec_start_pre = source.exec_start_pre;
        } else {
            target.exec_start_pre.extend(source.exec_start_pre);
        }
        if dropin_has_key(dropin_content, "Service", "ExecStartPost") {
            target.exec_start_post = source.exec_start_post;
        } else {
            target.exec_start_post.extend(source.exec_start_post);
        }
        if dropin_has_key(dropin_content, "Service", "ExecStop") || !source.exec_stop.is_empty() {
            target.exec_stop = source.exec_stop;
        }
        if dropin_has_key(dropin_content, "Service", "ExecReload") {
            target.exec_reload = source.exec_reload;
        } else {
            target.exec_reload.extend(source.exec_reload);
        }
        if dropin_has_key(dropin_content, "Service", "Restart") {
            target.restart = source.restart;
        }
        if dropin_has_key(dropin_content, "Service", "RestartSec") {
            target.restart_sec = source.restart_sec;
        }
        for (k, v) in source.environment {
            target.environment.insert(k, v);
        }
        target.environment_files.extend(source.environment_files);
        if dropin_has_key(dropin_content, "Service", "WorkingDirectory") {
            target.working_directory = source.working_directory;
        }
        if dropin_has_key(dropin_content, "Service", "User") {
            target.user = source.user;
        }
        if dropin_has_key(dropin_content, "Service", "Group") {
            target.group = source.group;
        }
        if !source.supplementary_groups.is_empty() {
            target.supplementary_groups = source.supplementary_groups;
        }
        if dropin_has_key(dropin_content, "Service", "StandardOutput") {
            target.standard_output = source.standard_output;
        }
        if dropin_has_key(dropin_content, "Service", "StandardError") {
            target.standard_error = source.standard_error;
        }
        if dropin_has_key(dropin_content, "Service", "LimitNOFILE") {
            target.limit_nofile = source.limit_nofile;
        }
        if dropin_has_key(dropin_content, "Service", "LimitMEMLOCK") {
            target.limit_memlock = source.limit_memlock;
        }
        if dropin_has_key(dropin_content, "Service", "LimitNPROC") {
            target.limit_nproc = source.limit_nproc;
        }
        if dropin_has_key(dropin_content, "Service", "OOMScoreAdjust") {
            target.oom_score_adjust = source.oom_score_adjust;
        }
        if dropin_has_key(dropin_content, "Service", "WatchdogSec") {
            target.watchdog_sec = source.watchdog_sec;
        }
        if dropin_has_key(dropin_content, "Service", "PIDFile") {
            target.pid_file = source.pid_file;
        }
        if dropin_has_key(dropin_content, "Service", "BusName") {
            target.bus_name = source.bus_name;
        }
    }

    // Merge Socket section
    if let (Some(ref mut target), Some(source)) = (&mut unit.socket, dropin_unit.socket) {
        if !source.listen_stream.is_empty() {
            target.listen_stream = source.listen_stream;
        }
        if !source.listen_datagram.is_empty() {
            target.listen_datagram = source.listen_datagram;
        }
        if dropin_has_key(dropin_content, "Socket", "SocketMode") {
            target.socket_mode = source.socket_mode;
        }
        if dropin_has_key(dropin_content, "Socket", "SocketUser") {
            target.socket_user = source.socket_user;
        }
        if dropin_has_key(dropin_content, "Socket", "SocketGroup") {
            target.socket_group = source.socket_group;
        }
        if dropin_has_key(dropin_content, "Socket", "Service") {
            target.service = source.service;
        }
    }

    // Merge Install section (additive)
    unit.install.wanted_by.extend(dropin_unit.install.wanted_by);
    unit.install
        .required_by
        .extend(dropin_unit.install.required_by);
    unit.install.also.extend(dropin_unit.install.also);
    unit.install.alias.extend(dropin_unit.install.alias);
}

/// Expand environment variables in string like `${VAR}` or `$VAR`.
pub fn expand_env(input: &str, env: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut var_name = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        closed = true;
                        break;
                    }
                    var_name.push(inner);
                }
                if !closed {
                    // Unclosed `${VAR`: drop malformed variable name per test_env_expansion_edge_cases
                    continue;
                }
                if let Some(val) = env.get(&var_name) {
                    result.push_str(val);
                } else if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                }
            } else {
                let mut var_name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || c == '_' {
                        var_name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(val) = env.get(&var_name) {
                    result.push_str(val);
                } else if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Expand command arguments conforming to systemd syntax:
/// - Arguments with `$VAR` (no curly braces) are word-split on whitespace; empty values yield 0 arguments.
///   A path suffix after the variable name is preserved (`$DIR/sub`).
/// - Arguments with `${VAR}` (curly braces) are substituted in-place without word-splitting.
pub fn expand_command_args(args: &[String], env: &HashMap<String, String>) -> Vec<String> {
    let mut expanded_args = Vec::new();
    for raw_arg in args {
        let trimmed = raw_arg.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Naked $VAR (word-splitting, drops if empty). The variable name is
        // the longest [A-Za-z0-9_] prefix; any suffix is re-attached to each
        // expanded word (or once, when the value is empty and no split occurs).
        if trimmed.starts_with('$') && !trimmed.starts_with("${") {
            let name_end = trimmed[1..]
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                .map(|(i, _)| 1 + i)
                .unwrap_or(trimmed.len());
            let var_name = &trimmed[1..name_end];
            let suffix = &trimmed[name_end..];
            if var_name.is_empty() {
                let expanded = expand_env(trimmed, env);
                if !expanded.is_empty() {
                    expanded_args.push(expanded);
                }
                continue;
            }
            let val = env
                .get(var_name)
                .cloned()
                .or_else(|| std::env::var(var_name).ok())
                .unwrap_or_default();
            if val.is_empty() && suffix.is_empty() {
                continue;
            }
            let words: Vec<&str> = val.split_whitespace().collect();
            if words.is_empty() {
                // Empty value with a suffix (e.g. `$EMPTY/sub`) yields the
                // suffix alone rather than dropping the argument.
                if !suffix.is_empty() {
                    expanded_args.push(suffix.to_string());
                }
            } else if suffix.is_empty() {
                for word in words {
                    expanded_args.push(word.to_string());
                }
            } else {
                // Suffix re-attached per word is ambiguous; attach to the
                // last word (matches shell `$VAR/sub` intuition for single
                // values, stays deterministic for lists).
                for word in &words[..words.len() - 1] {
                    expanded_args.push(word.to_string());
                }
                expanded_args.push(format!("{}{}", words[words.len() - 1], suffix));
            }
        } else {
            let expanded = expand_env(trimmed, env);
            if !expanded.is_empty() {
                expanded_args.push(expanded);
            }
        }
    }
    expanded_args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_service() {
        let content = r#"
[Unit]
Description=OpenBSD Secure Shell server
After=network.target auditd.service
ConditionPathExists=!/etc/ssh/sshd_not_to_be_run

[Service]
EnvironmentFile=-/etc/default/ssh
Type=notify
ExecStartPre=/usr/sbin/sshd -t
ExecStart=/usr/sbin/sshd -D $SSHD_OPTS
ExecReload=/usr/sbin/sshd -t
ExecReload=/bin/kill -HUP $MAINPID
KillMode=process
Restart=on-failure
RestartSec=42s

[Install]
WantedBy=multi-user.target
Alias=sshd.service
"#;

        let unit = parse_unit(
            "ssh.service",
            Path::new("/usr/lib/systemd/system/ssh.service"),
            content,
        );
        assert_eq!(unit.name, "ssh.service");
        assert_eq!(unit.kind, UnitKind::Service);
        assert_eq!(unit.unit.description, "OpenBSD Secure Shell server");
        assert_eq!(unit.unit.after, vec!["network.target", "auditd.service"]);
        // Non-existent path with ! must pass
        assert!(unit.conditions_met());

        let svc = unit.service.expect("service section exists");
        assert_eq!(svc.service_type, ServiceType::Notify);
        assert_eq!(svc.restart, RestartPolicy::OnFailure);
        assert_eq!(svc.restart_sec, Duration::from_secs(42));
        assert_eq!(svc.exec_start.len(), 1);
        assert_eq!(svc.exec_start[0].binary, "/usr/sbin/sshd");
        assert_eq!(svc.exec_start[0].args, vec!["-D", "$SSHD_OPTS"]);

        assert_eq!(unit.install.wanted_by, vec!["multi-user.target"]);
        assert_eq!(unit.install.alias, vec!["sshd.service"]);
    }

    #[test]
    fn test_parse_exec_prefixes() {
        let cmd = ExecCommand::parse("-+/usr/bin/daemon --flag").unwrap();
        assert!(cmd.ignore_failure);
        assert!(cmd.privileged);
        assert!(!cmd.no_privileges);
        assert_eq!(cmd.binary, "/usr/bin/daemon");
        assert_eq!(cmd.args, vec!["--flag"]);
    }

    #[test]
    fn test_parse_dropin() {
        let base_content = r#"
[Unit]
Description=Base Service
After=network.target

[Service]
ExecStart=/usr/bin/base
Restart=no
"#;
        let mut unit = parse_unit("test.service", Path::new("/test.service"), base_content);

        let dropin_content = r#"
[Service]
Restart=always
RestartSec=5s
Environment="DEBUG=1"
"#;
        apply_dropin(&mut unit, dropin_content);

        let svc = unit.service.unwrap();
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert_eq!(svc.restart_sec, Duration::from_secs(5));
        assert_eq!(svc.environment.get("DEBUG").map(|s| s.as_str()), Some("1"));
    }

    #[test]
    fn test_expand_env() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("OPTS".to_string(), "-v -d".to_string());

        assert_eq!(
            expand_env("cmd ${FOO} $OPTS end", &env),
            "cmd bar -v -d end"
        );
    }

    #[test]
    fn test_expand_command_args() {
        let mut env = HashMap::new();
        env.insert("EMPTY_OPTS".to_string(), "".to_string());
        env.insert("MULTI_OPTS".to_string(), "-o Port=22 -D".to_string());
        env.insert("BRACED_SPACES".to_string(), "hello world".to_string());

        let raw_args = vec![
            "-D".to_string(),
            "$EMPTY_OPTS".to_string(),
            "$MULTI_OPTS".to_string(),
            "${BRACED_SPACES}".to_string(),
        ];

        let expanded = expand_command_args(&raw_args, &env);
        // Empty opts dropped, multi-opts split into individual words, braced kept together
        assert_eq!(expanded, vec!["-D", "-o", "Port=22", "-D", "hello world"]);
    }
}
