# System and Generic Utilities

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `read.rs` uses `core/filter` for language-aware code stripping (FilterLevel: none/minimal/aggressive)
- `search.rs` backs both `rtk grep` and `rtk rg`: it runs the invoked engine (never substituting one for the other) and groups its output, reading `core/config` for `limits.grep_max_results` and `limits.grep_max_per_file`. Format-altering flags (`-c`, `-l`, `-L`, `-o`, `-Z`) bypass RTK filtering and run raw, except a bare file list (`-l`, `-L`, `--files` with no other shape flag), whose shared directory prefix is folded into a `<prefix> (N files)` header under the `never_worse` guard.
- `ctest_cmd.rs` takes the run total from the first result line (covering `--stop-on-failure`, disabled tests, and forwarded suites) and validates both result lines and the summary against it, deduplicates retries by test number+name, folds wrapped result lines until their terminator, falls back to the raw `FAILED:` trailer for unparsed failures, keeps the error trailer behind an empty run, labels and caps failure details with tee recovery, and attributes diagnostics safely under `-j`; explicit verbose/show-only/help/version flags and dashboard modes bypass filtering, except `-T Test`, which prints ordinary test output and stays filtered.
- `opencode_cmd.rs` filters `opencode run` only (the hook rewrites nothing else; `rtk opencode <other>` passes through). The message after the last `> <agent> · <model>` header is the final answer and is kept verbatim; the rest is trace, reduced to a `steps:` count line plus the error-shaped lines of command output (diffs excluded), grouped under their command and capped with tee recovery. Output without a header is returned unchanged; a last block that still holds tool steps (run aborted) is kept as narration plus its tail. opencode writes the assistant text to stdout and the trace to stderr, so on Unix the child is started through `sh -c 'exec "$0" "$@" 2>&1'` to keep them in order in one pipe (elsewhere the command runs unfiltered); a header with no text after it is returned unchanged. Stdin is inherited so piped prompts keep working. Fixtures are real captures in `tests/fixtures/opencode_run_*_raw.txt`.
- `local_llm.rs` (`rtk smart`) uses `core/filter` for heuristic file summarization
- `format_cmd.rs` is a cross-ecosystem dispatcher: auto-detects and routes to `prettier_cmd` or `ruff_cmd` (black is handled inline, not as a separate module)
- `summary.rs` executes argv directly before summarizing output. Shell-specific syntax requires `--shell <name> '<script>'`; it never guesses from `$SHELL`.

## Cross-command

- `format_cmd` routes to `cmds/js/prettier_cmd` and `cmds/python/ruff_cmd`
