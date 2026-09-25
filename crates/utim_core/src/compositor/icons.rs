//! Freedesktop icon-theme lookup for the launcher.
//!
//! One bounded directory sweep resolves every requested icon name to its best
//! PNG candidate (theme rank, directory context and size proximity), which is
//! then decoded in-crate by [`crate::graphics::png`] and cached at icon
//! resolution. Keys that resolve to nothing are remembered as misses so the
//! next frame does not repeat the scan; callers drop the miss set with
//! [`IconCache::invalidate_misses`] whenever the application set changes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::graphics::png::{decode_png, RgbaImage};

/// Longest edge kept for a cached icon; the renderer scales it into its tile.
pub const ICON_MAX_EDGE: u32 = 64;
/// Reject absurdly large icon files before handing them to the decoder.
const MAX_ICON_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Dirent budget for a single resolution sweep (bounds worst-case scan cost).
const SWEEP_BUDGET: usize = 65_536;
/// Maximum directory depth visited (root -> theme -> size -> context -> file).
const MAX_DEPTH: usize = 6;

/// Directory contexts recognised by the freedesktop.org icon specification.
const CONTEXTS: [&str; 14] = [
    "actions",
    "animations",
    "apps",
    "categories",
    "devices",
    "emblems",
    "emotes",
    "intl",
    "legacy",
    "mimetypes",
    "misc",
    "places",
    "status",
    "ui",
];

/// Caches decoded application icons and resolves icon names against the
/// installed icon themes with a single batched directory sweep per request.
pub struct IconCache {
    roots: Vec<PathBuf>,
    preferred_theme: String,
    images: HashMap<String, Rc<RgbaImage>>,
    misses: HashSet<String>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconCache {
    /// Roots and preferred theme taken from the process environment
    /// (`XDG_DATA_DIRS`, `HOME`, `ICON_THEME`).
    pub fn new() -> IconCache {
        let mut roots = Vec::with_capacity(8);
        let data_dirs =
            std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
            roots.push(PathBuf::from(dir).join("icons"));
        }
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            roots.push(home.join(".icons"));
            roots.push(home.join(".local/share/icons"));
        }
        roots.push(PathBuf::from("/usr/share/pixmaps"));
        roots.push(PathBuf::from("/usr/local/share/pixmaps"));
        if std::path::Path::new("assets/icons").exists() {
            roots.push(PathBuf::from("assets/icons"));
        }
        let preferred_theme = std::env::var("ICON_THEME").unwrap_or_default();
        IconCache {
            roots,
            preferred_theme,
            images: HashMap::new(),
            misses: HashSet::new(),
        }
    }

    /// Explicit roots (used by tests and by callers with a custom search path).
    pub fn with_roots(roots: Vec<PathBuf>, preferred_theme: &str) -> IconCache {
        IconCache {
            roots,
            preferred_theme: preferred_theme.to_string(),
            images: HashMap::new(),
            misses: HashSet::new(),
        }
    }

    /// True once `key` has been looked up, whether it resolved or not.
    pub fn knows(&self, key: &str) -> bool {
        self.images.contains_key(key) || self.misses.contains(key)
    }

    /// Cached icon for `key`, if it resolved earlier.
    pub fn get(&self, key: &str) -> Option<Rc<RgbaImage>> {
        self.images.get(key).cloned()
    }

    /// Forget every recorded miss so the next [`Self::resolve_keys`] re-scans;
    /// resolved icons are kept because they stay valid until reinstalled.
    pub fn invalidate_misses(&mut self) {
        self.misses.clear();
    }

    /// Resolve every not-yet-looked-up key with one directory sweep, then
    /// decode the winners. Keys holding a path separator are read directly.
    pub fn resolve_keys(&mut self, keys: &[String]) {
        let mut pending: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if self.knows(key) {
                continue;
            }
            if key.contains('/') {
                // Explicit path: read it now, never enter the sweep.
                if let Some(img) = load_path(Path::new(key)) {
                    self.images.insert(key.clone(), Rc::new(img));
                } else {
                    self.misses.insert(key.clone());
                }
                continue;
            }
            pending.push(i);
        }
        if pending.is_empty() {
            return;
        }

        let wanted: Vec<&str> = pending.iter().map(|&i| keys[i].as_str()).collect();
        let mut best: Vec<Option<(i64, PathBuf)>> = (0..pending.len()).map(|_| None).collect();
        let mut budget = SWEEP_BUDGET;
        for root in &self.roots {
            if budget == 0 {
                break;
            }
            walk(root, 0, None, None, None, &wanted, &mut best, &mut budget, &self.preferred_theme);
        }

        for (slot, idx) in pending.iter().enumerate() {
            let key = &keys[*idx];
            match best[slot].take() {
                Some((_, path)) => match load_path(&path) {
                    Some(img) => {
                        self.images.insert(key.clone(), Rc::new(img));
                    }
                    None => {
                        // Corrupt candidate: remember so the sweep is not repeated.
                        self.misses.insert(key.clone());
                    }
                },
                None => {
                    self.misses.insert(key.clone());
                }
            }
        }
    }
}

