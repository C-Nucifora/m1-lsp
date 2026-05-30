use m1_core::Diagnostic;

#[derive(Debug)]
pub enum FormatError {
    SyntaxErrors(Vec<Diagnostic>),
    IoError(std::io::Error),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::SyntaxErrors(diags) => {
                write!(f, "input has {} syntax error(s)", diags.len())
            }
            FormatError::IoError(e) => write!(f, "IO error: {}", e),
        }
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug, Clone)]
pub struct FormatWarning {
    pub kind: WarningKind,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum WarningKind {
    LineTooLong,
}
