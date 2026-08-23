//! `--More--` paging for long show output, EOS/JunOS style.
//!
//! [`page`] prints at most one screenful, then waits: space = next page,
//! enter = next line, q (or Esc / ^C) = quit. Paging only engages when
//! stdout is a terminal — pipes, `ssh host cmd`, and redirects always get
//! the full text — so every show path can route through here
//! unconditionally.

use std::io::Write;

use crossterm::tty::IsTty;

/// What the operator asked for at the `--More--` prompt.
enum Advance {
    Page,
    Line,
    Quit,
}

/// Print `text`, paginating when stdout is an interactive terminal.
pub fn page(text: &str) {
    let mut stdout = std::io::stdout();
    let paged = stdout.is_tty() && std::io::stdin().is_tty();
    let size = crossterm::terminal::size().ok();
    let (Some((cols, rows)), true) = (size, paged) else {
        print!("{text}");
        let _ = stdout.flush();
        return;
    };
    // Leave one row for the --More-- prompt.
    let page_rows = usize::from(rows.max(2)) - 1;
    let cols = usize::from(cols.max(1));

    let mut budget = page_rows;
    for line in text.split_inclusive('\n') {
        if budget == 0 {
            match more_prompt(&mut stdout) {
                Advance::Page => budget = page_rows,
                Advance::Line => budget = 1,
                Advance::Quit => return,
            }
        }
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        // A long line wraps and eats several display rows.
        let rows_used = 1 + stripped.chars().count().saturating_sub(1) / cols;
        budget = budget.saturating_sub(rows_used);
        print!("{line}");
    }
    let _ = stdout.flush();
}

/// Show `--More--`, read one key in raw mode, erase the prompt.
fn more_prompt(stdout: &mut std::io::Stdout) -> Advance {
    print!(" --More-- ");
    let _ = stdout.flush();

    let advance = read_key();

    // Erase the prompt so output flows uninterrupted.
    print!("\r          \r");
    let _ = stdout.flush();
    advance
}

fn read_key() -> Advance {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    if crossterm::terminal::enable_raw_mode().is_err() {
        return Advance::Page;
    }
    let advance = loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char(' ') => break Advance::Page,
                KeyCode::Enter | KeyCode::Char('j') | KeyCode::Down => break Advance::Line,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break Advance::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Advance::Quit
                }
                _ => continue,
            },
            Ok(_) => continue,
            Err(_) => break Advance::Quit,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    advance
}
