//! Freedesktop icon-theme lookup for the launcher.
//!
//! One bounded directory sweep resolves every requested icon name to its best
//! PNG candidate (theme rank, directory context and size proximity), which is
//! then decoded in-crate by [`crate::graphics::png`] and cached at icon
//! resolution. Keys that resolve to nothing are remembered as misses so the
//! next frame does not repeat the scan; callers drop the miss set with
//! [`IconCache::invalidate_misses`] whenever the application set changes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::graphics::png::{decode_png, RgbaImage};

/// Longest edge kept for a cached icon; the renderer scales it into its tile.
pub const ICON_MAX_EDGE: u32 = 64;

/// Re-scale a decoded icon to the shell's display size.
///
/// Icons are cached at exactly the size the layout draws them, so the render
/// path can blit 1:1 instead of resampling bilinear on every frame. Called
/// when an icon enters the cache, never on the hot path.
pub fn icon_for_display(img: RgbaImage, edge: u32) -> RgbaImage {
    img.fit_within(edge.max(1))
}
/// Total decoded-pixel budget held by the cache; LRU eviction keeps the
/// live set under this (P2). 64x64 RGBA tiles are 16 KiB, so this holds
/// ~256 live icons.
pub const ICON_CACHE_BUDGET: usize = 4 * 1024 * 1024;
/// Icon sources with an edge larger than this are rejected before
/// resampling: pixels that would only be averaged away must not spike
/// frame-thread memory (P15).
pub const ICON_MAX_SOURCE_EDGE: u32 = 256;
/// Reject absurdly large icon files before handing them to the decoder.
/// Tied to the decoder's MAX_PIXELS (4M px): worst-case small-icon PNGs
/// stay far below this (P16).
const MAX_ICON_FILE_BYTES: u64 = 8 * 1024 * 1024;
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
    /// Last-use tick per cached key for LRU eviction (P2).
    used: RefCell<HashMap<String, u64>>,
    misses: HashSet<String>,
    /// Edge every decoded icon is resampled to on the way into the cache.
    display_edge: u32,
    /// Monotonic clock for `used` ticks.
    tick: Cell<u64>,
    /// Sum of `width*height*4` over `images`, bounded by ICON_CACHE_BUDGET.
    live_bytes: usize,
}

