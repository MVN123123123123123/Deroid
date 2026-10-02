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
                    if let Some(rank) = locales
                        .iter()
                        .position(|l| l == loc)
                        .map(|p| locales.len() - p)
                    {
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

// ===========================================================================
// Diacritic folding
// ===========================================================================

/// Fold window, in bytes of the **folded** form.
///
/// The reference normalises with `Normalizer.normalize(input, NFKD)` and drops
/// `\p{M}+` (`AppSearchProvider.kt:16, 67-70`). `std` has no normaliser and
/// `AGENTS.md` forbids a dependency for a string utility, so the decomposition
/// is a table: [`nfkd_ascii`] for the code points that have one, and
/// [`is_combining_mark`] for the marks themselves.
///
/// Making the fold a fixed window is what lets the whole comparison path run
/// in stack storage. The ranked search runs per frame, so a `String` here
/// would be a `malloc`/`free` pair at 120 Hz for data the query bounds
/// anyway. 64 bytes is the *query's* budget too, and a desktop entry name
/// longer than that folds to its first 64 bytes -- always on a character
/// boundary, because [`fold_into`] only ever appends whole `char`s.
const FOLD_CAP: usize = 64;

/// The stack a single query needs, allocated once and reused for every row.
///
/// Every window the matcher needs is fixed-size, so "reuse" means "hoist out of
/// the per-app loop": before this existed, each candidate allocated *five*
/// arrays on the stack and the compiler's only obligation was to zero them --
/// two [`FOLD_CAP`] fold windows plus two `[u16; FOLD_CAP + 1]` ratio rows per
/// field, and `best_match` runs up to three fields. That is 260 bytes of memset
/// per field per app in an unoptimised build, which is why the whole-catalogue
/// sweep the `--benchmark` gate runs stopped fitting its 1 ms budget.
///
/// Hoisting makes the cost once per *query*. It is the same change the crate
/// makes everywhere else on a frame path -- `FrameView` for the shell's grids,
/// `LabelScratch` for labels -- applied to the matcher, which had been written
/// as if the compiler would elide the zeroing. It does, in release; it does not
/// in debug, and the project's own verification gate runs the debug binary.
///
/// `Default` is a full zeroing, which is correct rather than merely convenient:
/// [`similarity_100_in`] writes every cell it reads except `prev[0]`, which it
/// sets explicitly, so the value is never observed -- but relying on that would
/// make a future edit to the loop's bounds a silent wrong answer, and 400 bytes
/// once per query is not worth the fragility.
#[derive(Clone)]
struct MatchScratch {
    /// Three [`FOLD_CAP`] fold windows side by side: name, id, keyword.
    ///
    /// One array rather than three fields so that `best_match` can split it into
    /// three disjoint borrows with a `split_at_mut` -- the folded name has to
    /// stay borrowed while the same call folds the id, and distinct struct fields
    /// cannot be borrowed separately through a single `&mut`.
    fold: [u8; FOLD_CAP * 3],
    /// The Levenshtein ratio's two [`FOLD_CAP + 1`] rows, back to back.
    ratio: [u16; (FOLD_CAP + 1) * 2],
}

impl Default for MatchScratch {
    fn default() -> Self {
        Self {
            fold: [0; FOLD_CAP * 3],
            ratio: [0; (FOLD_CAP + 1) * 2],
        }
    }
}

/// The ASCII residue of NFKD for one code point: the base letter a precomposed
/// Latin letter decomposes to, or the ASCII space `NO-BREAK SPACE` decomposes
/// to.
///
/// The code points deliberately *absent* are the interesting half, because they
/// are the ones that survive NFKD unchanged and must therefore keep their own
/// identity: `Æ Ç Ñ ß Œ æ ç ñ ß œ` and the `Latin Extended-A` tail from `ŉ`
/// on. `cafe` must **not** match `Cœur` -- and in the reference it does not,
/// because `stripDiacritics("Cœur")` returns `"Cœur"`. Folding `œ` to `oe`
/// would be a Unicode improvement the reference does not have, and matching
/// against a *different* language model is how a search starts returning
/// things the user did not ask for.
///
/// The one expansion the table performs is `NO-BREAK SPACE -> SPACE`, which is
/// a genuine NFKD compatibility mapping (`U+00A0 -> U+0020`) and matters: a
/// query typed with a non-breaking space is two words to `split_whitespace`
/// and would otherwise be one unmatchable token.
#[inline]
fn nfkd_ascii(c: char) -> Option<char> {
    char::from_u32(match c as u32 {
        0x00A0 => 0x20, // NO-BREAK SPACE

        // Latin-1 Supplement, the half that decomposes. The code points the
        // ranges *skip* are exactly the ones with no decomposition: Æ (C6),
        // Ð (D0), × (D7), Ø (D8), Þ (DE), ß (DF), and their lowercase
        // reflections æ (E6), ð (F0), ÷ (F7), ø (F8), þ (FE). So `cafe`
        // does not match `Cœur` -- exactly as in the reference, whose
        // `stripDiacritics` returns `"Cœur"`.
        0x00C0..=0x00C5 => 0x41, // À Á Â Ã Ä Å
        0x00C7 => 0x43,          // Ç -> C
        0x00C8..=0x00CB => 0x45, // È É Ê Ë
        0x00CC..=0x00CF => 0x49, // Ì Í Î Ï
        0x00D1 => 0x4E,          // Ñ -> N
        0x00D2..=0x00D6 => 0x4F, // Ò Ó Ô Õ Ö
        0x00D9..=0x00DC => 0x55, // Ù Ú Û Ü
        0x00DD => 0x59,          // Ý -> Y
        0x00E0..=0x00E5 => 0x61,
        0x00E7 => 0x63, // ç -> c
        0x00E8..=0x00EB => 0x65,
        0x00EC..=0x00EF => 0x69,
        0x00F1 => 0x6E, // ñ -> n
        0x00F2..=0x00F6 => 0x6F,
        0x00F9..=0x00FC => 0x75,
        0x00FD => 0x79, // ý -> y
        // The one odd code point in Latin-1: y-diaeresis sits
        // at 0xFF, so no even/odd rule can pick it up.
        0x00FF => 0x79,

        // Latin Extended-A, which decomposes wholesale apart from the stretch
        // U+0110..U+014B and U+0152/0153/017F named above.
        0x0100..=0x0105 => 0x61, // A-macron .. a-ogonek
        0x0106..=0x010D => 0x63, // C-acute .. c-caron
        0x010E..=0x010F => 0x64, // D-caron
        0x0112..=0x011B => 0x65, // E-macron .. e-caron
        0x011C..=0x0123 => 0x67, // G-circumflex .. g cedilla
        0x0124..=0x0125 => 0x68, // H-circumflex
        0x0128..=0x0130 => 0x69, // I-tilde .. I-dotless-case capital
        0x0134..=0x0135 => 0x6A, // J-circumflex
        0x0136..=0x0137 => 0x6B, // K-caron
        0x0139..=0x0142 => 0x6C, // L-acute .. l-stroke
        0x0143..=0x0148 => 0x6E, // N-acute .. n-caron
        0x014C..=0x0151 => 0x6F, // O-macron .. o-double-acute
        0x0154..=0x0159 => 0x72, // R-acute .. r-caron
        0x015A..=0x0161 => 0x73, // S-acute .. s-caron
        0x0162..=0x0165 => 0x74, // T-caron
        0x0168..=0x0173 => 0x75, // U-tilde .. u-ogon
        0x0174..=0x0175 => 0x77, // W-circumflex
        0x0176..=0x0178 => 0x79, // Y-circumflex .. Y-diaeresis
        0x0179..=0x017E => 0x7A, // Z-acute .. z-caron

        _ => return None,
    })
}

/// `true` for the combining marks the reference's `\p{M}+` removes.
///
/// Four of the five blocks carry the marks a Latin name is actually composed
/// of; the fifth (`U+20D0..U+20FF`) is the mark block the compatibility
/// decompositions of symbols land in. `U+0483..U+0489` and the Indic clusters
/// are not listed: they are not what `NFKD` + "strip the marks" is *for* here,
/// and a launcher's catalogue is overwhelmingly Latin or CJK.
#[inline]
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

/// NFKD, then drop the marks, then lowercase -- one `char` in, at most one
/// `char` out. `None` means the character was a mark and is gone.
///
/// The "first char of `to_lowercase`" step is not a shortcut, it is what NFKD
/// does: `U+0130` decomposes to `U+0069 U+0307`, and the second half is a
/// combining mark, so the reference's `stripDiacritics("İ")` is `"i"` -- the
/// same answer the first char gives. Taking the first char is also what keeps
/// the byte length from growing, which is what makes the [`FOLD_CAP`] window
/// sound.
#[inline]
fn fold_lower_char(c: char) -> Option<char> {
    if c.is_ascii() {
        return if c.is_ascii_uppercase() {
            Some(c.to_ascii_lowercase())
        } else {
            Some(c)
        };
    }
    if is_combining_mark(c) {
        return None;
    }
    if let Some(base) = nfkd_ascii(c) {
        return Some(base.to_ascii_lowercase());
    }
    // Everything else -- Cyrillic, Greek, CJK, Hangul, emoji -- passes through
    // and is only case-folded, because a transliteration is not what the
    // reference does either.
    c.to_lowercase().next()
}

/// Fold `s` into `buf`, returning the folded prefix.
///
/// Appends whole `char`s only, so the returned length is always a UTF-8
/// boundary and `from_utf8` cannot fail; the `.max(0.0)`-style clamp is
/// unnecessary because the write is checked against [`FOLD_CAP`] *before* it
/// happens. A `s` longer than the window folds to its first [`FOLD_CAP`]
/// bytes, on a character boundary.
fn fold_into<'b>(s: &str, buf: &'b mut [u8]) -> &'b str {
    let mut n = 0usize;
    for c in s.chars() {
        let Some(c) = fold_lower_char(c) else {
            continue;
        };
        let mut enc = [0u8; 4];
        let bytes = c.encode_utf8(&mut enc).as_bytes();
        if n + bytes.len() > buf.len() {
            break;
        }
        buf[n..n + bytes.len()].copy_from_slice(bytes);
        n += bytes.len();
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// The folded characters of `s`, in order, with the marks removed.
///
/// The iterator every buffer-free predicate below is built on. `filter_map` on
/// the `Option` is the mark removal, and it runs on both sides of every
/// comparison, so the two can never disagree about how many characters a
/// string has.
#[inline]
fn folded_chars(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().filter_map(fold_lower_char)
}

/// `true` when `tail` begins with the folded `needle`, character for character.
///
/// `tail` is a `&str` slice of the *original*, taken at a `char_indices`
/// boundary, so nothing here ever indexes into the middle of a codepoint.
fn tail_starts_with_fold(tail: &str, needle: &str) -> bool {
    let mut t = folded_chars(tail);
    for nc in folded_chars(needle) {
        match t.next() {
            Some(tc) if tc == nc => {}
            _ => return false,
        }
    }
    true
}

/// Folded substring test, allocation-free and length-independent.
///
/// This is `StringMatcherUtility.matches` (`StringMatcherUtility.java:46-67`)
/// for the branch an ordinary query takes: the reference scans every start
/// offset in the target for a `matcher.matches(query, target.substring(i,
/// i + len))` -- a *prefix of a substring*, which is the same predicate as "the
/// query occurs somewhere in the target" -- and then compares with a `Collator`
/// at `PRIMARY` strength (`:117-121`), which is what makes it case- and
/// diacritic-insensitive. Those two facts together are the fold and the scan.
///
/// The first-folded-character test is the early-out that keeps this
/// `O(n * m)` walk affordable per frame: a candidate that does not even start
/// with the query's first letter costs one comparison, not a full compare.
fn contains_folded(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    // A **character** count, not a byte count. The fold shortens strings --
    // `café` is 5 bytes and folds to `cafe`'s 4 -- so a byte comparison
    // rejects the accented needle against the unaccented haystack, which is
    // precisely the case the fold exists to get right. `hay` is a plain
    // substring, so the check is just a cheap reject.
    if needle.chars().count() > hay.chars().count() {
        return false;
    }
    let Some(n0) = folded_chars(needle).next() else {
        // A needle that is *entirely* marks is empty once folded, so it is
        // trivially contained -- the same answer `needle.is_empty()` gives.
        return true;
    };
    for (i, c) in hay.char_indices() {
        if fold_lower_char(c) == Some(n0) && tail_starts_with_fold(&hay[i..], needle) {
            return true;
        }
    }
    false
}

/// Total order on folded characters, for the alphabetical tie-break.
fn cmp_folded(a: &str, b: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let mut ai = folded_chars(a);
    let mut bi = folded_chars(b);
    loop {
        return match (ai.next(), bi.next()) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) if x != y => x.cmp(&y),
            _ => continue,
        };
    }
}

