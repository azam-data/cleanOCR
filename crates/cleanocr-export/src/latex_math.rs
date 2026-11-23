use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MathValidationError {
    #[error("formula is empty")]
    Empty,
    #[error("unescaped raw '$' inside formula body")]
    RawDollarSign,
    #[error("trailing lone backslash at end of formula")]
    TrailingBackslash,
    #[error("unmatched closing brace '}}'")]
    UnmatchedCloseBrace,
    #[error("unclosed '{{' (depth {0} remaining)")]
    UnclosedOpenBrace(usize),
    #[error("unmatched \\right without preceding \\left")]
    UnmatchedRightDelimiter,
    #[error("unclosed \\left delimiter (count {0} remaining)")]
    UnclosedLeftDelimiter(usize),
    #[error("mismatched \\end{{{found}}}, expected \\end{{{expected}}}")]
    MismatchedEnv { expected: String, found: String },
    #[error("unexpected \\end{{{0}}} with no matching \\begin")]
    UnexpectedEndEnv(String),
    #[error("unclosed environment \\begin{{{0}}}")]
    UnclosedEnv(String),
}

/// Strictly escapes all 10 LaTeX special characters for body text, headings, captions, and table cells.
/// NEVER call this on validated raw LaTeX math strings.
pub fn escape_latex(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str(r"\textbackslash{}"),
            '{' => out.push_str(r"\{"),
            '}' => out.push_str(r"\}"),
            '$' => out.push_str(r"\$"),
            '&' => out.push_str(r"\&"),
            '#' => out.push_str(r"\#"),
            '^' => out.push_str(r"\textasciicircum{}"),
            '_' => out.push_str(r"\_"),
            '%' => out.push_str(r"\%"),
            '~' => out.push_str(r"\textasciitilde{}"),
            _ => out.push(ch),
        }
    }
    out
}

/// Strips line breaks so diagnostic strings stay safely inside a single `% ...` TeX comment line.
pub fn sanitize_tex_comment(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\r' | '\n' => ' ',
            _ => c,
        })
        .collect()
}

