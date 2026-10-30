//! Terminal output: colors, times, diffs, pager.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

static COLOR: OnceLock<bool> = OnceLock::new();

pub fn color() -> bool {
    *COLOR.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

pub fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn red(s: &str) -> String { paint("31", s) }
pub fn green(s: &str) -> String { paint("32", s) }
pub fn yellow(s: &str) -> String { paint("33", s) }
pub fn cyan(s: &str) -> String { paint("36", s) }
pub fn dim(s: &str) -> String { paint("2", s) }
pub fn bold(s: &str) -> String { paint("1", s) }

pub fn ago(t: i64, now: i64) -> String {
    let d = (now - t).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

pub fn clock(t: i64) -> String {
    use chrono::{DateTime, Local};
    let Some(dt) = DateTime::from_timestamp(t, 0) else { return "?".into() };
    let dt = dt.with_timezone(&Local);
    if dt.date_naive() == Local::now().date_naive() {
        dt.format("%H:%M:%S").to_string()
    } else {
        dt.format("%b %d %H:%M").to_string()
    }
}

pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", b / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b / 1_048_576.0),
        _ => format!("{:.1} GB", b / 1_073_741_824.0),
    }
}

pub fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0) || std::str::from_utf8(b).is_err()
}

/// Lines added and removed, or None for binary content.
pub fn line_stats(a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    if is_binary(a) || is_binary(b) {
        return None;
    }
    let (sa, sb) = (std::str::from_utf8(a).ok()?, std::str::from_utf8(b).ok()?);
    let (mut ins, mut del) = (0, 0);
    for op in similar::TextDiff::from_lines(sa, sb).ops() {
        match op {
            similar::DiffOp::Insert { new_len, .. } => ins += new_len,
            similar::DiffOp::Delete { old_len, .. } => del += old_len,
            similar::DiffOp::Replace { old_len, new_len, .. } => {
                ins += new_len;
                del += old_len;
            }
            similar::DiffOp::Equal { .. } => {}
        }
    }
    Some((ins, del))
}

pub fn unified(path: &str, a: Option<&[u8]>, b: Option<&[u8]>) -> String {
    let mut out = bold(&format!("── {path}")) + "\n";
    let ea = a.unwrap_or(b"");
    let eb = b.unwrap_or(b"");
    if is_binary(ea) || is_binary(eb) {
        return out + &dim("   (binary file changed)") + "\n\n";
    }
    let ah = if a.is_some() { format!("a/{path}") } else { "/dev/null".into() };
    let bh = if b.is_some() { format!("b/{path}") } else { "/dev/null".into() };
    let (sa, sb) = (std::str::from_utf8(ea).unwrap(), std::str::from_utf8(eb).unwrap());
    let text = similar::TextDiff::from_lines(sa, sb)
        .unified_diff()
        .context_radius(3)
        .header(&ah, &bh)
        .to_string();
    if text.is_empty() {
        return out + &dim("   (only permissions changed)") + "\n\n";
    }
    for (i, line) in text.lines().enumerate() {
        let l = if i < 2 {
            bold(line)
        } else if line.starts_with('+') {
            green(line)
        } else if line.starts_with('-') {
            red(line)
        } else if line.starts_with("@@") {
            cyan(line)
        } else {
            line.to_string()
        };
        out.push_str(&l);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Print through `less` when on a terminal, so long diffs and logs scroll.
pub fn page(out: &str) {
    if out.is_empty() {
        return;
    }
    if std::io::stdout().is_terminal() {
        let pager = std::env::var("REWIND_PAGER").unwrap_or_else(|_| "less -FRX".into());
        let mut parts = pager.split_whitespace();
        if let Some(bin) = parts.next()
            && let Ok(mut child) = Command::new(bin).args(parts).stdin(Stdio::piped()).spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(out.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
    let _ = std::io::stdout().write_all(out.as_bytes());
}
