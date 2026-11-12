//! Filter for `opencode run`: keeps the final answer and every error, drops the step-by-step trace.
//!
//! `opencode run` prints its whole agent trace: a `> <agent> · <model>` header before every assistant
//! message, narration, one marker line per tool call (`$ cmd`, `→ Read f`, `← Edit f`, `✱ Grep ...`)
//! and the tool's output. The message that carries no tool call is the final answer, so it is the
//! last block after the last header. Everything before it is trace.
//!
//! Output: the model header, a one-line step summary, the error lines found in command output, then
//! the final answer verbatim. Output without a header (a crash, `--format json`, a version change)
//! is returned unchanged. Other `opencode` subcommands are passed through untouched.

use crate::core::runner::{self, RunOptions};
use crate::core::shell::display_args;
use crate::core::tee;
use crate::core::truncate::{CAP_ERRORS, reduced};
use crate::core::utils::{ChildArgExt, strip_ansi};
use anyhow::Result;
use regex::Regex;
use std::ffi::OsString;
use std::sync::LazyLock;

/// Error lines shown across the whole trace.
const MAX_ERRORS: usize = CAP_ERRORS;
/// One noisy step (a long traceback) must not use the whole error budget.
const MAX_ERRORS_PER_STEP: usize = reduced(CAP_ERRORS, 16);
/// Longest step label or error line shown, in characters.
const MAX_LINE_CHARS: usize = 160;
/// Lines of the unfinished last block kept when the run ended mid-step.
const MAX_TAIL_LINES: usize = 15;

/// `> build · deepseek-v4.1-flash`
static HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^> \S+ · \S").unwrap());
/// A tool line as opencode colours it: `ESC[0m$ ESC[0mcmd`. Only a real tool line has this shape,
/// so tool output that happens to start with `$ ` is never mistaken for a step.
static TOOL_ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[0m([^\s\x1b]+) \x1b\[0m").unwrap());
/// Error-shaped command output.
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)error|failed|failure|fatal|panic|exception|traceback|denied|not found|no such file|cannot |unable to|timed out|segmentation fault|aborted|exit(?:ed)? (?:with )?(?:code|status) [1-9]",
    )
    .unwrap()
});
/// An error opencode itself prints (bad model, auth, provider failure).
static OWN_ERROR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i:error):? ").unwrap());

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.first().map(String::as_str) != Some("run") {
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough("opencode", &os_args, verbose);
    }

    if verbose > 0 {
        eprintln!("Running: opencode {}", args.join(" "));
    }

    let Some(cmd) = ordered_command(args) else {
        // Without a way to keep stdout and stderr in order the trace cannot be read back.
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough("opencode", &os_args, verbose);
    };

    // Stdin stays the caller's: `cat notes | opencode run "..."` must keep feeding the prompt.
    runner::run_filtered(
        cmd,
        "opencode",
        &display_args(args),
        filter_opencode_run,
        RunOptions::default().inherit_stdin().tee("opencode"),
    )
}

