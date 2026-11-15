// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/core/java/android/os/PatternMatcher.java (matchPattern:214, matchGlobPattern:230, matchAdvancedPattern:481).
//! The five PatternMatcher kinds an IntentFilter keeps for its scheme-specific parts and data paths. Plain (no binder), so the rules are unit-tested without the converter.
//! Text is matched in UTF-16 units, as Java's charAt does: `.` takes one unit, so an astral character takes two.

/// PatternMatcher.PATTERN_LITERAL .. PATTERN_SUFFIX (PatternMatcher.java:38-73).
pub const PATTERN_LITERAL: i32 = 0;
pub const PATTERN_PREFIX: i32 = 1;
pub const PATTERN_SIMPLE_GLOB: i32 = 2;
pub const PATTERN_ADVANCED_GLOB: i32 = 3;
pub const PATTERN_SUFFIX: i32 = 4;

/// A parcelled PatternMatcher. `parsed` is an advanced pattern in AOSP's parsed form (parseAndVerifyAdvancedPattern:315); empty for the other kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub pattern: String,
    pub kind: i32,
    pub parsed: Vec<i32>,
}

/// Tokens of a parsed advanced pattern (PatternMatcher.java:98-105). A non-negative entry is a literal UTF-16 unit.
const PARSED_TOKEN_CHAR_SET_START: i32 = -1;
const PARSED_TOKEN_CHAR_SET_INVERSE_START: i32 = -2;
const PARSED_TOKEN_CHAR_SET_STOP: i32 = -3;
const PARSED_TOKEN_CHAR_ANY: i32 = -4;
const PARSED_MODIFIER_RANGE_START: i32 = -5;
const PARSED_MODIFIER_ZERO_OR_MORE: i32 = -7;
const PARSED_MODIFIER_ONE_OR_MORE: i32 = -8;

