/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `buck2 repl --json`: one JSON object per input on stdout (JSON Lines), and nothing else there.
//!
//! What an input writes (`print()`, `ctx.output.print`, `ctx.output.stream`, the text of
//! `:print`, `:help`, ...) is captured into the `stdout` and `stderr` fields of its record
//! instead of being written out: [`JsonCapture`] gets the output the daemon sends (from the
//! partial result handler), the console messages and streamed output of its events (from its
//! [`subscriber`](JsonCapture::subscriber)), and what the client itself prints for the input.
//!
//! A record:
//!
//! ```json
//! {"n":2,"input":"x + 1","ok":true,"type":"int","text":"2","json":2,"stdout":"","stderr":"","wait_ms":0,"eval_ms":3,"sources_changed":false}
//! ```
//!
//! `n` is the input's number (the `N` of `<repl:N>` in errors), `null` for an input the client
//! handles alone (`:help`, `:hist`, ...). `type`, `text` (and `truncated`, `json`) are there when
//! the input has a value; `json` when the value has a JSON form of at most 1 MiB. `run` is the
//! command line of `:run`, which is not run (as with `--print`, `stdout` has the command line,
//! quoted for a shell). `error` (`kind`, `message`) is there when `ok` is
//! false. `notices` lists what the daemon said about the input (`loaded ...`). `wait_ms`,
//! `eval_ms` and `sources_changed` are there when the daemon answered the input.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use async_trait::async_trait;
use buck2_cli_proto::ReplDone;
use buck2_cli_proto::ReplNotice;
use buck2_cli_proto::ReplRun;
use buck2_cli_proto::ReplValue;
use buck2_cli_proto::repl_done;
use buck2_cli_proto::repl_error;
use buck2_cli_proto::repl_notice;
use buck2_cli_proto::repl_output;
use buck2_client_ctx::subscribers::subscriber::EventSubscriber;
use buck2_events::BuckEvent;
use dupe::Dupe;

/// Most bytes kept of what one input writes to each of stdout and stderr: past that, the output
/// is cut and the record says so (`stdout_truncated`, `stderr_truncated`).
const MAX_CAPTURE_BYTES: usize = 64 << 20;

/// What the input that runs writes, captured for its record.
#[derive(Clone, Debug, Default, Dupe)]
pub(crate) struct JsonCapture(Arc<Mutex<Captured>>);

/// Output captured so far.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_cut: bool,
    stderr_cut: bool,
}

impl JsonCapture {
    fn lock(&self) -> MutexGuard<'_, Captured> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `data` to what was written to `channel`.
    pub(crate) fn write(&self, channel: repl_output::Channel, data: &[u8]) {
        let mut captured = self.lock();
        let Captured {
            stdout,
            stderr,
            stdout_cut,
            stderr_cut,
        } = &mut *captured;
        let (buf, cut) = match channel {
            repl_output::Channel::Stdout => (stdout, stdout_cut),
            repl_output::Channel::Stderr => (stderr, stderr_cut),
        };
        let room = MAX_CAPTURE_BYTES.saturating_sub(buf.len());
        if data.len() > room {
            *cut = true;
        }
        buf.extend_from_slice(data.get(..room.min(data.len())).unwrap_or_default());
    }

    /// Appends `text` and a newline to what was written to `channel`.
    pub(crate) fn write_line(&self, channel: repl_output::Channel, text: &str) {
        self.write(channel, text.as_bytes());
        if !text.ends_with('\n') {
            self.write(channel, b"\n");
        }
    }

    /// Takes what was captured so far.
    pub(crate) fn take(&self) -> Captured {
        std::mem::take(&mut *self.lock())
    }

    /// Captures the events that consoles print: `ctx.output.stream` output (stdout) and console
    /// messages and warnings (stderr), such as the help of a BXL function.
    pub(crate) fn subscriber(&self) -> Box<dyn EventSubscriber> {
        Box::new(CaptureSubscriber(self.dupe()))
    }
}

struct CaptureSubscriber(JsonCapture);

#[async_trait]
impl EventSubscriber for CaptureSubscriber {
    fn name(&self) -> &'static str {
        "repl-json-capture"
    }

    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> buck2_error::Result<()> {
        use buck2_data::instant_event::Data;

        for event in events {
            let buck2_data::buck_event::Data::Instant(instant) = event.data() else {
                continue;
            };
            match &instant.data {
                // Written as it is by the consoles (on stdout).
                Some(Data::StreamingOutput(output)) => self
                    .0
                    .write(repl_output::Channel::Stdout, output.message.as_bytes()),
                // One line each on stderr.
                Some(Data::ConsoleMessage(message)) => self
                    .0
                    .write_line(repl_output::Channel::Stderr, &message.message),
                Some(Data::ConsoleWarning(warning)) => self
                    .0
                    .write_line(repl_output::Channel::Stderr, &warning.message),
                _ => {}
            }
        }
        Ok(())
    }
}