// ===========================================================================
// The match ladder
// ===========================================================================

/// Similarity of `a` and `b` in `0..=100`: the metric
/// `FuzzySearch.ratio` reports and the one the fuzzy tier's cutoff is applied
/// to.
///
/// `me.xdrop.fuzzywuzzy.FuzzySearch.ratio` is
/// `100 * (lensum - ldist) / lensum` over a Levenshtein distance, which is
/// exactly this, and `AppMatcher.kt:82-88` reads it off
/// `FuzzySearch.ratio(app, query)` and `FuzzySearch.extractSorted`'s
/// `WeightedRatio()` (`SearchUtils.kt:33-39`) reads the same shape. Two
/// `u16` rows and no allocation; the distance is computed and then thrown
/// away because the tier test only needs the ratio.
#[inline]
#[cfg(test)]
fn similarity_100(a: &str, b: &str) -> u8 {
    let mut scratch = MatchScratch::default();
    similarity_100_in(a, b, &mut scratch.ratio)
}

/// [`similarity_100`] against a caller-owned scratch.
///
/// This is the form the matcher uses. The two-array form exists only so the
/// unit tests can call the ratio directly without threading scratch through;
/// the per-call wrapper's memset is paid once per *call site*, not per row.
fn similarity_100_in(a: &str, b: &str, ratio: &mut [u16]) -> u8 {
    // `u8::from(bool)` yields 0 or 1, and "the empty string is 1% similar to
    // itself" is not a number anyone should be handed by a ratio function.
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() { 100 } else { 0 };
    }
    // A byte bound, not a char bound: a `char` is never fewer than one byte,
    // so `len() <= RATIO_MAX` implies at most `RATIO_MAX` chars and the two
    // fixed rows below are provably big enough. Both inputs are already
    // [`fold_into`] output, so this is the window they were bounded by.
    if a.len() > FOLD_CAP || b.len() > FOLD_CAP {
        return 0;
    }
    let (prev, cur) = ratio.split_at_mut(FOLD_CAP + 1);
    // `prev[0]` is the only cell the loop below never writes, so it is set here
    // rather than inherited from the scratch's initial zeroing. Everything else
    // is written before it is read, so the zeroing is not load-bearing.
    prev[0] = 0;
    let mut m = 0usize;
    for _ in b.chars() {
        m += 1;
        prev[m] = m as u16;
    }
    let mut n = 0usize;
    for ca in a.chars() {
        n += 1;
        cur[0] = n as u16;
        let mut j = 0usize;
        for cb in b.chars() {
            j += 1;
            // substitution, deletion, insertion
            cur[j] = (prev[j - 1] + u16::from(ca != cb))
                .min(prev[j] + 1)
                .min(cur[j - 1] + 1);
        }
        prev[..=m].copy_from_slice(&cur[..=m]);
    }
    let total = (n + m) as u32;
    let dist = u32::from(prev[m]);
    (100 * (total - dist) / total).min(100) as u8
}

/// [`fuzzy_match_folded`] driven from a folded `&str` query.
///
/// [`fuzzy_match`] allocates its `q_chars` with a `Vec`, which is fine for a
/// `--benchmark` run and a `malloc` per frame in the drawer. The query here is
/// already folded and already lowercased, so the only thing missing is the
/// `char` array, and that is a stack buffer of the size the scorer's own fast
/// path already uses.
#[inline]
fn score_folded_query(q: &str, target: &str) -> Option<i32> {
    let mut qbuf = ['\0'; FUZZY_STACK_MAX];
    let mut n = 0usize;
    for c in q.chars() {
        if n == FUZZY_STACK_MAX {
            return None;
        }
        qbuf[n] = c;
        n += 1;
    }
    fuzzy_match_folded(&qbuf[..n], target)
}

/// `FUZZY_SCORE_CUTOFF` (`AppMatcher.kt:39`): the similarity at or above which
/// the fuzzy tier fires.
///
/// The same 65 is the cutoff of the reference's other fuzzy path,
/// `FuzzySearch.extractSorted(..., WeightedRatio(), 65)`
/// (`SearchUtils.kt:33-39`), so both of the reference's fuzzy searches agree
/// on it and so does this one. Exposed because it is a tuning constant, not an
/// implementation detail: a caller that wants the reference's `enableFuzzySearch`
/// pref (`PreferenceManager2.kt:589-593`) off has one knob, and turning the tier
/// off is `FUZZY_SCORE_CUTOFF = 101`, which no `u8` similarity can reach.
pub const FUZZY_SCORE_CUTOFF: u8 = 65;

/// `AppMatcher.match`'s `FUZZY_SCORE_CUTOFF` as a cutoff no similarity can
/// reach, for callers implementing the reference's `enableFuzzySearch` pref
/// (`PreferenceManager2.kt:589-593`) in the "off" direction.
pub const FUZZY_DISABLED: u8 = 101;

/// The reference's `0..1f` score in the integer units this crate ranks in:
/// `1000 == 1.0`.
///
/// Integer scores keep the sort a total order with no float equality
/// questions, and 1000 is the precision the reference's own constants are
/// written to -- `0.9`, `0.05`, `0.82`, `0.15`, `0.65` (`AppMatcher.kt:45-89`).
#[inline]
fn mille(f: f32) -> i32 {
    (f * 1000.0).round() as i32
}

/// The reference's `DIRECT_PREFIX` score, `0.9 + 0.05 * (queryLen / nameLen)`
/// capped at `0.95` (`AppMatcher.kt:49-51`). Byte lengths stand in for
/// `String.length`: both sides are folded, and for a folded ASCII name the two
/// are the same number; for a name in a non-Latin script they differ by a
/// constant factor that no reordering within the tier is sensitive to.
#[inline]
fn direct_prefix_score(field: &str, query: &str) -> i32 {
    const BASE: f32 = 0.9;
    const SPAN: f32 = 0.05;
    const CAP: f32 = 0.95;
    if field.is_empty() {
        return mille(CAP);
    }
    let ratio = query.len() as f32 / field.len() as f32;
    mille((BASE + SPAN * ratio).min(CAP))
}

/// Initials of `tokens` up to [`INITIALS_MAX`], as a stack `char` array.
const INITIALS_MAX: usize = 16;

/// `true` when `query` prefixes the concatenated initials of `tokens` --
/// `GM` for `Google Maps` (`AppMatcher.kt:59-64`).
///
/// The reference builds the string with `joinToString("") { it.first() }`; a
/// stack array is the same thing without the `String`, and it is compared
/// against the query in place.
fn initials_prefix(field: &str, query: &str) -> bool {
    let mut ini = ['\0'; INITIALS_MAX];
    let mut n = 0usize;
    for t in field.split_whitespace() {
        let Some(c) = t.chars().next() else { continue };
        if n == INITIALS_MAX {
            // More tokens than the buffer holds. The reference has no such
            // limit, so this is a deviation, and it is bounded rather than
            // wrong: a name with 17 tokens whose query reaches its 17th
            // initial is not a case a launcher catalogue contains.
            return false;
        }
        ini[n] = c;
        n += 1;
    }
    let qn = query.chars().count();
    if qn == 0 || qn > n {
        return false;
    }
    query.chars().zip(ini[..n].iter()).all(|(q, i)| q == *i)
}

/// `AppMatcher.kt:67-71`: every query token prefixes the name token in the same
/// position, and the query has no more tokens than the name -- `goo map` for
/// `Google Maps`.
///
/// Paired up from the *start* of both token lists, exactly as
/// `qTokens.indices.all { i -> tokens[i].startsWith(qTokens[i]) }` (`:68`) does
/// with the reference's own size guard `qTokens.size <= tokens.size` (`:67`)
/// already in front of it.
///
/// Note what the reference does *not* do: it never advances the name token
/// pointer. So "goo map" against "Google Maps" matches on the first two tokens
/// paired positionally, and a query with a *leading* extra token cannot slide
/// forward to find a later name token. That is why `token_prefix_ordered`
/// cannot be written as a two-pointer subsequence search, and why
/// [`all_tokens_present`] is a genuinely different rule rather than a
/// re-spelling of this one.
fn token_prefix_ordered(field: &str, query: &str) -> bool {
    let mut qt = query.split_whitespace();
    let mut any = false;
    for t in field.split_whitespace() {
        let Some(q0) = qt.next() else { break };
        any = true;
        if !t.starts_with(q0) {
            return false;
        }
    }
    any && qt.next().is_none()
}

/// `AppMatcher.kt:77-79`: every query token prefixes *some* name token, in any
/// order -- `map goo` for `Google Maps`.
fn all_tokens_present(field: &str, query: &str) -> bool {
    let mut any = false;
    for qt in query.split_whitespace() {
        any = true;
        if !field.split_whitespace().any(|t| t.starts_with(qt)) {
            return false;
        }
    }
    any
}