fn load_path(path: &Path) -> Option<RgbaImage> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_ICON_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    decode_png(&bytes).map(|img| img.fit_within(ICON_MAX_EDGE))
}

/// Depth-first sweep of one root. `best` is indexed like `wanted`; each entry
/// keeps the highest scoring candidate found so far.
#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    depth: usize,
    theme: Option<&str>,
    ctx: Option<&str>,
    size: Option<(u32, u32)>,
    wanted: &[&str],
    best: &mut [Option<(i64, PathBuf)>],
    budget: &mut usize,
    preferred: &str,
) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            // `file_type` never follows links, so a symlinked directory arrives
            // as a symlink (below) and is never descended into: a cyclic link
            // can therefore never burn the sweep budget.
            let (theme, ctx, size) = classify(name.as_ref(), depth, theme, ctx, size);
            walk(&entry.path(), depth + 1, theme, ctx, size, wanted, best, budget, preferred);
            continue;
        }
        if name.len() <= 4 {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".png") {
            continue;
        }
        let stem = &lower[..lower.len() - 4];
        if !file_type.is_file() {
            // Themes alias icons with symlinks; follow them, but only pay for
            // the stat when the name is one we are actually looking for.
            if !wanted.iter().any(|key| icon_name_eq(stem, key)) {
                continue;
            }
            let follows = std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file());
            if !follows {
                continue;
            }
        }
        let score = theme_rank(theme, preferred) + context_bonus(ctx) + size_score(size);
        for (slot, key) in wanted.iter().enumerate() {
            if !icon_name_eq(stem, key) {
                continue;
            }
            let better = match &best[slot] {
                None => true,
                Some((s, p)) => {
                    score > *s || (score == *s && entry.path() < *p)
                }
            };
            if better {
                best[slot] = Some((score, entry.path()));
            }
        }
    }
}

/// Classify a directory name as it is descended into: the first level holds
/// theme names, `NxN` / `48` / `scalable` directories hold the icon size and
/// known context directories select the icon category.
fn classify<'a>(
    name: &'a str,
    depth: usize,
    theme: Option<&'a str>,
    ctx: Option<&'a str>,
    size: Option<(u32, u32)>,
) -> (Option<&'a str>, Option<&'a str>, Option<(u32, u32)>) {
    if let Some(px) = parse_size(name) {
        return (theme, ctx, Some(px));
    }
    if depth == 0 {
        // Directly below a root: every subdirectory is a candidate theme.
        return (Some(name), ctx, size);
    }
    if is_context(name) {
        return (theme, Some(name), size);
    }
    (theme, ctx, size)
}