/// Deduplicate search roots preserving first-seen order: the default root
/// list aliases the same directories via XDG_DATA_DIRS/HOME/pixmaps (P27).
fn dedupe_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::with_capacity(roots.len());
    let mut out = Vec::with_capacity(roots.len());
    for r in roots {
        if seen.insert(r.clone()) {
            out.push(r);
        }
    }
    out
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
            roots: dedupe_roots(roots),
            preferred_theme,
            images: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            tick: Cell::new(0),
            live_bytes: 0,
        }
    }

    /// Set the edge cached icons are resampled to.
    ///
    /// The renderer blits 1:1 when the source matches the destination, so
    /// giving the cache the layout's icon size removes resampling from the
    /// frame path entirely.
    pub fn set_display_edge(&mut self, edge: u32) {
        self.display_edge = edge.max(1);
    }

    /// Explicit roots (used by tests and by callers with a custom search path).
    pub fn with_roots(roots: Vec<PathBuf>, preferred_theme: &str) -> IconCache {
        IconCache {
            roots: dedupe_roots(roots),
            preferred_theme: preferred_theme.to_string(),
            images: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            tick: Cell::new(0),
            live_bytes: 0,
        }
    }

    /// Current decoded-pixel footprint; always <= budget + one entry.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// Number of cached icons.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// True when no icons are cached.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// True once `key` has been looked up, whether it resolved or not.
    pub fn knows(&self, key: &str) -> bool {
        self.images.contains_key(key) || self.misses.contains(key)
    }

    /// Cached icon for `key`, if it resolved earlier. Records a use tick
    /// so LRU eviction keeps hot icons (P2).
    pub fn get(&self, key: &str) -> Option<Rc<RgbaImage>> {
        let img = self.images.get(key).cloned()?;
        self.tick.set(self.tick.get() + 1);
        self.used.borrow_mut().insert(key.to_string(), self.tick.get());
        Some(img)
    }

    /// Forget every recorded miss so the next [`Self::resolve_keys`] re-scans;
    /// resolved icons are kept because they stay valid until reinstalled.
    pub fn invalidate_misses(&mut self) {
        self.misses.clear();
    }

    /// Insert a decoded icon, evicting least-recently-used entries until
    /// the cache is back under [`ICON_CACHE_BUDGET`] (P2). A single icon
    /// larger than the whole budget still caches as the MRU entry and is
    /// evicted by the next insert, so `live_bytes` may transiently exceed
    /// the budget by one entry.
    fn insert_image(&mut self, key: String, img: RgbaImage) {
        fn img_bytes(img: &RgbaImage) -> usize {
            img.width as usize * img.height as usize * 4
        }
        self.tick.set(self.tick.get() + 1);
        let bytes = img_bytes(&img);
        while self.live_bytes + bytes > ICON_CACHE_BUDGET && !self.images.is_empty() {
            let lru = self
                .used
                .borrow()
                .iter()
                .min_by_key(|(_, &t)| t)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    if let Some(old) = self.images.remove(&k) {
                        self.live_bytes = self.live_bytes.saturating_sub(img_bytes(&old));
                    }
                    self.used.borrow_mut().remove(&k);
                }
                None => break,
            }
        }
        if let Some(old) = self.images.insert(key.clone(), Rc::new(img)) {
            self.live_bytes = self.live_bytes.saturating_sub(img_bytes(&old));
        }
        self.live_bytes += bytes;
        self.used.borrow_mut().insert(key, self.tick.get());
    }

    /// Resolve every not-yet-looked-up key with one directory sweep, then
    /// decode the winners. Keys holding a path separator are read directly.
    ///
    /// Decode cost note (P15): each call decodes at most one file per
    /// pending key, each file is capped at [`MAX_ICON_FILE_BYTES`] and each
    /// source at [`ICON_MAX_SOURCE_EDGE`]px per edge, so peak decode memory
    /// stays bounded. Callers should still batch keys into as few calls as
    /// possible (one per frame at most) rather than resolving per-tile.
    pub fn resolve_keys(&mut self, keys: &[String]) {
        let mut pending: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if self.knows(key) {
                continue;
            }
            if key.contains('/') {
                // Explicit path: read it now, never enter the sweep.
                if let Some(img) = load_path(Path::new(key), self.display_edge) {
                    self.insert_image(key.clone(), img);
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

        // Bucket wanted keys once (lowercased, suffix-stripped stem ->
        // pending slots) so each dirent costs O(1) instead of O(keys) (P24).
        let mut buckets: HashMap<Vec<u8>, Vec<usize>> = HashMap::with_capacity(pending.len());
        for (slot, &idx) in pending.iter().enumerate() {
            buckets
                .entry(normalize_key(keys[idx].as_bytes()))
                .or_default()
                .push(slot);
        }
        let mut best: Vec<Option<(i64, PathBuf)>> = (0..pending.len()).map(|_| None).collect();
        let mut budget = SWEEP_BUDGET;
        for root in &self.roots {
            if budget == 0 {
                break;
            }
            walk(
                root,
                0,
                None,
                None,
                None,
                &buckets,
                &mut best,
                &mut budget,
                &self.preferred_theme,
            );
        }
        // A budget-exhausted sweep proves nothing (P3).
        let exhausted = budget == 0;

        for (slot, idx) in pending.iter().enumerate() {
            let key = &keys[*idx];
            match best[slot].take() {
                Some((_, path)) => match load_path(&path, self.display_edge) {
                    Some(img) => {
                        self.insert_image(key.clone(), img);
                    }
                    None => {
                        // Corrupt candidate: remember so the sweep is not repeated.
                        self.misses.insert(key.clone());
                    }
                },
                None => {
                    // Record a miss only when the sweep actually completed;
                    // on exhaustion the key stays unknown so the next call
                    // retries instead of pinning a false miss.
                    if !exhausted {
                        self.misses.insert(key.clone());
                    }
                }
            }
        }
    }
}

fn load_path(path: &Path, display_edge: u32) -> Option<RgbaImage> {
    use std::io::Read;
    // Single open + bounded take(): no metadata/read TOCTOU window, and at
    // most MAX+1 bytes are ever pulled from disk (P16).
    let file = std::fs::File::open(path).ok()?;
    let mut limited = file.take(MAX_ICON_FILE_BYTES + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_ICON_FILE_BYTES {
        return None;
    }
    let img = decode_png(&bytes)?;
    // Drop the compressed bytes before resampling so peak memory is one
    // buffer at a time, not both (P15).
    drop(bytes);
    // Reject oversized sources: icons cache at <=64px tiles, so decoding
    // larger sources only burns frame-thread memory for pixels that get
    // averaged away.
    if img.width > ICON_MAX_SOURCE_EDGE || img.height > ICON_MAX_SOURCE_EDGE {
        return None;
    }
    // Resample once, here, to the size the layout draws: the render path then
    // blits 1:1 instead of filtering bilinear every frame.
    Some(img.fit_within(display_edge.max(1)))
}

/// `true` when `path` (known to be a symlink) resolves to a directory.
fn is_dir_link(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

#[cfg(unix)]
fn file_name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes()
}

#[cfg(not(unix))]
fn file_name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    name.to_str().map(|s| s.as_bytes()).unwrap_or(&[])
}

/// ASCII `.png` suffix check (any case) on raw bytes, before any allocation.
fn has_png_suffix(name: &[u8]) -> bool {
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".png")
}

