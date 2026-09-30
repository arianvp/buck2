/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Completion: the line editor's completer, and the hidden `:__complete` command, which runs
//! it on a given buffer and prints the candidates as JSON.
//!
//! The site of the cursor is classified locally ([`classify`]). Command names, help topics,
//! paths relative to the working directory (`:load`, `:bxl`, `load("…`, the files of a query)
//! and the words of queries are completed here; names, attributes, keyword arguments, target
//! patterns, labels of modules and their symbols, and the functions of a `.bxl` file
//! (`:bxl x.bxl:<TAB>`) by the daemon, which is asked with a `Complete` request and given a short
//! time to answer: Tab must not hang.
//!
//! What the daemon lists from DICE (the targets of a package, the files of a directory, the
//! symbols of a module, the query functions) is kept for a short while ([`LISTING_TTL`]), so
//! that more Tabs on the same listing are instant; an answer that comes too late is kept too, so
//! that the next Tab has it. Candidates are ranked by how they match the word (the word's prefix
//! first; ignoring case, then as a subsequence, only when nothing matches better).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use std::time::Instant;

use buck2_cli_proto::ReplCallStep;
use buck2_cli_proto::ReplCandidate;
use buck2_cli_proto::ReplChainStep;
use buck2_cli_proto::ReplComplete;
use buck2_cli_proto::ReplCompletions;
use buck2_cli_proto::ReplRequest;
use buck2_cli_proto::repl_candidate;
use buck2_cli_proto::repl_chain_step;
use buck2_cli_proto::repl_complete;
use buck2_cli_proto::repl_completions;
use buck2_cli_proto::repl_request;
use buck2_core::buck2_env;
use buck2_repl_syntax::candidates::insertion;
use buck2_repl_syntax::candidates::is_label;
use buck2_repl_syntax::candidates::load_listing;
use buck2_repl_syntax::candidates::short_display;
use buck2_repl_syntax::candidates::target_listing;
use buck2_repl_syntax::commands::ArgKind;
use buck2_repl_syntax::commands::COMMANDS;
use buck2_repl_syntax::commands::HELP_TOPICS;
use buck2_repl_syntax::commands::QueryDialect;
use buck2_repl_syntax::commands::split_bxl_function;
use buck2_repl_syntax::lexer::KEYWORDS;
use buck2_repl_syntax::matching::Ranked;
use buck2_repl_syntax::matching::match_tier;
use buck2_repl_syntax::query::LITERAL_CALLS;
use buck2_repl_syntax::query::OPERATOR_WORDS;
use buck2_repl_syntax::query::QueryArg;
use buck2_repl_syntax::query::QueryContext;
use buck2_repl_syntax::query::decode_query_args;
use buck2_repl_syntax::query::is_pattern_word;
use buck2_repl_syntax::site::SiteKind;
use buck2_repl_syntax::site::Step;
use buck2_repl_syntax::site::classify;
use buck2_repl_syntax::site::looks_like_pattern;
use rustyline::completion::Pair;
use rustyline::line_buffer::LineBuffer;

/// How long to wait for the daemon to complete a name, an attribute or a keyword argument
/// (from memory).
const STARLARK_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for the daemon to complete a target pattern (which may load a package), a
/// module to load or its symbols (which may load it), or a function of a `.bxl` file.
const TARGET_TIMEOUT: Duration = Duration::from_millis(1000);

/// Longest wait `BUCK2_REPL_COMPLETION_TIMEOUT_MS` may ask for.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a listing of the daemon is used again. Inputs clear them.
const LISTING_TTL: Duration = Duration::from_secs(10);

/// Most requests given up on whose late answers are kept.
const MAX_PENDING: usize = 64;

/// Most candidates completed here.
const MAX_LOCAL_CANDIDATES: usize = 500;

/// The extensions of the modules `load` and `:load` load.
const LOAD_EXTENSIONS: &[&str] = &[".bzl", ".bxl"];

/// The extension of the files `:bxl` runs functions of.
const BXL_EXTENSIONS: &[&str] = &[".bxl"];

/// Candidates for the word before the cursor.
#[derive(Debug)]
pub(crate) struct Completion {
    /// Byte offset where the word starts: a candidate replaces the text from here to the cursor.
    pub(crate) start: usize,
    pub(crate) status: repl_completions::Status,
    /// Why there are no candidates, or not all of them.
    pub(crate) message: String,
    pub(crate) candidates: Vec<ReplCandidate>,
}