/// Validates raw LaTeX math content (without outer `$` or `\[...\]` delimiters) so broken OCR
/// formulas never crash a LuaLaTeX/XeLaTeX build.
///
/// Checks:
/// - Non-empty content
/// - No raw unescaped `$`
/// - No trailing lone `\` that would escape the closing math delimiter
/// - Balanced `{` and `}` (properly treating `\\` as a line break, not an escaped brace)
/// - Balanced `\left` and `\right` delimiters
/// - Properly nested `\begin{...}` and `\end{...}` environments
/// - Ignores TeX `%` comments through end of line
pub fn validate_latex_math(raw: &str) -> Result<(), MathValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MathValidationError::Empty);
    }

    let bytes = trimmed.as_bytes();
    let mut i = 0;
    let mut brace_depth: usize = 0;
    let mut left_depth: usize = 0;
    let mut env_stack: Vec<String> = Vec::new();

    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'$' => return Err(MathValidationError::RawDollarSign),
            b'{' => {
                brace_depth += 1;
                i += 1;
            }
            b'}' => {
                if brace_depth == 0 {
                    return Err(MathValidationError::UnmatchedCloseBrace);
                }
                brace_depth -= 1;
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(MathValidationError::TrailingBackslash);
                }
                // Non-alphabetic control symbols: `\\` (line break), `\{`, `\}`, `\$`, `\%`, `\,`
                // Advancing past both bytes ensures `\\}` leaves `}` to be parsed as a real brace.
                if !bytes[i].is_ascii_alphabetic() {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let cmd = &trimmed[start..i];
                match cmd {
                    "left" => left_depth += 1,
                    "right" => {
                        if left_depth == 0 {
                            return Err(MathValidationError::UnmatchedRightDelimiter);
                        }
                        left_depth -= 1;
                    }
                    "begin" | "end" => {
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        if i < bytes.len() && bytes[i] == b'{' {
                            let env_start = i + 1;
                            if let Some(rel_end) = trimmed[env_start..].find('}') {
                                let env_name =
                                    trimmed[env_start..env_start + rel_end].trim().to_string();
                                i = env_start + rel_end + 1;
                                if cmd == "begin" {
                                    env_stack.push(env_name);
                                } else {
                                    match env_stack.pop() {
                                        Some(expected) if expected == env_name => {}
                                        Some(expected) => {
                                            return Err(MathValidationError::MismatchedEnv {
                                                expected,
                                                found: env_name,
                                            });
                                        }
                                        None => {
                                            return Err(MathValidationError::UnexpectedEndEnv(
                                                env_name,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => i += 1,
        }
    }

    if brace_depth > 0 {
        return Err(MathValidationError::UnclosedOpenBrace(brace_depth));
    }
    if left_depth > 0 {
        return Err(MathValidationError::UnclosedLeftDelimiter(left_depth));
    }
    if let Some(unclosed) = env_stack.pop() {
        return Err(MathValidationError::UnclosedEnv(unclosed));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_ten_latex_special_characters() {
        let raw = r"\{}$&#^_%~";
        let expected = r"\textbackslash{}\{\}\$\&\#\textasciicircum{}\_\%\textasciitilde{}";
        assert_eq!(escape_latex(raw), expected);
    }

    #[test]
    fn accepts_valid_complex_formulas() {
        assert!(validate_latex_math(r"\int_0^1 x^2 \, dx = \frac{1}{3}").is_ok());
        assert!(validate_latex_math(r"\left( \frac{a + b}{c} \right) = \{1, 2\}").is_ok());
        assert!(
            validate_latex_math(r"\begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix} % matrix comment")
                .is_ok()
        );
        // `\\}` closes the group after a line break (not an escaped brace).
        assert!(validate_latex_math(r"\text{a \\}").is_ok());
    }

    #[test]
    fn rejects_empty_raw_dollar_and_trailing_backslash() {
        assert_eq!(validate_latex_math("   "), Err(MathValidationError::Empty));
        assert_eq!(
            validate_latex_math(r"x + $y$"),
            Err(MathValidationError::RawDollarSign)
        );
        assert_eq!(
            validate_latex_math(r"x + y\"),
            Err(MathValidationError::TrailingBackslash)
        );
        // Escaped dollar `\$` is valid.
        assert!(validate_latex_math(r"\text{\$100}").is_ok());
    }

    #[test]
    fn distinguishes_linebreak_brace_from_escaped_brace() {
        // `\\}` closes the group -> valid.
        assert!(validate_latex_math(r"{a \\}").is_ok());
        // `\}` is an escaped literal brace, leaving `{` unclosed -> invalid.
        assert_eq!(
            validate_latex_math(r"{a \}"),
            Err(MathValidationError::UnclosedOpenBrace(1))
        );
        // Unmatched closing brace after line break -> invalid.
        assert_eq!(
            validate_latex_math(r"a \\}"),
            Err(MathValidationError::UnmatchedCloseBrace)
        );
    }

    #[test]
    fn catches_unbalanced_left_right_and_environments() {
        assert_eq!(
            validate_latex_math(r"\left( x + 1"),
            Err(MathValidationError::UnclosedLeftDelimiter(1))
        );
        assert_eq!(
            validate_latex_math(r"x + 1 \right)"),
            Err(MathValidationError::UnmatchedRightDelimiter)
        );
        assert_eq!(
            validate_latex_math(r"\begin{bmatrix} 1 & 2 \end{pmatrix}"),
            Err(MathValidationError::MismatchedEnv {
                expected: "bmatrix".to_string(),
                found: "pmatrix".to_string(),
            })
        );
        assert_eq!(
            validate_latex_math(r"\end{align}"),
            Err(MathValidationError::UnexpectedEndEnv("align".to_string()))
        );
        assert_eq!(
            validate_latex_math(r"\begin{cases} x & x > 0"),
            Err(MathValidationError::UnclosedEnv("cases".to_string()))
        );
    }
}
