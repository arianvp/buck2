/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! [`Candidates`]: the answer to a completion request, sorted and bounded, with the
//! candidates that match the word typed best ([`MatchTier`]).

use std::collections::BTreeMap;

use buck2_cli_proto::ReplCandidate;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_completions;
use buck2_repl_syntax::matching::MatchTier;
use buck2_repl_syntax::matching::match_tier;
use buck2_repl_syntax::text::truncate_to_bytes;

/// Most candidates in an answer.
pub(crate) const MAX_CANDIDATES: usize = 500;

/// Candidates longer than this are left out: nobody completes to them.
const MAX_CANDIDATE_BYTES: usize = 1 << 10;

/// Most bytes of candidates in an answer, which keeps its message within 64 KiB (INV-13).
const MAX_ANSWER_BYTES: usize = 60 << 10;

/// Longest message of an answer.
const MAX_MESSAGE_BYTES: usize = 2 << 10;

/// Longest detail (a type name) of a candidate.
const MAX_DETAIL_BYTES: usize = 128;

/// The candidates of a completion, by replacement: each replacement once, in order.
#[derive(Default)]
pub(crate) struct Candidates {
    by_replacement: BTreeMap<String, (repl_candidate::Kind, String)>,
    /// How the candidates offered match the word typed ([`offer`](Self::offer)).
    tier: Option<MatchTier>,
    /// Some candidates were left out.
    truncated: bool,
}

impl Candidates {
    /// Whether the answer holds as many candidates as it can: when candidates are added in
    /// order, the later ones are not needed (unless they would match the word better).
    pub(crate) fn is_full(&self) -> bool {
        self.by_replacement.len() >= MAX_CANDIDATES
            && self.tier.is_none_or(|tier| tier == MatchTier::Prefix)
    }

    /// Adds a candidate whose `name` completes `word` (the part of the word typed that it
    /// replaces), if it matches it at least as well as the candidates offered so far; those
    /// are dropped if it matches better. Candidates that start with the word come first; the
    /// others only when none does.
    pub(crate) fn offer(
        &mut self,
        word: &str,
        name: &str,
        replacement: String,
        kind: repl_candidate::Kind,
        detail: &str,
    ) {
        let Some(tier) = match_tier(word, name) else {
            return;
        };
        match self.tier {
            Some(best) if tier > best => return,
            Some(best) if tier < best => {
                self.by_replacement.clear();
                self.truncated = false;
            }
            _ => {}
        }
        self.tier = Some(tier);
        self.add(replacement, kind, detail);
    }

    /// Records that some candidates were left out.
    pub(crate) fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    /// Adds a candidate. The first one added with a replacement wins. `detail` is shown next to
    /// the candidate (e.g. its type). When there are too many, the last ones in order are left
    /// out, but keyword arguments are kept before any other candidate: they are few, and they
    /// are what `f(<TAB>` is for (names are offered with them).
    pub(crate) fn add(&mut self, replacement: String, kind: repl_candidate::Kind, detail: &str) {
        if replacement.len() > MAX_CANDIDATE_BYTES {
            self.truncated = true;
            return;
        }
        if !self.by_replacement.contains_key(&replacement) {
            self.by_replacement.insert(
                replacement,
                (kind, truncate_to_bytes(detail, MAX_DETAIL_BYTES).to_owned()),
            );
            if self.by_replacement.len() > MAX_CANDIDATES {
                let last_other = self
                    .by_replacement
                    .iter()
                    .rev()
                    .find(|(_, (kind, _))| *kind != repl_candidate::Kind::Kwarg)
                    .map(|(replacement, _)| replacement.clone());
                match last_other {
                    Some(replacement) => {
                        self.by_replacement.remove(&replacement);
                    }
                    None => {
                        self.by_replacement.pop_last();
                    }
                }
                self.truncated = true;
            }
        }
    }

    pub(crate) fn into_completions(self) -> ReplCompletions {
        let mut candidates = Vec::with_capacity(self.by_replacement.len());
        let mut bytes = 0usize;
        let mut truncated = self.truncated;
        // Keyword arguments first, so that the byte limit leaves out other candidates first.
        let (kwargs, others): (Vec<_>, Vec<_>) = self
            .by_replacement
            .into_iter()
            .partition(|(_, (kind, _))| *kind == repl_candidate::Kind::Kwarg);
        for (replacement, (kind, detail)) in kwargs.into_iter().chain(others) {
            // Replacement, detail, and a few bytes of framing for each.
            bytes = bytes.saturating_add(replacement.len() + detail.len() + 16);
            if bytes > MAX_ANSWER_BYTES {
                truncated = true;
                break;
            }
            candidates.push(ReplCandidate {
                replacement,
                // The client shows the replacement.
                display: String::new(),
                kind: kind as i32,
                detail,
            });
        }
        ReplCompletions {
            status: repl_completions::Status::Ok as i32,
            candidates,
            message: if truncated {
                "not every candidate is shown: type more of the word".to_owned()
            } else {
                String::new()
            },
        }
    }
}

/// An answer with no candidates.
pub(crate) fn completions_status(
    status: repl_completions::Status,
    message: &str,
) -> ReplCompletions {
    ReplCompletions {
        status: status as i32,
        candidates: Vec::new(),
        message: truncate_to_bytes(message, MAX_MESSAGE_BYTES).to_owned(),
    }
}