/// `"48x48"`, `"48X48"` and `"48"` -> pixel size; `"scalable"` -> `None`.
fn parse_size(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.split(['x', 'X']);
    let first = parts.next()?;
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let w: u32 = first.parse().ok()?;
    if w == 0 || w > 1024 {
        return None;
    }
    let h = match parts.next() {
        None => w,
        Some(second) => {
            if second.is_empty() || !second.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let h: u32 = second.parse().ok()?;
            if h == 0 || h > 1024 {
                return None;
            }
            h
        }
    };
    Some((w, h))
}

fn is_context(name: &str) -> bool {
    CONTEXTS.contains(&name)
}

/// Case-insensitive icon name comparison, ignoring a `.png` suffix on the key.
fn icon_name_eq(stem: &str, key: &str) -> bool {
    let key = key.strip_suffix(".png").unwrap_or(key);
    stem.as_bytes().eq_ignore_ascii_case(key.as_bytes())
}

/// Theme tier dominates every other factor: the configured theme wins, then
/// `hicolor` (the mandatory fallback theme), then anything else.
fn theme_rank(theme: Option<&str>, preferred: &str) -> i64 {
    match theme {
        Some(t) if !preferred.is_empty() && t.eq_ignore_ascii_case(preferred) => 3_000_000_000,
        Some(t) if t.eq_ignore_ascii_case("hicolor") => 2_000_000_000,
        Some(_) => 1_000_000_000,
        None => 0,
    }
}

/// Application icons first, status/emblem decorations last.
fn context_bonus(ctx: Option<&str>) -> i64 {
    match ctx {
        Some("apps") => 100_000,
        Some("legacy") => 60_000,
        Some("mimetypes") => 40_000,
        Some("actions") => 20_000,
        Some("devices") | Some("categories") => 15_000,
        Some("places") => 10_000,
        Some("status") | Some("emblems") => -5_000,
        _ => 0,
    }
}

/// Prefer the size closest to the 64 px tile on both axes (so a 64x16 strip
/// loses to 64x64); unknown/scalable stays mid-range.
fn size_score(size: Option<(u32, u32)>) -> i64 {
    match size {
        Some((sw, sh)) => {
            let penalty = (sw as i64 - 64).abs() + (sh as i64 - 64).abs();
            1000 - (penalty * 4).min(999)
        }
        None => 700,
    }
}

#[cfg(test)]
mod tests {
    // The shared PNG fixtures are only partially used by these tests.
    #![allow(dead_code)]
    use super::*;

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/png_fixtures.rs"));

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("utim_icons_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree(root)
        }

