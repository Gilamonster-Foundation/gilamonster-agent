//! Rust-native `gila commit-msg` — Phase 3 of the gila-parity plan.
//!
//! Validates a commit message against the conventional-commit shape used by
//! the workspace pre-push hooks: an optional scope, a required type from the
//! allowed set, a colon, and a non-empty subject (`type(scope): subject`).
//! Pure validation is unit-testable; the binary's `run_*` arm reads the
//! message (arg or file) and maps the verdict to an exit code.

/// The allowed conventional-commit types.
pub const ALLOWED_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// A validation verdict: Ok(()) or a human-readable reason.
pub type Verdict = Result<(), String>;

/// Validate a commit message's first line against the conventional shape.
pub fn validate(msg: &str) -> Verdict {
    let first = msg.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return Err("empty commit message".to_string());
    }
    // type(scope): subject  OR  type: subject
    let (head, subject) = match first.split_once(':') {
        Some((h, s)) => (h, s.trim()),
        None => return Err("missing `:` after the type".to_string()),
    };
    if subject.is_empty() {
        return Err("empty subject after `:`".to_string());
    }
    let ty = head.split('(').next().unwrap_or(head).trim();
    if !ALLOWED_TYPES.contains(&ty) {
        return Err(format!(
            "type `{ty}` not in the allowed set ({})",
            ALLOWED_TYPES.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_and_scoped() {
        assert!(validate("feat: add thing").is_ok());
        assert!(validate("fix(parser): handle empty").is_ok());
        assert!(validate("docs: readme").is_ok());
        // Body lines are ignored — only the first line is validated.
        assert!(validate("feat: x\n\nlong body\nmore").is_ok());
    }

    #[test]
    fn rejects_bad_shape() {
        assert!(validate("").is_err());
        assert!(validate("no colon here").is_err());
        assert!(validate("feat:").is_err());
        assert!(validate("wat: something").is_err());
        assert!(validate("FEAT: uppercase").is_err());
    }

    #[test]
    fn reports_allowed_types_on_bad_type() {
        let err = validate("bogus: x").unwrap_err();
        assert!(err.contains("bogus"));
        assert!(err.contains("feat"));
    }
}
