//! Comprehensive edge-case test suite for UTIM Core: unit parser, DAG cycle handling,
//! fstab, android_rc, environment expansion, and durations.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use utim_core::android_rc::parse_android_rc;
use utim_core::dag::UnitDag;
use utim_core::fstab::parse_fstab;
use utim_core::ipc::IpcRequest;
use utim_core::ring_buffer::ByteRingBuffer;
use utim_core::unit::{expand_env, parse_duration, parse_unit, parse_words};

#[test]
fn test_unit_parser_edge_cases() {
    // 1. Completely empty content
    let empty_unit = parse_unit("empty.service", Path::new("/empty.service"), "");
    assert_eq!(empty_unit.name, "empty.service");
    assert!(empty_unit.service.as_ref().unwrap().exec_start.is_empty());

    // 2. Only comments and blank lines
    let comment_unit = parse_unit(
        "comment.service",
        Path::new("/comment.service"),
        "# This is a comment\n; Semicolon comment\n   \n\t\n",
    );
    assert_eq!(comment_unit.unit.description, "");

    // 3. Multi-line continuation with trailing backslashes and spaces
    let continuation_unit = parse_unit(
        "cont.service",
        Path::new("/cont.service"),
        r#"
[Unit]
Description=Service with \
            long multi-line \
            description

[Service]
ExecStart=/bin/echo \
          "hello world" \
          --flag=1
"#,
    );
    assert!(continuation_unit
        .unit
        .description
        .contains("long multi-line"));
    let svc = continuation_unit.service.unwrap();
    assert_eq!(svc.exec_start.len(), 1);
    assert_eq!(svc.exec_start[0].binary, "/bin/echo");
    assert_eq!(svc.exec_start[0].args, vec!["hello world", "--flag=1"]);
}

#[test]
fn test_duration_parsing_edge_cases() {
    assert_eq!(parse_duration(""), Duration::ZERO);
    assert_eq!(parse_duration("0"), Duration::ZERO);
    assert_eq!(parse_duration("no"), Duration::ZERO);
    assert_eq!(parse_duration("infinity"), Duration::ZERO);
    assert_eq!(parse_duration("invalid_str"), Duration::ZERO);

    assert_eq!(parse_duration("100ms"), Duration::from_millis(100));
    assert_eq!(parse_duration("45s"), Duration::from_secs(45));
    assert_eq!(parse_duration("5min"), Duration::from_secs(300));
    assert_eq!(parse_duration("2h"), Duration::from_secs(7200));
    assert_eq!(parse_duration("120"), Duration::from_secs(120)); // raw seconds
}

#[test]
fn test_env_expansion_edge_cases() {
    let mut env = HashMap::new();
    env.insert("A".to_string(), "apple".to_string());
    env.insert("B".to_string(), "banana".to_string());

    // Undefined variables remain unchanged or expanded from process env
    assert_eq!(expand_env("eat ${A} and $B", &env), "eat apple and banana");
    assert_eq!(expand_env("cost is $100", &env), "cost is "); // $100 parsed as variable name starting with 100
    assert_eq!(expand_env("unclosed ${A without brace", &env), "unclosed ");
    assert_eq!(
        expand_env("no variables at all", &env),
        "no variables at all"
    );
}