/// PatternMatcher.match(s): whether `p` accepts the text `s`. An unknown kind accepts nothing.
pub fn matches(p: &Pattern, s: &str) -> bool {
    match p.kind {
        PATTERN_LITERAL => p.pattern == s,
        PATTERN_PREFIX => s.starts_with(p.pattern.as_str()),
        PATTERN_SUFFIX => s.ends_with(p.pattern.as_str()),
        PATTERN_SIMPLE_GLOB => match_glob(&units(&p.pattern), &units(s)),
        PATTERN_ADVANCED_GLOB => match_advanced(&p.parsed, &units(s)),
        _ => false,
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// matchGlobPattern:230-308: `c*` repeats c, `.*` takes any run, `\` escapes the next unit.
/// Kept from AOSP: an escaped `.` not followed by `*` still matches any unit, since the `c != '.'` test at :289 sees the unescaped '.'.
fn match_glob(pattern: &[u16], m: &[u16]) -> bool {
    let np = pattern.len();
    if np == 0 {
        return m.is_empty();
    }
    let nm = m.len();
    let (dot, star, slash) = (u16::from(b'.'), u16::from(b'*'), u16::from(b'\\'));
    // Java's `ip < NP ? charAt(ip) : 0`
    let at = |i: usize| pattern.get(i).copied().unwrap_or(0);
    let (mut ip, mut im) = (0, 0);
    let mut next = pattern[0];
    while ip < np && im < nm {
        let mut c = next;
        ip += 1;
        next = at(ip);
        let escaped = c == slash;
        if escaped {
            c = next;
            ip += 1;
            next = at(ip);
        }
        if next == star {
            if !escaped && c == dot {
                // `.*`: skip to the next pattern unit in the text
                if ip + 1 >= np {
                    return true;
                }
                ip += 1;
                next = pattern[ip];
                if next == slash {
                    ip += 1;
                    next = at(ip);
                }
                loop {
                    if m[im] == next {
                        break;
                    }
                    im += 1;
                    if im >= nm {
                        break;
                    }
                }
                if im == nm {
                    return false;
                }
                ip += 1;
                next = at(ip);
                im += 1;
            } else {
                // `c*`: take the run of c
                loop {
                    if m[im] != c {
                        break;
                    }
                    im += 1;
                    if im >= nm {
                        break;
                    }
                }
                ip += 1;
                next = at(ip);
            }
        } else {
            if c != dot && m[im] != c {
                return false;
            }
            im += 1;
        }
    }
    if ip >= np && im >= nm {
        return true;
    }
    // the text ended before a trailing `.*`
    ip + 2 == np && pattern[ip] == dot && pattern[ip + 1] == star
}

/// One token of an advanced pattern. `Set` indexes its range pairs in `parsed`: start..end, end exclusive.
#[derive(Clone, Copy)]
enum Token {
    Any,
    Literal(i32),
    Set {
        start: usize,
        end: usize,
        invert: bool,
    },
}

/// matchAdvancedPattern:481-569: the whole text against the whole parsed pattern. A token takes as many units as its modifier allows, without backtracking.
fn match_advanced(parsed: &[i32], m: &[u16]) -> bool {
    let (lp, lm) = (parsed.len(), m.len());
    let (mut ip, mut im) = (0, 0);
    while ip < lp {
        let pc = parsed[ip];
        let token = match pc {
            PARSED_TOKEN_CHAR_ANY => {
                ip += 1;
                Token::Any
            }
            PARSED_TOKEN_CHAR_SET_START | PARSED_TOKEN_CHAR_SET_INVERSE_START => {
                let start = ip + 1;
                ip += 1;
                while ip < lp && parsed[ip] != PARSED_TOKEN_CHAR_SET_STOP {
                    ip += 1;
                }
                let end = ip - 1;
                ip += 1;
                Token::Set {
                    start,
                    end,
                    invert: pc == PARSED_TOKEN_CHAR_SET_INVERSE_START,
                }
            }
            _ => {
                ip += 1;
                Token::Literal(pc)
            }
        };
        let (min, max) = if ip >= lp {
            (1, 1)
        } else {
            match parsed[ip] {
                PARSED_MODIFIER_ZERO_OR_MORE => {
                    ip += 1;
                    (0, i32::MAX)
                }
                PARSED_MODIFIER_ONE_OR_MORE => {
                    ip += 1;
                    (1, i32::MAX)
                }
                PARSED_MODIFIER_RANGE_START => {
                    let (Some(&min), Some(&max)) = (parsed.get(ip + 1), parsed.get(ip + 2)) else {
                        return false;
                    };
                    // RANGE_START, min, max, RANGE_STOP
                    ip += 4;
                    (min, max)
                }
                _ => (1, 1),
            }
        };
        if min > max {
            return false;
        }
        let mut n = 0;
        while n < max && match_unit(m, im + n as usize, token, parsed) {
            n += 1;
        }
        if n < min {
            return false;
        }
        im += n as usize;
    }
    ip >= lp && im >= lm
}

/// matchChar:585: whether `token` takes the unit at `i`; false past the end of the text.
fn match_unit(m: &[u16], i: usize, token: Token, parsed: &[i32]) -> bool {
    let Some(&unit) = m.get(i) else {
        return false;
    };
    let c = i32::from(unit);
    match token {
        Token::Any => true,
        Token::Literal(v) => c == v,
        Token::Set { start, end, invert } => {
            let inside = (start..end).step_by(2).any(|k| match parsed.get(k..k + 2) {
                Some(&[lo, hi]) => (lo..=hi).contains(&c),
                _ => false,
            });
            inside != invert
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(pattern: &str, kind: i32) -> Pattern {
        Pattern {
            pattern: pattern.into(),
            kind,
            parsed: Vec::new(),
        }
    }

    /// An advanced pattern in the form AOSP's parser yields: literal units, PARSED_* tokens, `{min,max}` as two ints.
    fn advanced(parsed: &[i32]) -> Pattern {
        Pattern {
            pattern: String::new(),
            kind: PATTERN_ADVANCED_GLOB,
            parsed: parsed.to_vec(),
        }
    }

    #[test]
    fn literal_prefix_and_suffix() {
        assert!(matches(&text("/a", PATTERN_LITERAL), "/a"));
        assert!(!matches(&text("/a", PATTERN_LITERAL), "/ab"));
        // PREFIX is startsWith, not segment-aware
        assert!(matches(&text("/app", PATTERN_PREFIX), "/apps"));
        assert!(!matches(&text("/app", PATTERN_PREFIX), "/ap"));
        assert!(matches(&text(".png", PATTERN_SUFFIX), "a.png"));
        assert!(!matches(&text(".png", PATTERN_SUFFIX), "a.pngx"));
        assert!(!matches(&text("a", 9), "a"), "unknown kind");
    }

    #[test]
    fn simple_glob() {
        let g = text("a.*b", PATTERN_SIMPLE_GLOB);
        assert!(matches(&g, "axxb"));
        assert!(matches(&g, "ab"));
        assert!(!matches(&g, "axxc"));
        assert!(matches(&text("ab*c", PATTERN_SIMPLE_GLOB), "abbbc"));
        assert!(matches(&text("ab*c", PATTERN_SIMPLE_GLOB), "ac"));
        assert!(!matches(&text("ab*c", PATTERN_SIMPLE_GLOB), "abbbd"));
        // `\.*` is a run of literal dots
        let dots = text("a\\.*b", PATTERN_SIMPLE_GLOB);
        assert!(matches(&dots, "a.b") && matches(&dots, "ab"));
        assert!(!matches(&dots, "axb"));
        // a trailing `.*` matches when the text ends first
        assert!(matches(&text("/x.*", PATTERN_SIMPLE_GLOB), "/x"));
        assert!(matches(&text("", PATTERN_SIMPLE_GLOB), ""));
        assert!(!matches(&text("", PATTERN_SIMPLE_GLOB), "a"));
        // `.` is one UTF-16 unit: U+1F389 takes two
        assert!(!matches(&text("a.b", PATTERN_SIMPLE_GLOB), "a\u{1F389}b"));
        assert!(matches(&text("a..b", PATTERN_SIMPLE_GLOB), "a\u{1F389}b"));
    }

    #[test]
    fn advanced_glob_tokens_and_modifiers() {
        // a.+ : literal a, ANY, ONE_OR_MORE
        let a_any_plus = advanced(&[97, -4, -8]);
        assert!(matches(&a_any_plus, "ab") && matches(&a_any_plus, "abc"));
        assert!(!matches(&a_any_plus, "a"));
        // [a-c]x* : a set with a range, literal x, ZERO_OR_MORE
        let set_then_x = advanced(&[-1, 97, 99, -3, 120, -7]);
        assert!(matches(&set_then_x, "b") && matches(&set_then_x, "bxxx"));
        assert!(!matches(&set_then_x, "d"));
        assert!(!matches(&set_then_x, "x"));
        assert!(!matches(&set_then_x, "bxa"));
        // [^a]
        let not_a = advanced(&[-2, 97, 97, -3]);
        assert!(matches(&not_a, "b"));
        assert!(!matches(&not_a, "a"));
        // a{2}: literal a, RANGE_START, 2, 2, RANGE_STOP
        let twice = advanced(&[97, -5, 2, 2, -6]);
        assert!(matches(&twice, "aa"));
        assert!(!matches(&twice, "a"));
        assert!(!matches(&twice, "aaa"));
    }
}