fn strip_png_suffix(name: &[u8]) -> &[u8] {
    if has_png_suffix(name) {
        &name[..name.len() - 4]
    } else {
        name
    }
}

/// Lowercased, suffix-stripped bucket key for a lookup key.
fn normalize_key(key: &[u8]) -> Vec<u8> {
    strip_png_suffix(key).to_ascii_lowercase()
}

/// Depth-first sweep of one root. `best` is indexed like `pending`; each
/// entry keeps the highest scoring candidate found so far. `wanted` maps
/// lowercased, suffix-stripped stems to pending slots for O(1) lookup (P24).
#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    depth: usize,
    theme: Option<&str>,
    ctx: Option<&str>,
    size: Option<(u32, u32)>,
    wanted: &HashMap<Vec<u8>, Vec<usize>>,
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
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // Single path() per entry; the file name borrows from it (P23).
        let path = entry.path();
        if file_type.is_dir() || (file_type.is_symlink() && is_dir_link(&path)) {
            // Symlinked directories are followed: `file_type` never follows
            // links, so the explicit metadata check above is the only thing
            // that sees them. Cycles terminate via MAX_DEPTH plus the sweep
            // budget (P14).
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let (theme, ctx, size) = classify(name, depth, theme, ctx, size);
            walk(&path, depth + 1, theme, ctx, size, wanted, best, budget, preferred);
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let raw = file_name_bytes(name);
        // ASCII suffix check before any allocation (P23).
        if !has_png_suffix(raw) {
            continue;
        }
        let stem_lower = strip_png_suffix(raw).to_ascii_lowercase();
        let Some(slots) = wanted.get(&stem_lower) else {
            continue;
        };
        if !file_type.is_file() {
            // Themes alias icons with symlinks; follow them, but only pay for
            // the stat when the name is one we are actually looking for.
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
        }
        let score = theme_rank(theme, preferred) + context_bonus(ctx) + size_score(size);
        for &slot in slots {
            let better = match &best[slot] {
                None => true,
                Some((s, p)) => {
                    score > *s || (score == *s && path < *p)
                }
            };
            if better {
                best[slot] = Some((score, path.clone()));
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

/// Case-insensitive icon name comparison, ignoring a `.png` suffix (any
/// case) on either side (P13). Thin wrapper over the byte helpers, kept
/// for tests; the sweep itself compares bytes without allocating.
#[cfg(test)]
fn icon_name_eq(stem: &str, key: &str) -> bool {
    strip_png_suffix(stem.as_bytes()).eq_ignore_ascii_case(strip_png_suffix(key.as_bytes()))
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
    fn symlinked_candidates_and_dirs_are_resolved_with_bounds() {
        use std::os::unix::fs::symlink;

        let tree = Tree::new("symlink");
        // The wanted file exists only behind a link, so it is found by name and
        // then measured through `fs::metadata`, which follows the link.
        tree.put("targets/real.png", RGBA8_PNG);
        let apps = tree.path().join("icons/hicolor/64x64/apps");
        std::fs::create_dir_all(&apps).unwrap();
        symlink(tree.path().join("targets/real.png"), apps.join("linked.png")).unwrap();
        // Directory links ARE descended now (P14): a cycle only burns
        // MAX_DEPTH levels plus sweep budget, so the sweep terminates.
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
            cache.get("secret").is_some(),
            "directory symlinks are followed within depth+budget guards"
        );
    }

    #[test]
    fn lru_budget_evicts_oldest() {
        let mut cache = IconCache::with_roots(vec![], "");
        // 2 MiB each: two fit the 4 MiB budget, the third forces eviction.
        let big = |v: u8| RgbaImage {
            width: 1024,
            height: 512,
            pixels: vec![v; 1024 * 512 * 4],
        };
        cache.insert_image("a".into(), big(1));
        cache.insert_image("b".into(), big(2));
        assert!(cache.get("a").is_some(), "touch a so b is LRU");
        cache.insert_image("c".into(), big(3));
        assert!(cache.get("b").is_none(), "LRU entry evicted under budget");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(
            cache.live_bytes() <= ICON_CACHE_BUDGET + 1024 * 512 * 4,
            "live set bounded by budget + one entry"
        );
    }

    #[test]
    fn duplicate_roots_are_deduped() {
        let root = PathBuf::from("/tmp/utim_icons_dedupe");
        let cache = IconCache::with_roots(vec![root.clone(), root.clone(), root], "");
        assert_eq!(cache.roots.len(), 1);
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
        assert!(icon_name_eq("phone", "phone.PNG"));
        assert!(!icon_name_eq("phone", "phon"));
        // A 64x16 strip must not outscore the true 64x64 icon.
        assert!(size_score(Some((64, 64))) > size_score(Some((64, 16))));
        assert!(size_score(Some((48, 48))) > size_score(None), "concrete size beats scalable");
    }
}