#[test]
fn test_word_parsing_quotes_and_escapes() {
    let words =
        parse_words(r#"/usr/bin/cmd "arg with spaces" 'single quotes' escaped\ space normal"#);
    assert_eq!(
        words,
        vec![
            "/usr/bin/cmd",
            "arg with spaces",
            "single quotes",
            "escaped space",
            "normal"
        ]
    );
}

#[test]
fn test_dag_self_loop_and_complex_cycles() {
    let mut dag = UnitDag::new();

    // Self loop: A -> A
    let self_loop = parse_unit(
        "self.service",
        Path::new("/self.service"),
        "[Unit]\nAfter=self.service\n",
    );
    dag.insert(self_loop);

    // 4-cycle: 1 -> 2 -> 3 -> 4 -> 1
    let u1 = parse_unit(
        "1.service",
        Path::new("/1.service"),
        "[Unit]\nAfter=2.service\n",
    );
    let u2 = parse_unit(
        "2.service",
        Path::new("/2.service"),
        "[Unit]\nAfter=3.service\n",
    );
    let u3 = parse_unit(
        "3.service",
        Path::new("/3.service"),
        "[Unit]\nAfter=4.service\n",
    );
    let u4 = parse_unit(
        "4.service",
        Path::new("/4.service"),
        "[Unit]\nAfter=1.service\n",
    );

    dag.insert(u1);
    dag.insert(u2);
    dag.insert(u3);
    dag.insert(u4);

    let cycles = dag.detect_cycles();
    assert_eq!(cycles.len(), 2); // self-loop cycle AND 4-node cycle
    assert!(cycles
        .iter()
        .any(|c| c.len() == 1 && c[0] == "self.service"));
    assert!(cycles.iter().any(|c| c.len() == 4));

    // resolve_start_queue must break cycles and not hang or crash
    let queue = dag.resolve_start_queue("1.service");
    assert_eq!(queue.len(), 4);
}

#[test]
fn test_fstab_parser_edge_cases() {
    let malformed_fstab = r#"
# Malformed line with fewer than 4 tokens
short_line /mnt

# Valid line
/dev/block/bootdevice/by-name/userdata /data f2fs noatime,nosuid,nodev wait,check,formattable

# Line with whitespace padding
  vendor   /vendor   erofs   ro   wait,logical  
"#;
    let entries = parse_fstab(malformed_fstab);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].mount_point, "/data");
    assert_eq!(entries[0].fs_type, "f2fs");
    assert_eq!(entries[1].mount_point, "/vendor");
    assert!(entries[1].is_logical());
}

#[test]
fn test_android_rc_parser_edge_cases() {
    let complex_rc = r#"
# Service with oneshot, disabled, critical
service vendor.ril-daemon /vendor/bin/hw/rild -l /vendor/lib64/libsec-ril.so
    class core
    user radio
    group radio cache inet misc audio log readproc wakelock
    capabilities BLOCK_SUSPEND NET_ADMIN NET_RAW
    disabled
    oneshot
    critical

service placeholder
    # Service without command, should be ignored
"#;
    let services = parse_android_rc(complex_rc);
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, "vendor.ril-daemon");
    assert_eq!(services[0].user, "radio");
    assert!(services[0].disabled);
    assert!(services[0].oneshot);
    assert!(services[0].critical);
    assert!(services[0].matches_subsystem("ril"));
    assert!(services[0].matches_subsystem("radio"));
}

#[test]
fn test_ring_buffer_edge_cases() {
    let mut rb = ByteRingBuffer::<4>::new();
    assert_eq!(rb.len(), 0);

    // Read on empty
    let mut out = [0u8; 2];
    assert_eq!(rb.read(&mut out), 0);

    // Exact capacity
    rb.write_overwrite(b"1234");
    assert_eq!(rb.len(), 4);
    assert!(rb.is_full());
    assert_eq!(rb.to_string_lossy(), "1234");

    // Overwrite by 1
    rb.push_overwrite(b'5');
    assert_eq!(rb.len(), 4);
    assert_eq!(rb.to_string_lossy(), "2345");

    // Clear
    rb.clear();
    assert_eq!(rb.len(), 0);
    assert!(rb.is_empty());
}

