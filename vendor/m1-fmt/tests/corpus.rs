mod common;

#[test]
fn corpus_no_overlong_lines_except_atoms() {
    let scripts = common::corpus_scripts();
    assert!(!scripts.is_empty(), "no corpus scripts found");

    let mut failures = Vec::new();
    for (path, src) in &scripts {
        if !m1_core::parse(src).syntax_diagnostics().is_empty() {
            continue;
        }
        let out = m1_fmt::format_str(src).unwrap().output;
        // Track `/* ... */` block-comment state across lines: comment prose is
        // not rewrapped in v2 (deferred to v3) and must not be judged.
        let mut in_block_comment = false;
        for (i, line) in out.lines().enumerate() {
            let entered_block = in_block_comment;
            // Update block-comment state for the next iteration.
            if in_block_comment {
                if line.contains("*/") {
                    in_block_comment = false;
                }
            } else if let Some(open) = line.rfind("/*") {
                if !line[open..].contains("*/") {
                    in_block_comment = true;
                }
            }
            if entered_block {
                continue;
            }
            if line.chars().count() <= 88 {
                continue;
            }
            // Judge only the *code* portion of the line: drop a whole-line
            // comment and any trailing EOL comment before looking for break
            // points. Ternary wrapping is deferred to v3, so skip ternaries.
            let trimmed_full = line.trim();
            if trimmed_full.starts_with("//") || trimmed_full.starts_with("/*") {
                continue;
            }
            let code = match trimmed_full.find("//") {
                Some(idx) => &trimmed_full[..idx],
                None => trimmed_full,
            };
            if code.contains(" ? ") && code.contains(" : ") {
                continue; // ternary — deferred
            }
            // If, after dropping the comment, the code itself fits, the overrun
            // is purely comment prose — allowed.
            let indent = line.chars().take_while(|c| *c == ' ').count();
            if indent + code.trim_end().chars().count() <= 88 {
                continue;
            }
            let trimmed = code.trim();
            let has_comma = trimmed.contains(", ");
            let has_op = [" | ", " || ", " && ", " + ", " & "]
                .iter()
                .any(|op| trimmed.contains(op));
            if has_comma || has_op {
                failures.push(format!(
                    "{}:{}: {} cols, breakable but not wrapped: {}",
                    path.display(),
                    i + 1,
                    line.chars().count(),
                    trimmed
                ));
            }
        }
    }
    if !failures.is_empty() {
        panic!("over-budget breakable lines:\n{}", failures.join("\n"));
    }
}

#[test]
fn corpus_no_crash_and_output_reparses() {
    let scripts = common::corpus_scripts();
    assert!(!scripts.is_empty(), "no corpus scripts found");

    for (path, src) in &scripts {
        let input_diags = m1_core::parse(src).syntax_diagnostics();

        // Should never panic
        let result = m1_fmt::format_str(src);

        if input_diags.is_empty() {
            let fmt_output = result
                .unwrap_or_else(|e| panic!("{}: format_str returned Err: {}", path.display(), e))
                .output;

            let output_diags = m1_core::parse(&fmt_output).syntax_diagnostics();
            assert!(
                output_diags.is_empty(),
                "{}: formatted output has {} syntax error(s): {:?}",
                path.display(),
                output_diags.len(),
                output_diags
            );
        } else {
            // Files with syntax errors: formatter should pass through unchanged
            let fmt_output = result
                .expect("should not error on syntax-error input")
                .output;
            assert_eq!(
                src,
                &fmt_output,
                "{}: syntax-error file was not passed through unchanged",
                path.display()
            );
        }
    }
}
