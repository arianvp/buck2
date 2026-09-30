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
//! The site of the cursor is classified locally ([`classify`]). Command names, help topics and
//! the paths of `:load` are completed here; names, attributes and target patterns by the
//! daemon, which is asked with a `Complete` request and given a short time to answer: Tab must
//! not hang. A late answer is dropped when the next completion reads its own.

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
use buck2_repl_syntax::commands::ArgKind;
use buck2_repl_syntax::commands::COMMANDS;
use buck2_repl_syntax::commands::HELP_TOPICS;
use buck2_repl_syntax::commands::Priority;
use buck2_repl_syntax::lexer::KEYWORDS;
use buck2_repl_syntax::site::SiteKind;
use buck2_repl_syntax::site::Step;
use buck2_repl_syntax::site::classify;
use rustyline::completion::Pair;

/// How long to wait for the daemon to complete a name or an attribute (from memory).
const STARLARK_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for the daemon to complete a target pattern (which may load a package).
const TARGET_TIMEOUT: Duration = Duration::from_millis(1000);

/// Longest wait `BUCK2_REPL_COMPLETION_TIMEOUT_MS` may ask for.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Most candidates completed here.
const MAX_LOCAL_CANDIDATES: usize = 500;

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
        })
    }

    /// The candidates for the cursor at byte `pos` of `buf`.
    pub(crate) fn complete(&self, buf: &str, pos: usize) -> Completion {
        let Some(site) = classify(buf, pos) else {
            return Completion::new(pos.min(buf.len()), Vec::new());
        };
        let start = site.start;
        let prefix = site.prefix();
        let mut completion = match site.kind {
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
            } => Completion::new(start, paths(&self.cwd, word)),
            SiteKind::CommandArg { word, .. } | SiteKind::TargetString { prefix: word } => self
                .ask(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::TargetPattern as i32,
                        prefix: word.to_owned(),
                        ..ReplComplete::default()
                    },
                    self.target_timeout,
                ),
            SiteKind::Name { prefix } => {
                let mut completion = self.ask(
                    start,
                    ReplComplete {
                        kind: repl_complete::Kind::Name as i32,
                        prefix: prefix.to_owned(),
                        ..ReplComplete::default()
                    },
                    self.starlark_timeout,
                );
                if completion.status == repl_completions::Status::Ok {
                    completion.candidates.extend(
                        KEYWORDS
                            .iter()
                            .filter(|k| k.starts_with(prefix))
                            .map(|k| candidate(k, repl_candidate::Kind::Keyword, "")),
                    );
                }
                completion
            }
            SiteKind::Attr {
                root,
                steps,
                prefix,
            } => self.ask(
                start,
                ReplComplete {
                    kind: repl_complete::Kind::Attr as i32,
                    prefix: prefix.to_owned(),
                    root: root.to_owned(),
                    steps: steps
                        .iter()
                        .map(|step| ReplChainStep {
                            step: Some(match step {
                                Step::Attr(name) => repl_chain_step::Step::Attr((*name).to_owned()),
                                Step::Call => repl_chain_step::Step::Call(ReplCallStep {}),
                            }),
                        })
                        .collect(),
                    ..ReplComplete::default()
                },
                self.starlark_timeout,
            ),
        };
        // Candidates replace the word, so they must start with it.
        completion
            .candidates
            .retain(|c| c.replacement.starts_with(prefix));
        completion
            .candidates
            .sort_by(|a, b| a.replacement.cmp(&b.replacement));
        completion
            .candidates
            .dedup_by(|a, b| a.replacement == b.replacement);
        completion
    }

    /// Asks the daemon, and waits for its answer at most `timeout`.
    fn ask(&self, start: usize, complete: ReplComplete, timeout: Duration) -> Completion {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ReplRequest {
            id,
            request: Some(repl_request::Request::Complete(complete)),
        };
        if self.req_tx.send(request).is_err() {
            return Completion::failed(
                start,
                repl_completions::Status::Error,
                "the session is over",
            );
        }
        let started = Instant::now();
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let left = timeout.saturating_sub(Instant::now() - started);
            match answers.recv_timeout(left) {
                Ok((answer_id, answer)) if answer_id == id => {
                    return Completion {
                        start,
                        status: answer.status(),
                        message: answer.message,
                        candidates: if answer.status == repl_completions::Status::Ok as i32 {
                            answer.candidates
                        } else {
                            Vec::new()
                        },
                    };
                }
                // The answer to an earlier request, which was given up on.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Completion::failed(
                        start,
                        repl_completions::Status::Timeout,
                        "the daemon did not answer in time",
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Completion::failed(
                        start,
                        repl_completions::Status::Error,
                        "the session is over",
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

/// The commands `:help` lists: the ones available (P1 commands are not implemented yet).
fn available_commands() -> impl Iterator<Item = &'static buck2_repl_syntax::commands::CommandSpec> {
    COMMANDS
        .iter()
        .filter(|c| !c.hidden && c.priority == Priority::P0)
}

/// Command names and aliases, with their colon.
fn commands(prefix: &str) -> Vec<ReplCandidate> {
    let mut candidates = Vec::new();
    for spec in available_commands() {
        for name in std::iter::once(&spec.name).chain(spec.aliases) {
            let name = format!(":{name}");
            if name.starts_with(prefix) {
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
        if name.starts_with(word) {
            candidates.push(candidate(
                &name,
                repl_candidate::Kind::Command,
                spec.summary,
            ));
        }
    }
    for topic in HELP_TOPICS {
        if topic.starts_with(word) {
            candidates.push(candidate(topic, repl_candidate::Kind::Keyword, ""));
        }
    }
    candidates
}

/// Files that `:load` can load (`.bzl` and `.bxl`) and directories, relative to `cwd`. Labels
/// (`//pkg:x.bzl`) are not completed.
fn paths(cwd: &Path, word: &str) -> Vec<ReplCandidate> {
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
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(fragment) || (name.starts_with('.') && !fragment.starts_with('.')) {
            continue;
        }
        // Following symlinks.
        let is_dir = entry.path().is_dir();
        if is_dir {
            candidates.push(candidate(
                &format!("{dir_part}{name}/"),
                repl_candidate::Kind::Directory,
                "",
            ));
        } else if name.ends_with(".bzl") || name.ends_with(".bxl") {
            candidates.push(candidate(
                &format!("{dir_part}{name}"),
                repl_candidate::Kind::File,
                "",
            ));
        }
        if candidates.len() >= MAX_LOCAL_CANDIDATES {
            break;
        }
    }
    candidates
}

/// The line editor's completer.
pub(crate) struct ReplCompleter(pub(crate) Arc<Completer>);

impl rustyline::completion::Completer for ReplCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let completion = self.0.complete(line, pos);
        let candidates = completion
            .candidates
            .into_iter()
            .map(|c| Pair {
                display: if c.display.is_empty() {
                    c.replacement.clone()
                } else {
                    c.display
                },
                replacement: c.replacement,
            })
            .collect();
        Ok((completion.start, candidates))
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
                "display": if c.display.is_empty() { &c.replacement } else { &c.display },
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