#[test]
fn test_ipc_parser_edge_cases() {
    // Empty and malformed
    assert_eq!(IpcRequest::deserialize(""), None);
    assert_eq!(IpcRequest::deserialize("   "), None);
    assert_eq!(IpcRequest::deserialize("NONEXISTENT_COMMAND"), None);

    // Missing args
    assert_eq!(IpcRequest::deserialize("SET_OOM_SCORE"), None);
    assert_eq!(IpcRequest::deserialize("SET_OOM_SCORE 123"), None);

    // Valid
    assert_eq!(
        IpcRequest::deserialize("DAEMON_RELOAD"),
        Some(IpcRequest::DaemonReload)
    );
    assert_eq!(
        IpcRequest::deserialize("LIST_UNITS"),
        Some(IpcRequest::ListUnits)
    );
    assert_eq!(
        IpcRequest::deserialize("ANALYZE_TIME"),
        Some(IpcRequest::AnalyzeTime)
    );
    assert_eq!(IpcRequest::deserialize("REBOOT"), Some(IpcRequest::Reboot));
    assert_eq!(
        IpcRequest::deserialize("POWEROFF"),
        Some(IpcRequest::Poweroff)
    );
    assert_eq!(IpcRequest::Reboot.serialize(), "REBOOT\n");
    assert_eq!(IpcRequest::Poweroff.serialize(), "POWEROFF\n");
}

#[test]
fn test_condition_negation_edge_cases() {
    let mut unit = parse_unit("test.service", Path::new("/test.service"), "");

    // 1. ConditionPathExists with negation: !/nonexistent_path_12345 should be TRUE
    unit.unit.condition_path_exists = vec!["!/nonexistent_path_123456789".to_string()];
    assert!(unit.conditions_met());

    // ConditionPathExists without negation: /nonexistent_path_12345 should be FALSE
    unit.unit.condition_path_exists = vec!["/nonexistent_path_123456789".to_string()];
    assert!(!unit.conditions_met());

    // 2. Existing path (/tmp or /dev) with negation should be FALSE
    unit.unit.condition_path_exists = vec!["!/dev".to_string()];
    assert!(!unit.conditions_met());

    // Existing path (/dev) without negation should be TRUE
    unit.unit.condition_path_exists = vec!["/dev".to_string()];
    assert!(unit.conditions_met());
    unit.unit.condition_path_exists.clear();

    // 3. ConditionFileNotEmpty with negation on nonexistent should be TRUE
    unit.unit.condition_file_not_empty = vec!["!/nonexistent_file_987".to_string()];
    assert!(unit.conditions_met());
    unit.unit.condition_file_not_empty.clear();

    // 4. ConditionDirectoryNotEmpty with negation on nonexistent should be TRUE
    unit.unit.condition_directory_not_empty = vec!["!/nonexistent_dir_987".to_string()];
    assert!(unit.conditions_met());
}

#[test]
fn test_expand_command_args_systemd_compliance() {
    use utim_core::unit::expand_command_args;

    let mut env = HashMap::new();
    env.insert("FLAGS".to_string(), "-v --debug".to_string());
    env.insert("EMPTY".to_string(), "".to_string());
    env.insert("WHITESPACE".to_string(), "   ".to_string());
    env.insert("TARGET".to_string(), "my service".to_string());

    // Naked $FLAGS should be word-split into two arguments: "-v", "--debug"
    let raw = vec!["$FLAGS".to_string(), "arg1".to_string()];
    let expanded = expand_command_args(&raw, &env);
    assert_eq!(expanded, vec!["-v", "--debug", "arg1"]);

    // Naked $EMPTY and $UNSET should be completely omitted from the arg list
    let raw = vec![
        "$EMPTY".to_string(),
        "$UNSET".to_string(),
        "$WHITESPACE".to_string(),
        "keep".to_string(),
    ];
    let expanded = expand_command_args(&raw, &env);
    assert_eq!(expanded, vec!["keep"]);

    // Braced ${TARGET} should NOT be word-split; it preserves spaces as a single argument
    let raw = vec!["${TARGET}".to_string(), "second".to_string()];
    let expanded = expand_command_args(&raw, &env);
    assert_eq!(expanded, vec!["my service", "second"]);

    // Braced ${EMPTY} when unquoted expands to empty string and is omitted per systemd spec
    let raw = vec!["${EMPTY}".to_string(), "after".to_string()];
    let expanded = expand_command_args(&raw, &env);
    assert_eq!(expanded, vec!["after"]);
}
