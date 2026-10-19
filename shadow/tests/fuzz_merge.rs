// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! The alphabet skips `<`, `>`, `=` and `|`, so random input can never look
//! like a conflict marker. A marker in a clean merge is therefore a real bug.

use shadow::engine::workspace::validate_workspace_name;
use shadow::merger::diff3::{MergeOutcome, is_text, join_lines, merge_lines, split_lines};
use shadow::merger::merger::sanitize_rel;

/// Tiny xorshift64* PRNG so we don't need a fuzz crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn string(&mut self) -> String {
        const ALPHA: &[u8] = b"abcXYZ019 \t\r\n-_/#*";
        let len = self.below(48) as usize;
        (0..len)
            .map(|_| ALPHA[self.below(ALPHA.len() as u64) as usize] as char)
            .collect()
    }
}

#[test]
fn fuzz_merge_lines_never_panics_and_invariants_hold() {
    let mut rng = Rng::new(0x5EED_CAFE_F00D_D00D);
    let markers = ["<<<<<<<", "=======", ">>>>>>>"];

    for _ in 0..5_000 {
        let (b, o, t) = (rng.string(), rng.string(), rng.string());
        let (bl, ol, tl) = (split_lines(&b), split_lines(&o), split_lines(&t));

        match merge_lines(&bl, &ol, &tl, "ours", "theirs") {
            MergeOutcome::Clean(lines) => {
                let out = join_lines(&lines);
                // The alphabet can't produce markers, so any here is a leak.
                for m in markers {
                    assert!(
                        !out.contains(m),
                        "clean merge leaked marker {m:?}\nbase={b:?}\nours={o:?}\ntheirs={t:?}"
                    );
                }
            }
            MergeOutcome::Conflicted { lines, conflicts } => {
                let out = join_lines(&lines);
                assert!(!conflicts.is_empty());
                assert!(out.contains("<<<<<<<"));
                assert!(out.contains(">>>>>>>"));
            }
        }
    }
}

#[test]
fn fuzz_split_join_roundtrips_content() {
    let mut rng = Rng::new(0x0BAD_C0DE_1234_5678);
    for _ in 0..2_000 {
        let s = rng.string();
        // If this isn't lossless, merges would quietly corrupt files.
        let parts: Vec<std::borrow::Cow<'_, str>> = split_lines(&s)
            .into_iter()
            .map(std::borrow::Cow::Borrowed)
            .collect();
        let rejoined = join_lines(&parts);
        assert_eq!(rejoined, s, "split/join must be lossless");
    }
}

#[test]
fn fuzz_is_text_matches_utf8() {
    let mut rng = Rng::new(0xFEED_5EED_AAAA_1111);
    for _ in 0..2_000 {
        let s = rng.string();
        assert!(is_text(s.as_bytes()));
        let mut bytes = s.into_bytes();
        if rng.below(4) == 0 {
            bytes.push(0xFF);
        }
        assert_eq!(is_text(&bytes), std::str::from_utf8(&bytes).is_ok());
    }
}

#[test]
fn fuzz_sanitize_rel_never_accepts_escapes() {
    let mut rng = Rng::new(0xDEAD_BEEF_0000_1111);
    const NASTY: &[&str] = &[
        "..", ".", "/", "//", "~", "a/../..", "../", "/x", "\\", "C:\\",
    ];

    for _ in 0..3_000 {
        let segments = rng.below(5) as usize + 1;
        let mut parts: Vec<String> = Vec::new();
        for i in 0..segments {
            if i > 0 && rng.below(3) == 0 {
                parts.push(NASTY[rng.below(NASTY.len() as u64) as usize].to_string());
            } else {
                parts.push(format!("seg{}", rng.below(100)));
            }
        }
        let rel = parts.join("/");

        if sanitize_rel(&rel).is_ok() {
            // Anything we accept has to stay under the base dir.
            let base = tempfile::tempdir().unwrap();
            let target = base.path().join(&rel);
            assert!(
                target.starts_with(base.path()),
                "accepted path escaped the base: {rel}"
            );
        }
    }
}

#[test]
fn fuzz_workspace_names_follow_the_contract() {
    let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
    const SAFE: &[char] = &['a', 'Z', '0', '-', '_', '.'];

    for _ in 0..2_000 {
        let len = rng.below(40) as usize + 1;
        let mut name = String::new();
        name.push(SAFE[rng.below(3) as usize]); // a/Z/0 only
        for _ in 1..len {
            name.push(SAFE[rng.below(SAFE.len() as u64) as usize]);
        }
        if name.len() <= 64 {
            assert!(
                validate_workspace_name(&name).is_ok(),
                "should accept {name}"
            );
        }
    }
}