/// Why an input the client handled failed, for the `kind` of its error.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ClientError {
    /// A command or its arguments are wrong (`:zz`, `:help nope`, `:set x`).
    Usage,
    /// A file could not be read or written, or a program could not be started.
    Io,
    /// The command of `:!` did not exit with 0.
    Exit,
    /// The session ended before the input was answered.
    Lost,
    /// The command of `:!` was interrupted (Ctrl-C).
    Interrupted,
}

impl ClientError {
    fn kind(self) -> &'static str {
        match self {
            ClientError::Usage => "usage",
            ClientError::Io => "io",
            ClientError::Exit => "exit",
            ClientError::Lost => "lost",
            ClientError::Interrupted => "interrupted",
        }
    }
}

/// The record of one input, built while it runs.
#[derive(Debug)]
pub(crate) struct JsonRecord {
    number: Option<u32>,
    input: String,
    /// The file of the command line this input comes from.
    file: Option<String>,
    /// The first failure: its kind and message.
    error: Option<(String, String)>,
    value: Option<ReplValue>,
    run: Option<ReplRun>,
    notices: Vec<ReplNotice>,
    /// `wait_ms`, `eval_ms`, `sources_changed` of the daemon's answer.
    answer: Option<(u64, u64, bool)>,
}

impl JsonRecord {
    pub(crate) fn new(input: String, file: Option<String>) -> Self {
        JsonRecord {
            number: None,
            input,
            file,
            error: None,
            value: None,
            run: None,
            notices: Vec::new(),
            answer: None,
        }
    }

    /// The input (of a file) is known.
    pub(crate) fn set_input(&mut self, input: &str) {
        input.clone_into(&mut self.input);
    }

    /// The input was sent to the daemon as `<repl:number>`.
    pub(crate) fn set_number(&mut self, number: u32) {
        self.number = Some(number);
    }

    pub(crate) fn notice(&mut self, notice: &ReplNotice) {
        self.notices.push(notice.clone());
    }

    /// The input failed (only the first failure is kept).
    pub(crate) fn fail(&mut self, kind: ClientError, message: &str) {
        self.fail_with(kind.kind(), message);
    }

    fn fail_with(&mut self, kind: &str, message: &str) {
        if self.error.is_none() {
            self.error = Some((kind.to_owned(), message.trim_end_matches('\n').to_owned()));
        }
    }

    /// The daemon's answer to the input.
    pub(crate) fn done(&mut self, done: &ReplDone) {
        self.answer = Some((done.wait_ms, done.eval_ms, done.sources_changed));
        match &done.outcome {
            None => {}
            Some(repl_done::Outcome::Value(value)) => self.value = Some(value.clone()),
            Some(repl_done::Outcome::Run(run)) => self.run = Some(run.clone()),
            Some(repl_done::Outcome::Error(error)) => {
                self.fail_with(error_kind(error.kind()), &error.message)
            }
        }
    }

    /// The record as one line of JSON, with what the input wrote.
    pub(crate) fn into_line(self, captured: Captured) -> String {
        let mut line = JsonLine::default();
        line.field("n", self.number.into());
        line.field("input", self.input.into());
        if let Some(file) = self.file {
            line.field("file", file.into());
        }
        line.field("ok", self.error.is_none().into());
        if let Some(value) = self.value {
            line.field("type", value.r#type.into());
            line.field("text", value.text.into());
            if value.truncated {
                line.field("truncated", true.into());
            }
            if let Some(json) = value.json {
                // The daemon's JSON is embedded as it is, once checked to be JSON.
                if let Ok(raw) = serde_json::value::RawValue::from_string(json) {
                    line.raw_field("json", raw.get());
                }
            }
        }
        if let Some(run) = self.run {
            let mut fields = serde_json::Map::new();
            fields.insert("argv".to_owned(), run.argv.into());
            fields.insert("label".to_owned(), run.label.into());
            if !run.cwd.is_empty() {
                fields.insert("cwd".to_owned(), run.cwd.into());
            }
            line.field("run", fields.into());
        }
        if let Some((kind, message)) = self.error {
            let mut fields = serde_json::Map::new();
            fields.insert("kind".to_owned(), kind.into());
            fields.insert("message".to_owned(), message.into());
            line.field("error", fields.into());
        }
        if !self.notices.is_empty() {
            let notices: Vec<serde_json::Value> = self
                .notices
                .into_iter()
                .map(|notice| {
                    let level = match notice.level() {
                        repl_notice::Level::Info => "info",
                        repl_notice::Level::Warning => "warning",
                    };
                    let mut fields = serde_json::Map::new();
                    fields.insert("level".to_owned(), level.into());
                    fields.insert("text".to_owned(), notice.text.into());
                    fields.into()
                })
                .collect();
            line.field("notices", notices.into());
        }
        line.field(
            "stdout",
            String::from_utf8_lossy(&captured.stdout)
                .into_owned()
                .into(),
        );
        line.field(
            "stderr",
            String::from_utf8_lossy(&captured.stderr)
                .into_owned()
                .into(),
        );
        if captured.stdout_cut {
            line.field("stdout_truncated", true.into());
        }
        if captured.stderr_cut {
            line.field("stderr_truncated", true.into());
        }
        if let Some((wait_ms, eval_ms, sources_changed)) = self.answer {
            line.field("wait_ms", wait_ms.into());
            line.field("eval_ms", eval_ms.into());
            line.field("sources_changed", sources_changed.into());
        }
        line.finish()
    }
}

/// The `kind` of a daemon error.
fn error_kind(kind: repl_error::Kind) -> &'static str {
    match kind {
        repl_error::Kind::Unknown => "unknown",
        repl_error::Kind::Syntax => "syntax",
        repl_error::Kind::Eval => "eval",
        repl_error::Kind::Buck => "buck",
        repl_error::Kind::Interrupted => "interrupted",
        repl_error::Kind::Unsupported => "unsupported",
        repl_error::Kind::Busy => "busy",
        repl_error::Kind::Usage => "usage",
        repl_error::Kind::Internal => "internal",
    }
}

