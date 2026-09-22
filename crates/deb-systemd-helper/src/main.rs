//! deb-systemd-helper: Debian package maintainer helper compatibility layer for UTIM.
//! Manages unit enablement state in /var/lib/systemd/deb-systemd-helper-enabled/ and /etc/systemd/system/*.wants/.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

fn resolve_root_path(root: &str, rel_path: &str) -> PathBuf {
    let rel = rel_path.strip_prefix('/').unwrap_or(rel_path);
    if root.is_empty() || root == "/" {
        Path::new("/").join(rel)
    } else {
        Path::new(root).join(rel)
    }
}

fn get_enabled_state_dir(root: &str) -> PathBuf {
    resolve_root_path(root, "var/lib/systemd/deb-systemd-helper-enabled")
}

fn get_masked_state_dir(root: &str) -> PathBuf {
    resolve_root_path(root, "var/lib/systemd/deb-systemd-helper-masked")
}

fn get_etc_systemd(root: &str) -> PathBuf {
    resolve_root_path(root, "etc/systemd/system")
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let mut root_dir = env::var("DPKG_ROOT").unwrap_or_default();
    let mut no_enable = false;
    let mut clean_args = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--no-enable" {
            no_enable = true;
        } else if arg == "--quiet" || arg == "-q" {
            // Quiet flag ignored
        } else if let Some(stripped) = arg.strip_prefix("--root=") {
            root_dir = stripped.to_string();
        } else if arg == "--root" {
            if i + 1 < args.len() {
                i += 1;
                root_dir = args[i].clone();
            }
        } else if !arg.starts_with("--") {
            clean_args.push(arg.as_str());
        }
        i += 1;
    }

    if clean_args.is_empty() {
        print_usage();
        process::exit(1);
    }

    let action = clean_args[0];
    let units = &clean_args[1..];

    if units.is_empty() && action != "purge" {
        eprintln!(
            "deb-systemd-helper: error: {} requires at least one unit name",
            action
        );
        process::exit(1);
    }

    match action {
        "enable" => {
            for unit in units {
                enable_unit(&root_dir, unit, no_enable);
            }
            process::exit(0);
        }
        "disable" => {
            for unit in units {
                disable_unit(&root_dir, unit);
            }
            process::exit(0);
        }
        "is-enabled" => {
            let all_enabled = units.iter().all(|u| is_unit_enabled(&root_dir, u));
            if all_enabled {
                println!("enabled");
                process::exit(0);
            } else {
                println!("disabled");
                process::exit(1);
            }
        }
        "was-enabled" => {
            let all_was = units.iter().all(|u| was_unit_enabled(&root_dir, u));
            if all_was {
                process::exit(0);
            } else {
                process::exit(1);
            }
        }
        "mask" => {
            for unit in units {
                mask_unit(&root_dir, unit);
            }
            process::exit(0);
        }
        "unmask" => {
            for unit in units {
                unmask_unit(&root_dir, unit);
            }
            process::exit(0);
        }
        "purge" => {
            for unit in units {
                purge_unit(&root_dir, unit);
            }
            process::exit(0);
        }
        _ => {
            eprintln!("deb-systemd-helper: unknown action '{}'", action);
            process::exit(1);
        }
    }
}

fn find_unit_path(root: &str, unit: &str) -> Option<(PathBuf, PathBuf)> {
    let candidate_dirs = [
        "etc/systemd/system",
        "usr/lib/systemd/system",
        "lib/systemd/system",
    ];

    let names = if unit.contains('.') {
        vec![unit.to_string()]
    } else {
        vec![format!("{}.service", unit), unit.to_string()]
    };

    for name in &names {
        for dir in candidate_dirs {
            let host_path = resolve_root_path(root, dir).join(name);
            if host_path.exists() {
                let target_path = Path::new("/").join(dir).join(name);
                return Some((host_path, target_path));
            }
        }
    }
    None
}

fn enable_unit(root: &str, unit: &str, no_enable: bool) {
    let enabled_dir = get_enabled_state_dir(root);
    let _ = fs::create_dir_all(&enabled_dir);

    let Some((source_path, target_path)) = find_unit_path(root, unit) else {
        return;
    };

    if let Ok(content) = fs::read_to_string(&source_path) {
        let parsed = utim_core::unit::parse_unit(unit, &source_path, &content);

        // Record enablement in state file
        let state_file = enabled_dir.join(format!("{}.dsh-also", unit));
        let _ = fs::write(&state_file, format!("{}\n", unit));

        if !no_enable {
            for target in &parsed.install.wanted_by {
                let target_name = if target.contains('.') {
                    target.clone()
                } else {
                    format!("{}.target", target)
                };
                let target_wants = get_etc_systemd(root).join(format!("{}.wants", target_name));
                let _ = fs::create_dir_all(&target_wants);
                let symlink_path = target_wants.join(unit);
                let _ = fs::remove_file(&symlink_path);
                let _ = std::os::unix::fs::symlink(&target_path, &symlink_path);
            }
            for target in &parsed.install.required_by {
                let target_name = if target.contains('.') {
                    target.clone()
                } else {
                    format!("{}.target", target)
                };
                let target_req = get_etc_systemd(root).join(format!("{}.requires", target_name));
                let _ = fs::create_dir_all(&target_req);
                let symlink_path = target_req.join(unit);
                let _ = fs::remove_file(&symlink_path);
                let _ = std::os::unix::fs::symlink(&target_path, &symlink_path);
            }
        }
    }
}

