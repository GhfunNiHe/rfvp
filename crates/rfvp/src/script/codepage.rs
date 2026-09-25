//! Character remap tables shipped inside localized releases' engine binaries.
//!
//! Some localized FVP releases store script text as Shift-JIS code points of
//! obscure substitute kanji, because the engine can only address glyphs
//! through the JIS tables; plain SJIS decoding then shows a wrong character
//! for anything outside JIS X 0208. The release's patched engine binary
//! carries the fix: a table of `(substitute, real)` UTF-16 pairs (found in
//! the Steam Chinese release's `Sakura.dll`). Rather than embedding per-game
//! snapshots, the parser scans the game directory's own binaries for such a
//! table at startup and, when one is found, rewrites decoded script strings
//! through it — mirroring the original engine.
//!
//! How such a table is laid out, and how to tell one apart from a look-alike
//! run of bytes, was mapped independently by the Tyranor-Next porting notes;
//! the thresholds below follow those measurements. See `LocalizationPatch.md`
//! beside this crate for the numbers on this tree's sample.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// `[(substitute, real)]` pairs, sorted by `substitute` for binary search.
#[derive(Clone, Debug)]
pub(crate) struct Codepage {
    pairs: Vec<(u16, u16)>,
}

impl Codepage {
    /// Rewrite substitute characters in a decoded script string.
    pub(crate) fn remap(&self, s: &str) -> String {
        if !s.chars().any(|c| self.lookup(c).is_some()) {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match self.lookup(c) {
                Some(real) => out.push(real),
                None => out.push(c),
            }
        }
        out
    }

    fn lookup(&self, c: char) -> Option<char> {
        let i = self
            .pairs
            .binary_search_by_key(&(c as u32), |&(src, _)| src as u32)
            .ok()?;
        char::from_u32(self.pairs[i].1 as u32)
    }
}

#[cfg(all(not(feature = "no_std"), not(target_os = "uefi")))]
mod scan {
    use alloc::vec::Vec;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use super::Codepage;

    /// Opcode that pushes a script string: `0x0E <u8 len> <bytes…>`, with the
    /// operand terminated by NUL.
    const OPCODE_PUSH_STRING: u8 = 0x0E;

    /// Longest `pushstring` operand worth decoding; dialogue lines are far
    /// shorter and a wrong length would desynchronize the scan.
    const MAX_PUSHSTRING_LEN: usize = 200;

    /// A real table has hundreds of entries, and runs of binary noise never
    /// come close: on this tree's sample the largest noise run was 6 entries
    /// against 1206 real ones. This is what keeps a stray DLL, or a release
    /// with no patch at all, from inventing a table.
    const MIN_TABLE_ENTRIES: usize = 32;

    /// Largest binary size worth scanning for a remap table.
    const MAX_BINARY_SIZE: u64 = 64 * 1024 * 1024;

    /// Share of a candidate's keys that must occur in the release's own text.
    /// Substitutes are only in the table because the script uses them, so a
    /// table that does not line up with the text is not a localization table.
    /// A matched table sits at 100%; the best look-alike run found in an
    /// unrelated 48 MB binary reached 73% (52% before the key modes were read
    /// apart by hand), so the threshold is set where both stay clear of it.
    /// Erring strict is deliberate: a missed table leaves the release as
    /// garbled as it was, a wrong one rewrites readable text.
    const MIN_SCRIPT_HIT_PERCENT: usize = 90;

    /// Share of a candidate's targets that must be unrepresentable in
    /// Shift-JIS — the reason the table exists at all. Measured separation on
    /// this tree's sample: 100% for the real table, 0% for the biggest
    /// look-alike run inside an unrelated binary.
    const MIN_NON_SJIS_TARGET_PERCENT: usize = 30;

    /// How the 16-bit key of an entry is to be read. Most frameworks store
    /// the substitute character itself; some store its Shift-JIS code unit.
    #[derive(Clone, Copy)]
    enum KeyMode {
        Unicode,
        ShiftJis,
    }

    /// Candidate tables found in a game directory, plus the characters of
    /// that release's scripts seen so far.
    struct DirScan {
        tables: Vec<Codepage>,
        text: HashSet<char>,
    }

