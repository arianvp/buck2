/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! How well a completion candidate matches the word typed.
//!
//! Candidates that start with the word come first; only when there are none, candidates that
//! start with it ignoring (ASCII) case; only when there are none of those either, candidates
//! that contain its characters in order (a subsequence, ignoring case) and start with its first
//! character. A completion offers the candidates of the best tier that has any ([`Ranked`]).

/// How a candidate matches the word typed, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchTier {
    /// The candidate starts with the word.
    Prefix,
    /// The candidate starts with the word, ignoring ASCII case.
    CaseInsensitivePrefix,
    /// The candidate starts with the first character of the word and contains the others in
    /// order, ignoring ASCII case.
    Subsequence,
}

/// How `candidate` matches `word`, if it does.
pub fn match_tier(word: &str, candidate: &str) -> Option<MatchTier> {
    if candidate.starts_with(word) {
        return Some(MatchTier::Prefix);
    }
    if candidate
        .get(..word.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(word))
    {
        return Some(MatchTier::CaseInsensitivePrefix);
    }
    let mut wanted = word.chars();
    let first = wanted.next()?;
    let mut rest = candidate.chars();
    if !rest.next()?.eq_ignore_ascii_case(&first) {
        return None;
    }
    for c in wanted {
        if !rest.by_ref().any(|r| r.eq_ignore_ascii_case(&c)) {
            return None;
        }
    }
    Some(MatchTier::Subsequence)
}

/// How `candidate` matches `word` when it completes the last part of it: the part after the
/// last `/`, `:` or `[` of a path, a target, a label (`//pkg:y` completes `y`, as the daemon
/// matches the names it lists). The candidate must start with what comes before that part
/// exactly, and its rest is matched with [`match_tier`]. A leading `@` (of a cell, `@root//`)
/// is ignored on both sides.
pub fn match_last_part(word: &str, candidate: &str) -> Option<MatchTier> {
    let (word, candidate) = (without_at(word), without_at(candidate));
    let head = word.rfind(['/', ':', '[']).map_or(0, |i| i + 1);
    let fragment = word.get(head..)?;
    let rest = candidate.strip_prefix(word.get(..head)?)?;
    match_tier(fragment, rest)
}

fn without_at(s: &str) -> &str {
    s.strip_prefix('@').unwrap_or(s)
}

/// Items of the best tier offered so far.
#[derive(Debug, Clone)]
pub struct Ranked<T> {
    tier: Option<MatchTier>,
    items: Vec<T>,
}

impl<T> Default for Ranked<T> {
    fn default() -> Self {
        Ranked {
            tier: None,
            items: Vec::new(),
        }
    }
}

impl<T> Ranked<T> {
    /// Keeps `item` if it matches at least as well as the items kept so far; drops those if it
    /// matches better.
    pub fn offer(&mut self, tier: MatchTier, item: T) {
        match self.tier {
            Some(best) if tier > best => {}
            Some(best) if tier == best => self.items.push(item),
            _ => {
                self.tier = Some(tier);
                self.items.clear();
                self.items.push(item);
            }
        }
    }

