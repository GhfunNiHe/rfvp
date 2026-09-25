# Localization patch character maps

This note documents how `script/codepage.rs` restores text of localized FVP
releases whose script stores Chinese as substitute kanji. It only states what
is backed by measurements on a real release and by an independent mapping of
the same table done by another port.

## The problem

The engine can only address glyphs through the JIS tables, so a patch that
wants to show simplified Chinese stores placeholder characters instead: the
Shift-JIS code points of obscure kanji. The patch ships a Windows DLL (a
`d3d9` proxy or similar) that carries a `(substitute, real)` table and
rewrites the decoded text at runtime. That DLL cannot run here.

The Tyranor-Next porting notes describe the same situation for Android and
work around it by rewriting a font's `cmap` so the substitute code points
draw the Chinese glyphs, with the user generating that font by hand.

This tree does what the notes list as the alternative "needs engine support"
path: it reads the table out of the release's own binary and rewrites
characters after decoding. Nothing is embedded, generated or redistributed —
the release's own files are read at startup, and a release without such a
table is untouched.

## Confirmed on the sample (Steam Chinese "Sakura, Moyu.")

| Item | Value |
|---|---|
| Table | `Sakura.dll`, file offset `0x47960` (RVA `0x48960`), 1206 entries × 4 bytes, loop count `0x4B6` |
| Entry | little-endian `(u16 substitute, u16 real)`, both Unicode code points |
| Script use | all 1206 keys occur in `Sakura.hcb`; no chained mappings |
| Cross-check | the Tyranor-Next notes report the same offset, the same 1206 entries and 1206/1206 script coverage |

## How a table is told apart from a look-alike run

Scanning a binary for a plausible `(u16, u16)` array finds noise everywhere,
so a candidate has to clear all of these. Measured separation on the sample:

| Gate | Threshold | Real table | Best look-alike run |
|---|---|---|---|
| Length | ≥ 32 entries | 1206 | 6 (`Sakura.exe`, `steam_api.dll`), 3203 (`rfvp` binary) |
| Targets unrepresentable in Shift-JIS | ≥ 30% | 100% | 0% |
| Keys matching the release's own text | ≥ 90% | 100% | 73% |

The third gate is the one that actually settles it, and it is the step the
porting notes describe: substitutes are in the table only because the script
uses them. Characters are collected from the `0x0E <len> <bytes>` string
operands of the script being parsed, and they accumulate across scripts so a
table shared by several scripts still validates.

Runs are looked for on 2-byte boundaries: on the sample, shifting the binary
by two bytes used to drop the table from 1206 entries to a 4-entry noise run.

## Where a table can come from

1. **The release's own binary**, by the scan above.
2. **`uif_config.json`**, if the patch ships one: a `replace_chars` rule
   states `source_chars` (the substitutes stored in the script) and
   `target_chars` (what they should show) and the two pair up by position.
   Nothing is guessed there, so such a table is used as given — including
   when it is shorter than the 32 entries a scanned table needs. It still has
   to line up with the release's text before it is applied. Unlike the scanned
   form, this source has not been exercised against a real release yet; a
   config whose shape differs from the documented one yields no table rather
   than a wrong one.

## What is deliberately not claimed here

- **SJIS code-unit keys.** Some frameworks are said to store the Shift-JIS
  code unit of the substitute instead of the character; that form is read and
  normalized, but no such release has been verified in this tree yet.
- **Tunnel patches** (keys are Shift-JIS code points the encoding does not
  define, plus an `sjis_ext.bin` / `jis_ext.bin` order table). Those bytes
  decode to replacement characters, so the text is lost before any table can
  help; this path would have to work on raw bytes instead.
- **Font-carried maps.** A patch can carry the mapping inside a font's `cmap`
  instead of a DLL. Reading that back would need inverting the glyph ids.
- **`jis_map.bin`.** Which characters go with which slot is decided by the
  companion DLL's code, so the file is not read.
- **GBK patches** need no table at all: they are plain GBK, i.e. `--nls gbk`.