/// opencode prints the assistant's text on stdout and the whole trace (headers, tool lines, tool
/// output) on stderr. Captured through two pipes the interleaving is lost and the final answer
/// would sit before the trace, so the child merges stderr into stdout itself: one pipe, the order
/// opencode wrote. `exec` keeps the exit status and signals those of opencode.
#[cfg(unix)]
fn ordered_command(args: &[String]) -> Option<std::process::Command> {
    let program = crate::core::utils::resolve_binary("opencode")
        .map(|p| p.into_os_string())
        .unwrap_or_else(|_| OsString::from("opencode"));
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(r#"exec "$0" "$@" 2>&1"#).arg(program);
    cmd.child_args(args);
    Some(cmd)
}

#[cfg(not(unix))]
fn ordered_command(_args: &[String]) -> Option<std::process::Command> {
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolKind {
    Command,
    Edit,
    Read,
    Search,
    Other,
}

impl ToolKind {
    fn from_symbol(sym: &str) -> Self {
        match sym {
            "$" => Self::Command,
            "←" => Self::Edit,
            "→" => Self::Read,
            "✱" => Self::Search,
            _ => Self::Other,
        }
    }
}

struct Step {
    kind: ToolKind,
    label: String,
    errors: Vec<String>,
}

struct Line<'a> {
    raw: &'a str,
    clean: String,
}

pub fn filter_opencode_run(raw: &str) -> String {
    filter_with_recovery(raw, &|full| tee::force_tee_hint(full, "opencode-errors"))
}

fn filter_with_recovery(raw: &str, recover: &dyn Fn(&str) -> Option<String>) -> String {
    let lines: Vec<Line> = raw
        .lines()
        .map(|l| Line {
            raw: l,
            clean: strip_ansi(l),
        })
        .collect();

    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| HEADER.is_match(&l.clean))
        .map(|(i, _)| i)
        .collect();
    let Some(&last) = headers.last() else {
        // Not the shape this filter knows: leave the output alone.
        return raw.to_string();
    };

    let ansi_mode = lines.iter().any(|l| TOOL_ANSI.is_match(l.raw));
    let header_line = lines[last].clean.trim_end().to_string();
    let final_block = &lines[last + 1..];

    // A finished run ends on a message with no tool call. A tool line in the last block means the
    // run stopped mid-step (error, abort), so there is no answer to separate from the trace.
    let unfinished = !scan_steps(final_block, ansi_mode).0.is_empty();
    let trace_end = if unfinished { lines.len() } else { last };
    let (steps, loose_errors) = scan_steps(&lines[..trace_end], ansi_mode);

    let mut out: Vec<String> = vec![header_line];
    if let Some(summary) = summarize(&steps) {
        out.push(summary);
    }
    let errors = render_errors(&steps, &loose_errors, recover);
    let has_trace_info = out.len() > 1 || !errors.is_empty();
    out.extend(errors);

    let answer: Vec<&str> = if unfinished {
        unfinished_text(final_block)
    } else {
        final_block.iter().map(|l| l.clean.as_str()).collect()
    };
    let answer = trim_blank_edges(&answer);
    if answer.is_empty() {
        // A header with nothing after it is a stream we cannot read (a lost or reordered answer),
        // not a model that said nothing: never hide what might be the answer.
        return raw.to_string();
    }
    if has_trace_info {
        out.push(String::new());
    }
    out.extend(answer.iter().map(|s| s.to_string()));
    out.join("\n")
}

/// Walks `lines` collecting tool steps with the error lines of their output, plus error lines
/// opencode printed outside any tool step.
fn scan_steps(lines: &[Line], ansi_mode: bool) -> (Vec<Step>, Vec<String>) {
    let mut steps: Vec<Step> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    let mut current: Option<usize> = None;
    let mut in_fence = false;

    for line in lines {
        if HEADER.is_match(&line.clean) {
            current = None;
            in_fence = false;
            continue;
        }
        if let Some(kind) = tool_kind(line, ansi_mode, in_fence) {
            steps.push(Step {
                kind,
                label: clip(line.clean.trim()),
                errors: Vec::new(),
            });
            current = Some(steps.len() - 1);
            continue;
        }
        if line.clean.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let text = line.clean.trim();
        match current.map(|i| &mut steps[i]) {
            // A diff shows arbitrary code, which says nothing about the step failing.
            Some(step) if step.kind != ToolKind::Edit => {
                if ERROR_LINE.is_match(text) || OWN_ERROR.is_match(text) {
                    step.errors.push(clip(text));
                }
            }
            Some(_) => {}
            None => {
                if OWN_ERROR.is_match(text) {
                    loose.push(clip(text));
                }
            }
        }
    }
    (steps, loose)
}

fn tool_kind(line: &Line, ansi_mode: bool, in_fence: bool) -> Option<ToolKind> {
    if ansi_mode {
        return TOOL_ANSI
            .captures(line.raw)
            .map(|c| ToolKind::from_symbol(&c[1]));
    }
    if in_fence {
        return None;
    }
    let (sym, rest) = line.clean.split_once(' ')?;
    (matches!(sym, "$" | "→" | "←" | "✱" | "⚙") && !rest.is_empty())
        .then(|| ToolKind::from_symbol(sym))
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_LINE_CHARS).collect();
    format!("{head}...")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `steps: 7 commands, 3 edits, 2 reads`
