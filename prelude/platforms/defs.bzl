# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _execution_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        label = name,
        configuration = cfg,
        executor_config = _executor_config(ctx),
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(label = str(name), configuration = cfg),
        ExecutionPlatformRegistrationInfo(
            platforms = [platform],
            exec_marker_constraint = get_exec_platform_marker(),
        ),
    ]

def _config_bool(key: str, default: bool) -> bool:
    value = read_root_config("execution_platform", key)
    if value == None:
        return default
    if value.lower() not in ("true", "false"):
        fail("[execution_platform] {} must be true or false, got `{}`".format(key, value))
    return value.lower() == "true"

def remote_execution_attrs() -> dict[str, typing.Any]:
    """`execution_platform` attributes from the root cell's .buckconfig.

    Remote execution is opt-in (the RE endpoints themselves go in
    [buck2_re_client]):

      [execution_platform]
        # Run actions remotely. Actions marked local_only (or prefer_local)
        # still run locally. Default: false.
        remote_enabled = true
        # Look up actions in the remote action cache. Default: remote_enabled.
        remote_cache_enabled = true
        # Upload results of local actions that allow it (allow_cache_upload)
        # to the remote action cache. Default: remote_enabled. No effect
        # unless remote_cache_enabled is true.
        allow_cache_uploads = true
        # Platform properties sent with every remote action, as
        # space-separated key=value pairs.
        remote_execution_properties = OSFamily=linux container-image=...

    Without remote_enabled, remote_cache_enabled and allow_cache_uploads
    still apply (a remote cache with local execution), and
    remote_execution_properties is ignored. Platform properties are part of
    an action's key, so that mode doesn't share cache entries with remote
    execution builds.

    The platform keeps the host's OS/CPU constraints either way, so
    toolchains select() the same way locally and remotely: RE workers must
    match the host's OS and CPU.
    """
    remote_enabled = _config_bool("remote_enabled", False)
    properties = {}
    for pair in (read_root_config("execution_platform", "remote_execution_properties") or "").split():
        key, sep, value = pair.partition("=")
        if not sep:
            fail("[execution_platform] remote_execution_properties: expected key=value, got `{}`".format(pair))
        properties[key] = value
    return {
        "allow_cache_uploads": _config_bool("allow_cache_uploads", remote_enabled),
        "remote_cache_enabled": _config_bool("remote_cache_enabled", remote_enabled),
        "remote_enabled": remote_enabled,
        "remote_execution_properties": properties,
    }

def _executor_config(ctx: AnalysisContext) -> CommandExecutorConfig:
    if not ctx.attrs.remote_enabled:
        return CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = False,
            remote_cache_enabled = ctx.attrs.remote_cache_enabled,
            allow_cache_uploads = ctx.attrs.allow_cache_uploads,
            remote_execution_use_case = "buck2-default",
            use_windows_path_separators = ctx.attrs.use_windows_path_separators,
        )
    return CommandExecutorConfig(
        local_enabled = True,
        remote_enabled = True,
        remote_cache_enabled = ctx.attrs.remote_cache_enabled,
        allow_cache_uploads = ctx.attrs.allow_cache_uploads,
        remote_execution_properties = ctx.attrs.remote_execution_properties,
        remote_execution_use_case = "buck2-default",
        # Run everything remotely except actions that must (or prefer to) run
        # locally, and don't retry failed remote actions locally: a failure on
        # RE is a real (hermeticity) problem, not a hiccup.
        use_limited_hybrid = True,
        use_windows_path_separators = ctx.attrs.use_windows_path_separators,
    )

execution_platform = rule(
    impl = _execution_platform_impl,
    attrs = {
        "allow_cache_uploads": attrs.bool(default = False),
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "remote_cache_enabled": attrs.bool(default = False),
        "remote_enabled": attrs.bool(default = False),
        "remote_execution_properties": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "use_windows_path_separators": attrs.bool(),
    },
)

def _host_cpu_configuration() -> str:
    arch = host_info().arch
    if arch.is_aarch64:
        return "prelude//cpu:arm64"
    elif arch.is_arm:
        return "prelude//cpu:arm32"
    elif arch.is_i386:
        return "prelude//cpu:x86_32"
    elif arch.is_riscv64:
        return "prelude//cpu:riscv64"
    else:
        return "prelude//cpu:x86_64"

def _host_os_configuration() -> str:
    os = host_info().os
    if os.is_macos:
        return "prelude//os:macos"
    elif os.is_windows:
        return "prelude//os:windows"
    else:
        return "prelude//os:linux"

host_configuration = struct(
    cpu = _host_cpu_configuration(),
    os = _host_os_configuration(),
)