    /// The tier of the items kept.
    pub fn tier(&self) -> Option<MatchTier> {
        self.tier
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn into_items(self) -> Vec<T> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_tier() {
        assert_eq!(match_tier("", "abc"), Some(MatchTier::Prefix));
        assert_eq!(match_tier("ab", "abc"), Some(MatchTier::Prefix));
        assert_eq!(match_tier("abc", "abc"), Some(MatchTier::Prefix));
        assert_eq!(
            match_tier("AB", "abc"),
            Some(MatchTier::CaseInsensitivePrefix)
        );
        assert_eq!(
            match_tier("Ctx", "ctx"),
            Some(MatchTier::CaseInsensitivePrefix)
        );
        assert_eq!(match_tier("rdp", "rdeps("), Some(MatchTier::Subsequence));
        assert_eq!(
            match_tier("cfgt", "configured_targets("),
            Some(MatchTier::Subsequence)
        );
        assert_eq!(
            match_tier("CT", "configured_targets("),
            Some(MatchTier::Subsequence)
        );
        // The first character must match.
        assert_eq!(match_tier("li", "allbuildfiles("), None);
        assert_eq!(match_tier("abcd", "abc"), None);
        assert_eq!(match_tier("x", ""), None);
        assert_eq!(match_tier("ba", "abc"), None);
        // Not ASCII: no panics, exact comparisons only.
        assert_eq!(match_tier("é", "étoile"), Some(MatchTier::Prefix));
        assert_eq!(match_tier("e", "étoile"), None);
        assert_eq!(match_tier("ét", "éxt"), Some(MatchTier::Subsequence));
        assert_eq!(match_tier("xé", "x"), None);
        assert_eq!(match_tier("a", "é"), None);
    }

    #[test]
    fn test_match_last_part() {
        // The last part is matched, and the rest must be there as typed.
        assert_eq!(
            match_last_part("//pkg:hel", "//pkg:helpers.bxl\""),
            Some(MatchTier::Prefix)
        );
        assert_eq!(match_last_part("//pkg:y", "//pkg:typed.bxl"), None);
        assert_eq!(match_last_part("//pkg:l", "//pkg:helpers.bxl"), None);
        assert_eq!(match_last_part("//big:9", "//big:t9"), None);
        assert_eq!(
            match_last_part("//big:t9", "//big:t90"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(
            match_last_part("//lib:C", "//lib:c"),
            Some(MatchTier::CaseInsensitivePrefix)
        );
        assert_eq!(
            match_last_part("//lb", "//lib/"),
            Some(MatchTier::Subsequence)
        );
        assert_eq!(match_last_part("//pkg:sub/x", "//pkg:sub.bzl"), None);
        assert_eq!(
            match_last_part("//x:y[o", "//x:y[out]"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(
            match_last_part("//x:y.bxl:MA", "//x:y.bxl:main"),
            Some(MatchTier::CaseInsensitivePrefix)
        );
        assert_eq!(match_last_part(":cq", ":cquery"), Some(MatchTier::Prefix));
        assert_eq!(match_last_part(":x", ":fix"), None);
        // Names, and cells with or without `@`.
        assert_eq!(
            match_last_part("rdp", "rdeps("),
            Some(MatchTier::Subsequence)
        );
        assert_eq!(
            match_last_part("pre", "@prelude//"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(
            match_last_part("@pre", "@prelude//"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(match_last_part("", "@prelude//"), Some(MatchTier::Prefix));
        assert_eq!(match_last_part("x", "@prelude//"), None);
        assert_eq!(
            match_last_part("root//pk", "@root//pkg/"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(
            match_last_part("root//pkg:h", "@root//pkg:helpers.bxl"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(match_last_part("root//pkg:h", "@other//pkg:h.bzl"), None);
        // Not ASCII.
        assert_eq!(match_last_part("é/", "é/a"), Some(MatchTier::Prefix));
        assert_eq!(match_last_part("a/é", "b/é"), None);
    }

    #[test]
    fn test_ranked() {
        let mut r = Ranked::default();
        assert!(r.is_empty());
        r.offer(MatchTier::Subsequence, "a");
        r.offer(MatchTier::Subsequence, "b");
        assert_eq!(r.tier(), Some(MatchTier::Subsequence));
        r.offer(MatchTier::CaseInsensitivePrefix, "c");
        r.offer(MatchTier::Subsequence, "d");
        r.offer(MatchTier::CaseInsensitivePrefix, "e");
        assert_eq!(r.len(), 2);
        r.offer(MatchTier::Prefix, "f");
        r.offer(MatchTier::CaseInsensitivePrefix, "g");
        assert_eq!(r.tier(), Some(MatchTier::Prefix));
        assert_eq!(r.into_items(), vec!["f"]);
    }
}
