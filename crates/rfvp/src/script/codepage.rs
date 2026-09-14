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

/// `[(substitute, real)]` pairs, sorted by `substitute` for binary search.
#[derive(Clone, Debug)]
pub(crate) struct Codepage {
    pairs: alloc::vec::Vec<(u16, u16)>,
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
    use std::collections::HashSet;

    use super::Codepage;

    /// A real table has hundreds of entries; noise runs stay far below this.
    const MIN_TABLE_ENTRIES: usize = 32;

    /// Largest binary size worth scanning for a remap table.
    const MAX_BINARY_SIZE: u64 = 64 * 1024 * 1024;

    /// Look for a remap table in the engine binaries of the game directory.
    pub(crate) fn load_from_game_dir() -> Option<Codepage> {
        let base = crate::utils::file::app_base_path().get_path().clone();
        let mut best: Option<Codepage> = None;
        for entry in std::fs::read_dir(&base).ok()?.flatten() {
            let path = entry.path();
            let is_engine_binary = path.extension().and_then(|ext| ext.to_str()).is_some_and(
                |ext| ext.eq_ignore_ascii_case("dll") || ext.eq_ignore_ascii_case("exe"),
            );
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
            if let Some(table) = scan(&bytes) {
                let longer = best.as_ref().is_none_or(|b| table.pairs.len() > b.pairs.len());
                if longer {
                    best = Some(table);
                }
            }
        }
        best
    }

    /// Find the longest contiguous run of valid `(u16, u16)` pairs that
    /// passes [`validate`]. Real tables are stored as one aligned array, so
    /// every entry checks out; noise around them breaks the run.
    fn scan(bytes: &[u8]) -> Option<Codepage> {
        let mut best: Option<Codepage> = None;
        let mut off = 0usize;
        while off + 4 <= bytes.len() {
            let start = off;
            while off + 4 <= bytes.len() && is_valid_pair(bytes, off) {
                off += 4;
            }
            if off > start {
                let pairs: Vec<(u16, u16)> = (start..off)
                    .step_by(4)
                    .map(|p| {
                        (
                            u16::from_le_bytes([bytes[p], bytes[p + 1]]),
                            u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]),
                        )
                    })
                    .collect();
                if let Some(table) = validate(pairs) {
                    let longer =
                        best.as_ref().is_none_or(|b| table.pairs.len() > b.pairs.len());
                    if longer {
                        best = Some(table);
                    }
                }
            }
            off += 4;
        }
        best
    }

    fn is_valid_pair(bytes: &[u8], off: usize) -> bool {
        let src = u16::from_le_bytes([bytes[off], bytes[off + 1]]) as u32;
        let dst = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) as u32;
        (0x3000..=0x9FFF).contains(&src) && (0x20..=0x9FFF).contains(&dst)
    }

    /// Reject anything that does not look like a localization remap table:
    /// substitutes must be distinct Shift-JIS characters (they came out of a
    /// SJIS decode), and a large share of the real characters must be
    /// unrepresentable in Shift-JIS — the whole reason the table exists.
    /// Random binary data never satisfies both for hundreds of entries.
    fn validate(pairs: Vec<(u16, u16)>) -> Option<Codepage> {
        let mut seen = HashSet::new();
        let mut non_sjis_dsts = 0usize;
        for &(src, dst) in &pairs {
            let src_ch = char::from_u32(src as u32)?;
            let dst_ch = char::from_u32(dst as u32)?;
            if !seen.insert(src) || !encodable_in_shift_jis(src_ch) {
                return None;
            }
            if !encodable_in_shift_jis(dst_ch) {
                non_sjis_dsts += 1;
            }
        }
        if non_sjis_dsts * 10 < pairs.len() * 3 {
            return None;
        }
        let mut pairs = pairs;
        pairs.sort_unstable_by_key(|&(src, _)| src);
        Some(Codepage { pairs })
    }

    fn encodable_in_shift_jis(c: char) -> bool {
        let mut buf = [0u8; 4];
        let (_, _, had_errors) = encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut buf));
        !had_errors
    }
}

#[cfg(all(not(feature = "no_std"), not(target_os = "uefi")))]
pub(crate) use scan::load_from_game_dir;

#[cfg(any(feature = "no_std", target_os = "uefi"))]
pub(crate) fn load_from_game_dir() -> Option<Codepage> {
    None
}