        fn put(&self, rel: &str, png: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, png).unwrap();
        }

        /// Root in the same shape as `/usr/share/icons`: its children are themes.
        fn icons_root(&self) -> PathBuf {
            self.0.join("icons")
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prefers_bigger_apps_icon_in_hicolor() {
        let tree = Tree::new("rank");
        tree.put("icons/hicolor/16x16/apps/demo.png", GRAY1_PNG);
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        tree.put("icons/hicolor/64x16/apps/demo.png", RGB8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "Adwaita");

        let keys = vec!["demo".to_string()];
        cache.resolve_keys(&keys);
        let icon = cache.get("demo").expect("icon resolved");
        assert_eq!(icon.pixels, RGBA8_EXPECT, "64x64 apps icon wins");
        assert!(cache.knows("demo"));
    }

    #[test]
    fn theme_rank_beats_size_and_context() {
        let tree = Tree::new("theme");
        // Better size and context, but only in hicolor.
        tree.put("icons/hicolor/64x64/apps/better.png", RGBA8_PNG);
        // Preferred theme with a worse size still wins.
        tree.put("icons/Adwaita/16x16/apps/better.png", GRAY1_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "Adwaita");

        cache.resolve_keys(&["better".to_string()]);
        let icon = cache.get("better").expect("icon resolved");
        assert_eq!(icon.pixels, GRAY1_EXPECT, "preferred theme wins regardless of size");
    }

    #[test]
    fn case_insensitive_names_and_png_suffix() {
        let tree = Tree::new("case");
        tree.put("icons/hicolor/48x48/apps/Web-Browser.PNG", RGBA8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        cache.resolve_keys(&["web-browser".to_string()]);
        assert!(cache.get("web-browser").is_some(), "case and extension insensitive match");
    }

    #[test]
    fn misses_are_recorded_and_can_be_invalidated() {
        let tree = Tree::new("miss");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        let keys = vec!["nope".to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_none());
        assert!(cache.knows("nope"), "miss is remembered so the sweep is skipped");

        // A second call must not resurrect the key from the miss set.
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_none());

        // Installing the icon and invalidating makes it resolvable.
        tree.put("icons/hicolor/64x64/apps/nope.png", RGBA8_PNG);
        cache.invalidate_misses();
        assert!(!cache.knows("nope"));
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_some(), "invalidated miss re-resolves");
    }

    #[test]
    fn absolute_path_keys_bypass_the_sweep() {
        let tree = Tree::new("path");
        let file = tree.path().join("direct.png");
        std::fs::write(&file, RGBA8_PNG).unwrap();
        // Root that does not exist at all: the sweep has nothing to see.
        let mut cache = IconCache::with_roots(vec![tree.path().join("empty")], "");

        let keys = vec![file.to_string_lossy().to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get(&keys[0]).is_some(), "path key decoded directly");
        assert!(cache.knows(&keys[0]));
    }

    #[test]
    fn corrupt_candidate_is_recorded_as_a_miss() {
        let tree = Tree::new("corrupt");
        tree.put("icons/hicolor/64x64/apps/broken.png", b"not a png at all");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        cache.resolve_keys(&["broken".to_string()]);
        assert!(cache.get("broken").is_none());
        assert!(cache.knows("broken"), "corrupt winner does not rescan every frame");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_candidates_are_resolved_without_link_chasing() {
        use std::os::unix::fs::symlink;

        let tree = Tree::new("symlink");
        // The wanted file exists only behind a link, so it is found by name and
        // then measured through `fs::metadata`, which follows the link.
        tree.put("targets/real.png", RGBA8_PNG);
        let apps = tree.path().join("icons/hicolor/64x64/apps");
        std::fs::create_dir_all(&apps).unwrap();
        symlink(tree.path().join("targets/real.png"), apps.join("linked.png")).unwrap();
        // Directory links are never descended: a cycle must not cost anything,
        // and a PNG reachable only through such a link stays unreachable.
        symlink(
            tree.path().join("icons/hicolor/64x64"),
            tree.path().join("icons/hicolor/loop"),
        )
        .unwrap();
        tree.put("hidden/64x64/apps/secret.png", RGBA8_PNG);
        symlink(tree.path().join("hidden"), tree.path().join("icons/through-link")).unwrap();

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.resolve_keys(&["linked".to_string(), "secret".to_string()]);
        assert!(
            cache.get("linked").is_some(),
            "a symlinked PNG is a valid icon candidate"
        );
        assert!(cache.knows("linked"));
        assert!(
            cache.get("secret").is_none(),
            "directory symlinks are not swept"
        );
    }

    #[test]
    fn classification_helpers() {
        assert_eq!(parse_size("48x48"), Some((48, 48)));
        assert_eq!(parse_size("64X16"), Some((64, 16)));
        assert_eq!(parse_size("128"), Some((128, 128)));
        assert_eq!(parse_size("scalable"), None);
        assert_eq!(parse_size("apps"), None);
        assert!(is_context("apps") && is_context("legacy") && !is_context("hicolor"));
        assert!(icon_name_eq("phone", "phone"));
        assert!(icon_name_eq("phone", "phone.png"));
        assert!(!icon_name_eq("phone", "phon"));
        // A 64x16 strip must not outscore the true 64x64 icon.
        assert!(size_score(Some((64, 64))) > size_score(Some((64, 16))));
        assert!(size_score(Some((48, 48))) > size_score(None), "concrete size beats scalable");
    }
}
