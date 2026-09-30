/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The settings of `:set` that the client keeps: how results are shown and how long Tab waits.
//! (The daemon keeps the target platform and the modifiers.)

use std::time::Duration;

use buck2_repl_syntax::commands::SETTINGS;
use buck2_repl_syntax::commands::SettingSide;
use buck2_repl_syntax::commands::SettingSpec;
use buck2_repl_syntax::commands::setting_line;

use crate::complete::Completer;
use crate::complete::MAX_TIMEOUT;

/// A setting that is on, off, or decided by the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Switch {
    Auto,
    On,
    Off,
}

impl Switch {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Switch::Auto),
            "on" => Some(Switch::On),
            "off" => Some(Switch::Off),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Switch::Auto => "auto",
            Switch::On => "on",
            Switch::Off => "off",
        }
    }
}

/// The client's settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// Colour errors and notes.
    pub(crate) color: Switch,
    /// Show how long inputs take.
    pub(crate) timing: Switch,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            color: Switch::Auto,
            timing: Switch::Auto,
        }
    }
}

impl Settings {
    /// The value of the client's setting `name`, as `:set` shows it.
    pub(crate) fn value(self, name: &str, completer: &Completer) -> String {
        match name {
            "color" => self.color.as_str().to_owned(),
            "timing" => self.timing.as_str().to_owned(),
            "completion_timeout_ms" => match completer.timeout_override() {
                Some(timeout) => timeout.as_millis().to_string(),
                None => {
                    let (names, targets) = completer.timeouts();
                    format!(
                        "auto ({} for names, {} for targets and modules)",
                        names.as_millis(),
                        targets.as_millis()
                    )
                }
            },
            _ => String::new(),
        }
    }

    /// `:set <name> <value>` for a setting of the client. The error is a message (without
    /// `error: `).
    pub(crate) fn set(
        &mut self,
        spec: &SettingSpec,
        value: &[String],
        completer: &Completer,
    ) -> Result<(), String> {
        let value = match value {
            [value] => value.as_str(),
            _ => {
                return Err(format!("`{}` takes one value: {}", spec.name, spec.values));
            }
        };
        let bad = || format!("`{}` takes {}, not `{value}`", spec.name, spec.values);
        match spec.name {
            "color" => self.color = Switch::parse(value).ok_or_else(bad)?,
            "timing" => self.timing = Switch::parse(value).ok_or_else(bad)?,
            "completion_timeout_ms" => {
                let timeout = match value {
                    "auto" => None,
                    ms => match ms.parse::<u64>() {
                        Ok(ms) if ms > 0 && Duration::from_millis(ms) <= MAX_TIMEOUT => {
                            Some(Duration::from_millis(ms))
                        }
                        _ => {
                            return Err(format!(
                                "`{}` takes a number of milliseconds from 1 to {}, or `auto`",
                                spec.name,
                                MAX_TIMEOUT.as_millis()
                            ));
                        }
                    },
                };
                completer.set_timeout_override(timeout);
            }
            name => return Err(format!("the setting `{name}` is not the client's")),
        }
        Ok(())
    }

    /// The client's settings (or the one named), one line each.
    pub(crate) fn show(self, key: Option<&SettingSpec>, completer: &Completer) -> String {
        SETTINGS
            .iter()
            .filter(|s| s.side == SettingSide::Client && key.is_none_or(|k| k.name == s.name))
            .map(|s| setting_line(s.name, &self.value(s.name, completer)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
