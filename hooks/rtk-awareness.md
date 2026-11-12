# Command output

Command output here is condensed to save tokens, keeping every signal and dropping costly noise. Treat it as the complete result: run commands normally, and batch related commands into one call to avoid extra turns. Truncated results state their recovery path in their own output.

Exact text stays exact: file contents (`cat`, `head`, `sed -n`), `grep -o` and `grep -c` results, `jq -r` values, and `curl -o` / `-w` results come back unchanged, and in-place edits such as `sed -i` are never rewritten. Do not add `RTK_DISABLED=1` or `rtk bypass` to get exact text. Re-run a command as `rtk bypass <cmd>` only when its result is unusable: empty when output was clearly expected, contradicting its exit code, or garbled.