fn disable_unit(root: &str, unit: &str) {
    let etc_systemd = get_etc_systemd(root);
    if let Ok(entries) = fs::read_dir(&etc_systemd) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ext = path.extension().and_then(|s| s.to_str());
                if ext == Some("wants") || ext == Some("requires") {
                    let symlink = path.join(unit);
                    if fs::symlink_metadata(&symlink).is_ok() {
                        let _ = fs::remove_file(&symlink);
                    }
                }
            }
        }
    }

    // Remove state file
    let state_file = get_enabled_state_dir(root).join(format!("{}.dsh-also", unit));
    let _ = fs::remove_file(state_file);
}

fn is_unit_enabled(root: &str, unit: &str) -> bool {
    let etc_systemd = get_etc_systemd(root);
    if let Ok(entries) = fs::read_dir(&etc_systemd) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ext = path.extension().and_then(|s| s.to_str());
                if (ext == Some("wants") || ext == Some("requires"))
                    && fs::symlink_metadata(path.join(unit)).is_ok()
                {
                    return true;
                }
            }
        }
    }
    false
}

fn was_unit_enabled(root: &str, unit: &str) -> bool {
    let state_file = get_enabled_state_dir(root).join(format!("{}.dsh-also", unit));
    state_file.exists()
}

fn mask_unit(root: &str, unit: &str) {
    let masked_dir = get_masked_state_dir(root);
    let _ = fs::create_dir_all(&masked_dir);
    let mask_state = masked_dir.join(unit);
    let _ = fs::write(&mask_state, "");

    let etc_dir = get_etc_systemd(root);
    let _ = fs::create_dir_all(&etc_dir);
    let link_target = etc_dir.join(unit);
    let _ = fs::remove_file(&link_target);
    let _ = std::os::unix::fs::symlink("/dev/null", &link_target);
}

fn unmask_unit(root: &str, unit: &str) {
    let masked_dir = get_masked_state_dir(root);
    let mask_state = masked_dir.join(unit);
    let _ = fs::remove_file(&mask_state);

    let etc_dir = get_etc_systemd(root);
    let link_target = etc_dir.join(unit);
    if let Ok(dest) = fs::read_link(&link_target) {
        if dest == Path::new("/dev/null") {
            let _ = fs::remove_file(&link_target);
        }
    }
}

fn purge_unit(root: &str, unit: &str) {
    disable_unit(root, unit);
    let state_file = get_enabled_state_dir(root).join(format!("{}.dsh-also", unit));
    let _ = fs::remove_file(state_file);
    let mask_state = get_masked_state_dir(root).join(unit);
    let _ = fs::remove_file(mask_state);
}

fn print_usage() {
    eprintln!("Usage: deb-systemd-helper <action> [options] <unit>...");
    eprintln!("Options:");
    eprintln!("  --root=<path>       Specify chroot/rootfs path");
    eprintln!("  --no-enable         Record enablement in state file without creating symlinks");
    eprintln!("  --quiet, -q         Suppress informational output");
    eprintln!("Actions:");
    eprintln!("  enable <unit>...");
    eprintln!("  disable <unit>...");
    eprintln!("  is-enabled <unit>...");
    eprintln!("  was-enabled <unit>...");
    eprintln!("  mask <unit>...");
    eprintln!("  unmask <unit>...");
    eprintln!("  purge <unit>...");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deb_systemd_helper_chroot_relative_symlinks() {
        let temp = std::env::temp_dir().join(format!("test_dsh_chroot_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let service_dir = temp.join("usr/lib/systemd/system");
        fs::create_dir_all(&service_dir).unwrap();
        fs::write(
            service_dir.join("test.service"),
            "[Unit]\nDescription=Test\n\n[Install]\nWantedBy=multi-user.target\nRequiredBy=basic.target\n",
        ).unwrap();

        let root_str = temp.to_str().unwrap();
        enable_unit(root_str, "test.service", false);

        let wants_link = temp.join("etc/systemd/system/multi-user.target.wants/test.service");
        assert!(
            fs::symlink_metadata(&wants_link).is_ok(),
            "wants symlink should exist"
        );
        let target = fs::read_link(&wants_link).unwrap();
        assert_eq!(
            target,
            PathBuf::from("/usr/lib/systemd/system/test.service")
        );

        let req_link = temp.join("etc/systemd/system/basic.target.requires/test.service");
        assert!(
            fs::symlink_metadata(&req_link).is_ok(),
            "requires symlink should exist"
        );
        let target_req = fs::read_link(&req_link).unwrap();
        assert_eq!(
            target_req,
            PathBuf::from("/usr/lib/systemd/system/test.service")
        );

        assert!(is_unit_enabled(root_str, "test.service"));
        assert!(was_unit_enabled(root_str, "test.service"));

        disable_unit(root_str, "test.service");
        assert!(!is_unit_enabled(root_str, "test.service"));
        assert!(fs::symlink_metadata(&wants_link).is_err());
        assert!(fs::symlink_metadata(&req_link).is_err());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_deb_systemd_helper_bare_name_and_bare_target() {
        let temp = std::env::temp_dir().join(format!("test_dsh_bare_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let service_dir = temp.join("usr/lib/systemd/system");
        fs::create_dir_all(&service_dir).unwrap();
        fs::write(
            service_dir.join("bare.service"),
            "[Unit]\nDescription=Bare\n\n[Install]\nWantedBy=multi-user\n",
        )
        .unwrap();

        let root_str = temp.to_str().unwrap();
        // Invoke enable with "bare" instead of "bare.service"
        enable_unit(root_str, "bare", false);

        let wants_link = temp.join("etc/systemd/system/multi-user.target.wants/bare");
        assert!(
            fs::symlink_metadata(&wants_link).is_ok(),
            "wants symlink should exist under normalized multi-user.target.wants"
        );

        let _ = fs::remove_dir_all(&temp);
    }
}
