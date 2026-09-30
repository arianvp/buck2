/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! [`ReplCtx`]: the command context of one request.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use buck2_certs::validate::CertState;
use buck2_cli_proto::client_context::ExitWhen;
use buck2_cli_proto::client_context::PreemptibleWhen;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern::ParsedPatternWithModifiers;
use buck2_core::pattern::pattern_type::ConfiguredProvidersPatternExtra;
use buck2_events::dispatch::EventDispatcher;
use buck2_execute::materialize::materializer::Materializer;
use buck2_fs::paths::file_name::FileName;
use buck2_fs::working_dir::AbsWorkingDir;
use buck2_hash::IntentionallyStdHashMap;
use buck2_repl_syntax::text::truncate_to_bytes;
use buck2_server_ctx::ctx::DiceAccessor;
use buck2_server_ctx::ctx::LockedPreviousCommandData;
use buck2_server_ctx::ctx::PrivateStruct;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::stderr_output_guard::StderrOutputGuard;
use dice::DiceComputations;
use dice_futures::cancellation::CancellationContext;

/// Longest input excerpt shown to other commands that wait for a request.
const MAX_ARGV_INPUT: usize = 60;

/// The session's command context with the DICE acquisition policy of one request.
///
/// Every method forwards to the session's context, except `dice_accessor`, which sets how the
/// request's transaction is taken. Use it as `(&ctx as &dyn ServerCommandContextTrait)
/// .with_dice_ctx(..)`.
pub(crate) struct ReplCtx<'a> {
    inner: &'a dyn ServerCommandContextTrait,
    preemptible: PreemptibleWhen,
    exit_when: ExitWhen,
    sanitized_argv: Vec<String>,
}

impl<'a> ReplCtx<'a> {
    /// For an evaluation: it runs to completion (it is only ever cancelled from the inside,
    /// never dropped, INV-8), waiting for other commands if needed. Other commands that wait for
    /// it see the first line of the input.
    pub(crate) fn eval(inner: &'a dyn ServerCommandContextTrait, input: &str) -> Self {
        let first_line = input.trim_start().lines().next().unwrap_or("");
        let mut sanitized_argv = vec!["buck2".to_owned(), "repl".to_owned()];
        if !first_line.is_empty() {
            let mut excerpt = truncate_to_bytes(first_line, MAX_ARGV_INPUT).to_owned();
            if excerpt.len() < first_line.len() {
                excerpt.push('…');
            }
            sanitized_argv.push(excerpt);
        }
        ReplCtx {
            inner,
            preemptible: PreemptibleWhen::Never,
            exit_when: ExitWhen::ExitNever,
            sanitized_argv,
        }
    }
}

#[async_trait]
impl ServerCommandContextTrait for ReplCtx<'_> {
    fn working_dir(&self) -> &ProjectRelativePath {
        self.inner.working_dir()
    }

    fn working_dir_abs(&self) -> &AbsWorkingDir {
        self.inner.working_dir_abs()
    }

    fn command_name(&self) -> &str {
        self.inner.command_name()
    }

    fn isolation_prefix(&self) -> &FileName {
        self.inner.isolation_prefix()
    }

    fn project_root(&self) -> &ProjectRoot {
        self.inner.project_root()
    }

    fn cert_state(&self) -> CertState {
        self.inner.cert_state()
    }

    fn materializer(&self) -> Arc<dyn Materializer> {
        self.inner.materializer()
    }

    async fn dice_accessor<'s>(
        &'s self,
        private: PrivateStruct,
    ) -> buck2_error::Result<DiceAccessor<'s>> {
        let mut accessor = self.inner.dice_accessor(private).await?;
        accessor.preemptible = self.preemptible;
        accessor.exit_when = self.exit_when;
        accessor.sanitized_argv = self.sanitized_argv.clone();
        Ok(accessor)
    }

    fn events(&self) -> &EventDispatcher {
        self.inner.events()
    }

    fn previous_command_data(&self) -> Arc<LockedPreviousCommandData> {
        self.inner.previous_command_data()
    }

    fn stderr(&self) -> buck2_error::Result<StderrOutputGuard<'_>> {
        self.inner.stderr()
    }

    async fn command_start_event(
        &self,
        data: buck2_data::command_start::Data,
    ) -> buck2_error::Result<buck2_data::CommandStart> {
        self.inner.command_start_event(data).await
    }

    async fn request_metadata(
        &self,
    ) -> buck2_error::Result<IntentionallyStdHashMap<String, String>> {
        self.inner.request_metadata().await
    }

    async fn config_metadata(
        &self,
        ctx: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<IntentionallyStdHashMap<String, String>> {
        self.inner.config_metadata(ctx).await
    }

    fn log_target_pattern(
        &self,
        providers_patterns: &[ParsedPattern<ConfiguredProvidersPatternExtra>],
    ) {
        self.inner.log_target_pattern(providers_patterns)
    }

    fn log_target_pattern_with_modifiers(
        &self,
        providers_patterns_with_modifiers: &[ParsedPatternWithModifiers<
            ConfiguredProvidersPatternExtra,
        >],
    ) {
        self.inner
            .log_target_pattern_with_modifiers(providers_patterns_with_modifiers)
    }

    fn cancellation_context(&self) -> &CancellationContext {
        self.inner.cancellation_context()
    }

    fn command_start(&self) -> Instant {
        self.inner.command_start()
    }
}