fn summarize(steps: &[Step]) -> Option<String> {
    let count = |k: ToolKind| steps.iter().filter(|s| s.kind == k).count();
    let parts: Vec<String> = [
        (count(ToolKind::Command), "command", "commands"),
        (count(ToolKind::Edit), "edit", "edits"),
        (count(ToolKind::Read), "read", "reads"),
        (count(ToolKind::Search), "search", "searches"),
        (count(ToolKind::Other), "other step", "other steps"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| plural(n, one, many))
    .collect();
    (!parts.is_empty()).then(|| format!("steps: {}", parts.join(", ")))
}

/// Error lines grouped under the step that printed them, capped, with a recovery hint on overflow.
fn render_errors(
    steps: &[Step],
    loose: &[String],
    recover: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut shown: Vec<String> = Vec::new();
    let mut full: Vec<String> = Vec::new();
    let mut shown_count = 0usize;
    let mut total = 0usize;

    let mut emit_group = |label: Option<&str>, errors: &[String]| {
        if errors.is_empty() {
            return;
        }
        total += errors.len();
        if let Some(l) = label {
            full.push(l.to_string());
        }
        full.extend(errors.iter().map(|e| format!("  {e}")));

        let room = MAX_ERRORS.saturating_sub(shown_count);
        let take = errors.len().min(MAX_ERRORS_PER_STEP).min(room);
        if take == 0 {
            return;
        }
        if let Some(l) = label {
            shown.push(l.to_string());
        }
        shown.extend(errors[..take].iter().map(|e| format!("  {e}")));
        shown_count += take;
    };
    emit_group(None, loose);
    for step in steps {
        emit_group(Some(&step.label), &step.errors);
    }

    if total == 0 {
        return Vec::new();
    }
    let mut out = vec!["errors in trace:".to_string()];
    out.extend(shown);
    let hidden = total - shown_count;
    if hidden > 0 {
        let hint = recover(&full.join("\n"));
        out.push(match hint {
            Some(h) => format!("[+{hidden} more error lines: {h}]"),
            None => format!("[+{hidden} more error lines not shown]"),
        });
    }
    out
}

/// A last block that still holds tool steps: its leading narration plus the tail of its output.
fn unfinished_text<'a>(block: &'a [Line]) -> Vec<&'a str> {
    let narration_end = block
        .iter()
        .position(|l| TOOL_ANSI.is_match(l.raw) || is_plain_tool(&l.clean))
        .unwrap_or(block.len());
    let mut out: Vec<&str> = block[..narration_end]
        .iter()
        .map(|l| l.clean.as_str())
        .collect();
    out.push("[run ended during a tool step, no final answer; last output:]");
    let tail_start = block
        .len()
        .saturating_sub(MAX_TAIL_LINES)
        .max(narration_end);
    out.extend(block[tail_start..].iter().map(|l| l.clean.as_str()));
    out
}

fn is_plain_tool(clean: &str) -> bool {
    clean
        .split_once(' ')
        .is_some_and(|(sym, rest)| matches!(sym, "$" | "→" | "←" | "✱" | "⚙") && !rest.is_empty())
}