impl Completion {
    fn new(start: usize, candidates: Vec<ReplCandidate>) -> Self {
        Completion {
            start,
            status: repl_completions::Status::Ok,
            message: String::new(),
            candidates,
        }
    }

    fn failed(start: usize, status: repl_completions::Status, message: &str) -> Self {
        Completion {
            start,
            status,
            message: message.to_owned(),
            candidates: Vec::new(),
        }
    }

    fn is_ok(&self) -> bool {
        self.status == repl_completions::Status::Ok
    }

    /// Adds the candidates of `other`, which completes the same word in another way (target
    /// patterns to query functions). Its message is kept if this one has none; the status is
    /// this one's.
    fn merge(&mut self, other: Completion) {
        if self.message.is_empty() {
            self.message = other.message;
        }
        self.candidates.extend(other.candidates);
    }
}

/// What a listing of the daemon lists: the request that asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ListingKey {
    kind: i32,
    prefix: String,
    module: String,
    dialect: i32,
}

impl ListingKey {
    fn of(request: &ReplComplete) -> Self {
        ListingKey {
            kind: request.kind,
            prefix: request.prefix.clone(),
            module: request.load_module.clone(),
            dialect: request.query_dialect,
        }
    }

    /// The functions of a query language do not change during a session.
    fn expires(&self) -> bool {
        self.kind != repl_complete::Kind::Query as i32
    }
}

/// Listings of the daemon, kept for a short while.
#[derive(Default)]
struct ListingCache {
    listings: HashMap<ListingKey, (Instant, Vec<ReplCandidate>)>,
    /// Requests for listings that were given up on, whose answers may still come.
    pending: HashMap<u64, ListingKey>,
}

impl ListingCache {
    fn get(&self, key: &ListingKey) -> Option<Vec<ReplCandidate>> {
        let (at, candidates) = self.listings.get(key)?;
        (!key.expires() || Instant::now() - *at < LISTING_TTL).then(|| candidates.clone())
    }

    /// Keeps an answer, if it is complete.
    fn put(&mut self, key: ListingKey, answer: &ReplCompletions) {
        if answer.status == repl_completions::Status::Ok as i32 && answer.message.is_empty() {
            self.listings
                .insert(key, (Instant::now(), answer.candidates.clone()));
        }
    }

    /// An answer to the request `id`, which came after the completion that asked for it gave
    /// up.
    fn late(&mut self, id: u64, answer: &ReplCompletions) {
        if let Some(key) = self.pending.remove(&id) {
            self.put(key, answer);
        }
    }

    fn give_up(&mut self, id: u64, key: ListingKey) {
        if self.pending.len() >= MAX_PENDING {
            self.pending.clear();
        }
        self.pending.insert(id, key);
    }

    fn clear(&mut self) {
        self.listings.clear();
    }
}

/// Completes inputs, asking the daemon when needed.
pub(crate) struct Completer {
    req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
    /// The daemon's answers to `Complete` requests, with their request ids.
    answers: Mutex<std::sync::mpsc::Receiver<(u64, ReplCompletions)>>,
    next_id: Arc<AtomicU64>,
    /// The client's working directory, which paths are relative to.
    cwd: PathBuf,
    starlark_timeout: Duration,
    target_timeout: Duration,
    cache: Mutex<ListingCache>,
}

impl Completer {
    pub(crate) fn new(
        req_tx: tokio::sync::mpsc::UnboundedSender<ReplRequest>,
        answers: std::sync::mpsc::Receiver<(u64, ReplCompletions)>,
        next_id: Arc<AtomicU64>,
        cwd: PathBuf,
    ) -> buck2_error::Result<Self> {
        // One timeout for every completion, if set (e.g. to wait longer on a slow machine).
        let timeout = buck2_env!("BUCK2_REPL_COMPLETION_TIMEOUT_MS", type=u64)?
            .map(|ms| Duration::from_millis(ms).min(MAX_TIMEOUT));
        Ok(Completer {
            req_tx,
            answers: Mutex::new(answers),
            next_id,
            cwd,
            starlark_timeout: timeout.unwrap_or(STARLARK_TIMEOUT),
            target_timeout: timeout.unwrap_or(TARGET_TIMEOUT),
            cache: Mutex::new(ListingCache::default()),
        })
    }

