pub mod configure;
pub mod doctor;
pub mod explain;
pub mod status;
pub mod systemd;
pub mod tail;
pub mod timeline;
pub mod uninstall;
pub mod wait;

/// Write a verb's answer to stdout; a reader that closed the pipe early is not an error.
pub(crate) fn emit(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => r,
    }
}

/// Pretty JSON with a trailing newline.
pub(crate) fn emit_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    Ok(emit(&(serde_json::to_string_pretty(value)? + "\n"))?)
}