fn trim_blank_edges<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let end = lines.iter().rposition(|l| !l.trim().is_empty());
    match (start, end) {
        (Some(s), Some(e)) => lines[s..=e].to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT: &str = include_str!("../../../tests/fixtures/opencode_run_edit_raw.txt");
    const ANSWER: &str = include_str!("../../../tests/fixtures/opencode_run_answer_raw.txt");
    const FAILING: &str = include_str!("../../../tests/fixtures/opencode_run_failing_raw.txt");
    const MULTISTEP: &str = include_str!("../../../tests/fixtures/opencode_run_multistep_raw.txt");
    const TOOL_ERRORS: &str =
        include_str!("../../../tests/fixtures/opencode_run_tool_errors_raw.txt");
    const EXPLORE: &str = include_str!("../../../tests/fixtures/opencode_run_explore_raw.txt");
    const BASH_HEAVY: &str =
        include_str!("../../../tests/fixtures/opencode_run_bash_heavy_raw.txt");
    const MODEL_ERROR: &str =
        include_str!("../../../tests/fixtures/opencode_run_model_error_raw.txt");

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    fn savings(raw: &str, out: &str) -> f64 {
        100.0 - (count_tokens(out) as f64 / count_tokens(raw) as f64 * 100.0)
    }

    fn no_recovery(_: &str) -> Option<String> {
        Some("recall-hint".to_string())
    }

    fn filter(raw: &str) -> String {
        filter_with_recovery(raw, &no_recovery)
    }

    /// The final answer of a fixture: everything after its last header, ANSI removed.
    fn final_answer(raw: &str) -> String {
        let clean = strip_ansi(raw);
        let lines: Vec<&str> = clean.lines().collect();
        let last = lines
            .iter()
            .rposition(|l| HEADER.is_match(l))
            .expect("fixture has a header");
        lines[last + 1..].join("\n").trim().to_string()
    }

    fn assert_answer_survives(raw: &str) {
        let out = filter(raw);
        let answer = final_answer(raw);
        assert!(!answer.is_empty());
        assert!(
            out.contains(&answer),
            "final answer missing from:\n{out}\n--- expected ---\n{answer}"
        );
    }

    #[test]
    fn final_answer_survives_verbatim_in_every_fixture() {
        for raw in [
            EDIT,
            ANSWER,
            FAILING,
            MULTISTEP,
            TOOL_ERRORS,
            EXPLORE,
            BASH_HEAVY,
        ] {
            assert_answer_survives(raw);
        }
    }

    #[test]
    fn keeps_header_and_summary_and_drops_trace() {
        let out = filter(MULTISTEP);
        assert!(out.starts_with("> build · "), "{out}");
        assert!(out.contains("steps: "), "{out}");
        assert!(!out.contains("Ran 2 tests in 0.000s"), "{out}");
        assert!(!out.contains("Created file successfully"), "{out}");
        assert!(!out.contains('\u{1b}'), "ANSI must be stripped");
    }

    #[test]
    fn counts_each_tool_kind() {
        let out = filter(EDIT);
        assert!(out.contains("steps: 2 commands, 1 edit, 1 read"), "{out}");
    }

    #[test]
    fn keeps_tool_failures_with_their_command() {
        let out = filter(TOOL_ERRORS);
        assert!(out.contains("errors in trace:"), "{out}");
        assert!(out.contains("$ ./check.sh"), "{out}");
        assert!(out.contains("check failed: missing config"), "{out}");
        assert!(
            out.contains("cat: does-not-exist.txt: No such file or directory"),
            "{out}"
        );
        assert!(
            out.contains("ModuleNotFoundError: No module named 'nosuchmodule'"),
            "{out}"
        );
    }

    #[test]
    fn keeps_failing_test_lines_from_a_superseded_step() {
        let out = filter(MULTISTEP);
        assert!(out.contains("AssertionError: -1 != 5"), "{out}");
    }

    #[test]
    fn edit_diffs_are_not_scanned_for_errors() {
        let raw = "> build · m\n\u{1b}[0m← \u{1b}[0mEdit a.py\n-    raise ValueError('error')\n+    return 1\n> build · m\nDone.\n";
        let out = filter(raw);
        assert!(!out.contains("errors in trace"), "{out}");
        assert_eq!(out, "> build · m\nsteps: 1 edit\n\nDone.");
    }

    #[test]
    fn opencode_own_error_is_kept() {
        // No header at all: unexpected shape, returned unchanged (see the passthrough test).
        assert_eq!(filter(MODEL_ERROR), MODEL_ERROR);
        // A provider error after tool steps leaves the last block empty: nothing is hidden.
        let raw = "> build · m\nlooking\n\u{1b}[0m→ \u{1b}[0mRead a.py\n\u{1b}[91m\u{1b}[1mError: \u{1b}[0mrate limited\n> build · m\n";
        assert_eq!(filter(raw), raw);
        // An error before the final answer is listed.
        let raw = "> build · m\n\u{1b}[0m→ \u{1b}[0mRead a.py\n\u{1b}[91m\u{1b}[1mError: \u{1b}[0mfile locked\n> build · m\nGave up.\n";
        let out = filter(raw);
        assert!(out.contains("Error: file locked"), "{out}");
        assert!(out.ends_with("Gave up."), "{out}");
    }

    #[test]
    fn answer_written_before_the_trace_is_not_hidden() {
        // stdout then stderr, as two separate pipes would be concatenated: the answer comes first.
        let raw = "PONG\n\u{1b}[0m\n> build · m\n";
        assert_eq!(filter(raw), raw);
    }

    #[test]
    fn unexpected_shape_passes_through_unchanged() {
        for raw in [
            "",
            "plain text\nwithout any header\n",
            MODEL_ERROR,
            "{\"type\":\"text\",\"part\":{\"text\":\"hi\"}}\n",
        ] {
            assert_eq!(filter(raw), raw);
        }
    }

    #[test]
    fn answer_only_run_keeps_header_and_answer() {
        let raw = "\u{1b}[0m\n> build · deepseek-v4.1-flash\nPONG\n";
        assert_eq!(filter(raw), "> build · deepseek-v4.1-flash\nPONG");
    }

    #[test]
    fn dollar_in_the_final_answer_is_not_a_step() {
        // c.txt's final answer holds a fenced `$ ./check.sh`; it must survive as answer text.
        let out = filter(FAILING);
        assert!(out.contains("```\n$ ./check.sh\nrunning checks"), "{out}");
        assert!(!out.contains("run ended during a tool step"), "{out}");
    }

    #[test]
    fn plain_text_without_ansi_is_understood_too() {
        let raw = "> build · m\nI'll check.\n$ ls\na.py\nerror: boom\n> build · m\nAll good.\n";
        let out = filter(raw);
        assert_eq!(
            out,
            "> build · m\nsteps: 1 command\nerrors in trace:\n$ ls\n  error: boom\n\nAll good."
        );
    }

    #[test]
    fn run_ended_mid_step_keeps_the_tail() {
        let mut raw = String::from("> build · m\nworking\n\u{1b}[0m$ \u{1b}[0mbig\n");
        for i in 0..40 {
            raw.push_str(&format!("line {i}\n"));
        }
        let out = filter(&raw);
        assert!(out.contains("run ended during a tool step"), "{out}");
        assert!(out.contains("line 39"), "{out}");
        assert!(!out.contains("line 3\n"), "{out}");
        assert!(out.contains("working"), "{out}");
    }

    #[test]
    fn error_overflow_is_capped_and_recoverable() {
        let mut raw = String::from("> build · m\n");
        for s in 0..8 {
            raw.push_str(&format!("\u{1b}[0m$ \u{1b}[0mcmd{s}\n"));
            for e in 0..6 {
                raw.push_str(&format!("error number {e} in cmd{s}\n"));
            }
        }
        raw.push_str("> build · m\nDone.\n");
        let out = filter(&raw);
        let shown = out
            .lines()
            .filter(|l| l.starts_with("  error number"))
            .count();
        assert_eq!(shown, MAX_ERRORS);
        assert!(out.contains("more error lines: recall-hint"), "{out}");
        assert!(out.ends_with("Done."), "{out}");
    }

    #[test]
    fn long_lines_are_clipped() {
        let long = "x".repeat(400);
        let raw = format!("> build · m\n\u{1b}[0m$ \u{1b}[0mc\nerror {long}\n> build · m\nok\n");
        let out = filter(&raw);
        assert!(
            out.lines()
                .all(|l| l.chars().count() <= MAX_LINE_CHARS + 20),
            "{out}"
        );
    }

    #[test]
    fn token_savings_on_every_fixture() {
        for (name, raw, floor) in [
            ("multistep", MULTISTEP, 20.0),
            ("failing", FAILING, 20.0),
            ("explore", EXPLORE, 20.0),
            ("bash_heavy", BASH_HEAVY, 50.0),
        ] {
            let out = filter(raw);
            let s = savings(raw, &out);
            assert!(
                s >= floor,
                "{name}: expected >= {floor}% savings, got {s:.1}%"
            );
        }
    }

    #[test]
    fn small_fixtures_never_grow() {
        for raw in [EDIT, ANSWER, TOOL_ERRORS] {
            let out = filter(raw);
            assert!(count_tokens(&out) <= count_tokens(raw), "{out}");
        }
    }
}