    /// Forgets the listings: an input may have changed what they list.
    pub(crate) fn clear_listings(&self) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The candidates for the cursor at byte `pos` of `buf`.
    pub(crate) fn complete(&self, buf: &str, pos: usize) -> Completion {
        let Some(site) = classify(buf, pos) else {
            return Completion::new(pos.min(buf.len()), Vec::new());
        };
        let start = site.start;
        let word = site.prefix();
        let mut completion = match &site.kind {
            SiteKind::Command { prefix } => Completion::new(start, commands(prefix)),
            SiteKind::CommandArg {
                arg: ArgKind::Topic,
                word,
                ..
            } => Completion::new(start, topics(word)),
            SiteKind::CommandArg {
                arg: ArgKind::Path,
                word,
                ..
            }
            | SiteKind::LoadPath { prefix: word } => self.load_paths(start, word, LOAD_EXTENSIONS),
            SiteKind::CommandArg {
                arg: ArgKind::BxlLabel,
                word,
                ..
            } => match split_bxl_function(word) {
                // `x.bxl:ma`: the functions of the file.
                Some((module, prefix)) => self.listing_for(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::BxlFunction as i32,
                        // Names starting with `_` are listed only for a prefix starting with `_`.
                        prefix: if prefix.starts_with('_') {
                            prefix.to_owned()
                        } else {
                            String::new()
                        },
                        load_module: module.to_owned(),
                        ..ReplComplete::default()
                    },
                    prefix,
                ),
                None => self.load_paths(start, word, BXL_EXTENSIONS),
            },
            SiteKind::CommandArg { word, .. } | SiteKind::TargetString { prefix: word } => {
                self.targets(start, word)
            }
            SiteKind::LoadSymbol { module, used, .. } => {
                let mut completion = self.listing_for(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::LoadSymbol as i32,
                        load_module: (*module).to_owned(),
                        ..ReplComplete::default()
                    },
                    word,
                );
                completion
                    .candidates
                    .retain(|c| !used.contains(&c.replacement.as_str()));
                completion
            }
            SiteKind::Query { dialect, context } => self.query(start, *dialect, context),
            SiteKind::Name { prefix } => {
                let completion = self.ask(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::Name as i32,
                        prefix: (*prefix).to_owned(),
                        ..ReplComplete::default()
                    },
                    self.starlark_timeout,
                );
                with_keywords(completion, KEYWORDS)
            }
            SiteKind::Attr {
                root,
                steps,
                prefix,
            } => self.ask(
                start,
                ReplComplete {
                    kind: repl_complete::Kind::Attr as i32,
                    prefix: (*prefix).to_owned(),
                    root: (*root).to_owned(),
                    steps: chain_steps(steps),
                    ..ReplComplete::default()
                },
                self.starlark_timeout,
            ),
            SiteKind::CallArg {
                root,
                steps,
                used,
                positional,
                prefix,
            } => {
                let completion = self.ask(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::Kwarg as i32,
                        prefix: (*prefix).to_owned(),
                        root: (*root).to_owned(),
                        steps: chain_steps(steps),
                        used_kwargs: used.iter().map(|u| (*u).to_owned()).collect(),
                        positional_args: u32::try_from(*positional).unwrap_or(u32::MAX),
                        ..ReplComplete::default()
                    },
                    self.starlark_timeout,
                );
                with_keywords(completion, ARGUMENT_KEYWORDS)
            }
        };
        completion.candidates = rank(std::mem::take(&mut completion.candidates), word);
        if let Some(quote) = closing_quote(&site.kind, buf, start, pos) {
            for c in &mut completion.candidates {
                if is_complete(&site.kind, c) {
                    c.replacement.push(quote);
                }
            }
        }
        for c in &mut completion.candidates {
            if c.display.is_empty() {
                c.display = short_display(&c.replacement, is_path(c.kind()));
            }
        }
        completion
    }

    /// Target patterns: the targets of a package or the subtargets of a target are a listing,
    /// kept for a while; the subdirectories of a directory are asked for each word (only those
    /// that match are checked for build files).
    fn targets(&self, start: usize, word: &str) -> Completion {
        let prefix = target_listing(word).unwrap_or(word);
        self.listing_for(
            start,
            ReplComplete {
                kind: repl_complete::Kind::TargetPattern as i32,
                prefix: prefix.to_owned(),
                ..ReplComplete::default()
            },
            word,
        )
    }

    /// Modules to load, with one of the `extensions`: labels (`//pkg:x.bzl`, `:x.bzl`) by the
    /// daemon, paths relative to the working directory here (and the cells a word without a
    /// slash may start, by the daemon).
    fn load_paths(&self, start: usize, word: &str, extensions: &[&str]) -> Completion {
        if !is_label(word) {
            let mut completion = Completion::new(start, paths(&self.cwd, word, extensions));
            if !word.contains('/') {
                let mut cells = self.listing(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::LoadPath as i32,
                        prefix: word.to_owned(),
                        ..ReplComplete::default()
                    },
                );
                cells
                    .candidates
                    .retain(|c| c.kind() == repl_candidate::Kind::Cell);
                completion.merge(cells);
            }
            return completion;
        }
        let prefix = load_listing(word).unwrap_or(word);
        let mut completion = self.listing_for(
            start,
            ReplComplete {
                kind: repl_complete::Kind::LoadPath as i32,
                prefix: prefix.to_owned(),
                ..ReplComplete::default()
            },
            word,
        );
        completion.candidates.retain(|c| {
            c.kind() != repl_candidate::Kind::File
                || extensions.iter().any(|e| c.replacement.ends_with(e))
        });
        completion
    }

    /// A word of a query, from what the cursor is in: the functions of the query language (and
    /// the words of the grammar), target patterns, files, or the binary operators.
    fn query(&self, start: usize, dialect: QueryDialect, context: &QueryContext) -> Completion {
        let word = context.word;
        if context.quoted {
            // A quoted word is a target, a file, a regex, ...: only patterns are completed.
            return if looks_like_pattern(word) {
                self.targets(start, word)
            } else {
                Completion::new(start, Vec::new())
            };
        }
        if context.operator {
            let operators = OPERATOR_WORDS
                .iter()
                .map(|op| candidate(&format!("{op} "), repl_candidate::Kind::Keyword, ""))
                .collect();
            return Completion::new(start, operators);
        }
        let functions = self.listing(
            start,
            ReplComplete {
                kind: repl_complete::Kind::Query as i32,
                query_dialect: match dialect {
                    QueryDialect::Uquery => repl_complete::QueryDialect::Uquery,
                    QueryDialect::Cquery => repl_complete::QueryDialect::Cquery,
                    QueryDialect::Aquery => repl_complete::QueryDialect::Aquery,
                } as i32,
                ..ReplComplete::default()
            },
        );
        let arg = match context.call {
            Some(("set", _)) => Some(QueryArg::Targets),
            Some(("fileset", _)) => Some(QueryArg::Files),
            Some((name, index)) => functions
                .candidates
                .iter()
                .find(|f| f.replacement.strip_suffix('(') == Some(name))
                .and_then(|f| decode_query_args(&f.detail).get(index).copied().flatten()),
            None => None,
        };
        match arg {
            Some(QueryArg::String | QueryArg::Integer) => Completion::new(start, Vec::new()),
            Some(QueryArg::Files) => Completion::new(start, paths(&self.cwd, word, &[""])),
            Some(QueryArg::Targets) if context.call.is_some_and(|(n, _)| n == "set") => {
                self.targets(start, word)
            }
            _ if is_pattern_word(word) => self.targets(start, word),
            // A function, or a target pattern relative to the working directory (`lib:`).
            _ => {
                let mut completion = functions;
                completion
                    .candidates
                    .extend(LITERAL_CALLS.iter().map(|name| {
                        candidate(&format!("{name}("), repl_candidate::Kind::Function, "")
                    }));
                if !word.is_empty() {
                    completion.merge(self.targets(start, word));
                }
                completion
            }
        }
    }

    /// The listing `request` (for a shorter prefix than `word`, the prefix the candidates are
    /// for), filtered here; or, when the daemon could not list every candidate (too many), the
    /// candidates for `word` itself.
    fn listing_for(&self, start: usize, request: ReplComplete, word: &str) -> Completion {
        let completion = self.listing(start, request.clone());
        if completion.is_ok() && !completion.message.is_empty() && request.prefix != word {
            return self.listing(
                start,
                ReplComplete {
                    prefix: word.to_owned(),
                    ..request
                },
            );
        }
        completion
    }

    /// A listing of the daemon: kept, or asked for (and kept).
    fn listing(&self, start: usize, request: ReplComplete) -> Completion {
        let key = ListingKey::of(&request);
        self.keep_late_answers();
        if let Some(candidates) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Completion::new(start, candidates);
        }
        let (id, completion) = self.ask_id(start, request, self.target_timeout);
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        match completion.status {
            repl_completions::Status::Ok if completion.message.is_empty() => {
                cache
                    .listings
                    .insert(key, (Instant::now(), completion.candidates.clone()));
            }
            // Still loading: the answer is kept when it comes.
            repl_completions::Status::Timeout => cache.give_up(id, key),
            _ => {}
        }
        completion
    }

    /// Keeps the answers that came after the completions that asked for them gave up.
    fn keep_late_answers(&self) {
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok((id, answer)) = answers.try_recv() {
            self.cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .late(id, &answer);
        }
    }

    /// Asks the daemon, and waits for its answer at most `timeout`.
    fn ask(&self, start: usize, complete: ReplComplete, timeout: Duration) -> Completion {
        self.ask_id(start, complete, timeout).1
    }

    /// Asks the daemon, and waits for its answer at most `timeout`. Returns the id of the
    /// request too.
    fn ask_id(&self, start: usize, complete: ReplComplete, timeout: Duration) -> (u64, Completion) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ReplRequest {
            id,
            request: Some(repl_request::Request::Complete(complete)),
        };
        if self.req_tx.send(request).is_err() {
            return (
                id,
                Completion::failed(
                    start,
                    repl_completions::Status::Error,
                    "the session is over",
                ),
            );
        }
        let started = Instant::now();
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let left = timeout.saturating_sub(Instant::now() - started);
            match answers.recv_timeout(left) {
                Ok((answer_id, answer)) if answer_id == id => {
                    return (
                        id,
                        Completion {
                            start,
                            status: answer.status(),
                            message: answer.message,
                            candidates: if answer.status == repl_completions::Status::Ok as i32 {
                                answer.candidates
                            } else {
                                Vec::new()
                            },
                        },
                    );
                }
                // The answer to an earlier request, which was given up on.
                Ok((answer_id, answer)) => self
                    .cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .late(answer_id, &answer),
                Err(RecvTimeoutError::Timeout) => {
                    return (
                        id,
                        Completion::failed(
                            start,
                            repl_completions::Status::Timeout,
                            "the daemon did not answer in time",
                        ),
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return (
                        id,
                        Completion::failed(
                            start,
                            repl_completions::Status::Error,
                            "the session is over",
                        ),
                    );
                }
            }
        }
    }
}

