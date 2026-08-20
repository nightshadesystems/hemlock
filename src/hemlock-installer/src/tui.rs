//! ratatui installer flow: pick a disk, confirm, watch the steps run.
//!
//! Deliberately spartan — this renders on a 9600-baud serial console in the
//! ONIE rescue environment.

use std::io;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::install::{Disk, InstallPlan};

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

struct Term {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Term {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Pick a target disk. `Ok(None)` = operator aborted.
pub fn select_disk(disks: &[Disk], platform_id: &str) -> Result<Option<Disk>> {
    if disks.is_empty() {
        anyhow::bail!("no installable disks found");
    }
    let mut term = Term::new()?;
    let mut state = ListState::default();
    state.select(Some(0));

    loop {
        term.terminal.draw(|frame| {
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(2),
            ])
            .split(frame.area());

            frame.render_widget(
                Paragraph::new(format!("Hemlock installer — platform {platform_id}"))
                    .block(Block::default().borders(Borders::ALL)),
                chunks[0],
            );

            let items: Vec<ListItem> = disks
                .iter()
                .map(|d| {
                    ListItem::new(format!(
                        "{:<14} {:>8.1} GiB  {}",
                        d.device.display(),
                        gib(d.size_bytes),
                        d.model
                    ))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Install target"),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, chunks[1], &mut state);

            frame.render_widget(
                Paragraph::new("up/down: select   enter: install (ERASES DISK)   q: abort"),
                chunks[2],
            );
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let selected = state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up => state.select(Some(selected.saturating_sub(1))),
                KeyCode::Down => state.select(Some((selected + 1).min(disks.len() - 1))),
                KeyCode::Enter => return Ok(Some(disks[selected].clone())),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                _ => {}
            }
        }
    }
}

/// Run the plan's steps with a live progress view.
pub fn run_install(plan: &InstallPlan) -> Result<()> {
    let steps = plan.steps();
    let mut done = vec![false; steps.len()];
    let mut term = Term::new()?;

    for (i, step) in steps.iter().enumerate() {
        draw_progress(&mut term, plan, &steps, &done, Some(i))?;
        plan.run_step(step)?;
        done[i] = true;
    }
    draw_progress(&mut term, plan, &steps, &done, None)?;
    Ok(())
}

fn draw_progress(
    term: &mut Term,
    plan: &InstallPlan,
    steps: &[crate::install::Step],
    done: &[bool],
    current: Option<usize>,
) -> Result<()> {
    term.terminal.draw(|frame| {
        let chunks =
            Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(frame.area());
        frame.render_widget(
            Paragraph::new(format!(
                "Installing Hemlock ({}) to {}{}",
                plan.platform_id,
                plan.disk.display(),
                if plan.dry_run { "  [DRY RUN]" } else { "" }
            ))
            .block(Block::default().borders(Borders::ALL)),
            chunks[0],
        );
        let items: Vec<ListItem> = steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let marker = if done[i] {
                    "[done] "
                } else if current == Some(i) {
                    "[....] "
                } else {
                    "[    ] "
                };
                ListItem::new(format!("{marker}{}", step.title))
            })
            .collect();
        frame.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title("Progress")),
            chunks[1],
        );
    })?;
    Ok(())
}
