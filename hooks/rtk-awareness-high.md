# Command output

Command output here is condensed to save tokens, keeping every signal and dropping costly noise. Treat it as the complete result: run commands normally, and batch related commands into one call to avoid extra turns. Truncated results state their recovery path in their own output.

Exact text stays exact: file contents (`cat`, `head`, `sed -n`), `grep -o` and `grep -c` results, `jq -r` values, and `curl -o` / `-w` results come back unchanged, and in-place edits such as `sed -i` are never rewritten. Do not add `RTK_DISABLED=1` or `rtk bypass` to get exact text. Re-run a command as `rtk bypass <cmd>` only when its result is unusable: empty when output was clearly expected, contradicting its exit code, or garbled.

## About RTK

The condensing is done by RTK (Rust Token Killer), a CLI proxy. A hook
rewrites each shell command to `rtk <cmd>` before it runs; behavior and
exit code are unchanged, only the output is filtered. Commands RTK has no
filter for run as-is.

- `rtk gain` / `rtk gain --history` — token savings, overall and per command.
- `rtk bypass <cmd>` — run a command unfiltered, still tracked.
- `RTK_DISABLED=1 <cmd>` — skip the hook for one command.
- `rtk discover` — find past commands RTK could have condensed.
