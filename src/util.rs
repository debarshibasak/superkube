//! Small shared helpers used by both the binary entry point and the server.

/// Best-effort hostname detection. Falls back to `localhost` if the system
/// call doesn't produce anything usable. The first DNS label is kept (so
/// `Foo.local` becomes `Foo`) since Kubernetes node names are conventionally
/// short identifiers, not fully-qualified names.
pub fn detect_hostname() -> String {
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                let short = trimmed.split('.').next().unwrap_or(trimmed);
                return short.to_string();
            }
        }
    }
    "localhost".to_string()
}