fn candidate(replacement: &str, kind: repl_candidate::Kind, detail: &str) -> ReplCandidate {
    ReplCandidate {
        replacement: replacement.to_owned(),
        display: String::new(),
        kind: kind as i32,
        detail: detail.to_owned(),
    }
}

fn chain_steps(steps: &[Step]) -> Vec<ReplChainStep> {
    steps
        .iter()
        .map(|step| ReplChainStep {
            step: Some(match step {
                Step::Attr(name) => repl_chain_step::Step::Attr((*name).to_owned()),
                Step::Call => repl_chain_step::Step::Call(ReplCallStep {}),
            }),
        })
        .collect()
}

/// The keywords that may start an argument of a call.
const ARGUMENT_KEYWORDS: &[&str] = &["lambda", "not"];

/// Adds `keywords` to an answer with names, if the daemon answered.
fn with_keywords(mut completion: Completion, keywords: &[&str]) -> Completion {
    if completion.is_ok() {
        completion.candidates.extend(
            keywords
                .iter()
                .map(|k| candidate(k, repl_candidate::Kind::Keyword, "")),
        );
    }
    completion
}

/// The candidates that match `word` best, sorted (keyword arguments first), each replacement
/// once.
fn rank(candidates: Vec<ReplCandidate>, word: &str) -> Vec<ReplCandidate> {
    let mut ranked = Ranked::default();
    for c in candidates {
        if let Some(tier) = match_tier(word, &c.replacement) {
            ranked.offer(tier, c);
        }
    }
    let mut candidates = ranked.into_items();
    let group = |c: &ReplCandidate| c.kind() != repl_candidate::Kind::Kwarg;
    candidates.sort_by(|a, b| (group(a), &a.replacement).cmp(&(group(b), &b.replacement)));
    candidates.dedup_by(|a, b| a.replacement == b.replacement);
    candidates
}