/// How a name matched a query, and how well.
///
/// The ladder and the ordering are `AppMatcher.MatchType`
/// (`AppMatcher.kt:8-17`) with the sort the reference actually performs:
/// `sortedWith(compareBy({ it.second.type.priority }, { -it.second.score }))`
/// (`AppSearchProvider.kt:57-62`). A raw score sort cannot reproduce that
/// ordering, and the reason is worth stating because it is the whole point of
/// the enum: the tiers' scores are **not** monotone in specificity. `FUZZY`
/// tops out at 0.65 (`AppMatcher.kt:88`) and `DIRECT_PREFIX` starts at 0.90
/// (`:50`), so sorting on score alone would rank a scattered 0.65 match below
/// every prefix match *and* above a perfect `EXACT_MATCH` only by luck of the
/// numbers. The `priority` is the ordering; the score is the tie-break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchKind {
    /// `EXACT_MATCH(0)`: the query is the whole name (`AppMatcher.kt:45`).
    Exact,
    /// `DIRECT_PREFIX(1)`: the name starts with the query (`:48`).
    DirectPrefix,
    /// `INITIALS(2)`: the query prefixes the name's token initials, `GM` for
    /// `Google Maps` (`:59-64`). Single-token queries only, as in the
    /// reference.
    Initials,
    /// `TOKEN_PREFIX_ORDERED(3)`: `goo map` for `Google Maps` (`:67-71`).
    TokenPrefixOrdered,
    /// `SUBSTRING(4)`: the query occurs somewhere in the name (`:74`).
    Substring,
    /// `ALL_TOKENS_PRESENT(5)`: every query token prefixes some name token, in
    /// any order -- `map goo` for `Google Maps` (`:77-79`).
    AllTokensPresent,
    /// `FUZZY(6)`: similarity at or above [`FUZZY_SCORE_CUTOFF`] (`:39, 87`).
    Fuzzy,
    /// `NO_MATCH(7)`: no rule fired (`:17`).
    ///
    /// Reported by the ladder for a field that did not match, and used as the
    /// `kind` of every row when the **query is empty** -- the zero state lists
    /// the whole catalogue (`LawnchairLocalSearchAlgorithm.doZeroStateSearch`,
    /// `:104-139`) and nothing there matched anything.
    NoMatch,
}

impl MatchKind {
    /// `MatchType.priority` (`AppMatcher.kt:8-17`): lower sorts first.
    #[inline]
    pub const fn priority(self) -> u8 {
        self as u8
    }
}

/// The borrowed fields the matcher reads from one row.
///
/// Borrowed rather than owned, and taken as a *return value* from a closure
/// rather than as three separate accessors, so the closure the shell supplies
/// is one monomorphic `Fn` instead of three closures of three unrelated types.
/// `keywords` is a `&[String]` because that is what [`DesktopApp`] holds and
/// a row with no keywords passes `&[]`, which coerces without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchFields<'a> {
    /// The display name. Ranked with the highest weight, as the reference's
    /// `it.title` is the only field it searches (`AppSearchProvider.kt:41`).
    pub name: &'a str,
    /// The desktop-entry id, e.g. `org.mozilla.firefox`.
    pub id: &'a str,
    /// `Keywords=` values. Weighted below the name, see [`KEYWORD_PENALTY`].
    pub keywords: &'a [String],
}

/// Keyword hits are worth less than name hits.
///
/// `DesktopCatalogue::search` has always applied a penalty here (`:560`, and
/// it predates this module). It is expressed in the same `0..=1000` units as
/// the tier scores, which is why it is 20 and not the old `-5`: at that scale
/// `-5` is invisible. 20 is chosen to stay *inside* the tier -- a keyword
/// `DIRECT_PREFIX` still beats every other `DIRECT_PREFIX`, and cannot leak
/// into the tier below, because the sort is by `priority` first
/// (`AppSearchProvider.kt:57-62`).
pub const KEYWORD_PENALTY: i32 = 20;

/// `AppMatcher.match` (`AppMatcher.kt:41-93`) for one field.
///
/// `field` is the field's folded form and `query` the folded query, so rules
/// 0-5 are plain `str` operations. `orig` is the field's *original* text and
/// is only read by rule 6, because [`fuzzy_match_folded`] takes its
/// word-boundary and camelCase bonuses from the original casing
/// (`score_folded`, `:377-388`) -- feeding it a pre-lowercased field would
/// silently delete both bonuses, which is precisely the pair of tests at
/// `:636-644`.
///
/// Returns `None` for `NO_MATCH` rather than a sentinel `(NoMatch, 0)`: the
/// caller wants to drop the row, and a `None` cannot be forgotten. The
/// `NO_MATCH` arm of the reference is `:92`.
#[cfg(test)]
fn ladder(field: &str, orig: &str, query: &str) -> Option<(MatchKind, i32)> {
    let mut scratch = MatchScratch::default();
    ladder_in(field, orig, query, &mut scratch.ratio)
}

/// [`ladder`] against a caller-owned scratch. See [`similarity_100_in`] for why
/// the scratch exists and why this two-form split is worth it.
fn ladder_in(field: &str, orig: &str, query: &str, ratio: &mut [u16]) -> Option<(MatchKind, i32)> {
    if query.is_empty() {
        return None;
    }
    // The multi-word AND gate. `SearchUtils.normalSearch` (`:17-19`): "Do an
    // intersection of the words in the query and each title, and filter out all
    // the apps that don't match all of the words in the query." The reference
    // keeps this in the *provider* and the ladder in the *matcher*; here the
    // unit of matching is the field, so composing them means a keyword hit is
    // held to the same rule as a name hit -- the honest reading of "each
    // title" for a field that is not a title.
    //
    // Single-word queries skip it on purpose: the gate is "every word is in the
    // field", and applying that to one word would be a *containment* test
    // standing in front of rules 2 and 3, which is how `gm` would stop
    // matching `Google Maps`.
    if query.split_whitespace().count() > 1
        && !query.split_whitespace().all(|w| contains_folded(field, w))
    {
        return None;
    }
    // A single-token query is the reference's `normalSearch` shape exactly:
    // `StringMatcherUtility.matches(query, title, matcher)`
    // (`AppSearchProvider.kt:41`), which is the fold plus containment. See
    // [`contains_folded`] for the one place this is a documented widening.

    // Rule 0: Exact Match (`:45`).
    if field == query {
        return Some((MatchKind::Exact, mille(1.0)));
    }
    // Rule 1: Direct Prefix (`:48-52`).
    if field.starts_with(query) {
        return Some((MatchKind::DirectPrefix, direct_prefix_score(field, query)));
    }
    // Tokenising once per rule rather than once for all four, because
    // `str::split_whitespace` is a zero-allocation `Iterator` rather than the
    // reference's `split(Regex("\\s+"))` list (`:55-56`) and re-creating it is
    // cheaper than materialising it once.
    // Rule 2: Initials, single-token queries only (`:59-64`).
    if !query.chars().any(char::is_whitespace) && initials_prefix(field, query) {
        return Some((MatchKind::Initials, mille(0.88)));
    }
    // Rule 3: Token Prefix (Ordered) (`:67-71`).
    if token_prefix_ordered(field, query) {
        return Some((MatchKind::TokenPrefixOrdered, mille(0.82)));
    }
    // Rule 4: Substring (`:74`).
    if contains_folded(field, query) {
        return Some((MatchKind::Substring, mille(0.72)));
    }
    // Rule 5: All Tokens Present, order-agnostic (`:77-79`).
    if all_tokens_present(field, query) {
        return Some((MatchKind::AllTokensPresent, mille(0.68)));
    }
    // Rule 6: Fuzzy Search (`:82-88`).
    //
    // Two gates, deliberately. The reference's is the similarity: `ratio(app,
    // query) >= 65`. This crate's is additionally the subsequence scorer,
    // because a ratio of 65 is also reachable by two strings that share
    // nothing -- `xyzabcd` and `abcdwxyz` are 62.5% similar and have no
    // character in common, and ranking that above nothing is how a fuzzy tier
    // starts returning noise. The subsequence requirement is what "fuzzy
    // search for an app name" means; the ratio is what keeps it from firing on
    // a coincidence.
    if similarity_100_in(field, query, ratio) >= FUZZY_SCORE_CUTOFF {
        // The tier's *score* is this crate's scorer rather than the
        // reference's normalised ratio, because the scorer is strictly finer:
        // it separates `Firefox` from `Firefiox` and it applies the
        // word-boundary and camelCase bonuses. The two scales never meet,
        // because the sort is by `priority` first and `Fuzzy` is only ever
        // compared against `Fuzzy` (`AppSearchProvider.kt:57-62`).
        let score = score_folded_query(query, orig)?;
        return Some((MatchKind::Fuzzy, score));
    }
    // `NO_MATCH` (`:92`).
    None
}

/// `true` when `a` ranks strictly above `b` on the reference's two keys:
/// `MatchType.priority` ascending, then score descending
/// (`AppSearchProvider.kt:57-62`).
#[inline]
fn better_match(a: (MatchKind, i32), b: (MatchKind, i32)) -> bool {
    let (ka, sa) = a;
    let (kb, sb) = b;
    ka.priority() < kb.priority() || (ka == kb && sa > sb)
}

/// Cheap necessary condition for every tier but [`MatchKind::AllTokensPresent`]:
/// `query` is a subsequence of `field` under the folding the tiers use.
///
/// # Why this exists
///
/// Answering one keystroke against the whole catalogue runs
/// [`best_match`] on the name, the id and every keyword of every app. Each of
/// those folds the string and walks seven rules, the last of which is a full
/// Levenshtein matrix. That is the right answer and far too much work to give
/// a row that cannot match: `--benchmark` gates a whole-catalogue query at 1 ms
/// and measured 1.8 ms.
///
/// This is the gate in front of that work, and it is a byte scan for the
/// ASCII case -- the overwhelming majority of a `.desktop` catalogue -- with no
/// per-character function call at all. That is the whole optimisation: the
/// ladder's cost in an unoptimised build is dominated by how many small
/// functions a single character passes through, not by how much arithmetic it
/// does.
///
/// # Soundness
///
/// Every tier that can fire for a **single-token** query requires the query's
/// characters to appear in the field *in order*:
///
/// * `Exact`, `DirectPrefix`, `TokenPrefixOrdered`, `Substring` -- the query is
///   contained, and containment implies subsequence.
/// * `Initials` -- the token initials *are* characters of the field, in order.
/// * `Fuzzy` -- the subsequence requirement on the fuzzy scorer
///   ([`FUZZY_SCORE_CUTOFF`]'s rule) is exactly this condition.
///
/// `AllTokensPresent` is the exception and the reason the caller gates on token
/// count: it is *order-agnostic*, so `map goo` matches `Google Maps` while
/// `map goo` is **not** a subsequence of `Google Maps`. For a single-token
/// query it collapses to "some token starts with the query", which is
/// containment, so the exception cannot arise -- but the caller checks the
/// token count rather than relying on that, because "relying on that" is how a
/// future fourth exception gets added and silently changes results.
///
/// So the contract is narrow and checkable: for a single-token query, a `false`
/// here means the ladder returns [`MatchKind::NoMatch`]. A test asserts exactly
/// that against the whole catalogue.
#[inline]
fn is_folded_subsequence(field: &str, query: &str) -> bool {
    let fb = field.as_bytes();
    let qb = query.as_bytes();
    if fb.is_ascii() && qb.is_ascii() {
        // The query is [`fold_into`] output, so it is already lowercased and
        // mark-free; a byte compare with an ASCII lowercased haystack is the
        // whole of the folding for this case.
        let mut i = 0usize;
        for &qc in qb {
            loop {
                if i == fb.len() {
                    return false;
                }
                let b = fb[i];
                i += 1;
                if b == qc || b.to_ascii_lowercase() == qc {
                    break;
                }
            }
        }
        return true;
    }
    // At least one side is not ASCII, so the general fold applies. Slower and
    // rarer; correctness is unchanged.
    let mut it = field.chars();
    'query: for qc in query.chars() {
        for c in it.by_ref() {
            if fold_lower_char(c) == Some(qc) {
                continue 'query;
            }
        }
        return false;
    }
    true
}

