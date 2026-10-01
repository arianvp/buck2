#!/usr/bin/env python3
"""Summarize how a buck2 command's actions were executed.

    buck2 log show | re_stats.py [--list]

Reads the event log of the last command (or pipe any `buck2 log show` output)
and prints, per action category, how many actions ran locally, ran on RE, or
were served from the action cache, plus cache upload results for local
actions. `--list` also lists every action that was not a cache hit.

Actions buck2 performs itself (writes, symlinks, copies) are counted
separately: they never run remotely and are never cached.
"""

import collections
import json
import sys

# buck2_data.ActionExecutionKind
KINDS = {
    0: "not_set",
    1: "local",
    2: "remote",
    3: "cache",
    4: "simple",
    6: "deferred",
    7: "local_dep_file",
    8: "local_worker",
    9: "remote_dep_file_cache",
    10: "local_action_cache",
    11: "remote_worker",
}

# buck2_data.UploadResult
UPLOADS = {
    0: "did_not_upload",
    1: "uploaded",
    2: "not_attempted",
    3: "non_command_action",
    4: "action_not_successful",
    5: "remote_dep_file_cache_hit",
    6: "upload_not_allowed",
    7: "executor_upload_disabled",
    8: "non_local_execution",
    9: "rejected_output_exceeds_limit",
    10: "rejected_permission_denied",
    11: "rejected_symlink_output",
    12: "failed_upload_action_blobs",
    13: "failed_upload_outputs",
}

CACHED = {"cache", "remote_dep_file_cache", "local_action_cache"}


def main() -> None:
    list_misses = "--list" in sys.argv[1:]
    by_category = collections.defaultdict(collections.Counter)
    uploads = collections.Counter()
    misses = []
    for line in sys.stdin:
        if '"SpanEnd"' not in line or '"ActionExecution"' not in line:
            continue
        event = json.loads(line)["Event"]["data"]["SpanEnd"]["data"]
        action = event.get("ActionExecution")
        if action is None:
            continue
        kind = KINDS.get(action.get("execution_kind", 0), "unknown")
        name = action.get("name", {})
        category = name.get("category", "?")
        if kind == "simple":
            by_category["(buck2-internal)"][kind] += 1
            continue
        by_category[category][kind] += 1
        if action.get("failed"):
            by_category[category]["failed"] += 1
        if kind in ("local", "local_worker"):
            uploads[(category, UPLOADS.get(action.get("cache_upload_result", 0), "?"))] += 1
        if kind not in CACHED and kind != "deferred":
            misses.append((category, name.get("identifier", ""), kind))

    total = collections.Counter()
    width = max([len(c) for c in by_category] + [8])
    print(f"{'category':<{width}}  cache  remote  local  other")
    for category in sorted(by_category):
        counts = by_category[category]
        if category != "(buck2-internal)":
            total.update(counts)
        other = sum(v for k, v in counts.items() if k not in ("cache", "remote", "local", "failed"))
        print(f"{category:<{width}}  {counts['cache']:>5}  {counts['remote']:>6}  {counts['local']:>5}  {other:>5}"
              + (f"  ({counts['failed']} failed)" if counts["failed"] else ""))

    commands = sum(v for k, v in total.items() if k not in ("deferred", "failed"))
    cached = sum(total[k] for k in CACHED)
    print()
    print(f"command actions: {commands}  cache hits: {cached}"
          + (f" ({100.0 * cached / commands:.1f}%)" if commands else "")
          + f"  remote: {total['remote']}  local: {total['local']}  deferred downloads: {total['deferred']}")
    if uploads:
        print("local action cache uploads: " + ", ".join(f"{c} {r}: {n}" for (c, r), n in sorted(uploads.items())))
    if list_misses:
        print()
        for category, identifier, kind in sorted(misses):
            print(f"{kind:>8}  {category}  {identifier}")


if __name__ == "__main__":
    main()