    /// Look for a remap table in the engine binaries of the game directory.
    ///
    /// `script` is the raw script being parsed. Its characters decide between
    /// competing candidates, and they accumulate across scripts so that a
    /// table shared by several scripts still validates.
    pub(crate) fn load_from_game_dir(script: &[u8]) -> Option<Codepage> {
        let base = crate::utils::file::app_base_path().get_path().clone();
        let mut cache = cache().lock().unwrap_or_else(|err| err.into_inner());
        let entry = cache.entry(base.clone()).or_insert_with(|| DirScan {
            tables: scan_dir(&base),
            text: HashSet::new(),
        });
        entry.text.extend(script_chars(script));
        select(&entry.tables, &entry.text)
    }

    /// Parsing the game directory is not free, and every script of a release
    /// is parsed separately, so the scan result is kept per directory.
    fn cache() -> &'static Mutex<HashMap<PathBuf, DirScan>> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, DirScan>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Pick the table that best explains the release's own text.
    fn select(tables: &[Codepage], text: &HashSet<char>) -> Option<Codepage> {
        let mut best: Option<(usize, &Codepage)> = None;
        for table in tables {
            let hits = table
                .pairs
                .iter()
                .filter(|&&(src, _)| char::from_u32(src as u32).is_some_and(|c| text.contains(&c)))
                .count();
            if hits * 100 < table.pairs.len() * MIN_SCRIPT_HIT_PERCENT {
                continue;
            }
            let better = match &best {
                None => true,
                // The table explaining the most text wins; on a tie the
                // tightest one does, since its extra entries are unmatched.
                Some((best_hits, best_table)) => {
                    hits > *best_hits
                        || (hits == *best_hits && table.pairs.len() < best_table.pairs.len())
                }
            };
            if better {
                log::debug!(
                    "codepage: {} entry candidate matches {}/{} script characters",
                    table.pairs.len(),
                    hits,
                    table.pairs.len()
                );
                best = Some((hits, table));
            }
        }
        best.map(|(_, table)| table.clone())
    }

    /// Scan the engine binaries of the game directory for remap tables.
    fn scan_dir(base: &Path) -> Vec<Codepage> {
        let mut tables = Vec::new();
        // A UIF patch spells its mapping out, so it needs none of the
        // guessing below and may legitimately be shorter than a table
        // recovered from a binary.
        if let Some(table) = uif_config_table(base) {
            log::info!(
                "codepage: read a {} entry remap table from uif_config.json",
                table.pairs.len()
            );
            tables.push(table);
        }
        let Ok(entries) = std::fs::read_dir(base) else {
            return tables;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_engine_binary =
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("dll") || ext.eq_ignore_ascii_case("exe")
                    });
            if !is_engine_binary {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_BINARY_SIZE {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            for table in scan(&bytes) {
                log::info!(
                    "codepage: found a {} entry remap table in {}",
                    table.pairs.len(),
                    path.display()
                );
                tables.push(table);
            }
        }
        tables
    }

    /// Mapping from `uif_config.json`, which states the pairs directly:
    /// a `replace_chars` rule pairs `source_chars` with `target_chars`
    /// position by position, the substitute first and the real character
    /// second.
    #[cfg(feature = "runtime-core-deps")]
    fn uif_config_table(base: &Path) -> Option<Codepage> {
        let bytes = std::fs::read(base.join("uif_config.json")).ok()?;
        remap_from_uif_config(&bytes)
    }

    /// Read the pairs out of a `uif_config.json`.
    #[cfg(feature = "runtime-core-deps")]
    fn remap_from_uif_config(bytes: &[u8]) -> Option<Codepage> {
        let config: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let mut pairs = Vec::new();
        let mut seen = HashSet::new();
        collect_replace_chars(&config, &mut pairs, &mut seen);
        if pairs.is_empty() {
            return None;
        }
        pairs.sort_unstable_by_key(|&(key, _)| key);
        Some(Codepage { pairs })
    }

    #[cfg(not(feature = "runtime-core-deps"))]
    fn uif_config_table(_base: &Path) -> Option<Codepage> {
        None
    }

