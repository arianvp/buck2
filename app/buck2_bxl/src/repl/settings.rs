/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! `:set` on the daemon's side: the settings that change what the session computes (the target
//! platform and the modifiers of `ctx`, the queries, `:build`, `:run` and `:bxl`). The client
//! keeps the others (colour, timing, completion).

use buck2_cli_proto::TargetCfg;
use buck2_cli_proto::repl_error;
use buck2_repl_syntax::commands::SETTINGS;
use buck2_repl_syntax::commands::SetArgs;
use buck2_repl_syntax::commands::SettingSide;
use buck2_repl_syntax::commands::SettingSpec;
use buck2_repl_syntax::commands::setting_line;
use buck2_repl_syntax::text::shell_join;

use crate::repl::render::ReplFailure;

/// What `:set` asks of the driver.
#[derive(Clone, Debug)]
pub(crate) enum SetWork {
    /// Show a setting, or every setting of the daemon.
    Show(Option<&'static SettingSpec>),
    /// Set the target platform (checked first); empty for the default.
    TargetPlatforms(String),
    /// Set the modifiers; none clears them.
    Modifiers(Vec<String>),
}

fn usage(message: &dyn std::fmt::Display) -> ReplFailure {
    ReplFailure::new(repl_error::Kind::Usage, message)
}

/// What `:set <args>` asks of the daemon.
pub(crate) fn set_work(args: SetArgs) -> Result<SetWork, ReplFailure> {
    let SetArgs { key, value } = args;
    let Some(key) = key else {
        return Ok(SetWork::Show(None));
    };
    if key.side == SettingSide::Client {
        return Err(usage(&format_args!(
            "the setting `{}` is handled by the client",
            key.name
        )));
    }
    let Some(value) = value else {
        return Ok(SetWork::Show(Some(key)));
    };
    // `""` clears a setting.
    let value: Vec<String> = if value.iter().all(|v| v.is_empty()) {
        Vec::new()
    } else if value.iter().any(|v| v.is_empty()) {
        return Err(usage(&format_args!(
            "`{}` takes \"\" alone, to clear it",
            key.name
        )));
    } else {
        value
    };
    match key.name {
        "target_platforms" => match <[String; 1]>::try_from(value) {
            Ok([platform]) => Ok(SetWork::TargetPlatforms(platform)),
            Err(value) if value.is_empty() => Ok(SetWork::TargetPlatforms(String::new())),
            Err(_) => Err(usage(&"`target_platforms` takes one target")),
        },
        "modifiers" => Ok(SetWork::Modifiers(value)),
        name => Err(usage(&format_args!("unknown setting `{name}`"))),
    }
}

/// The value of a setting of the daemon, as `:set` shows it.
fn value(target_cfg: &TargetCfg, name: &str) -> String {
    match name {
        "target_platforms" if target_cfg.target_platform.is_empty() => {
            "\"\" (the default: each target's default_target_platform)".to_owned()
        }
        "target_platforms" => target_cfg.target_platform.clone(),
        "modifiers" if target_cfg.cli_modifiers.is_empty() => "\"\" (none)".to_owned(),
        "modifiers" => shell_join(&target_cfg.cli_modifiers),
        _ => String::new(),
    }
}

/// `:set` or `:set <key>`: one line per setting of the daemon.
pub(crate) fn show_settings(target_cfg: &TargetCfg, key: Option<&SettingSpec>) -> String {
    let lines: Vec<String> = SETTINGS
        .iter()
        .filter(|s| s.side == SettingSide::Server && key.is_none_or(|k| k.name == s.name))
        .map(|s| setting_line(s.name, &value(target_cfg, s.name)))
        .collect();
    lines.join("\n")
}

/// What a notice says once a setting changed.
pub(crate) fn setting_changed(target_cfg: &TargetCfg, name: &str) -> String {
    format!("{name} is now {}", value(target_cfg, name))
}

#[cfg(test)]
mod tests {
    use buck2_repl_syntax::commands::parse_set_args;

    use super::*;

    fn work(arg: &str) -> Result<SetWork, String> {
        set_work(parse_set_args(arg).unwrap()).map_err(|e| e.message)
    }

    #[test]
    fn test_set_work() {
        assert!(matches!(work(""), Ok(SetWork::Show(None))));
        assert!(matches!(work("modifiers"), Ok(SetWork::Show(Some(_)))));
        assert!(matches!(
            work("target_platforms //p:x"),
            Ok(SetWork::TargetPlatforms(p)) if p == "//p:x"
        ));
        assert!(matches!(
            work("target_platforms ''"),
            Ok(SetWork::TargetPlatforms(p)) if p.is_empty()
        ));
        assert!(work("target_platforms //a:b //c:d").is_err());
        assert!(matches!(
            work("modifiers a b"),
            Ok(SetWork::Modifiers(m)) if m == ["a", "b"]
        ));
        assert!(matches!(work("modifiers ''"), Ok(SetWork::Modifiers(m)) if m.is_empty()));
        assert!(work("modifiers a ''").is_err());
        assert!(work("color on").unwrap_err().contains("client"));
    }

    #[test]
    fn test_show() {
        let mut cfg = TargetCfg::default();
        let all = show_settings(&cfg, None);
        assert!(all.starts_with("target_platforms"), "{all}");
        assert!(all.contains("\nmodifiers"), "{all}");
        assert!(!all.contains("color"), "{all}");
        cfg.cli_modifiers = vec!["a b".to_owned(), "c".to_owned()];
        assert_eq!(
            setting_changed(&cfg, "modifiers"),
            "modifiers is now 'a b' c"
        );
    }
}