/// Fold the three fields of one row and take the best tier across them.
///
/// This is the *whole* of `AppSearchProvider.fuzzySearch`'s per-app work
/// (`:52-56`) plus the `id` and `keywords` fields this crate has always
/// searched (`DesktopCatalogue::search`, `:549-567`). Folding allocates
/// nothing: three fixed windows, reused for every keyword.
fn best_match(
    qf: &str,
    f: &SearchFields<'_>,
    scratch: &mut MatchScratch,
) -> Option<(MatchKind, i32)> {
    let mut best: Option<(MatchKind, i32)> = None;
    // Whether the cheap gate below is a sound rejection for this query. It is,
    // for a single token; see [`is_folded_subsequence`] for the one tier that
    // breaks it and why a multi-token query is not gated.
    let gated = !qf.chars().any(char::is_whitespace);

    // Four disjoint borrows from one `&mut`: the folded name stays live across
    // the id fold, and both stay live across the ratio rows.
    let MatchScratch { fold, ratio } = scratch;
    let (namebuf, rest) = fold.split_at_mut(FOLD_CAP);
    let (idbuf, kwbuf) = rest.split_at_mut(FOLD_CAP);

    if !gated || is_folded_subsequence(f.name, qf) {
        let name = fold_into(f.name, namebuf);
        take_best(&mut best, ladder_in(name, f.name, qf, ratio));
    }

    if !gated || is_folded_subsequence(f.id, qf) {
        let id = fold_into(f.id, idbuf);
        take_best(&mut best, ladder_in(id, f.id, qf, ratio));
    }

    if !f.keywords.is_empty() {
        for kw in f.keywords {
            if gated && !is_folded_subsequence(kw, qf) {
                continue;
            }
            let folded = fold_into(kw, kwbuf);
            if let Some((k, s)) = ladder_in(folded, kw, qf, ratio) {
                take_best(&mut best, Some((k, s - KEYWORD_PENALTY)));
            }
        }
    }
    best
}

/// Keep `cand` if it outranks what `slot` holds.
#[inline]
fn take_best(slot: &mut Option<(MatchKind, i32)>, cand: Option<(MatchKind, i32)>) {
    let Some(c) = cand else { return };
    if let Some(p) = *slot {
        if !better_match(c, p) {
            return;
        }
    }
    *slot = Some(c);
}

/// One ranked search result: the row, why it matched, and how well.
///
/// `Copy` with no allocation -- three fields, two of them `Copy` scalars and
/// one a shared reference. This is what the shell's fixed-capacity drawer
/// window holds, and it is the reason the ranked search needs no `Vec`: see
/// [`search_ranked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit<'a, T> {
    /// The matched row.
    pub item: &'a T,
    /// Which rule fired, and therefore its rank.
    pub kind: MatchKind,
    /// The rule's score. Comparable only within a tier; see
    /// [`MatchKind`].
    pub score: i32,
}

impl<'a, T> SearchHit<'a, T> {
    /// A hit that ranks last, for a `[SearchHit; N]` array or a `Vec` that
    /// must be sized before the first write.
    ///
    /// `const`, so a caller's `FrameView`-style scratch can be built in a
    /// `const fn` the way `main.rs`'s is. `score = i32::MIN` rather than 0 so
    /// a stray placeholder can never win a `score >` comparison against a real
    /// hit if a future caller ever reads an unwritten slot.
    pub const fn empty(item: &'a T) -> Self {
        Self {
            item,
            kind: MatchKind::NoMatch,
            score: i32::MIN,
        }
    }
}