    /// Walk the config for `replace_chars` rules. The rule lives under
    /// `text_processor.rules` in every config seen, but the walk does not
    /// depend on where it sits — a rule that states both strings is a rule.
    #[cfg(feature = "runtime-core-deps")]
    fn collect_replace_chars(
        value: &serde_json::Value,
        pairs: &mut Vec<(u16, u16)>,
        seen: &mut HashSet<char>,
    ) {
        match value {
            serde_json::Value::Object(fields) => {
                if fields
                    .get("type")
                    .and_then(|kind| kind.as_str())
                    .is_none_or(|kind| kind == "replace_chars")
                {
                    if let (Some(source), Some(target)) = (
                        fields.get("source_chars").and_then(|s| s.as_str()),
                        fields.get("target_chars").and_then(|s| s.as_str()),
                    ) {
                        for (substitute, real) in source.chars().zip(target.chars()) {
                            if substitute == real || !seen.insert(substitute) {
                                continue;
                            }
                            if let (Ok(key), Ok(value)) =
                                (u16::try_from(substitute as u32), u16::try_from(real as u32))
                            {
                                pairs.push((key, value));
                            }
                        }
                    }
                }
                for field in fields.values() {
                    collect_replace_chars(field, pairs, seen);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_replace_chars(item, pairs, seen);
                }
            }
            _ => {}
        }
    }

    /// Find every candidate table in `bytes`.
    ///
    /// A table is a contiguous array of 4-byte `(substitute, real)` records,
    /// so it shows up as a run of valid records; noise around it always
    /// breaks the run. The array is not guaranteed to start on a 4-byte
    /// boundary, hence the 2-byte step between candidate starts.
    fn scan(bytes: &[u8]) -> Vec<Codepage> {
        let mut found = Vec::new();
        for mode in [KeyMode::Unicode, KeyMode::ShiftJis] {
            let mut off = 0usize;
            while off + 4 <= bytes.len() {
                let start = off;
                while off + 4 <= bytes.len() && is_pair(bytes, off, mode) {
                    off += 4;
                }
                let accepted = (off > start)
                    .then(|| {
                        (start..off)
                            .step_by(4)
                            .map(|p| read_pair(bytes, p))
                            .collect()
                    })
                    .and_then(|pairs| validate(pairs, mode));
                match accepted {
                    // A real table is one array, and the runs nested inside it
                    // are the same data with fewer entries, so skip past it.
                    Some(table) => found.push(table),
                    // A rejected run must not swallow the offsets inside it:
                    // its tail may be the head of the real table.
                    None => off = start + 2,
                }
            }
        }
        found
    }

    fn is_pair(bytes: &[u8], off: usize, mode: KeyMode) -> bool {
        let (key, target) = read_pair(bytes, off);
        // Targets are the Chinese text the patch wants to show: anything but
        // controls, surrogates and the replacement character. Private use
        // symbols like U+F8F2 do occur here.
        if target < 0xA0 || (0xD800..=0xDFFF).contains(&target) || target == 0xFFFD {
            return false;
        }
        match mode {
            // Substitutes came out of a Shift-JIS decode of the script.
            KeyMode::Unicode => {
                (0x3000..=0x9FFF).contains(&key)
                    && char::from_u32(key as u32).is_some_and(encodable_in_shift_jis)
            }
            KeyMode::ShiftJis => shift_jis_code_to_char(key).is_some(),
        }
    }