/// The quote to close the string the word is in, if a complete candidate should close it: the
/// word is a target pattern, a module or a symbol of a `load` in a string, and no quote follows
/// the cursor.
fn closing_quote(site: &SiteKind, buf: &str, start: usize, pos: usize) -> Option<char> {
    if !matches!(
        site,
        SiteKind::TargetString { .. } | SiteKind::LoadPath { .. } | SiteKind::LoadSymbol { .. }
    ) {
        return None;
    }
    let quote = buf
        .get(..start)?
        .chars()
        .last()
        .filter(|c| matches!(c, '"' | '\''))?;
    (!buf.get(pos..)?.starts_with(quote)).then_some(quote)
}

/// Whether the candidate completes the string it is in: a target, a pattern, a module or a
/// symbol (not a directory, a package or a cell, which are continued).
fn is_complete(site: &SiteKind, c: &ReplCandidate) -> bool {
    match site {
        SiteKind::LoadSymbol { .. } => true,
        _ => matches!(
            c.kind(),
            repl_candidate::Kind::Target
                | repl_candidate::Kind::Pattern
                | repl_candidate::Kind::File
        ),
    }
}

/// Whether a candidate is a path (shown by its last part).
fn is_path(kind: repl_candidate::Kind) -> bool {
    matches!(
        kind,
        repl_candidate::Kind::Target
            | repl_candidate::Kind::Package
            | repl_candidate::Kind::Directory
            | repl_candidate::Kind::Pattern
            | repl_candidate::Kind::File
    )
}

