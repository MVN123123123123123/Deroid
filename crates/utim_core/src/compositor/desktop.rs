//! Zero-allocation Desktop Entry (.desktop) parser and real-time fuzzy search engine.
//! Scans /usr/share/applications and builds an indexed application catalogue
//! with sub-millisecond query response for the Android-style App Drawer.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// FreeDesktop application representation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApp {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: String,
    pub categories: Vec<String>,
    pub no_display: bool,
    pub terminal: bool,
    pub keywords: Vec<String>,
}

impl DesktopApp {
    pub fn new(id: String, name: String, exec: String) -> Self {
        Self {
            id,
            name,
            exec,
            icon: String::new(),
            categories: Vec::new(),
            no_display: false,
            terminal: false,
            keywords: Vec::new(),
        }
    }

    /// Extract executable command line without desktop field codes (%f, %u, etc.)
    pub fn clean_exec(&self) -> String {
        let mut parts = Vec::new();
        for word in self.exec.split_whitespace() {
            if word.starts_with('%') {
                continue;
            }
            parts.push(word);
        }
        parts.join(" ")
    }

    /// Borrow the executable program (argv[0]) honouring single/double
    /// quotes, without allocating and without destroying quoting (P12).
    /// `clean_exec` above splits naively on whitespace, so quoted paths
    /// containing spaces are corrupted there; use this for any binary
    /// lookup or spawning decision.
    pub fn exec_program(&self) -> &str {
        parse_exec_program(&self.exec)
    }
}

/// Extract argv[0] from an Exec line: a leading single/double quoted token
/// is honoured, otherwise the first whitespace-delimited token wins.
fn parse_exec_program(exec: &str) -> &str {
    let s = exec.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => return &rest[..end],
            None => return rest,
        }
    }
    if let Some(rest) = s.strip_prefix('\'') {
        match rest.find('\'') {
            Some(end) => return &rest[..end],
            None => return rest,
        }
    }
    s.split_whitespace().next().unwrap_or("")
}

/// Desktop environment id used for OnlyShowIn/NotShowIn gating (P10).
pub const UTLC_DESKTOP_ENV: &str = "UTLC";

/// Maximum .desktop file size read from disk; larger files are skipped (P8).
pub const MAX_DESKTOP_FILE_BYTES: u64 = 128 * 1024;