    fn read_pair(bytes: &[u8], off: usize) -> (u16, u16) {
        (
            u16::from_le_bytes([bytes[off], bytes[off + 1]]),
            u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]),
        )
    }

    /// Reject anything that cannot be a localization remap table.
    ///
    /// Keys are normalized to the character they stand for in decoded script
    /// text, so the remap side does not need to know which form was stored.
    fn validate(pairs: Vec<(u16, u16)>, mode: KeyMode) -> Option<Codepage> {
        if pairs.len() < MIN_TABLE_ENTRIES {
            return None;
        }
        let mut seen = HashSet::new();
        let mut non_sjis_targets = 0usize;
        let mut normalized = Vec::with_capacity(pairs.len());
        for &(key, target) in &pairs {
            let key = match mode {
                KeyMode::Unicode => char::from_u32(key as u32)?,
                KeyMode::ShiftJis => shift_jis_code_to_char(key)?,
            };
            if !seen.insert(key) {
                return None;
            }
            let target_char = char::from_u32(target as u32)?;
            if !encodable_in_shift_jis(target_char) {
                non_sjis_targets += 1;
            }
            normalized.push((u16::try_from(key as u32).ok()?, target));
        }
        if non_sjis_targets * 100 < pairs.len() * MIN_NON_SJIS_TARGET_PERCENT {
            return None;
        }
        normalized.sort_unstable_by_key(|&(key, _)| key);
        Some(Codepage { pairs: normalized })
    }

    /// Characters the script stores in its `pushstring` operands.
    fn script_chars(bytes: &[u8]) -> HashSet<char> {
        let mut chars = HashSet::new();
        for i in 0..bytes.len().saturating_sub(2) {
            if bytes[i] != OPCODE_PUSH_STRING {
                continue;
            }
            let len = bytes[i + 1] as usize;
            let start = i + 2;
            if len == 0 || len > MAX_PUSHSTRING_LEN || start + len > bytes.len() {
                continue;
            }
            let mut payload = &bytes[start..start + len];
            if payload.last() == Some(&0) {
                payload = &payload[..payload.len() - 1];
            }
            // Only an operand without embedded NULs is a string operand.
            if payload.contains(&0) {
                continue;
            }
            let (text, _, _) = encoding_rs::SHIFT_JIS.decode(payload);
            chars.extend(text.chars());
        }
        chars
    }

    /// Shift-JIS answers, precomputed once.
    ///
    /// Scanned binaries can be tens of megabytes, so the byte sweep must not
    /// encode a character per position.
    struct ShiftJis {
        /// Code points Shift-JIS can represent, as a bitmap over the BMP.
        encodable: Vec<u64>,
        /// Decoded character per double-byte code unit, NUL when unused.
        by_code_unit: Vec<char>,
    }

    fn shift_jis() -> &'static ShiftJis {
        static TABLES: OnceLock<ShiftJis> = OnceLock::new();
        TABLES.get_or_init(|| {
            let mut encodable = vec![0u64; 0x1_0000 / 64];
            for cp in 0..=0xFFFFu32 {
                let Some(c) = char::from_u32(cp) else {
                    continue;
                };
                let mut buf = [0u8; 4];
                let (_, _, had_errors) = encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut buf));
                if !had_errors {
                    encodable[cp as usize / 64] |= 1 << (cp % 64);
                }
            }
            let mut by_code_unit = vec!['\0'; 0x1_0000];
            for lead in (0x81u16..=0x9F).chain(0xE0..=0xEF) {
                for trail in (0x40u16..=0x7E).chain(0x80..=0xFC) {
                    let code = (lead << 8) | trail;
                    let unit = [lead as u8, trail as u8];
                    let (text, _, had_errors) = encoding_rs::SHIFT_JIS.decode(&unit);
                    if had_errors {
                        continue;
                    }
                    let mut chars = text.chars();
                    if let (Some(c), None) = (chars.next(), chars.next()) {
                        by_code_unit[code as usize] = c;
                    }
                }
            }
            ShiftJis {
                encodable,
                by_code_unit,
            }
        })
    }

    fn encodable_in_shift_jis(c: char) -> bool {
        let cp = c as u32;
        cp <= 0xFFFF && shift_jis().encodable[cp as usize / 64] & (1 << (cp % 64)) != 0
    }

    /// Decode a double-byte Shift-JIS code unit. `None` for unused code
    /// points, which is what a tunnel patch would need handled instead.
    fn shift_jis_code_to_char(code: u16) -> Option<char> {
        let c = shift_jis().by_code_unit[code as usize];
        (c != '\0').then_some(c)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Substitute characters: the JIS kanji a Shift-JIS patch falls back
        /// on for text it cannot store.
        fn substitutes(count: usize) -> Vec<char> {
            (0x4E00u32..=0x9FFF)
                .filter_map(char::from_u32)
                .filter(|c| encodable_in_shift_jis(*c))
                .take(count)
                .collect()
        }

        /// Target characters: the simplified text a patch wants to show, i.e.
        /// what Shift-JIS cannot represent.
        fn targets(count: usize) -> Vec<char> {
            (0x4E00u32..=0x9FFF)
                .filter_map(char::from_u32)
                .filter(|c| !encodable_in_shift_jis(*c))
                .take(count)
                .collect()
        }

        fn pairs(key: &[char], target: &[char]) -> Vec<(u16, u16)> {
            key.iter()
                .copied()
                .zip(target.iter().copied())
                .map(|(k, v)| (k as u32 as u16, v as u32 as u16))
                .collect()
        }

        fn encoded(pairs: &[(u16, u16)]) -> Vec<u8> {
            let mut bytes = Vec::new();
            for &(key, target) in pairs {
                bytes.extend_from_slice(&key.to_le_bytes());
                bytes.extend_from_slice(&target.to_le_bytes());
            }
            bytes
        }

        fn table(count: usize) -> Codepage {
            validate(
                pairs(&substitutes(count), &targets(count)),
                KeyMode::Unicode,
            )
            .expect("handed a well-formed table")
        }

        /// A table is found even when the array does not start on a 4-byte
        /// boundary, which a 4-byte step would walk straight past.
        #[test]
        fn finds_table_at_an_unaligned_offset() {
            let pairs = pairs(&substitutes(64), &targets(64));
            let mut bytes = vec![0u8; 4092];
            bytes.extend_from_slice(&encoded(&pairs));
            bytes.extend_from_slice(&[0u8; 512]);
            let found = scan(&bytes);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].pairs.len(), 64);
        }

        /// Runs below `MIN_TABLE_ENTRIES` are noise, not a table.
        #[test]
        fn rejects_runs_below_the_minimum_length() {
            let pairs = pairs(&substitutes(8), &targets(8));
            let mut bytes = vec![0u8; 512];
            bytes.extend_from_slice(&encoded(&pairs));
            bytes.extend_from_slice(&[0u8; 512]);
            assert!(scan(&bytes).is_empty());
        }

        /// A long run whose targets are all representable in Shift-JIS is not
        /// a localization table: the real thing exists because they are not.
        /// Big unrelated binaries do contain runs shaped like this.
        #[test]
        fn rejects_tables_of_shift_jis_targets() {
            let both = substitutes(64);
            let mut bytes = vec![0u8; 512];
            bytes.extend_from_slice(&encoded(&pairs(&both, &both)));
            bytes.extend_from_slice(&[0u8; 512]);
            assert!(scan(&bytes).is_empty());
        }

        #[test]
        fn finds_nothing_in_noise() {
            let mut seed = 0x1234_5678u32;
            let noise: Vec<u8> = (0..64 * 1024)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            assert!(scan(&noise).is_empty());
        }

        /// The script decides which candidate is the release's table: a
        /// look-alike run must lose to one matching the text.
        #[test]
        fn selection_is_confirmed_by_the_script_text() {
            let text: HashSet<char> = substitutes(64).into_iter().collect();
            assert!(select(&[table(64)], &text).is_some());
            // Nothing read from the script means nothing to confirm against.
            assert!(select(&[table(64)], &HashSet::new()).is_none());

            let matching = table(64);
            let unrelated = table(32);
            let picked = select(&[unrelated, matching.clone()], &text).expect("table matches");
            assert_eq!(picked.pairs.len(), matching.pairs.len());

            // A table that only mostly lines up is noise, not the release's
            // table: look-alike runs in large binaries reach the high 80s.
            let mostly: HashSet<char> = substitutes(56).into_iter().collect();
            assert!(select(&[table(64)], &mostly).is_none());
        }

        /// Tables that store Shift-JIS code units instead of characters are
        /// normalized to the characters they decode to.
        #[test]
        fn normalizes_shift_jis_code_unit_keys() {
            let mut codes = Vec::new();
            let mut seen = HashSet::new();
            'outer: for lead in 0x81u16..=0x9F {
                for trail in 0x40u16..=0xFC {
                    let code = (lead << 8) | trail;
                    let Some(c) = shift_jis_code_to_char(code) else {
                        continue;
                    };
                    if seen.insert(c) {
                        codes.push((code, c));
                    }
                    if codes.len() == 64 {
                        break 'outer;
                    }
                }
            }
            assert_eq!(codes.len(), 64);

            let pairs: Vec<(u16, u16)> = codes
                .iter()
                .zip(targets(64))
                .map(|(&(code, _), target)| (code, target as u32 as u16))
                .collect();
            let found = scan(&encoded(&pairs));
            let table = found
                .iter()
                .find(|t| t.pairs.len() == 64)
                .expect("table found");
            let keys: Vec<char> = table
                .pairs
                .iter()
                .map(|&(key, _)| char::from_u32(key as u32).expect("char"))
                .collect();
            for c in codes.iter().map(|&(_, c)| c) {
                assert!(keys.contains(&c), "{c:?} missing from normalized keys");
            }
        }

        #[test]
        fn collects_characters_from_pushstring_operands() {
            let mut script = Vec::new();
            for line in ["徼戍", "諍禳"] {
                let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(line);
                script.push(OPCODE_PUSH_STRING);
                script.push(bytes.len() as u8 + 1);
                script.extend_from_slice(&bytes);
                script.push(0);
            }
            // Operands with an embedded NUL or an absurd length are not
            // strings and must not contribute characters.
            script.extend_from_slice(&[OPCODE_PUSH_STRING, 3, 0x8D, 0x00, 0x8E]);
            script.extend_from_slice(&[OPCODE_PUSH_STRING, 255, 0x8D, 0x8E]);

            let chars = script_chars(&script);
            assert!(chars.contains(&'徼'));
            assert!(chars.contains(&'戍'));
            assert!(chars.contains(&'諍'));
            assert!(chars.contains(&'禳'));
            assert!(!chars.contains(&'\0'));
        }

        /// A UIF patch states its pairs, so they are read as given rather
        /// than recovered heuristically — including tables too short to pass
        /// the binary-scan gates.
        #[cfg(feature = "runtime-core-deps")]
        #[test]
        fn reads_replace_chars_rules() {
            let config = br#"{
                "text_processor": {
                    "rules": [
                        { "type": "replace_chars",
                          "source_chars": "\u9075\u8aac\u660e",
                          "target_chars": "\u8fdd\u8bf4\u5982" }
                    ]
                }
            }"#;
            let table = remap_from_uif_config(config).expect("table");
            assert_eq!(table.pairs.len(), 3);
            assert_eq!(table.remap("遵説明"), "违说如");

            // Explicit pairs still have to line up with the release's text.
            let text: HashSet<char> = "遵説明".chars().collect();
            assert!(select(&[table], &text).is_some());
        }

        #[cfg(feature = "runtime-core-deps")]
        #[test]
        fn ignores_configs_that_do_not_carry_a_mapping() {
            assert!(remap_from_uif_config(b"{}").is_none());
            assert!(remap_from_uif_config(b"not json").is_none());
            assert!(remap_from_uif_config(
                br#"{ "text_processor": { "rules": [
                    { "type": "tunnel_decoder", "source_chars": "\u9075", "target_chars": "\u8fdd" }
                ] } }"#
            )
            .is_none());
            // Pairs that match themselves carry no mapping.
            assert!(remap_from_uif_config(
                br#"{ "rules": [ {
                    "type": "replace_chars", "source_chars": "\u9075", "target_chars": "\u9075"
                } ] }"#
            )
            .is_none());
        }

        /// `source_chars` and `target_chars` pair up by position, and a
        /// repeated substitute keeps its first mapping.
        #[cfg(feature = "runtime-core-deps")]
        #[test]
        fn pairs_replace_chars_position_by_position() {
            let table = remap_from_uif_config(
                br#"{ "rules": [
                    { "type": "replace_chars", "source_chars": "\u9075\u8aac",
                      "target_chars": "\u8fdd" },
                    { "type": "replace_chars", "source_chars": "\u9075",
                      "target_chars": "\u9519" }
                ] }"#,
            )
            .expect("table");
            // The surplus substitute has no target, and the second rule
            // cannot override the first.
            assert_eq!(table.pairs.len(), 1);
            assert_eq!(table.remap("遵説"), "违説");
        }
    }
}

#[cfg(all(not(feature = "no_std"), not(target_os = "uefi")))]
pub(crate) use scan::load_from_game_dir;

#[cfg(any(feature = "no_std", target_os = "uefi"))]
pub(crate) fn load_from_game_dir(_script: &[u8]) -> Option<Codepage> {
    None
}