/// The commands `:help` lists: the ones available (most P1 commands are not implemented yet).
fn available_commands() -> impl Iterator<Item = &'static buck2_repl_syntax::commands::CommandSpec> {
    COMMANDS.iter().filter(|c| !c.hidden && c.is_available())
}

/// Command names and aliases, with their colon.
fn commands(prefix: &str) -> Vec<ReplCandidate> {
    let mut candidates = Vec::new();
    for spec in available_commands() {
        for name in std::iter::once(&spec.name).chain(spec.aliases) {
            let name = format!(":{name}");
            if match_tier(prefix, &name).is_some() {
                candidates.push(candidate(
                    &name,
                    repl_candidate::Kind::Command,
                    spec.summary,
                ));
            }
        }
    }
    candidates
}

/// What `:help` takes: a command (with or without its colon) or a topic.
fn topics(word: &str) -> Vec<ReplCandidate> {
    let colon = if word.starts_with(':') { ":" } else { "" };
    let mut candidates = Vec::new();
    for spec in available_commands() {
        let name = format!("{colon}{}", spec.name);
        candidates.push(candidate(
            &name,
            repl_candidate::Kind::Command,
            spec.summary,
        ));
    }
    for topic in HELP_TOPICS {
        candidates.push(candidate(topic, repl_candidate::Kind::Keyword, ""));
    }
    candidates
}

/// Files with one of the `extensions` (those `:load` can load, the `.bxl` files of `:bxl`; `""`
/// for any file) and directories, relative to `cwd`. Labels (`//pkg:x.bzl`) are not paths.
fn paths(cwd: &Path, word: &str, extensions: &[&str]) -> Vec<ReplCandidate> {
    if word.starts_with(['/', '@']) || word.contains(':') {
        return Vec::new();
    }
    let (dir_part, fragment) = match word.rfind('/') {
        Some(i) => (
            word.get(..i + 1).unwrap_or(""),
            word.get(i + 1..).unwrap_or(""),
        ),
        None => ("", word),
    };
    let Ok(entries) = std::fs::read_dir(cwd.join(dir_part)) else {
        return Vec::new();
    };
    let mut ranked = Ranked::default();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') && !fragment.starts_with('.') {
            continue;
        }
        let Some(tier) = match_tier(fragment, &name) else {
            continue;
        };
        // Following symlinks.
        let is_dir = entry.path().is_dir();
        let c = if is_dir {
            candidate(
                &format!("{dir_part}{name}/"),
                repl_candidate::Kind::Directory,
                "",
            )
        } else if extensions.iter().any(|e| name.ends_with(e)) {
            candidate(&format!("{dir_part}{name}"), repl_candidate::Kind::File, "")
        } else {
            continue;
        };
        if ranked.tier() == Some(tier) && ranked.len() >= MAX_LOCAL_CANDIDATES {
            continue;
        }
        ranked.offer(tier, c);
    }
    ranked.into_items()
}

