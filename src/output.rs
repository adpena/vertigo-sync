//! Structured terminal output with color support.
//!
//! All functions write to stderr, keeping stdout clean for JSON/machine output.
//! Respects `NO_COLOR` env var and non-TTY stderr for plain-text fallback.

use std::io::Write;

/// Returns `true` when color output is enabled (TTY stderr + no `NO_COLOR`).
fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    supports_color::on(supports_color::Stream::Stderr).is_some()
}

/// Print a success line: green checkmark prefix.
pub fn success(msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{} {msg}", "\u{2713}".green());
    } else {
        let _ = writeln!(w, "OK {msg}");
    }
}

/// Print an info line: blue arrow prefix.
pub fn info(msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{} {msg}", "\u{2192}".blue());
    } else {
        let _ = writeln!(w, "  {msg}");
    }
}

/// Print a warning line: yellow warning prefix.
pub fn warn(msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{} {msg}", "\u{26A0}".yellow());
    } else {
        let _ = writeln!(w, "WARNING {msg}");
    }
}

/// Print an error line: red X prefix.
pub fn error_msg(msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{} {msg}", "\u{2717}".red());
    } else {
        let _ = writeln!(w, "ERROR {msg}");
    }
}

/// Print a numbered step: `[n/total] msg`.
pub fn step(n: usize, total: usize, msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{} {msg}", format!("[{n}/{total}]").dimmed());
    } else {
        let _ = writeln!(w, "[{n}/{total}] {msg}");
    }
}

/// Print a bold section header.
pub fn header(msg: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "{}", msg.bold());
    } else {
        let _ = writeln!(w, "{msg}");
    }
}

/// Print a right-padded key: value pair.
pub fn kv(key: &str, value: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        use owo_colors::OwoColorize;
        let _ = writeln!(w, "  {:>14} {value}", format!("{key}:").dimmed());
    } else {
        let _ = writeln!(w, "  {key:>14}: {value}");
    }
}

/// Print a boxed startup banner (Vite-style).
///
/// Each tuple is `(label, value)`. Labels get a colored arrow prefix inside the box.
pub fn banner(version: &str, lines: &[(&str, &str)]) {
    let mut w = std::io::stderr().lock();

    // Compute inner width: max of header line and all label+value lines.
    let header_text = format!("Vertigo Sync  v{version}");
    let mut max_inner = header_text.len();
    for (label, value) in lines {
        // "  -> Label:    Value" — arrow(2) + space + label + colon + 4-space pad + value
        let line_len = 5 + label.len() + 1 + 4 + value.len();
        if line_len > max_inner {
            max_inner = line_len;
        }
    }

    // Add horizontal padding.
    let pad = 2;
    let box_inner = max_inner + pad * 2;

    if color_enabled() {
        use owo_colors::OwoColorize;
        let border_h = "\u{2500}".repeat(box_inner);
        let _ = writeln!(w, "{}{border_h}{}", "\u{250C}".dimmed(), "\u{2510}".dimmed());

        // Empty line.
        let _ = writeln!(w, "{}{:box_inner$}{}", "\u{2502}".dimmed(), "", "\u{2502}".dimmed());

        // Header line.
        let header_pad = box_inner - header_text.len();
        let left_pad = header_pad / 2;
        let right_pad = header_pad - left_pad;
        let _ = writeln!(
            w,
            "{}{:left_pad$}{}{:right_pad$}{}",
            "\u{2502}".dimmed(),
            "",
            header_text.bold(),
            "",
            "\u{2502}".dimmed()
        );

        // Empty line.
        let _ = writeln!(w, "{}{:box_inner$}{}", "\u{2502}".dimmed(), "", "\u{2502}".dimmed());

        // Content lines.
        for (label, value) in lines {
            let content = format!("  {} {label:<10} {value}", "\u{2192}".cyan());
            // content contains ANSI escapes, so compute visual length separately.
            let visual_len = 2 + 1 + 1 + label.len().max(10) + 1 + value.len();
            let right = box_inner.saturating_sub(visual_len + pad);
            let _ = writeln!(
                w,
                "{}{:pad$}{content}{:right$}{}",
                "\u{2502}".dimmed(),
                "",
                "",
                "\u{2502}".dimmed()
            );
        }

        // Empty line.
        let _ = writeln!(w, "{}{:box_inner$}{}", "\u{2502}".dimmed(), "", "\u{2502}".dimmed());

        let _ = writeln!(w, "{}{border_h}{}", "\u{2514}".dimmed(), "\u{2518}".dimmed());
    } else {
        // Plain-text fallback.
        let _ = writeln!(w, "Vertigo Sync v{version}");
        for (label, value) in lines {
            let _ = writeln!(w, "  {label}: {value}");
        }
        let _ = writeln!(w);
    }
}

/// Print a separator line.
pub fn separator(label: &str) {
    let mut w = std::io::stderr().lock();
    if color_enabled() {
        let bar = "\u{2550}".repeat(label.len() + 4);
        let _ = writeln!(w, "{bar}");
    } else {
        let bar = "=".repeat(label.len() + 4);
        let _ = writeln!(w, "{bar}");
    }
}