/// A JSON object written field by field, in order (a `serde_json::Map` sorts its keys).
#[derive(Default)]
struct JsonLine(String);

impl JsonLine {
    fn field(&mut self, key: &str, value: serde_json::Value) {
        self.raw_field(key, &value.to_string());
    }

    /// A field whose value is JSON text.
    fn raw_field(&mut self, key: &str, json: &str) {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push_str(&serde_json::Value::from(key).to_string());
        self.0.push(':');
        self.0.push_str(json);
    }

    fn finish(mut self) -> String {
        if self.0.is_empty() {
            self.0.push('{');
        }
        self.0.push('}');
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record() {
        let mut record = JsonRecord::new("x + 1".to_owned(), None);
        record.set_number(2);
        record.done(&ReplDone {
            outcome: Some(repl_done::Outcome::Value(ReplValue {
                r#type: "int".to_owned(),
                text: "2".to_owned(),
                truncated: false,
                json: Some("2".to_owned()),
            })),
            wait_ms: 1,
            eval_ms: 3,
            sources_changed: false,
            heap_bytes: 10,
        });
        let capture = JsonCapture::default();
        capture.write(repl_output::Channel::Stdout, b"hi\n");
        assert_eq!(
            record.into_line(capture.take()),
            r#"{"n":2,"input":"x + 1","ok":true,"type":"int","text":"2","json":2,"stdout":"hi\n","stderr":"","wait_ms":1,"eval_ms":3,"sources_changed":false}"#
        );
        assert!(capture.take().stdout.is_empty());

        let mut record = JsonRecord::new(":zz".to_owned(), None);
        record.fail(ClientError::Usage, "error: unknown command `:zz`\n");
        record.fail(ClientError::Io, "error: second");
        let line = record.into_line(Captured::default());
        assert_eq!(
            line,
            r#"{"n":null,"input":":zz","ok":false,"error":{"kind":"usage","message":"error: unknown command `:zz`"},"stdout":"","stderr":""}"#
        );

        // A value whose JSON is not JSON has none.
        let mut record = JsonRecord::new("s".to_owned(), Some("f.star".to_owned()));
        record.done(&ReplDone {
            outcome: Some(repl_done::Outcome::Value(ReplValue {
                r#type: "str".to_owned(),
                text: "\"a\"".to_owned(),
                truncated: true,
                json: Some("[1,".to_owned()),
            })),
            ..ReplDone::default()
        });
        let line: serde_json::Value =
            serde_json::from_str(&record.into_line(Captured::default())).unwrap();
        assert_eq!(line["file"], "f.star");
        assert_eq!(line["truncated"], true);
        assert!(line.get("json").is_none());
    }

    #[test]
    fn test_capture_cap() {
        let capture = JsonCapture::default();
        capture.write(
            repl_output::Channel::Stderr,
            &vec![b'x'; MAX_CAPTURE_BYTES - 1],
        );
        capture.write(repl_output::Channel::Stderr, b"yz");
        let captured = capture.take();
        assert_eq!(captured.stderr.len(), MAX_CAPTURE_BYTES);
        assert_eq!(captured.stderr.last(), Some(&b'y'));
        assert!(captured.stderr_cut);
        assert!(!captured.stdout_cut);
    }
}
