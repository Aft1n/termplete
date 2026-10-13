//! A small, shared terminal picker with a numbered fallback.

use std::io::{self, IsTerminal, Write};

use dialoguer::console::{truncate_str, Key, Term};

pub fn interactive() -> bool {
    io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        && Term::stderr().size().0 >= 8
}

fn paint(text: &str, code: &str) -> String {
    if interactive() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Escape/q/Ctrl+D cancels. All drawing and prompts share stderr.
pub fn select(title: &str, items: &[String], default: usize) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !interactive() {
        eprintln!("\n{title}");
        for (i, item) in items.iter().enumerate() {
            eprintln!("  {}. {item}", i + 1);
        }
        loop {
            eprint!("Choose 1-{} (q to cancel): ", items.len());
            io::stderr().flush()?;
            let mut answer = String::new();
            if io::stdin().read_line(&mut answer)? == 0 {
                return Ok(None);
            }
            let answer = answer.trim();
            if matches!(answer, "q" | "quit" | "") {
                return Ok(None);
            }
            if let Ok(n) = answer.parse::<usize>() {
                if (1..=items.len()).contains(&n) {
                    return Ok(Some(n - 1));
                }
            }
            eprintln!("Enter a number from 1 to {}.", items.len());
        }
    }

    // Enter raw input before drawing so a fast keypress cannot get stranded
    // in the cooked-mode buffer between redraw and read_key.
    let _input_mode = crate::shellutil::RawTerminal::enter(true)?;
    let term = Term::stderr();
    let mut selected = default.min(items.len() - 1);
    let mut drawn = 0;
    loop {
        if drawn > 0 {
            term.clear_last_lines(drawn)?;
        }
        let (rows, cols) = term.size();
        let width = usize::from(cols).saturating_sub(1).max(1);
        let page = usize::from(rows)
            .saturating_sub(6)
            .clamp(1, 7)
            .min(items.len());
        let start = (selected / page) * page;
        let end = (start + page).min(items.len());
        let line = |text: &str| term.write_line(&truncate_str(text, width, ""));
        line(&paint(&format!("+ termplete / {title}"), "1;36"))?;
        line(&paint(&"-".repeat(width.min(52)), "36"))?;
        for (i, item) in items.iter().enumerate().take(end).skip(start) {
            let prefix = if i == selected { ">" } else { " " };
            let row = format!("{prefix} {}  {item}", i + 1);
            line(&paint(&row, if i == selected { "1;32" } else { "0" }))?;
        }
        line(&paint(
            &format!("  {}/{} | Up/Down | Enter | Esc", selected + 1, items.len()),
            "33",
        ))?;
        term.flush()?;
        drawn = end - start + 3;
        let result = match term.read_key_raw()? {
            Key::ArrowUp | Key::BackTab | Key::Char('k') => {
                selected = (selected + items.len() - 1) % items.len();
                continue;
            }
            Key::ArrowDown | Key::Tab | Key::Char('j') => {
                selected = (selected + 1) % items.len();
                continue;
            }
            Key::Home => {
                selected = 0;
                continue;
            }
            Key::End => {
                selected = items.len() - 1;
                continue;
            }
            Key::PageUp => {
                selected = selected.saturating_sub(page);
                continue;
            }
            Key::PageDown => {
                selected = (selected + page).min(items.len() - 1);
                continue;
            }
            Key::Enter => Some(selected),
            Key::Escape | Key::CtrlC | Key::Char('q' | '\u{4}') => None,
            _ => continue,
        };
        term.clear_last_lines(drawn)?;
        if let Some(index) = result {
            line(&paint(&format!("+ {title}: {}", items[index]), "32"))?;
        }
        term.flush()?;
        return Ok(result);
    }
}
