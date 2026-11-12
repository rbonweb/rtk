//! Filters ESLint and Biome linter output, grouping violations by rule.

use crate::cmds::python::sqlfluff_cmd;
use crate::core::config;
use crate::core::shell::{display_args, quote_program, with_args};
use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::truncate::{CAP_ERRORS, CAP_WARNINGS};
use crate::core::utils::{MissingTool, resolved_command, tool_exec, truncate};
use crate::mypy_cmd;
use crate::ruff_cmd;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Deserialize, Serialize)]
struct EslintMessage {
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    severity: u8,
    message: String,
    line: usize,
    column: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct EslintResult {
    #[serde(rename = "filePath")]
    file_path: String,
    messages: Vec<EslintMessage>,
    #[serde(rename = "errorCount")]
    error_count: usize,
    #[serde(rename = "warningCount")]
    warning_count: usize,
}

#[derive(Debug, Deserialize)]
struct PylintDiagnostic {
    #[serde(rename = "type")]
    msg_type: String, // "warning", "error", "convention", "refactor"
    #[allow(dead_code)]
    module: String,
    #[allow(dead_code)]
    obj: String,
    #[allow(dead_code)]
    line: usize,
    #[allow(dead_code)]
    column: usize,
    path: String,
    symbol: String, // rule code like "unused-variable"
    #[allow(dead_code)]
    message: String,
    #[serde(rename = "message-id")]
    message_id: String, // e.g., "W0612"
}

/// Check if a linter is Python-based (uses pip/pipx, not npm/pnpm)
fn is_python_linter(linter: &str) -> bool {
    matches!(linter, "ruff" | "pylint" | "mypy" | "flake8" | "sqlfluff")
}

/// Strip package manager prefixes (npx, bunx, pnpm, pnpm exec, yarn) from args.
/// Returns the number of args to skip.
fn strip_pm_prefix(args: &[String]) -> usize {
    let pm_names = ["npx", "bunx", "pnpm", "yarn"];
    let mut skip = 0;
    for arg in args {
        if pm_names.contains(&arg.as_str()) || arg == "exec" {
            skip += 1;
        } else {
            break;
        }
    }
    skip
}

/// The package runner named at the front of the args, if any. `bunx eslint`
/// and `pnpm exec eslint` both name one; a bare `exec` does not.
fn named_runner(args: &[String], skip: usize) -> Option<&str> {
    args[..skip]
        .iter()
        .map(String::as_str)
        .find(|a| *a != "exec")
}

/// Detect the linter name from args (after stripping PM prefixes).
/// Returns the linter name and whether it was explicitly specified.
fn detect_linter(args: &[String]) -> (&str, bool) {
    let is_path_or_flag = args.is_empty()
        || args[0].starts_with('-')
        || args[0].contains('/')
        || args[0].contains('.');

    if is_path_or_flag {
        ("eslint", false)
    } else {
        (&args[0], true)
    }
}

/// `runner` is the package runner the user named (`bunx eslint`), or None when
/// nothing was named and lockfile detection applies.
pub fn run(runner: Option<&str>, args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let skip = strip_pm_prefix(args);
    let effective_args = &args[skip..];
    // A runner stripped from the args was still named by the user, so it wins
    // over lockfile detection just as an explicitly threaded one does.
    let runner = runner.or_else(|| named_runner(args, skip));

    let (linter, explicit) = detect_linter(effective_args);

    // sqlfluff owns its own argv and its own rendering: routing, format-flag
    // detection and failure handling live in `sqlfluff_cmd::plan` so this entry
    // point and `rtk sqlfluff ...` cannot drift apart.
    let sqlfluff =
        (linter == "sqlfluff").then(|| sqlfluff_cmd::plan_for_lint(&effective_args[1..]));

    // Python linter use resolved_command() directly (they're on PATH via pip/pipx)
    // JS linters use package_manager_exec (npx/pnpm exec)
    let mut cmd = if is_python_linter(linter) {
        resolved_command(linter)
    } else {
        tool_exec(runner, linter, MissingTool::Fail)
    };

    // Add format flags based on linter
    match linter {
        "eslint" => {
            cmd.arg("-f").arg("json");
        }
        // Force JSON output for ruff check
        "ruff" if !effective_args.contains(&"--output-format".to_string()) => {
            cmd.arg("check").arg("--output-format=json");
        }
        // Force JSON2 output for pylint
        "pylint" if !effective_args.contains(&"--output-format".to_string()) => {
            cmd.arg("--output-format=json2");
        }
        // sqlfluff's full argv comes from the plan below, flags included.
        "sqlfluff" => {}
        "mypy" => {
            // mypy uses default text output (no special flags)
        }
        _ => {
            // Other linters: no special formatting
        }
    }

    // Add user arguments (skip first if it was the linter name, and skip "check" for ruff if we added it)
    let start_idx = if !explicit {
        0
    } else if linter == "ruff" && !effective_args.is_empty() && effective_args[0] == "ruff" {
        // Skip "ruff" and "check" if we already added "check"
        if effective_args.len() > 1 && effective_args[1] == "check" {
            2
        } else {
            1
        }
    } else {
        1
    };

    if let Some(plan) = &sqlfluff {
        // sqlfluff's argv is planned whole, subcommand and format flags included.
        cmd.args(&plan.args);
    } else {
        for arg in &effective_args[start_idx..] {
            // Skip --output-format if we already added it
            if linter == "ruff" && arg.starts_with("--output-format") {
                continue;
            }
            if linter == "pylint" && arg.starts_with("--output-format") {
                continue;
            }
            cmd.arg(arg);
        }
    }

    // Default to current directory if no path specified (for ruff/pylint/mypy/eslint)
    if matches!(linter, "ruff" | "pylint" | "mypy" | "eslint") {
        let has_path = effective_args
            .iter()
            .skip(start_idx)
            .any(|a| !a.starts_with('-') && !a.contains('='));
        if !has_path {
            cmd.arg(".");
        }
    }

    if verbose > 0 {
        eprintln!("Running: {} with structured output", linter);
    }

    let result = exec_capture(&mut cmd).context(format!(
        "Failed to run {}. Is it installed? Try: pip install {} (or npm/pnpm for JS linters)",
        linter, linter
    ))?;

    // Check if process was killed by signal (SIGABRT, SIGKILL, etc.)
    if !result.success() && result.exit_code > 128 {
        eprintln!("[warn] Linter process terminated abnormally (possibly out of memory)");
        if !result.stderr.is_empty() {
            eprintln!(
                "stderr: {}",
                result.stderr.lines().take(5).collect::<Vec<_>>().join("\n")
            );
        }
        return Ok(result.exit_code);
    }

    let raw = format!("{}\n{}", result.stdout, result.stderr);

    // Dispatch to appropriate filter based on linter
    let filtered = if let Some(plan) = &sqlfluff {
        plan.render(&result.stdout, result.exit_code)
    } else {
        match linter {
            "eslint" => filter_eslint_json(&result.stdout),
            "biome" => filter_biome_output(&raw),
            "ruff" => {
                // Reuse ruff_cmd's JSON parser
                if !result.stdout.trim().is_empty() {
                    ruff_cmd::filter_ruff_check_json(&result.stdout)
                } else {
                    "Ruff: No issues found".to_string()
                }
            }
            "pylint" => filter_pylint_json(&result.stdout),
            "mypy" => mypy_cmd::filter_mypy_output(&raw),
            _ => filter_generic_lint(&raw),
        }
    };

    let hint = crate::core::tee::tee_and_hint(&raw, "lint", result.exit_code);
    let shown = crate::core::runner::emit_guarded(&filtered, hint.as_deref(), &raw);

    let tracked = with_args(
        &quote_program(linter),
        &display_args(&effective_args[start_idx..]),
    );
    timer.track(&tracked, &format!("rtk lint {tracked}"), &raw, &shown);

    if !result.success() {
        return Ok(result.exit_code);
    }

    Ok(0)
}

/// Filter ESLint JSON output - group by rule and file
fn filter_eslint_json(output: &str) -> String {
    let results: Result<Vec<EslintResult>, _> = serde_json::from_str(output);

    let results = match results {
        Ok(r) => r,
        Err(e) => {
            // Fallback if JSON parsing fails
            return format!(
                "ESLint output (JSON parse failed: {})\n{}",
                e,
                truncate(output, config::limits().passthrough_max_chars)
            );
        }
    };

    // Count total issues
    let total_errors: usize = results.iter().map(|r| r.error_count).sum();
    let total_warnings: usize = results.iter().map(|r| r.warning_count).sum();
    let total_files = results.iter().filter(|r| !r.messages.is_empty()).count();

    if total_errors == 0 && total_warnings == 0 {
        return "ESLint: No issues found".to_string();
    }

    // Group messages by rule
    let mut by_rule: HashMap<String, usize> = HashMap::new();
    for result in &results {
        for msg in &result.messages {
            if let Some(rule) = &msg.rule_id {
                *by_rule.entry(rule.clone()).or_insert(0) += 1;
            }
        }
    }

    // Group by file
    let mut by_file: Vec<(&EslintResult, usize)> = results
        .iter()
        .filter(|r| !r.messages.is_empty())
        .map(|r| (r, r.messages.len()))
        .collect();
    by_file.sort_by_key(|b| std::cmp::Reverse(b.1));

    // Build output
    let mut result = String::new();
    result.push_str(&format!(
        "ESLint: {} errors, {} warnings in {} files\n",
        total_errors, total_warnings, total_files
    ));

    // Show top rules
    let mut rule_counts: Vec<_> = by_rule.iter().collect();
    rule_counts.sort_by(|a, b| b.1.cmp(a.1));

    if !rule_counts.is_empty() {
        result.push_str("Top rules:\n");
        for (rule, count) in rule_counts.iter().take(10) {
            result.push_str(&format!("  {} ({}x)\n", rule, count));
        }
        result.push('\n');
    }

    // Show top files with most issues, plus the top rules in each
    const MAX_FILES: usize = CAP_WARNINGS;
    result.push_str("Top files:\n");
    for (file_result, count) in by_file.iter().take(MAX_FILES) {
        let short_path = compact_path(&file_result.file_path);
        result.push_str(&format!("  {} ({} issues)\n", short_path, count));

        let mut file_rules: HashMap<String, usize> = HashMap::new();
        for msg in &file_result.messages {
            if let Some(rule) = &msg.rule_id {
                *file_rules.entry(rule.clone()).or_insert(0) += 1;
            }
        }
        let mut file_rule_counts: Vec<_> = file_rules.iter().collect();
        file_rule_counts.sort_by(|a, b| b.1.cmp(a.1));
        for (rule, count) in file_rule_counts.iter().take(3) {
            result.push_str(&format!("    {} ({})\n", rule, count));
        }
    }

    if by_file.len() > MAX_FILES {
        result.push_str(&format!("\n… +{} more files\n", by_file.len() - MAX_FILES));
        let all_file_lines = by_file
            .iter()
            .map(|(r, count)| format!("{} ({} issues)", compact_path(&r.file_path), count))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all_file_lines, "eslint-files", MAX_FILES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

/// Filter pylint JSON2 output - group by symbol and file
fn filter_pylint_json(output: &str) -> String {
    let diagnostics: Result<Vec<PylintDiagnostic>, _> = serde_json::from_str(output);

    let diagnostics = match diagnostics {
        Ok(d) => d,
        Err(e) => {
            // Fallback if JSON parsing fails
            return format!(
                "Pylint output (JSON parse failed: {})\n{}",
                e,
                truncate(output, config::limits().passthrough_max_chars)
            );
        }
    };

    if diagnostics.is_empty() {
        return "Pylint: No issues found".to_string();
    }

    // Count by type
    let mut errors = 0;
    let mut warnings = 0;
    let mut conventions = 0;
    let mut refactors = 0;

    for diag in &diagnostics {
        match diag.msg_type.as_str() {
            "error" => errors += 1,
            "warning" => warnings += 1,
            "convention" => conventions += 1,
            "refactor" => refactors += 1,
            _ => {}
        }
    }

    // Count unique files
    let unique_files: std::collections::HashSet<_> = diagnostics.iter().map(|d| &d.path).collect();
    let total_files = unique_files.len();

    // Group by symbol (rule code)
    let mut by_symbol: HashMap<String, usize> = HashMap::new();
    for diag in &diagnostics {
        let key = format!("{} ({})", diag.symbol, diag.message_id);
        *by_symbol.entry(key).or_insert(0) += 1;
    }

    // Group by file
    let mut by_file: HashMap<&str, usize> = HashMap::new();
    for diag in &diagnostics {
        *by_file.entry(&diag.path).or_insert(0) += 1;
    }

    let mut file_counts: Vec<_> = by_file.iter().collect();
    file_counts.sort_by(|a, b| b.1.cmp(a.1));

    // Build output
    let mut result = String::new();
    result.push_str(&format!(
        "Pylint: {} issues in {} files\n",
        diagnostics.len(),
        total_files
    ));

    if errors > 0 || warnings > 0 {
        result.push_str(&format!("  {} errors, {} warnings", errors, warnings));
        if conventions > 0 || refactors > 0 {
            result.push_str(&format!(
                ", {} conventions, {} refactors",
                conventions, refactors
            ));
        }
        result.push('\n');
    }

    // Show top symbols (rules)
    let mut symbol_counts: Vec<_> = by_symbol.iter().collect();
    symbol_counts.sort_by(|a, b| b.1.cmp(a.1));

    if !symbol_counts.is_empty() {
        result.push_str("Top rules:\n");
        for (symbol, count) in symbol_counts.iter().take(10) {
            result.push_str(&format!("  {} ({}x)\n", symbol, count));
        }
        result.push('\n');
    }

    // Show top files
    const MAX_FILES: usize = CAP_WARNINGS;
    result.push_str("Top files:\n");
    for (file, count) in file_counts.iter().take(MAX_FILES) {
        let short_path = compact_path(file);
        result.push_str(&format!("  {} ({} issues)\n", short_path, count));

        // Show top 3 rules in this file
        let mut file_symbols: HashMap<String, usize> = HashMap::new();
        for diag in diagnostics.iter().filter(|d| &d.path == *file) {
            let key = format!("{} ({})", diag.symbol, diag.message_id);
            *file_symbols.entry(key).or_insert(0) += 1;
        }

        let mut file_symbol_counts: Vec<_> = file_symbols.iter().collect();
        file_symbol_counts.sort_by(|a, b| b.1.cmp(a.1));

        for (symbol, count) in file_symbol_counts.iter().take(3) {
            result.push_str(&format!("    {} ({})\n", symbol, count));
        }
    }

    if file_counts.len() > MAX_FILES {
        result.push_str(&format!(
            "\n… +{} more files\n",
            file_counts.len() - MAX_FILES
        ));
        let all_file_lines = file_counts
            .iter()
            .map(|(file, count)| format!("{} ({} issues)", compact_path(file), count))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all_file_lines, "pylint-files", MAX_FILES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

/// Filter generic linter output (fallback for non-ESLint linters)
fn filter_generic_lint(output: &str) -> String {
    let mut warnings = 0;
    let mut errors = 0;
    let mut issues: Vec<String> = Vec::new();

    for line in output.lines() {
        let line_lower = line.to_lowercase();
        if line_lower.contains("warning") {
            warnings += 1;
            issues.push(line.to_string());
        }
        if line_lower.contains("error") && !line_lower.contains("0 error") {
            errors += 1;
            issues.push(line.to_string());
        }
    }

    if errors == 0 && warnings == 0 {
        return "Lint: No issues found".to_string();
    }

    let mut result = String::new();
    result.push_str(&format!("Lint: {} errors, {} warnings\n", errors, warnings));

    const MAX_ISSUES: usize = CAP_ERRORS;
    for issue in issues.iter().take(MAX_ISSUES) {
        result.push_str(&format!("{}\n", truncate(issue, 100)));
    }

    if issues.len() > MAX_ISSUES {
        result.push_str(&format!("\n… +{} more issues\n", issues.len() - MAX_ISSUES));
        let all_issues = issues.join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all_issues, "lint-issues", MAX_ISSUES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

/// A biome diagnostic header: `path:line:col rule [FIXABLE ...] ━━━━`. Diagnostics that have no
/// source position (`format`, `organizeImports`, configuration) carry `path rule ━━━` instead.
static BIOME_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<path>\S.*?)(?::(?P<line>\d+):(?P<col>\d+))? (?P<rule>[A-Za-z][\w/-]*)(?:\s+[A-Z]+)*\s+━+\s*$")
        .unwrap()
});
/// The first body line of a diagnostic: a severity marker, then biome's one-line explanation.
static BIOME_MESSAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+(?P<mark>[!×✖✕xi])\s+(?P<text>\S.*)$").unwrap());
static BIOME_FOUND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Found (\d+) (errors?|warnings?|infos?)\.").unwrap());
static BIOME_NOT_SHOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Diagnostics not shown: (\d+)\.").unwrap());

#[derive(Debug, PartialEq)]
struct BiomeDiagnostic {
    location: String,
    rule: String,
    message: String,
    is_error: bool,
}

/// Condense biome's default text output to one line per diagnostic.
///
/// Biome prints a code frame, caret art and an explanation per diagnostic. This keeps the
/// header position, the rule and the first line of the explanation, and takes the exact counts
/// from biome's own `Found N errors.` summary (falling back to counting the diagnostics shown).
/// Output with no recognisable diagnostic header (a different `--reporter`, a crash, a clean
/// run) is returned unchanged, so nothing is ever hidden by a format this parser does not know.
fn filter_biome_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut diagnostics: Vec<BiomeDiagnostic> = Vec::new();
    let (mut found_errors, mut found_warnings, mut not_shown) = (None, None, None);

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(caps) = BIOME_FOUND.captures(line) {
            let count: usize = caps[1].parse().unwrap_or(0);
            if caps[2].starts_with("error") {
                found_errors = Some(count);
            } else if caps[2].starts_with("warning") {
                found_warnings = Some(count);
            }
        } else if let Some(caps) = BIOME_NOT_SHOWN.captures(line) {
            not_shown = caps[1].parse::<usize>().ok();
        } else if let Some(caps) = BIOME_HEADER.captures(line) {
            let location = match (caps.name("line"), caps.name("col")) {
                (Some(l), Some(c)) => format!("{}:{}:{}", &caps["path"], l.as_str(), c.as_str()),
                _ => caps["path"].to_string(),
            };
            // The explanation is the first marked line after the header, before the next header.
            let mut message = String::new();
            let mut is_error = false;
            for body in lines[i + 1..]
                .iter()
                .take_while(|l| !BIOME_HEADER.is_match(l))
            {
                if let Some(m) = BIOME_MESSAGE.captures(body) {
                    is_error = !matches!(&m["mark"], "!" | "i");
                    message = m["text"].trim().to_string();
                    break;
                }
            }
            diagnostics.push(BiomeDiagnostic {
                location,
                rule: caps["rule"].to_string(),
                message,
                is_error,
            });
        }
        i += 1;
    }

    if diagnostics.is_empty() {
        return output.trim().to_string();
    }

    let shown_errors = diagnostics.iter().filter(|d| d.is_error).count();
    let errors = found_errors.unwrap_or(shown_errors);
    let warnings = found_warnings.unwrap_or(diagnostics.len() - shown_errors);

    // Errors first: they are what fails the run.
    diagnostics.sort_by_key(|d| !d.is_error);
    let rendered: Vec<String> = diagnostics
        .iter()
        .map(|d| {
            let line = format!("{} {} {}", d.location, d.rule, d.message);
            truncate(line.trim_end(), 200)
        })
        .collect();

    let mut result = format!("Biome: {errors} errors, {warnings} warnings\n");
    const MAX_DIAGNOSTICS: usize = CAP_ERRORS;
    for line in rendered.iter().take(MAX_DIAGNOSTICS) {
        result.push_str(line);
        result.push('\n');
    }
    if rendered.len() > MAX_DIAGNOSTICS {
        result.push_str(&format!(
            "\n… +{} more diagnostics\n",
            rendered.len() - MAX_DIAGNOSTICS
        ));
        if let Some(hint) = crate::core::tee::force_tee_tail_hint(
            &rendered.join("\n"),
            "biome-diagnostics",
            MAX_DIAGNOSTICS + 1,
        ) {
            result.push_str(&format!("  {hint}\n"));
        }
    }
    if let Some(n) = not_shown {
        result.push_str(&format!(
            "{n} more not shown by biome (raise --max-diagnostics)\n"
        ));
    }

    result.trim().to_string()
}

/// Compact file path (remove common prefixes)
fn compact_path(path: &str) -> String {
    // Remove common prefixes like /Users/..., /home/..., C:\
    let path = path.replace('\\', "/");

    if let Some(pos) = path.rfind("/src/") {
        format!("src/{}", &path[pos + 5..])
    } else if let Some(pos) = path.rfind("/lib/") {
        format!("lib/{}", &path[pos + 5..])
    } else if let Some(pos) = path.rfind('/') {
        path[pos + 1..].to_string()
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_eslint_json() {
        let json = r#"[
            {
                "filePath": "/Users/test/project/src/utils.ts",
                "messages": [
                    {
                        "ruleId": "prefer-const",
                        "severity": 1,
                        "message": "Use const instead of let",
                        "line": 10,
                        "column": 5
                    },
                    {
                        "ruleId": "prefer-const",
                        "severity": 1,
                        "message": "Use const instead of let",
                        "line": 15,
                        "column": 5
                    }
                ],
                "errorCount": 0,
                "warningCount": 2
            },
            {
                "filePath": "/Users/test/project/src/api.ts",
                "messages": [
                    {
                        "ruleId": "@typescript-eslint/no-unused-vars",
                        "severity": 2,
                        "message": "Variable x is unused",
                        "line": 20,
                        "column": 10
                    }
                ],
                "errorCount": 1,
                "warningCount": 0
            }
        ]"#;

        let result = filter_eslint_json(json);
        assert!(result.contains("ESLint:"));
        assert!(result.contains("prefer-const"));
        assert!(result.contains("no-unused-vars"));
        assert!(result.contains("src/utils.ts"));
    }

    const BIOME_WARNINGS_RAW: &str =
        include_str!("../../../tests/fixtures/biome_check_warnings_raw.txt");

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    #[test]
    fn test_biome_counts_are_exact_and_every_location_survives() {
        let out = filter_biome_output(BIOME_WARNINGS_RAW);

        assert!(out.starts_with("Biome: 0 errors, 18 warnings"), "{out}");
        // One line per diagnostic, in biome's file:line:col form.
        let expected: Vec<&str> = BIOME_WARNINGS_RAW
            .lines()
            .filter(|l| l.contains(" ━━"))
            .map(|l| l.split(' ').next().expect("location"))
            .collect();
        assert_eq!(expected.len(), 18);
        for location in &expected {
            assert!(out.contains(location), "missing {location} in:\n{out}");
        }
        assert!(
            out.contains(
                "lint/complexity/noImportantStyles Avoid the use of the !important style."
            )
        );
        assert!(
            out.contains(
                "lint/style/noDescendingSpecificity Descending specificity selector found."
            )
        );
    }

    #[test]
    fn test_biome_drops_code_frames() {
        let out = filter_biome_output(BIOME_WARNINGS_RAW);
        assert!(!out.contains('│'), "{out}");
        assert!(!out.contains("^^^^"), "{out}");
        assert!(!out.contains("Unsafe fix"), "{out}");
    }

    #[test]
    fn test_biome_savings_at_least_20_percent() {
        let out = filter_biome_output(BIOME_WARNINGS_RAW);
        let savings =
            100.0 - count_tokens(&out) as f64 / count_tokens(BIOME_WARNINGS_RAW) as f64 * 100.0;
        assert!(savings >= 20.0, "expected >=20% savings, got {savings:.1}%");
    }

    #[test]
    fn test_biome_errors_come_first_and_use_the_summary_counts() {
        let raw = "src/a.ts:3:7 lint/style/useConst  FIXABLE  ━━━━━━━━━━\n\n  ! Use const.\n\n    3 │ let a = 1;\n      │     ^\n\nsrc/b.ts:9:1 lint/correctness/noUnusedVariables ━━━━━━━━━━\n\n  ✖ This variable is unused.\n\n    9 │ const b = 1;\n      │       ^\n\nChecked 2 files in 3ms. No fixes applied.\nFound 1 error.\nFound 1 warning.\n";
        let out = filter_biome_output(raw);
        assert_eq!(
            out,
            "Biome: 1 errors, 1 warnings\nsrc/b.ts:9:1 lint/correctness/noUnusedVariables This variable is unused.\nsrc/a.ts:3:7 lint/style/useConst Use const."
        );
    }

    #[test]
    fn test_biome_diagnostic_without_position_keeps_its_path() {
        let raw = "src/a.ts format ━━━━━━━━━━\n\n  ✖ Formatter would have printed the following content:\n\n    1 │ - a\n\nChecked 1 file in 1ms. No fixes applied.\nFound 1 error.\n";
        let out = filter_biome_output(raw);
        assert!(
            out.contains("src/a.ts format Formatter would have printed"),
            "{out}"
        );
        assert!(out.starts_with("Biome: 1 errors, 0 warnings"), "{out}");
    }

    #[test]
    fn test_biome_caps_diagnostics_with_overflow_line() {
        let mut raw = String::new();
        for n in 1..=25 {
            raw.push_str(&format!(
                "src/a.ts:{n}:1 lint/style/useConst ━━━━━━━━━━\n\n  ! Use const.\n\n"
            ));
        }
        raw.push_str("Found 25 warnings.\n");
        let out = filter_biome_output(&raw);
        assert!(out.starts_with("Biome: 0 errors, 25 warnings"), "{out}");
        assert_eq!(out.matches("lint/style/useConst").count(), CAP_ERRORS);
        assert!(out.contains("… +5 more diagnostics"), "{out}");
    }

    #[test]
    fn test_biome_unparsed_output_falls_back_to_raw() {
        // A machine reporter the user chose, a crash, or a clean run has no diagnostic header.
        for raw in [
            "{\"summary\":{\"errors\":1},\"diagnostics\":[]}",
            "Checked 12 files in 4ms. No fixes applied.",
            "internal error: something broke",
            "",
        ] {
            assert_eq!(filter_biome_output(raw), raw.trim());
        }
    }

    #[test]
    fn test_compact_path() {
        assert_eq!(
            compact_path("/Users/foo/project/src/utils.ts"),
            "src/utils.ts"
        );
        assert_eq!(
            compact_path("C:\\Users\\project\\src\\api.ts"),
            "src/api.ts"
        );
        assert_eq!(compact_path("simple.ts"), "simple.ts");
    }

    #[test]
    fn test_filter_pylint_json_no_issues() {
        let output = "[]";
        let result = filter_pylint_json(output);
        assert!(result.contains("Pylint"));
        assert!(result.contains("No issues found"));
    }

    #[test]
    fn test_filter_pylint_json_with_issues() {
        let json = r#"[
            {
                "type": "warning",
                "module": "main",
                "obj": "",
                "line": 10,
                "column": 0,
                "path": "src/main.py",
                "symbol": "unused-variable",
                "message": "Unused variable 'x'",
                "message-id": "W0612"
            },
            {
                "type": "warning",
                "module": "main",
                "obj": "foo",
                "line": 15,
                "column": 4,
                "path": "src/main.py",
                "symbol": "unused-variable",
                "message": "Unused variable 'y'",
                "message-id": "W0612"
            },
            {
                "type": "error",
                "module": "utils",
                "obj": "bar",
                "line": 20,
                "column": 0,
                "path": "src/utils.py",
                "symbol": "undefined-variable",
                "message": "Undefined variable 'z'",
                "message-id": "E0602"
            }
        ]"#;

        let result = filter_pylint_json(json);
        assert!(result.contains("3 issues"));
        assert!(result.contains("2 files"));
        assert!(result.contains("1 errors, 2 warnings"));
        assert!(result.contains("unused-variable (W0612)"));
        assert!(result.contains("undefined-variable (E0602)"));
        assert!(result.contains("main.py"));
        assert!(result.contains("utils.py"));
    }

    #[test]
    fn test_strip_pm_prefix_npx() {
        let args: Vec<String> = vec!["npx".into(), "eslint".into(), "src/".into()];
        assert_eq!(strip_pm_prefix(&args), 1);
    }

    #[test]
    fn test_strip_pm_prefix_bunx() {
        let args: Vec<String> = vec!["bunx".into(), "eslint".into(), ".".into()];
        assert_eq!(strip_pm_prefix(&args), 1);
    }

    #[test]
    fn test_strip_pm_prefix_pnpm_exec() {
        let args: Vec<String> = vec!["pnpm".into(), "exec".into(), "eslint".into()];
        assert_eq!(strip_pm_prefix(&args), 2);
    }

    #[test]
    fn test_strip_pm_prefix_none() {
        let args: Vec<String> = vec!["eslint".into(), "src/".into()];
        assert_eq!(strip_pm_prefix(&args), 0);
    }

    #[test]
    fn test_strip_pm_prefix_empty() {
        let args: Vec<String> = vec![];
        assert_eq!(strip_pm_prefix(&args), 0);
    }

    #[test]
    fn test_detect_linter_eslint() {
        let args: Vec<String> = vec!["eslint".into(), "src/".into()];
        let (linter, explicit) = detect_linter(&args);
        assert_eq!(linter, "eslint");
        assert!(explicit);
    }

    #[test]
    fn test_detect_linter_default_on_path() {
        let args: Vec<String> = vec!["src/".into()];
        let (linter, explicit) = detect_linter(&args);
        assert_eq!(linter, "eslint");
        assert!(!explicit);
    }

    #[test]
    fn test_detect_linter_default_on_flag() {
        let args: Vec<String> = vec!["--max-warnings=0".into()];
        let (linter, explicit) = detect_linter(&args);
        assert_eq!(linter, "eslint");
        assert!(!explicit);
    }

    #[test]
    fn test_detect_linter_after_npx_strip() {
        // Simulates: rtk lint npx eslint src/ → after strip_pm_prefix, args = ["eslint", "src/"]
        let full_args: Vec<String> = vec!["npx".into(), "eslint".into(), "src/".into()];
        let skip = strip_pm_prefix(&full_args);
        let effective = &full_args[skip..];
        let (linter, _) = detect_linter(effective);
        assert_eq!(linter, "eslint");
    }

    #[test]
    fn test_detect_linter_after_pnpm_exec_strip() {
        let full_args: Vec<String> =
            vec!["pnpm".into(), "exec".into(), "biome".into(), "check".into()];
        let skip = strip_pm_prefix(&full_args);
        let effective = &full_args[skip..];
        let (linter, _) = detect_linter(effective);
        assert_eq!(linter, "biome");
    }

    #[test]
    fn test_is_python_linter() {
        assert!(is_python_linter("ruff"));
        assert!(is_python_linter("pylint"));
        assert!(is_python_linter("mypy"));
        assert!(is_python_linter("flake8"));
        assert!(is_python_linter("sqlfluff"));
        assert!(!is_python_linter("eslint"));
        assert!(!is_python_linter("biome"));
        assert!(!is_python_linter("unknown"));
    }

    #[test]
    fn test_named_runner_recovers_the_stripped_prefix() {
        let args: Vec<String> = ["bunx", "eslint", "."]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let skip = strip_pm_prefix(&args);
        assert_eq!(named_runner(&args, skip), Some("bunx"));

        let args: Vec<String> = ["pnpm", "exec", "eslint"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let skip = strip_pm_prefix(&args);
        assert_eq!(named_runner(&args, skip), Some("pnpm"));

        let args: Vec<String> = ["eslint", "src"].iter().map(|s| s.to_string()).collect();
        let skip = strip_pm_prefix(&args);
        assert_eq!(named_runner(&args, skip), None);
    }
}
