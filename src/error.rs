use std::fmt;

/// A single validation failure with a stable, machine-comparable rendering:
/// `error[<code>] <subject>: <message> (<file>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub code: &'static str,
    pub subject: String,
    pub message: String,
    /// House-relative path, when the error is attributable to one file.
    pub file: Option<String>,
}

impl ValidationError {
    pub fn new(
        code: &'static str,
        subject: impl Into<String>,
        message: impl Into<String>,
        file: Option<String>,
    ) -> Self {
        Self {
            code,
            subject: subject.into(),
            message: message.into(),
            file,
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}] {}: {}", self.code, self.subject, self.message)?;
        if let Some(file) = &self.file {
            write!(f, " ({file})")?;
        }
        Ok(())
    }
}

/// Deterministic rendering used by the CLI and the corpus tests.
pub fn render_sorted(errors: &[ValidationError]) -> Vec<String> {
    let mut lines: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    lines.sort();
    lines.dedup();
    lines
}
