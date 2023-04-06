//! An interface to github actions workflow commands.

use std::fmt::Write;

/// Shows an error message directly in a github diff view on drop.
pub struct Error {
    file: String,
    line: usize,
    title: String,
    message: String,
}
impl Error {
    /// Set a line for this error. By default the message is shown at the top of the file.
    pub fn line(mut self, line: usize) -> Self {
        self.line = line;
        self
    }
}

/// Create an error to be shown for the given file and with the given title.
pub fn error(file: impl std::fmt::Display, title: impl Into<String>) -> Error {
    Error {
        file: file.to_string(),
        line: 0,
        title: title.into(),
        message: String::new(),
    }
}

impl Write for Error {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.message.write_str(s)
    }
}

impl Drop for Error {
    fn drop(&mut self) {
        if std::env::var_os("GITHUB_ACTION").is_some() {
            let Error {
                file,
                line,
                title,
                message,
            } = self;
            let message = message.trim();
            let message = if message.is_empty() {
                "::no message".into()
            } else {
                format!("::{}", github_action_multiline_escape(message))
            };
            eprintln!("::error file={file},line={line},title={title}{message}");
            eprintln!("error file={file},line={line},title={title}{message}");
        }
    }
}

fn github_action_multiline_escape(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
}

/// All github actions log messages from this call to the Drop of the return value
/// will be grouped and hidden by default in logs. Note that nesting these does
/// not really work.
pub fn group(name: impl std::fmt::Display) -> Group {
    if std::env::var_os("GITHUB_ACTION").is_some() {
        eprintln!("::group::{name}");
    }
    Group(())
}

/// A guard that closes the current github actions log group on drop.
pub struct Group(());

impl Drop for Group {
    fn drop(&mut self) {
        if std::env::var_os("GITHUB_ACTION").is_some() {
            eprintln!("::endgroup::");
        }
    }
}