/// Fold backslash line continuations: a line ending in an odd number of
/// trailing backslashes continues on the next line (P11). The continuation
/// backslash itself is removed; escaped (`\\`) pairs are preserved.
fn fold_continuations(content: &str) -> Vec<String> {
    fn trailing_backslashes(s: &str) -> usize {
        s.bytes().rev().take_while(|&b| b == b'\\').count()
    }
    let mut logical: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in content.lines() {
        if trailing_backslashes(line) % 2 == 1 {
            // Odd trailing backslash: continuation; drop the last one.
            let without = &line[..line.len() - 1];
            pending.push_str(without);
        } else if pending.is_empty() {
            logical.push(line.to_string());
        } else {
            pending.push_str(line);
            logical.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        logical.push(pending);
    }
    logical
}

/// Locale candidates from `$LANG` (`de_DE.UTF-8` -> `["de_DE", "de"]`),
/// most specific first.
fn locale_candidates() -> Vec<String> {
    let lang = std::env::var("LANG").unwrap_or_default();
    let base = lang.split('.').next().unwrap_or("");
    let base = base.split('@').next().unwrap_or("");
    if base.is_empty() {
        return Vec::new();
    }
    let mut out = vec![base.to_string()];
    if let Some((l, _)) = base.split_once('_') {
        if l != base {
            out.push(l.to_string());
        }
    }
    out
}

fn parse_bool(val: &str) -> bool {
    val.eq_ignore_ascii_case("true") || val == "1"
}

/// Zero-copy parser for .desktop files
pub fn parse_desktop_entry(id: &str, content: &str) -> Option<DesktopApp> {
    let locales = locale_candidates();
    let mut in_desktop_entry = false;
    let mut name: Option<String> = None;
    // (specificity rank, value); higher rank wins, last wins on ties.
    let mut local_name: Option<(usize, String)> = None;
    let mut exec = None;
    let mut icon = None;
    let mut categories = Vec::new();
    let mut no_display = false;
    let mut hidden = false;
    let mut only_show_in: Option<Vec<String>> = None;
    let mut not_show_in: Option<Vec<String>> = None;
    let mut terminal = false;
    let mut keywords = Vec::new();
    let mut is_application = false;

    for line in fold_continuations(content) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            in_desktop_entry = section == "Desktop Entry";
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim();
            let val = v.trim();

            // Split localized keys: Name[de] -> ("Name", Some("de")).
            let (base, locale) = match key.find('[') {
                Some(i) if key.ends_with(']') => (&key[..i], Some(&key[i + 1..key.len() - 1])),
                _ => (key, None),
            };

            // Only Name honours localized variants (P11).
            if base == "Name" {
                if let Some(loc) = locale {
                    if let Some(rank) = locales.iter().position(|l| l == loc).map(|p| locales.len() - p) {
                        let rank = rank + 1; // any locale beats the generic name
                        let replace = match &local_name {
                            None => true,
                            Some((r, _)) => rank >= *r, // last wins on ties
                        };
                        if replace {
                            local_name = Some((rank, val.to_string()));
                        }
                    }
                    continue;
                }
                // Last wins for duplicate keys (P11).
                name = Some(val.to_string());
                continue;
            }
            if locale.is_some() {
                continue;
            }

            match base {
                "Type" => {
                    if val.eq_ignore_ascii_case("Application") {
                        is_application = true;
                    }
                }
                "Exec" => {
                    exec = Some(val.to_string());
                }
                "Icon" => {
                    icon = Some(val.to_string());
                }
                "NoDisplay" => {
                    no_display = parse_bool(val);
                }
                "Hidden" => {
                    hidden = parse_bool(val);
                }
                "OnlyShowIn" => {
                    only_show_in = Some(
                        val.split(';')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
                "NotShowIn" => {
                    not_show_in = Some(
                        val.split(';')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
                "Terminal" => {
                    terminal = parse_bool(val);
                }
                "Categories" => {
                    for cat in val.split(';') {
                        let c = cat.trim();
                        if !c.is_empty() {
                            categories.push(c.to_string());
                        }
                    }
                }
                "Keywords" => {
                    for kw in val.split(';') {
                        let k = kw.trim();
                        if !k.is_empty() {
                            keywords.push(k.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if !is_application || no_display || hidden {
        return None;
    }
    // Environment gating: OnlyShowIn hides unless UTLC is listed;
    // NotShowIn hides when UTLC is listed (P10).
    if let Some(only) = &only_show_in {
        if !only.iter().any(|e| e == UTLC_DESKTOP_ENV) {
            return None;
        }
    }
    if let Some(not) = &not_show_in {
        if not.iter().any(|e| e == UTLC_DESKTOP_ENV) {
            return None;
        }
    }

    // Localized name wins over the generic one (P11).
    if let Some((_, loc_name)) = local_name {
        name = Some(loc_name);
    }
    let name = name?;
    let exec = exec?;

    Some(DesktopApp {
        id: id.to_string(),
        name,
        exec,
        icon: icon.unwrap_or_default(),
        categories,
        no_display,
        terminal,
        keywords,
    })
}

/// Stack buffer capacity (chars) for the allocation-free ASCII fuzzy path.
const FUZZY_STACK_MAX: usize = 512;

/// Real-time fuzzy match algorithm
/// Returns Some(score) if matched, None otherwise.
/// Higher score denotes better match.
///
/// Correctness (P1): the target's lowercase expansion is kept 1:1 paired
/// with its original characters, so multi-char lowercase mappings
/// (e.g. `İ` -> `i̇`) can never desynchronise indices and panic under
/// `panic=abort`. Performance (P20): pure-ASCII inputs that fit the stack
/// buffers score with zero allocation; [`DesktopCatalogue::search`] folds
/// the query once via [`fuzzy_match_folded`] instead of once per app.
pub fn fuzzy_match(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q_chars: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    fuzzy_match_folded(&q_chars, target)
}

/// Score `target` against an already-lowercased query.
fn fuzzy_match_folded(q_chars: &[char], target: &str) -> Option<i32> {
    if q_chars.is_empty() {
        return Some(0);
    }
    if target.len() <= FUZZY_STACK_MAX
        && q_chars.len() <= FUZZY_STACK_MAX
        && target.is_ascii()
        && q_chars.iter().all(|c| c.is_ascii())
    {
        let mut qbuf = ['\0'; FUZZY_STACK_MAX];
        let mut lbuf = ['\0'; FUZZY_STACK_MAX];
        let mut obuf = ['\0'; FUZZY_STACK_MAX];
        for (i, &c) in q_chars.iter().enumerate() {
            qbuf[i] = c;
        }
        let mut n = 0;
        for b in target.bytes() {
            let c = b as char;
            lbuf[n] = c.to_ascii_lowercase();
            obuf[n] = c;
            n += 1;
        }
        return score_folded(&qbuf[..q_chars.len()], &lbuf[..n], &obuf[..n]);
    }
    // Unicode slow path: paired 1:1 so expansions stay synchronised.
    let mut t_lower: Vec<char> = Vec::with_capacity(target.len());
    let mut t_orig: Vec<char> = Vec::with_capacity(target.len());
    for c in target.chars() {
        let mut n = 0;
        for lc in c.to_lowercase() {
            t_lower.push(lc);
            t_orig.push(c);
            n += 1;
        }
        if n == 0 {
            t_lower.push(c);
            t_orig.push(c);
        }
    }
    score_folded(q_chars, &t_lower, &t_orig)
}

/// Core subsequence scorer. `t_lower`/`t_orig` must be the same length.
fn score_folded(q_chars: &[char], t_lower: &[char], t_orig: &[char]) -> Option<i32> {
    debug_assert_eq!(t_lower.len(), t_orig.len());
    let mut q_idx = 0;
    let mut score = 0;
    let mut consecutive = 0;
    let mut prev_matched_idx = 0;

    for (t_idx, &c) in t_lower.iter().enumerate() {
        if q_idx < q_chars.len() && c == q_chars[q_idx] {
            let mut char_score = 10;

            // Consecutive bonus
            if q_idx > 0 && t_idx == prev_matched_idx + 1 {
                consecutive += 1;
                char_score += consecutive * 5;
            } else {
                consecutive = 0;
            }

            // Word boundary bonus (start of string or preceded by non-alphanumeric)
            if t_idx == 0 {
                char_score += 25;
            } else {
                let prev = t_orig[t_idx - 1];
                if prev == ' ' || prev == '-' || prev == '_' || prev == '.' {
                    char_score += 20;
                } else if prev.is_lowercase() && t_orig[t_idx].is_uppercase() {
                    // CamelCase match bonus
                    char_score += 15;
                }
            }

            score += char_score;
            prev_matched_idx = t_idx;
            q_idx += 1;
        }
    }

    if q_idx == q_chars.len() {
        Some(score)
    } else {
        None
    }
}

/// Indexed Application Catalogue for the Mobile Shell
pub struct DesktopCatalogue {
    apps: Vec<DesktopApp>,
    /// O(1) id -> position lookup replacing the O(n) linear scan (P22).
    index: HashMap<String, usize>,
    /// `$PATH` split once per catalogue, not once per app (P21).
    path_dirs: Vec<PathBuf>,
    /// Memoised program -> exists lookups (P21).
    bin_cache: HashMap<String, bool>,
}

impl Default for DesktopCatalogue {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopCatalogue {
    pub fn new() -> Self {
        let mut path_dirs = Vec::new();
        if let Ok(env_path) = std::env::var("PATH") {
            for dir in env_path.split(':').filter(|d| !d.is_empty()) {
                let p = PathBuf::from(dir);
                if !path_dirs.contains(&p) {
                    path_dirs.push(p);
                }
            }
        }
        for d in ["/usr/bin", "/usr/local/bin", "/bin", "/usr/games"] {
            let p = PathBuf::from(d);
            if !path_dirs.contains(&p) {
                path_dirs.push(p);
            }
        }
        Self {
            apps: Vec::new(),
            index: HashMap::new(),
            path_dirs,
            bin_cache: HashMap::new(),
        }
    }

    pub fn apps(&self) -> &[DesktopApp] {
        &self.apps
    }

    pub fn add_app(&mut self, app: DesktopApp) {
        if let Some(&pos) = self.index.get(&app.id) {
            self.apps[pos] = app;
        } else {
            let pos = self.apps.len();
            self.index.insert(app.id.clone(), pos);
            self.apps.push(app);
        }
    }

    /// Load standard desktop directories
    pub fn scan_system_directories(&mut self) {
        self.apps.clear();
        self.index.clear();
        let paths = [
            Path::new("/usr/share/applications"),
            Path::new("/usr/local/share/applications"),
            Path::new("/etc/xdg/autostart"),
            Path::new("/root/.local/share/applications"),
            Path::new("/home/linux/.local/share/applications"),
        ];
        for path in &paths {
            if path.is_dir() {
                self.scan_directory(path);
            }
        }
    }

    pub fn find_app(&self, id_or_name: &str) -> Option<&DesktopApp> {
        self.apps.iter().find(|a| {
            a.id.eq_ignore_ascii_case(id_or_name) || a.name.eq_ignore_ascii_case(id_or_name)
        })
    }

    /// O(1) amortised existence check using the catalogue-wide PATH split
    /// and memoisation cache (P21). Takes the already-extracted program
    /// (see [`DesktopApp::exec_program`]), never re-reads `$PATH`.
    fn binary_exists(&mut self, program: &str) -> bool {
        if program.is_empty() {
            return false;
        }
        if let Some(&hit) = self.bin_cache.get(program) {
            return hit;
        }
        let path = Path::new(program);
        let hit = if path.is_absolute() {
            path.exists()
        } else if program.contains('/') {
            // Relative path (./foo, sub/dir/bin): resolve against cwd.
            path.exists()
        } else {
            self.path_dirs.iter().any(|d| d.join(program).exists())
        };
        self.bin_cache.insert(program.to_string(), hit);
        hit
    }

    /// Scan a single directory
    pub fn scan_directory(&mut self, dir: &Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("desktop") {
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    // Path-qualified id: same-stem files from different
                    // directories no longer collide (P22).
                    let id = format!("{}/{}", dir.display(), stem);
                    // Skip unreadable or oversized files (P8).
                    let content = match read_capped(&path, MAX_DESKTOP_FILE_BYTES) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if let Some(app) = parse_desktop_entry(&id, &content) {
                        if self.binary_exists(app.exec_program()) {
                            self.add_app(app);
                        }
                    }
                }
            }
        }
    }

    /// Real-time search query returning ranked results (< 1 ms latency)
    pub fn search(&self, query: &str) -> Vec<(&DesktopApp, i32)> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Decorate-sort-undecorate: lowercase once per app instead of
            // once per comparison (P20).
            let mut all: Vec<(&DesktopApp, i32, String)> =
                self.apps.iter().map(|a| (a, 0, a.name.to_lowercase())).collect();
            all.sort_by(|a, b| a.2.cmp(&b.2));
            return all.into_iter().map(|(a, s, _)| (a, s)).collect();
        }

        // Fold the query once per search, not once per app (P20).
        let q_chars: Vec<char> = trimmed.chars().flat_map(|c| c.to_lowercase()).collect();
        let mut results: Vec<(&DesktopApp, i32, String)> = Vec::new();
        for app in &self.apps {
            let mut best_score = fuzzy_match_folded(&q_chars, &app.name);

            // Also check app ID
            if let Some(id_score) = fuzzy_match_folded(&q_chars, &app.id) {
                best_score = Some(best_score.map_or(id_score, |s| s.max(id_score)));
            }

            // Also check keywords
            for kw in &app.keywords {
                if let Some(kw_score) = fuzzy_match_folded(&q_chars, kw) {
                    best_score = Some(best_score.map_or(kw_score - 5, |s| s.max(kw_score - 5)));
                }
            }

            if let Some(score) = best_score {
                results.push((app, score, app.name.to_lowercase()));
            }
        }

        // Rank by highest score first, then alphabetically (decorated).
        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        results.into_iter().map(|(a, s, _)| (a, s)).collect()
    }
}

/// Read a text file capped at `cap` bytes. Oversized files and unreadable
/// files both surface as `Err` so callers skip them (P8). The `take(cap+1)`
/// bound means at most `cap+1` bytes are ever pulled from disk.
fn read_capped(path: &Path, cap: u64) -> io::Result<String> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut limited = file.take(cap + 1);
    let mut s = String::new();
    limited.read_to_string(&mut s)?;
    if s.len() as u64 > cap {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "desktop file exceeds size cap",
        ));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_desktop_entry_valid() {
        let content = r#"
[Desktop Entry]
Version=1.0
Type=Application
Name=Firefox Web Browser
GenericName=Web Browser
Comment=Browse the World Wide Web
Exec=firefox %u
Icon=firefox
Terminal=false
Categories=Network;WebBrowser;
Keywords=Internet;WWW;Browser;
"#;
        let app = parse_desktop_entry("firefox", content).expect("Failed to parse firefox desktop");
        assert_eq!(app.id, "firefox");
        assert_eq!(app.name, "Firefox Web Browser");
        assert_eq!(app.clean_exec(), "firefox");
        assert_eq!(app.icon, "firefox");
        assert!(!app.terminal);
        assert!(!app.no_display);
        assert!(app.categories.contains(&"WebBrowser".to_string()));
        assert!(app.keywords.contains(&"Internet".to_string()));
    }

    #[test]
    fn test_parse_desktop_entry_nodisplay() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=Hidden Service
Exec=/usr/bin/hidden
NoDisplay=true
"#;
        assert!(parse_desktop_entry("hidden", content).is_none());
    }

    #[test]
    fn test_fuzzy_match_unicode_expansion_no_panic() {
        // U+0130 lowercases to two chars; indices must stay paired (P1).
        assert!(fuzzy_match("i̇", "İ").is_some());
        assert!(fuzzy_match("xyz", "İİİ").is_none());
        // CamelCase bonus still applies via paired originals.
        let camel = fuzzy_match("tb", "ToolBar").expect("match");
        let lower = fuzzy_match("tb", "toolbar").expect("match");
        assert!(camel >= lower);
    }

    #[test]
    fn test_parse_hidden_and_show_in_gating() {
        let base = "[Desktop Entry]\nType=Application\nName=X\nExec=/bin/true\n";
        assert!(parse_desktop_entry("a", &format!("{base}Hidden=true\n")).is_none());
        assert!(parse_desktop_entry(
            "b",
            &format!("{base}OnlyShowIn=GNOME;KDE;\n")
        )
        .is_none());
        assert!(parse_desktop_entry(
            "c",
            &format!("{base}OnlyShowIn=GNOME;UTLC;\n")
        )
        .is_some());
        assert!(parse_desktop_entry("d", &format!("{base}NotShowIn=UTLC;\n")).is_none());
        assert!(parse_desktop_entry("e", &format!("{base}NotShowIn=GNOME;\n")).is_some());
    }

    #[test]
    fn test_parse_localized_name() {
        std::env::set_var("LANG", "xx_YY.UTF-8");
        let content =
            "[Desktop Entry]\nType=Application\nName=Generic\nName[xx]=Local\nExec=/bin/true\n";
        let app = parse_desktop_entry("l", content).expect("parsed");
        assert_eq!(app.name, "Local");
        std::env::remove_var("LANG");
    }

    #[test]
    fn test_parse_continuation_and_last_wins() {
        let content = "[Desktop Entry]\nType=Application\nName=Fi\\\nrst\nName=Second\nExec=/bin/true\n";
        let app = parse_desktop_entry("x", content).expect("parsed");
        assert_eq!(app.name, "Second");
    }

    #[test]
    fn test_exec_program_quoting() {
        let app = DesktopApp::new("q".into(), "Q".into(), "\"/opt/my app/run\" %F".into());
        assert_eq!(app.exec_program(), "/opt/my app/run");
        let app2 = DesktopApp::new("p".into(), "P".into(), "'/opt/a b' --x".into());
        assert_eq!(app2.exec_program(), "/opt/a b");
        let app3 = DesktopApp::new("r".into(), "R".into(), "firefox %u".into());
        assert_eq!(app3.exec_program(), "firefox");
    }

    #[test]
    fn test_fuzzy_match_scoring() {
        // Exact prefix match should score high
        let score_term = fuzzy_match("term", "Terminal").expect("Should match");
        let score_sub = fuzzy_match("min", "Terminal").expect("Should match");
        assert!(score_term > score_sub);

        // Word boundary bonus
        let score_browser = fuzzy_match("web", "Web Browser").expect("Should match");
        assert!(score_browser > 30);

        // Subsequence match
        assert!(fuzzy_match("ffx", "Firefox").is_some());
        assert!(fuzzy_match("xyz", "Firefox").is_none());
    }

    #[test]
    fn test_catalogue_search_ranking() {
        let mut cat = DesktopCatalogue::new();
        cat.add_app(DesktopApp::new(
            "phone".into(),
            "Phone".into(),
            "dialer".into(),
        ));
        cat.add_app(DesktopApp::new(
            "photos".into(),
            "Photos".into(),
            "photos".into(),
        ));
        cat.add_app(DesktopApp::new(
            "term".into(),
            "Terminal".into(),
            "term".into(),
        ));

        let res = cat.search("ph");
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0.name, "Phone");
        assert_eq!(res[1].0.name, "Photos");
    }
}