/// The line editor's completer.
pub(crate) struct ReplCompleter {
    completer: Arc<Completer>,
    /// The replacements of the last candidates, to tell a candidate chosen from the common
    /// prefix of several.
    last: Mutex<Vec<String>>,
}

impl ReplCompleter {
    pub(crate) fn new(completer: Arc<Completer>) -> Self {
        ReplCompleter {
            completer,
            last: Mutex::new(Vec::new()),
        }
    }
}

impl rustyline::completion::Completer for ReplCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let completion = self.completer.complete(line, pos);
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = completion
            .candidates
            .iter()
            .map(|c| c.replacement.clone())
            .collect();
        let candidates = completion
            .candidates
            .into_iter()
            .map(|c| Pair {
                display: c.display,
                replacement: c.replacement,
            })
            .collect();
        Ok((completion.start, candidates))
    }

    /// Inserts the candidate chosen. A whole candidate replaces the rest of the identifier under
    /// the cursor too (`ctx.cq▮uery()` becomes `ctx.cquery()`), and does not repeat the `(`,
    /// `=` or quote that it ends with and that follows (then the cursor goes past it).
    fn update(
        &self,
        line: &mut LineBuffer,
        start: usize,
        elected: &str,
        cl: &mut rustyline::Changeset,
    ) {
        let end = line.pos();
        let whole = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|c| c == elected);
        let (replaced, insert, skip) = if whole {
            insertion(line.as_str(), end, elected)
        } else {
            (0, elected, 0)
        };
        line.replace(start..end + replaced, insert, cl);
        let pos = line.pos() + skip;
        if pos <= line.len() && line.as_str().is_char_boundary(pos) {
            line.set_pos(pos);
        }
    }
}

/// `:__complete {"buf": ..., "pos": N}`: the candidates of the buffer as one line of JSON,
/// `{"start": S, "status": "ok", "candidates": [{"replacement": ..., "display": ..., "kind":
/// ...}]}`, or an error message (without `error: `).
pub(crate) fn complete_command(completer: &Completer, arg: &str) -> Result<String, String> {
    const USAGE: &str = "`:__complete` takes a JSON object {\"buf\": <input>, \"pos\": <byte offset of the cursor>}";
    let args: serde_json::Value = serde_json::from_str(arg).map_err(|e| format!("{USAGE}: {e}"))?;
    let Some(buf) = args.get("buf").and_then(|b| b.as_str()) else {
        return Err(format!("{USAGE}: `buf` must be a string"));
    };
    // The end of the buffer if not given.
    let pos = match args.get("pos") {
        None | Some(serde_json::Value::Null) => buf.len(),
        Some(pos) => match pos.as_u64().and_then(|p| usize::try_from(p).ok()) {
            Some(pos) => pos,
            None => return Err(format!("{USAGE}: `pos` must be a non-negative integer")),
        },
    };
    if !buf.is_char_boundary(pos) {
        return Err(format!(
            "`:__complete`: `pos` ({pos}) is not a character boundary of `buf` ({} bytes)",
            buf.len()
        ));
    }
    let completion = completer.complete(buf, pos);
    let candidates: Vec<serde_json::Value> = completion
        .candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "replacement": c.replacement,
                "display": c.display,
                "kind": c.kind().as_str_name().to_ascii_lowercase(),
            })
        })
        .collect();
    let mut out = serde_json::json!({
        "start": completion.start,
        "status": completion.status.as_str_name().to_ascii_lowercase(),
        "candidates": candidates,
    });
    if !completion.message.is_empty() {
        out["message"] = serde_json::Value::String(completion.message);
    }
    Ok(out.to_string())
}