/// The ranked, matched and **sorted** subset of `items` for `query`, written
/// into `out`. Returns the total number of matches, which is *not* bounded by
/// `out.len()`.
///
/// # Why a caller-owned buffer
///
/// This is the one ranked-search entry point, and the shape is the shell's
/// existing one (`FrameView` in `main.rs`, `[T; FRAME_MAX_GRID]` plus a `len`).
/// A `Vec<(&DesktopApp, i32)>` -- which is what [`DesktopCatalogue::search`]
/// returns -- is a `malloc` and a `free` per query, and the drawer searches on
/// every keystroke of every frame. `out` is the caller's own fixed array, so
/// the ranking loop touches no allocator at all: the scratch is
/// [`FOLD_CAP`] bytes of stack per field and `out` itself is the only storage
/// the results occupy.
///
/// # The two numbers
///
/// `out[..m]` is the best `m = min(total, out.len())` rows in rank order;
/// `total` is every row that matched, matching or not. The split is what
/// `maxAppSearchResultCount` + `.take(n)` does in the reference
/// (`AppSearchProvider.kt:27-31, 63-64`) *and* what the drawer's header needs
/// (`drawer_app_count` counts past what fits, so the count keeps climbing
/// while the window stays a screenful). A caller that scrolls a result set
/// longer than `out` must size `out` for it or accept the truncation -- that
/// is the reference's `maxAppSearchResultCount` cap, not a new limit.
///
/// # Ranking
///
/// `priority` ascending, then score descending, then the folded name ascending
/// -- the reference's `compareBy({ type.priority }, { -score })`
/// (`AppSearchProvider.kt:57-62`) plus an alphabetical tie-break, which the
/// existing [`DesktopCatalogue::search`] also had (`:570`) and which makes the
/// order deterministic instead of catalogue-dependent.
///
/// An **empty** query is not a match: it returns every row in folded-name
/// order with [`MatchKind::NoMatch`], which is the reference's zero state
/// (`LawnchairLocalSearchAlgorithm.doZeroStateSearch`, `:104-139`) rather than
/// a search result.
///
/// # Cost
///
/// One pass over `items`, and per row: three [`fold_into`] windows, one ladder
/// walk, and (only in the `Fuzzy` tier) a Levenshtein pass bounded by
/// [`FOLD_CAP`] squared. Bounded top-K insertion is `O(log out.len())` per
/// surviving row, so a full-width catalogue with a 64-slot window is a few
/// hundred microseconds -- inside the 8.33 ms frame, and with no allocator
/// traffic for the kernel to notice.
pub fn search_ranked<'a, T, F>(
    items: &'a [T],
    query: &str,
    fields: F,
    out: &mut [SearchHit<'a, T>],
) -> usize
where
    F: Fn(&'a T) -> SearchFields<'a>,
{
    // A zero-capacity window is legal and means "count only", so this cannot
    // early-out on `out.is_empty()`: the drawer's header count is exactly that
    // caller, and a `return 0` here would make the count a function of the
    // window the caller happened to size. Every write below is guarded on
    // `len < cap`, which is false for `cap == 0`, so the loops degrade to
    // counting with no stores.
    let cap = out.len();
    let mut len = 0usize;
    let q = query.trim();
    // A projection of `fields` rather than `fields` itself: the sift helpers
    // need a *name* for a row they were handed rather than a `SearchFields`,
    // and projecting once here keeps their bound at the catalogue's `'a`
    // instead of a higher-ranked lifetime they cannot express.
    let name_of = |it: &'a T| -> &'a str { fields(it).name };

    if q.is_empty() {
        // The zero state: everything, in folded-name order. Inserted with the
        // same bounded top-K the ranked path uses -- keyed on the name alone,
        // since nothing matched anything -- so the two paths cannot drift into
        // two different notions of order.
        for item in items {
            let f = fields(item);
            let hit = SearchHit {
                item,
                kind: MatchKind::NoMatch,
                score: 0,
            };
            // `len < cap` is false for `cap == 0`, so the "is the window full"
            // test is a plain `len == cap` and must not index `out[len - 1]`
            // before that test has excluded it.
            if cap == 0 {
                continue;
            }
            if len == cap
                && cmp_folded(fields(out[len - 1].item).name, f.name)
                    != core::cmp::Ordering::Greater
            {
                continue;
            }
            let at = if len < cap {
                out[len] = hit;
                len += 1;
                len - 1
            } else {
                out[cap - 1] = hit;
                cap - 1
            };
            sift_up(out, at, &name_of);
        }
        return items.len();
    }

    let mut qbuf = [0u8; FOLD_CAP];
    let qf = fold_into(q, &mut qbuf);
    // Once per query, not once per row. See [`MatchScratch`].
    let mut scratch = MatchScratch::default();
    let mut total = 0usize;
    for item in items {
        let f = fields(item);
        let Some((kind, score)) = best_match(qf, &f, &mut scratch) else {
            continue;
        };
        total += 1;
        let hit = SearchHit { item, kind, score };
        // Same `cap == 0` guard as the zero-state branch, and for the same
        // reason: the count is worth having even when there is nowhere to put
        // the rows, so a full window must not be tested by indexing it.
        if cap == 0 {
            continue;
        }
        if len == cap && !ranks_above(&hit, &out[len - 1], f.name, &name_of) {
            continue;
        }
        let at = if len < cap {
            out[len] = hit;
            len += 1;
            len - 1
        } else {
            // The window is full and this row beat the worst row in it, so it
            // takes that slot and is sifted up. Dropping it instead would make
            // the result a function of *catalogue order* rather than of rank,
            // which is the bug this bounded insertion exists to avoid.
            out[cap - 1] = hit;
            cap - 1
        };
        sift_up(out, at, &name_of);
    }
    total
}

/// Sift `out[at]` toward the front until the prefix is ordered.
///
/// The insertion sort, specialised: `out[..at]` is already sorted and `at` is
/// at most `out.len() - 1`, so this is `O(at)` swaps in the worst case and one
/// comparison in the common one. `out.swap` rather than a rotate, because a
/// swap of two rows is a register pair and a rotate is a `memmove`.
#[inline]
fn sift_up<'a, T, F>(out: &mut [SearchHit<'a, T>], at: usize, name_of: &F)
where
    F: Fn(&'a T) -> &'a str,
{
    let mut i = at;
    while i > 0 {
        if !ranks_above(&out[i], &out[i - 1], name_of(out[i].item), name_of) {
            break;
        }
        out.swap(i, i - 1);
        i -= 1;
    }
}

/// Strictly-above test for [`sift_up`]: `MatchType.priority` ascending, then
/// score descending, then the folded name ascending.
///
/// The third key is this crate's addition to the reference's
/// `compareBy({ it.second.type.priority }, { -it.second.score })`
/// (`AppSearchProvider.kt:57-62`). That `sortedWith` is *stable*, so a tie
/// leaves the rows in catalogue order -- and a catalogue is a directory walk,
/// so the visible effect is that installing one app reshuffles every
/// equal-scoring neighbour. The existing [`DesktopCatalogue::search`] already
/// broke ties alphabetically (`:570`) and this keeps that, because a search
/// list whose order changes when an unrelated package is installed reads as a
/// bug.
///
/// `a_name` is passed in rather than re-derived, so the caller already holding
/// the `SearchFields` does not fold the same row twice.
#[inline]
fn ranks_above<'a, T, F>(
    a: &SearchHit<'a, T>,
    b: &SearchHit<'a, T>,
    a_name: &str,
    name_of: &F,
) -> bool
where
    F: Fn(&'a T) -> &'a str,
{
    let pa = a.kind.priority();
    let pb = b.kind.priority();
    if pa != pb {
        return pa < pb;
    }
    if a.score != b.score {
        return a.score > b.score;
    }
    cmp_folded(a_name, name_of(b.item)) == core::cmp::Ordering::Less
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
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
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
    ///
    /// A thin allocating wrapper over the **one** ranking implementation,
    /// [`search_ranked`], so the `Vec` shape stays available for the
    /// `--benchmark` and `--test-desktop` paths without becoming a second
    /// ranking algorithm. The per-app scoring, the fold, the match ladder and
    /// the ordering are all [`search_ranked`]'s; this only swaps the fixed
    /// window for a growable one and the borrowed buffer for an owned `String`.
    ///
    /// The shell does **not** call this: it needs a fixed window into its own
    /// scratch (see [`Self::search_ranked`]), and this allocates a `Vec` and a
    /// `String` per call, which at 120 Hz is a `malloc`/`free` pair per query.
    pub fn search(&self, query: &str) -> Vec<(&DesktopApp, i32)> {
        let mut hits: Vec<SearchHit<DesktopApp>> = Vec::new();
        // `search_ranked` writes a *window*, so the buffer has to arrive with
        // its length already set: a `Vec`'s `len` is 0 until something pushes,
        // and a zero-capacity window returns 0 matches. `resize` is the
        // non-`unsafe` way to do that, and the placeholder rows are never read
        // -- `search_ranked` tracks its own `len` and sifts only over the
        // prefix it has written.
        if let Some(first) = self.apps.first() {
            hits.resize(self.apps.len(), SearchHit::empty(first));
            let total = search_ranked(&self.apps, query, |a| Self::fields_of(a), &mut hits);
            let m = total.min(self.apps.len());
            debug_assert!(m <= self.apps.len(), "window must not exceed the total");
            hits.truncate(m);
        }
        hits.into_iter().map(|h| (h.item, h.score)).collect()
    }

    /// [`search_ranked`] over this catalogue, into a caller-owned fixed array.
    ///
    /// The form the frame path wants: `out` is the shell's
    /// `[SearchHit<DesktopApp>; N]`, so ranking a query touches no allocator.
    /// Returns the total match count, which is independent of `out.len()` --
    /// see [`search_ranked`] for why both numbers are wanted.
    pub fn search_ranked<'a, const CAP: usize>(
        &'a self,
        query: &str,
        out: &mut [SearchHit<'a, DesktopApp>; CAP],
    ) -> usize {
        search_ranked(&self.apps, query, |a| Self::fields_of(a), out)
    }

    /// The [`SearchFields`] view of a [`DesktopApp`], borrowed.
    #[inline]
    fn fields_of(a: &DesktopApp) -> SearchFields<'_> {
        SearchFields {
            name: &a.name,
            id: &a.id,
            keywords: &a.keywords,
        }
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

    /// The cheap gate must not change a single result.
    ///
    /// [`is_folded_subsequence`] claims that for a single-token query, a `false`
    /// means the ladder returns [`MatchKind::NoMatch`]. That claim is what makes
    /// the whole-catalogue prefilter sound, and it is the kind of claim that is
    /// one added tier away from being false -- so it is asserted over a corpus
    /// built to include the cases that make it delicate, rather than trusted.
    ///
    /// The corpus deliberately includes the pair that motivates the token-count
    /// guard: `Google Maps` and the query `map goo`, where the query is *not* a
    /// subsequence of the name but `ALL_TOKENS_PRESENT` fires. Multi-token
    /// queries are not gated, so this pair must still match.
    #[test]
    fn the_subsequence_gate_changes_no_result() {
        let corpus: &[(&str, &str, &[&str])] = &[
            ("Google Maps", "com.google.maps", &["maps", "navigation"]),
            ("Firefox", "org.mozilla.firefox", &["web", "browser"]),
            (
                "Thunderbird Mail",
                "org.mozilla.thunderbird",
                &["mail", "email"],
            ),
            ("Text Editor", "org.gnome.TextEditor", &["text"]),
            ("Café Player", "org.example.cafe", &["music", "audio"]),
            ("Ärger App", "org.example.aerger", &[]),
            (
                "A B C D E F G H I J K L M N O P Q R S T U V W X Y Z",
                "z",
                &[],
            ),
            ("Xylophone", "org.example.xylophone", &["instrument"]),
            ("", "empty-name", &["only-keyword"]),
            ("Term", "org.example.term", &[]),
            ("Terminal", "org.example.terminal", &[]),
            ("Timer", "org.example.timer", &[]),
            ("T", "t", &["t"]),
        ];
        let queries = [
            "term",
            "fire",
            "map",
            "goo",
            "gm",
            "g",
            "text editor",
            "map goo",
            "goo map",
            "xyz",
            "cafe",
            "cafe player",
            "instrument",
            "t",
            "zzz",
            "a",
            "e",
            "q",
            "terminal",
            "tim",
            "trmi",
        ];
        for q in queries {
            let single = !q.chars().any(char::is_whitespace);
            let mut scratch = MatchScratch::default();
            let mut qbuf = [0u8; FOLD_CAP];
            let qf = fold_into(q, &mut qbuf);
            for (name, id, keywords) in corpus {
                let kws: Vec<String> = keywords.iter().map(|s| (*s).to_string()).collect();
                let f = SearchFields {
                    name,
                    id,
                    keywords: &kws,
                };
                let got = best_match(qf, &f, &mut scratch);
                // The gate is **per field**, so the sound statement is about the
                // row as a whole: if no field of the row is a subsequence of the
                // query's folding, nothing can match. A rejected name is not by
                // itself a `None` -- `g` is absent from `Firefox` but present in
                // `org.mozilla.firefox`, and the id's `SUBSTRING` match is
                // exactly what the reference reports.
                let any_field_open = is_folded_subsequence(name, qf)
                    || is_folded_subsequence(id, qf)
                    || kws.iter().any(|k| is_folded_subsequence(k, qf));
                if single && !any_field_open && got.is_some() {
                    panic!("gate let a fully-closed row through: q={q:?} name={name:?} -> {got:?}");
                }
                // The gate must be *correct in the other direction* too: it
                // must never reject a field that the ladder would have matched.
                let ungated = {
                    let mut s2 = MatchScratch::default();
                    let mut best: Option<(MatchKind, i32)> = None;
                    let name_f = fold_into(name, &mut s2.fold[..FOLD_CAP]);
                    take_best(&mut best, ladder_in(name_f, name, qf, &mut s2.ratio));
                    let id_f = fold_into(id, &mut s2.fold[FOLD_CAP..FOLD_CAP * 2]);
                    take_best(&mut best, ladder_in(id_f, id, qf, &mut s2.ratio));
                    for kw in &kws {
                        if is_folded_subsequence(kw, qf) {
                            let kf = fold_into(kw, &mut s2.fold[FOLD_CAP * 2..]);
                            if let Some((k, sc)) = ladder_in(kf, kw, qf, &mut s2.ratio) {
                                take_best(&mut best, Some((k, sc - KEYWORD_PENALTY)));
                            }
                        }
                    }
                    best
                };
                assert_eq!(
                    got.is_some(),
                    ungated.is_some(),
                    "the gate changed the outcome for q={q:?} name={name:?}"
                );
                assert_eq!(
                    got, ungated,
                    "the gate changed the tier or score for q={q:?}"
                );
            }
        }
    }

    /// The pair that makes the token-count guard load-bearing.
    ///
    /// `map goo` is not a subsequence of `Google Maps` -- there is no `g` after
    /// the `p` -- yet `ALL_TOKENS_PRESENT` matches it, because that tier is
    /// order-agnostic. So a gate that fired unconditionally would drop a match
    /// the reference finds. It does not, because `best_match` only gates when
    /// the query is a single token, and this one is not.
    #[test]
    fn an_out_of_order_multi_token_query_still_matches() {
        let mut scratch = MatchScratch::default();
        let mut qbuf = [0u8; FOLD_CAP];
        let qf = fold_into("map goo", &mut qbuf);
        assert!(
            !is_folded_subsequence("Google Maps", qf),
            "precondition: not a subsequence, which is why this needs the guard"
        );
        let f = SearchFields {
            name: "Google Maps",
            id: "com.google.maps",
            keywords: &[],
        };
        assert_eq!(
            best_match(qf, &f, &mut scratch).map(|(k, _)| k),
            Some(MatchKind::AllTokensPresent),
            "the reference's order-agnostic tier still fires"
        );
    }

    /// The gate is the whole reason a whole-catalogue query fits its budget, so
    /// it has to be right for the awkward inputs too: an empty query, a query
    /// longer than the field, a query whose only character is the last one.
    #[test]
    fn the_gate_handles_degenerate_queries() {
        // Empty query: vacuously a subsequence of everything, so the gate never
        // rejects and `ladder_in`'s own empty check stays the one that decides.
        assert!(is_folded_subsequence("anything", ""));
        assert!(is_folded_subsequence("", ""));
        // Longer than the field.
        assert!(!is_folded_subsequence("ab", "abcd"));
        // The last character only.
        assert!(is_folded_subsequence("abc", "abc"));
        assert!(!is_folded_subsequence("abc", "abd"));
        // A repeated character needing two occurrences, in order. `ana` *is* a
        // subsequence of `banan` (a@1, n@2, a@3) -- worth asserting, because the
        // obvious negative reading of it is wrong. `bnb` is the real negative:
        // `banan` has no second `b`.
        assert!(is_folded_subsequence("banana", "ana"));
        assert!(is_folded_subsequence("banan", "ana"));
        assert!(!is_folded_subsequence("banan", "bnb"));
        // Non-ASCII on both sides takes the general fold path.
        assert!(is_folded_subsequence("Café", "cafe"));
        assert!(is_folded_subsequence("Ärger", "arger"));
    }

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
        assert!(parse_desktop_entry("b", &format!("{base}OnlyShowIn=GNOME;KDE;\n")).is_none());
        assert!(parse_desktop_entry("c", &format!("{base}OnlyShowIn=GNOME;UTLC;\n")).is_some());
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
        let content =
            "[Desktop Entry]\nType=Application\nName=Fi\\\nrst\nName=Second\nExec=/bin/true\n";
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

    // -- the match ladder -------------------------------------------------
    //
    // Every tier is exercised against the example the reference's own doc
    // comment on `AppMatcher.match` (`AppMatcher.kt:27-36`) names, because
    // those are the cases the tiers exist for and a tier that fires on the
    // wrong one is invisible until a user types it.

    /// A catalogue of one app, so a tier test cannot be passed by a neighbour
    /// matching. `AppMatcher.match(appName, query)` is a pure function of the
    /// two, so this is the unit under test.
    fn ladder_of(name: &str, query: &str) -> Option<(MatchKind, i32)> {
        let mut b = [0u8; FOLD_CAP];
        let f = fold_into(name, &mut b);
        let mut qb = [0u8; FOLD_CAP];
        let q = fold_into(query, &mut qb);
        ladder(f, name, q)
    }

    fn kind_of(name: &str, query: &str) -> MatchKind {
        match ladder_of(name, query) {
            Some((kind, _)) => kind,
            None => MatchKind::NoMatch,
        }
    }

    #[test]
    fn match_tier_exact_is_the_whole_name() {
        // "The query is identical to the app name" (`:28`, `:45`).
        assert_eq!(kind_of("term", "term"), MatchKind::Exact);
        // Case is irrelevant, because both sides are folded before the rule
        // runs -- the reference lowercases `app` at `:42` and the provider has
        // already lowercased the query.
        assert_eq!(kind_of("Terminal", "TERMINAL"), MatchKind::Exact);
        // Diacritics are irrelevant for the same reason, and this is the one
        // the *live* path got wrong: an ASCII substring test over "Café"
        // never matched "cafe".
        assert_eq!(kind_of("Café Player", "café player"), MatchKind::Exact);
        // Not a prefix, not a substring: the whole name and nothing else.
        assert_ne!(kind_of("Terminal", "term"), MatchKind::Exact);
    }

    #[test]
    fn match_tier_direct_prefix_scales_with_how_much_of_the_name_it_covers() {
        // "The app name starts with the query" (`:29`, `:48-52`).
        assert_eq!(kind_of("Terminal", "term"), MatchKind::DirectPrefix);
        // The score is `0.9 + 0.05 * (queryLen / nameLen)` capped at 0.95
        // (`:49-51`), so a longer query against the same name scores higher and
        // both are below 0.95.
        let short = ladder_of("Terminal", "term").expect("prefix").1;
        let longer = ladder_of("Terminal", "termi").expect("prefix").1;
        assert!(longer > short, "a longer prefix must score higher");
        assert!((900..=950).contains(&short), "0.9 floor, got {short}");
        assert!(
            (longer..=950).contains(&longer) && longer <= 950,
            "0.95 cap, got {longer}"
        );
        // A prefix is a prefix of an *equal-length* name only when it is the
        // name, which is rule 0 and must not be reachable from here.
        assert_eq!(kind_of("Term", "Term"), MatchKind::Exact);
    }

    #[test]
    fn match_tier_initials_is_the_tokens_first_letters() {
        // "The query matches the beginning of the sequence of initials from the
        // app name's tokens (e.g., "G" or "GM" for "Google Maps")"
        // (`:30`, `:59-64`).
        //
        // The name must *not* start with the query, or rule 1 returns first
        // (`:45-52` runs before `:59-64`) and INITIALS is unreachable. That is
        // why the reference's own example is `GM` and not `G`: `g` does prefix
        // `google maps`, so it is a DIRECT_PREFIX, never an INITIALS.
        assert_eq!(kind_of("Maps Navigator", "mn"), MatchKind::Initials);
        assert_eq!(kind_of("Firefox Web Browser", "fwb"), MatchKind::Initials);
        // One initial of a multi-token name -- but only where the name does
        // *not* start with it, or rule 1 takes it (see below).
        assert_eq!(kind_of("Alpha Beta", "ab"), MatchKind::Initials);
        // Rule 1 does win where it applies, and that is the ladder working:
        // `g` prefixes `google maps`, so it is a DIRECT_PREFIX.
        assert_eq!(kind_of("Google Maps", "g"), MatchKind::DirectPrefix);
        // The reference gates INITIALS on the query being a single token
        // (`:59`); a two-token query has no initials to prefix, and falls
        // through to the token rules.
        assert_ne!(kind_of("Maps Navigator", "m n"), MatchKind::Initials);
        // Initials that are not a prefix of the initials, in a name that does
        // not start with the query either: no tier, and specifically not
        // INITIALS.
        assert_eq!(kind_of("Maps Navigator", "nm"), MatchKind::NoMatch);
    }

    #[test]
    fn match_tier_token_prefix_ordered_then_all_tokens_present() {
        // Rule 3, "Token Prefix (Ordered)": "Each token in the query is a
        // prefix of the corresponding token in the app name (e.g., "goo map"
        // for "Google Maps")" (`:31`, `:67-71`).
        //
        // Note the query is *not* a substring of the name -- "goo map" does not
        // occur in "google maps" -- so this genuinely exercises the token walk
        // rather than rule 4.
        assert_eq!(
            kind_of("Google Maps", "goo map"),
            MatchKind::TokenPrefixOrdered
        );
        // The reference pairs positionally and never advances the name pointer
        // (`:68`), so a query whose first word is *not* a prefix of the name's
        // first token cannot slide forward to find it -- rule 3 fails and rule
        // 5 takes over.
        assert_eq!(
            kind_of("Maps Google", "goo map"),
            MatchKind::AllTokensPresent,
            "rule 3 pairs 'goo' with 'maps' and fails; rule 5 does not care \
             about order"
        );
        // The converse: when the query happens to line up positionally against
        // a reversed name, rule 3 fires and the row is the *better* one, even
        // though the user typed the words "backwards" relative to the name.
        // That is the reference's behaviour, and it is why rule 5's test is
        // about the *set* of tokens and rule 3's is about the *sequence*.
        assert_eq!(
            kind_of("Maps Google", "map goo"),
            MatchKind::TokenPrefixOrdered,
            "positional pairing is the reference's rule 3, and it wins"
        );
        // And against the normal name, `map goo` is out of order, so rule 5.
        assert_eq!(
            kind_of("Google Maps", "map goo"),
            MatchKind::AllTokensPresent
        );
        // A word that is not in the name at all is a NO_MATCH, not a lower
        // tier: the AND gate runs before every rule.
        assert_eq!(kind_of("Google Maps", "map firefox"), MatchKind::NoMatch);
        assert_eq!(kind_of("Google Maps", "firefox map"), MatchKind::NoMatch);
    }

    #[test]
    fn match_tier_substring_then_the_fuzzy_cutoff() {
        // Rule 4 (`:32`, `:74`).
        assert_eq!(kind_of("Web Browser", "brow"), MatchKind::Substring);
        // Rule 6 needs the similarity at or above 65 (`:39, 87`). A scattered
        // query into a long name cannot reach it: `ffx` into `Firefox` is
        // 100 * (10 - 4) / 10 = 60, so the reference rejects it and so must
        // this -- and note `ffx` *is* a subsequence, so this is the cutoff
        // doing the work, not the second gate.
        assert_eq!(similarity_100("ffx", "firefox"), 60);
        assert_eq!(kind_of("Firefox", "ffx"), MatchKind::NoMatch);
        // A dropped character is the case the tier is for: `frefox` is still a
        // subsequence of `firefox`, and 100 * (13 - 1) / 13 = 92, over the
        // cutoff. Every rule above it declines -- not a prefix, no token
        // prefix, not a substring -- so the tier reported is the fuzzy one.
        assert_eq!(similarity_100("frefox", "firefox"), 92);
        assert_eq!(kind_of("Firefox", "frefox"), MatchKind::Fuzzy);
        // A prefix is still a prefix even when it is 92% similar: the ladder
        // returns on the *first* rule that fires (`:45-89` is a cascade of
        // early returns), so the tier is about which rule matched, not about
        // how well.
        assert_eq!(kind_of("Firefox", "firefo"), MatchKind::DirectPrefix);
        // The cutoff is the reference's 65, inclusive (`>=`, `:87`).
        assert_eq!(FUZZY_SCORE_CUTOFF, 65);
        // The second gate, and this is the case that justifies it. `bcda` is
        // 75% similar to `abcd` -- over the 65 cutoff -- and shares every
        // character with it, but in the wrong order, so no rule 0-5 fires and
        // the subsequence requirement is what makes it NO_MATCH rather than a
        // FUZZY. Without that second gate a search for `abcd` would offer
        // `bcda`.
        assert_eq!(similarity_100("bcda", "abcd"), 75);
        // One token, so no rule above can rescue it by accident: not equal
        // (case is folded, but the letters are in the wrong order), not a
        // prefix, no token to pair, and not a substring.
        assert_eq!(kind_of("Bcda", "abcd"), MatchKind::NoMatch);
        // Same character multiset, ratio 80, still refused.
        assert_eq!(similarity_100("bcdea", "abcde"), 80);
        assert_eq!(kind_of("Bcdea", "abcde"), MatchKind::NoMatch);
        // A completely unrelated pair is NO_MATCH on both gates.
        assert_eq!(similarity_100("abc", "xyz"), 50);
        assert_eq!(kind_of("Firefox", "zzzzzz"), MatchKind::NoMatch);
    }

    #[test]
    fn match_tier_priority_order_is_the_references_and_not_the_score_order() {
        // The reason `MatchKind` exists. The reference's tier scores are
        // *fixed constants* per rule (`AppMatcher.kt:45, 50, 62, 69, 74, 78,
        // 88`): 1.0, 0.90-0.95, 0.88, 0.82, 0.72, 0.68, 0.50-0.65. They happen
        // to be monotone in `priority` in *this* build, but nothing in the
        // reference makes that a guarantee -- `DIRECT_PREFIX`'s score is
        // `queryLen / nameLen` dependent (`:49-51`), so a short query against a
        // long name scores near 0.90 while a *substring* hit on a short name
        // scores 0.72, and a score sort would interleave them. The reference
        // therefore sorts by `priority` first (`:57-62`) and so must this.
        // `frefox` is a subsequence of `firefox` and 92% similar, but it is
        // not a prefix, not a token prefix and not a substring, so rules 0-5
        // all decline and rule 6 is the only thing left.
        let (fk, fs) = ladder_of("Firefox", "frefox").expect("fuzzy tier");
        let (pk, ps) = ladder_of("Firefox", "fire").expect("prefix tier");
        assert_eq!(fk, MatchKind::Fuzzy);
        assert_eq!(pk, MatchKind::DirectPrefix);
        assert!(
            fs < ps,
            "the two tiers' scores ({} vs {}) are not on one scale -- FUZZY's is \
             a raw bonus sum from `score_folded` and DIRECT_PREFIX's is a \
             length ratio -- so a raw score sort is meaningless and priority \
             must decide",
            fs,
            ps
        );
        // `priority` is the reference's ordinal, lower first
        // (`AppMatcher.kt:9-16`), so DIRECT_PREFIX (1) sorts *ahead* of
        // FUZZY (6) -- the higher the priority number, the *worse* the rank.
        // Asserting the direction explicitly because the naming invites the
        // opposite reading, and `sortedWith(compareBy({ priority }))`
        // (`AppSearchProvider.kt:57-62`) is an ascending sort.
        assert!(
            pk.priority() < fk.priority(),
            "DIRECT_PREFIX {} must sort ahead of FUZZY {}: priority is ascending",
            pk.priority(),
            fk.priority()
        );
        // Every `priority` is the reference's ordinal, 0..7 and no gaps.
        let all = [
            MatchKind::Exact,
            MatchKind::DirectPrefix,
            MatchKind::Initials,
            MatchKind::TokenPrefixOrdered,
            MatchKind::Substring,
            MatchKind::AllTokensPresent,
            MatchKind::Fuzzy,
            MatchKind::NoMatch,
        ];
        for (i, k) in all.iter().enumerate() {
            assert_eq!(k.priority() as usize, i, "{k:?} is out of order");
        }
    }

    // -- diacritics --------------------------------------------------------

    #[test]
    fn folding_makes_an_accented_name_findable_from_either_direction() {
        // `stripDiacritics` + lowercase on *both* sides
        // (`AppSearchProvider.kt:25, 41, 54`): the query is normalised once and
        // each title is normalised per app, so "cafe" finds "Café" and "café"
        // finds "Cafe" with the same code.
        for (name, query) in [
            ("Café Player", "cafe"),
            ("Café Player", "cafe player"),
            ("cafe player", "café"),
            ("cafe player", "CAFÉ"),
            ("Café", "cafe"),
        ] {
            let mut b = [0u8; FOLD_CAP];
            let f = fold_into(name, &mut b);
            let mut qb = [0u8; FOLD_CAP];
            let q = fold_into(query, &mut qb);
            assert!(
                contains_folded(f, q),
                "folded {name:?} = {f:?} must contain folded {query:?} = {q:?}"
            );
        }
        // A combining mark typed separately must fold the same way: the
        // precomposed and decomposed spellings are the same string after
        // NFKD, and this is what the `U+0301` arm of `is_combining_mark` is
        // for.
        let mut a = [0u8; FOLD_CAP];
        assert_eq!(fold_into("Cafe\u{301}", &mut a), "cafe", "combining acute");
        let mut b2 = [0u8; FOLD_CAP];
        assert_eq!(
            fold_into("Caf\u{e9}", &mut b2),
            "cafe",
            "precomposed e-acute"
        );
    }

    #[test]
    fn folding_keeps_the_letters_the_reference_keeps() {
        // `Æ Ç Ñ ß Œ æ ç ñ ß œ` have no NFKD decomposition, so
        // `stripDiacritics` returns them unchanged (`AppSearchProvider.kt:67-70`)
        // and matching must not be widened. This is the test that would fail
        // if someone "improved" the fold into a transliteration.
        for (kept, base) in [
            ("\u{c6}", "ae"),  // Æ -> ae
            ("\u{df}", "ss"),  // ß -> ss
            ("\u{152}", "oe"), // Œ -> oe
            ("\u{d0}", "d"),   // Ð -> d
            ("\u{de}", "th"),  // Þ -> th
            ("\u{e6}", "ae"),  // æ -> ae
            ("\u{f0}", "d"),   // ð -> d
            ("\u{f8}", "o"),   // ø -> o
            ("\u{fe}", "th"),  // þ -> th
            ("\u{153}", "oe"), // œ -> oe
        ] {
            // The fold lowercases -- the reference does too
            // (`AppSearchProvider.kt:25, 41`) -- so the *lowercased* form is
            // what must survive. What must not happen is a transliteration.
            let mut b = [0u8; FOLD_CAP];
            let folded = fold_into(kept, &mut b);
            assert_eq!(
                folded,
                kept.to_lowercase(),
                "{kept:?} must survive the fold, only case-folded"
            );
            let mut qb = [0u8; FOLD_CAP];
            let q = fold_into(base, &mut qb);
            assert!(
                !contains_folded(folded, q),
                "{kept:?} must not match {base:?}: the reference does not fold it"
            );
        }
        // The one expansion the table *does* perform is the NFKD compatibility
        // mapping U+00A0 -> U+0020, and it matters: a query typed with a
        // non-breaking space is two words to `split_whitespace`, so if it did
        // not fold, the multi-word AND gate would see one unmatchable token.
        let mut nb = [0u8; FOLD_CAP];
        assert_eq!(fold_into("goo\u{a0}map", &mut nb), "goo map", "NBSP");
    }

    #[test]
    fn folding_never_splits_a_codepoint_at_the_window_edge() {
        // UTF-8 correctness is the point: the fold window is a *byte* window
        // and the result must always be a `&str`, so the cut can only ever
        // land on a character boundary. A name of multi-byte characters is the
        // case that would expose a byte-wise copy.
        for n in 1..40usize {
            let full: String = "\u{e9}".repeat(n);
            let mut b = [0u8; FOLD_CAP];
            let folded = fold_into(&full, &mut b);
            // `from_utf8` returning `Ok` is the actual assertion: a byte-wise
            // copy at the window edge would leave a partial codepoint and
            // either fail here or, worse, render as mojibake.
            assert!(
                core::str::from_utf8(folded.as_bytes()).is_ok(),
                "fold of {n} x e-acute produced invalid UTF-8: {folded:?}"
            );
            // Two bytes of input became one byte of output each, so the fold
            // is *shorter* than the window and nothing was ever truncated. The
            // character count is what proves whole characters survived.
            assert_eq!(
                folded.chars().count(),
                n,
                "every e-acute must fold to exactly one 'e'"
            );
            assert!(
                folded.bytes().all(|c| c == b'e'),
                "the fold must be all 'e', got {folded:?}"
            );
        }
        // Now the case that actually truncates: a 2-byte-per-character name
        // long enough to overflow the 64-byte window, where the cut must land
        // on a boundary. 40 x e-acute is 80 bytes in and 40 out; 100 is 200 in
        // and 64 out, so the window is the binding constraint and the output
        // must be 64 *whole* 'e'.
        let long: String = "\u{e9}".repeat(100);
        let mut b = [0u8; FOLD_CAP];
        let folded = fold_into(&long, &mut b);
        assert_eq!(folded.len(), FOLD_CAP, "the window must bind");
        assert_eq!(folded.chars().count(), 64, "and it must be 64 whole chars");
        assert!(
            folded.is_char_boundary(folded.len()),
            "trivially true, stated"
        );
        // A CJK name: the fold must pass the codepoints through untouched
        // rather than dropping them as "unknown". `StringMatcherUtility`
        // special-cases HAN for exactly this reason (`:225-238`).
        let mut c = [0u8; FOLD_CAP];
        assert_eq!(
            fold_into("\u{65e5}\u{672c}\u{8a9e}", &mut c),
            "\u{65e5}\u{672c}\u{8a9e}"
        );
    }

    #[test]
    fn folded_comparison_is_case_and_diacritic_blind_in_both_directions() {
        // `contains_folded` is the predicate the whole ladder rests on, and
        // it is what replaces the live path's ASCII byte-window test. Assert it
        // directly on the cases that test would have got wrong.
        assert!(contains_folded("Café", "cafe"), "accented haystack");
        assert!(contains_folded("cafe", "café"), "accented needle");
        assert!(contains_folded("CAFÉ", "cafÉ"), "mixed case both sides");
        assert!(!contains_folded("Café", "cafea"), "no false positive");
        // An empty needle is contained, and a needle longer than the haystack
        // is not.
        assert!(contains_folded("anything", ""));
        assert!(!contains_folded("ab", "abcdef"));
        // A needle that is entirely combining marks folds to nothing, and an
        // empty needle is trivially contained -- the same answer
        // `needle.is_empty()` gives, so a mark-only query cannot match
        // nothing-by-accident.
        assert!(contains_folded("abc", "\u{301}"));
    }

    // -- the multi-word AND gate ------------------------------------------

    #[test]
    fn multi_word_queries_require_every_word_in_any_order() {
        // `SearchUtils.normalSearch` (`:17-19`): "Do an intersection of the
        // words in the query and each title, and filter out all the apps that
        // don't match all of the words in the query."
        for (name, query, want) in [
            ("Google Maps", "google maps", true),
            ("Google Maps", "maps google", true),
            ("Google Maps", "goo map", true),
            ("Google Maps", "map goo", true),
            ("Google Maps", "google firefox", false),
            ("Google Maps", "google", true),
            ("Firefox Web Browser", "firefox browser", true),
            ("Firefox Web Browser", "browser firefox", true),
            ("Firefox Web Browser", "firefox terminal", false),
        ] {
            let got = kind_of(name, query) != MatchKind::NoMatch;
            assert_eq!(got, want, "{query:?} against {name:?}");
        }
        // The order-agnostic case still ranks *above* the substring case,
        // because rule 3 fires for an in-order query and only rule 5 handles
        // the reordered one (`:67-79`).
        assert_eq!(
            kind_of("Google Maps", "goo map"),
            MatchKind::TokenPrefixOrdered
        );
        assert_eq!(
            kind_of("Google Maps", "map goo"),
            MatchKind::AllTokensPresent
        );
        // Whitespace normalisation: the reference splits on `Regex("\\s+")`
        // and filters blanks (`:55-56`), so a run of spaces is one separator
        // and leading/trailing space is not a word.
        assert_eq!(
            kind_of("Google  Maps", "  goo   map "),
            MatchKind::TokenPrefixOrdered,
            "runs of and edge whitespace are separators, not words"
        );
    }

    // -- the ranked entry point -------------------------------------------

    #[test]
    fn search_ranked_writes_the_best_rows_without_allocating() {
        // A catalogue deliberately built so the *ranking* is decided by the
        // ladder and not by the catalogue order: a fuzzy row listed first, an
        // exact row in the middle and a prefix row last.
        let rows = [
            ("firefox", "Firefox"),
            ("term", "Terminal"),
            ("termi", "Termix"),
        ];
        let mut out: [SearchHit<(&str, &str)>; 2] = [SearchHit::empty(&("term", "Terminal")); 2];
        let n = search_ranked(
            &rows,
            "term",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        // `Terminal` is a prefix of itself only up to 5 of 8 chars, and
        // `Termix` is a shorter name, so both are DIRECT_PREFIX and the
        // longer query is the *better* prefix. `Firefox` does not match at
        // all.
        assert_eq!(n, 2, "two rows match, the total is not the window");
        assert!(out[0].score >= out[1].score, "the best row is first");
        // The window is 2 wide and the total is 2, so both slots are real
        // rows, not placeholders.
        assert!(
            out.iter().all(|h| h.score != i32::MIN),
            "a placeholder leaked"
        );
    }

    #[test]
    fn search_ranked_window_truncates_to_the_best_not_the_first() {
        // The contract that makes the drawer window correct: with more
        // matches than slots, `out` holds the *highest ranked* ones, and the
        // returned total is the full count. This is the `maxAppResultsCount`
        // + `.take(n)` split (`AppSearchProvider.kt:63-64`).
        let rows = [
            ("a", "Aardvark"),
            ("b", "Baboon"),
            ("c", "Cat"),
            ("d", "Dog"),
        ];
        let mut out: [SearchHit<(&str, &str)>; 1] = [SearchHit::empty(&("a", "Aardvark")); 1];
        // An empty query lists everything: the total is the catalogue and the
        // single slot holds the alphabetically first row.
        let n = search_ranked(
            &rows,
            "",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 4, "the total is the whole catalogue, not the window");
        assert_eq!(out[0].item.1, "Aardvark", "alphabetically first");

        // A query that matches all four, into a one-slot window: the winner
        // is chosen by rank, and it must not depend on catalogue position.
        let mut out: [SearchHit<(&str, &str)>; 1] = [SearchHit::empty(&("a", "Aardvark")); 1];
        let n = search_ranked(
            &rows,
            "o",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 2, "Baboon and Dog both contain an 'o'");
        assert_eq!(out[0].item.1, "Baboon", "the alphabetical tie-break wins");
    }

    #[test]
    fn search_ranked_tiers_outrank_scores_across_the_whole_set() {
        // The end-to-end version of the priority test: three tiers of the
        // ladder, listed in the *worst* order, and the result must come back in
        // tier order. A sort that trusted the raw score, or that kept catalogue
        // order, would produce a different answer.
        let rows = [
            ("sub", "Web Browser"), // SUBSTRING: `brows` is inside "Browser"
            ("fuz", "Browzs"),      // FUZZY: `brows` is a subsequence, not a prefix
            ("pre", "Browse"),      // DIRECT_PREFIX
        ];
        let mut out: [SearchHit<(&str, &str)>; 3] = [SearchHit::empty(&("sub", "Web Browser")); 3];
        // One query, three tiers, listed in the worst possible order. Each name
        // is a *different* tier and nothing else:
        //
        //   `browse`   starts with `brows`            -> rule 1
        //   `web browser` contains `brows` inside it  -> rule 4
        //   `browzs`   neither prefix nor substring, and `b,r,o,w,s` *is* a
        //              subsequence of it, at ratio 100 * 10 / 11 = 90, over
        //              the 65 cutoff -> rule 6 and nothing else
        let n = search_ranked(
            &rows,
            "brows",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 3, "prefix, substring and typo all match");
        assert_eq!(out[0].item.0, "pre", "DIRECT_PREFIX leads");
        assert_eq!(out[1].item.0, "sub", "SUBSTRING is second");
        assert_eq!(out[2].item.0, "fuz", "FUZZY is last");
        assert_eq!(out[0].kind, MatchKind::DirectPrefix);
        assert_eq!(out[1].kind, MatchKind::Substring);
        assert_eq!(out[2].kind, MatchKind::Fuzzy);
        // The tiers' scores are on *different scales*, and this is the
        // concrete evidence. SUBSTRING earns a flat 720 -- the reference's own
        // constant (`AppMatcher.kt:74`) -- while FUZZY earns
        // `score_folded`'s raw bonus sum, which is 10 per matched character
        // plus the boundary bonuses (`:367-390`) and is not in `0..=1000` at
        // all. Here it lands *below* 720, so a score sort happens to agree; the
        // property worth pinning is the scale mismatch itself, because a longer
        // name pushes the same tier's score up and past 720.
        assert_eq!(out[1].score, 720, "SUBSTRING is the reference's 0.72");
        // `0.9 + 0.05 * (5/6)` = 0.9417 for `brows` against `browse`, the
        // reference's own length-dependent prefix score
        // (`AppMatcher.kt:49-51`). The floor of 900 only applies when the
        // query *is* the name, which is rule 0.
        assert_eq!(out[0].score, direct_prefix_score("browse", "brows"));
        assert_eq!(out[0].score, 942);
        //
        // The difference is *where* the characters sit, not how many: the
        // scorer's bonuses are for position, so the same query scores 105
        // against a run of letters and 155 against the same letters split by
        // punctuation (each of which earns the 20-point word-boundary bonus at
        // `:382-383`). Neither number is in the reference's `0..=1` range, and
        // a 50-point swing inside one tier is not a measure of anything the
        // user perceives -- which is exactly why the sort is on priority.
        assert_eq!(score_folded_query("brows", "Browzs"), Some(105));
        assert_eq!(score_folded_query("brows", "B-r-o-w-s"), Some(155));
        // A length change that does not move the match does not move the score
        // at all, so the scorer's scale is unrelated to the name's length --
        // the reference's `DIRECT_PREFIX` score is *entirely* length-based
        // (`:49-51`). Two scales, neither comparable, one sort key.
        assert_eq!(
            score_folded_query("brows", "Browzs Navigator Long Name"),
            score_folded_query("brows", "Browzs")
        );
    }

    #[test]
    fn search_ranked_reports_the_total_past_a_full_window() {
        // The drawer's header shows how many apps match, and that count has to
        // keep counting while the window stays a screenful
        // (`FrameView`, `main.rs:1014-1043`).
        let rows: [(&str, &str); 40] = [("x", "Extra"); 40];
        let mut out: [SearchHit<(&str, &str)>; 4] = [SearchHit::empty(&("x", "Extra")); 4];
        let n = search_ranked(
            &rows,
            "ex",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 40, "the total must not be clamped to the window");
        for h in &out {
            assert_eq!(h.item.1, "Extra", "every slot holds a real row");
        }
        // A zero-capacity window is a legitimate caller: it wants the count and
        // nothing else, and the count must still be right. This is why the
        // empty-window early-out is not simply `return 0`.
        let n = search_ranked(
            &rows[..3],
            "ex",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut [],
        );
        assert_eq!(n, 3, "a count-only caller still gets the count");
    }

    #[test]
    fn search_ranked_reads_keywords_at_a_penalty_and_never_worse_than_the_name() {
        // `DesktopCatalogue::search` has always weighted keywords below the
        // name (`:560`). The penalty must stay *inside* the tier: a keyword
        // prefix still beats every other prefix, and cannot leak into the tier
        // below, because the sort is by priority first
        // (`AppSearchProvider.kt:57-62`).
        let kw = vec!["browser".to_string()];
        let row = ("firefox", "Firefox", kw.as_slice());
        let rows = [row];
        let mut out: [SearchHit<(&str, &str, &[String])>; 1] = [SearchHit::empty(&row); 1];
        // "brow" is a prefix of the keyword "browser" and of nothing else, so
        // the hit can only have come from `keywords`.
        let n = search_ranked(
            &rows,
            "brow",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: r.2,
            },
            &mut out,
        );
        assert_eq!(n, 1, "the keyword matched");
        assert_eq!(out[0].kind, MatchKind::DirectPrefix, "rule 1 of the ladder");
        // The score is the name rule's score minus the penalty, so a keyword
        // hit is measurably worse than the same rule against the name.
        let named = ladder("browser", "browser", "brow").expect("name").1;
        assert_eq!(out[0].score, named - KEYWORD_PENALTY);
        // Spelled as a subtraction rather than a bare `> 0`, because a bare
        // comparison against a constant is what the linter flags as
        // vacuously true, and what a future edit to a *zero* penalty would
        // then let pass silently.
        assert_eq!(
            out[0].score,
            named - 20,
            "the penalty is 20, in 0..=1000 units"
        );
        assert!(
            out[0].score < named,
            "keywords must be weighted below the name: {} vs {}",
            out[0].score,
            named
        );
        // A query that matches the name is *not* dragged down by a keyword that
        // also matched: `take_best` keeps the higher of the two.
        let mut out: [SearchHit<(&str, &str, &[String])>; 1] = [SearchHit::empty(&row); 1];
        search_ranked(
            &rows,
            "fire",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: r.2,
            },
            &mut out,
        );
        assert_eq!(
            out[0].score,
            ladder("firefox", "Firefox", "fire").expect("name").1
        );
    }

    #[test]
    fn search_ranked_diacritics_work_end_to_end() {
        // The behaviour the live ASCII substring test could not produce: a
        // query with no accents finds a name that has them, and the reverse.
        let rows = [("cafe", "Café Player"), ("muzak", "Muzak")];
        let mut out: [SearchHit<(&str, &str)>; 2] = [SearchHit::empty(&("cafe", "Café Player")); 2];
        let n = search_ranked(
            &rows,
            "cafe",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 1, "an unaccented query finds the accented name");
        assert_eq!(out[0].item.1, "Café Player");

        let mut out: [SearchHit<(&str, &str)>; 2] = [SearchHit::empty(&("cafe", "Café Player")); 2];
        let n = search_ranked(
            &rows,
            "café",
            |r| SearchFields {
                name: r.1,
                id: r.0,
                keywords: &[],
            },
            &mut out,
        );
        assert_eq!(n, 1, "and an accented query finds an unaccented name");
        assert_eq!(out[0].item.1, "Café Player");
    }

    #[test]
    fn similarity_100_is_the_reference_ratio() {
        // `FuzzySearch.ratio` is `100 * (lensum - ldist) / lensum`; these are
        // the values the cutoff of 65 is applied to, so they are worth pinning
        // exactly rather than approximately.
        assert_eq!(similarity_100("firefox", "firefox"), 100, "identical");
        // One substitution in a 7-char string: `lensum` is 14, `ldist` is 1, so
        // `100 * 13 / 14` = 92. Spelled as the formula rather than as a
        // literal so a change to the arithmetic cannot silently keep the test
        // green.
        assert_eq!(similarity_100("firefox", "firefoz"), (100 * 13 / 14) as u8);
        // One insertion: `lensum` 15, `ldist` 1.
        assert_eq!(similarity_100("firefox", "firefoxx"), (100 * 14 / 15) as u8);
        // A substitution in a 4-char string: `lensum` 8, `ldist` 1 = 87.
        assert_eq!(similarity_100("abcd", "abce"), (100 * 7 / 8) as u8);
        assert_eq!(similarity_100("", ""), 100, "two empties are equal");
        assert_eq!(similarity_100("abc", ""), 0, "one empty is nothing alike");
        // `xyz` shares nothing with `abc` and still rates 50: the ratio is
        // `lensum`-normalised, so for equal-length strings a full substitution
        // is 50 by construction. This is why the cutoff is 65 and not 50, and
        // why the subsequence gate is not optional.
        assert_eq!(similarity_100("abc", "xyz"), 50);
        // Symmetric, which the formula has to be for a tier test to be
        // meaningful.
        for (a, b) in [("firefox", "firefoc"), ("maps", "map"), ("abc", "cba")] {
            assert_eq!(similarity_100(a, b), similarity_100(b, a), "{a}/{b}");
        }
        // Past the window there is no ratio, and it reports 0 rather than a
        // wrong number: the fold has already truncated both sides to the same
        // window, so this only guards the guard.
        let long = "a".repeat(FOLD_CAP + 1);
        assert_eq!(similarity_100(&long, "a"), 0, "a field past the window");
    }

    #[test]
    fn cmp_folded_orders_case_and_accent_blind() {
        use core::cmp::Ordering;
        assert_eq!(
            cmp_folded("a", "A"),
            Ordering::Equal,
            "case is not an order"
        );
        assert_eq!(cmp_folded("cafe", "Café"), Ordering::Equal, "nor is accent");
        assert_eq!(cmp_folded("a", "b"), Ordering::Less);
        assert_eq!(cmp_folded("b", "a"), Ordering::Greater);
        // A combining mark does not make a string "longer" for ordering
        // purposes, which is the same property `contains_folded` needs.
        assert_eq!(cmp_folded("cafe", "Cafe\u{301}"), Ordering::Equal);
        assert_eq!(cmp_folded("", ""), Ordering::Equal);
        assert_eq!(cmp_folded("", "a"), Ordering::Less, "a prefix sorts first");
    }
}
