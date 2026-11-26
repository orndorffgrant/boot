// SPDX-License-Identifier: AGPL-3.0-or-later
//! Terminal UI: progress, prompts, views, and session output.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

use boot_protocol::{RttRecord, Task};
use comfy_table::{Table as ComfyTable, presets::NOTHING};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::mpsc;

use crate::domain::SessionKind;
use crate::error::CliError;
use crate::follow::SessionGap;
use crate::plan::{Confirmation, Level, Plan, ProgressMode, Table, TaskId, Unit, View, Visibility};

#[derive(Debug, Clone)]
pub struct TaskProgress {
    pub done: u64,
    pub total: Option<u64>,
    #[allow(dead_code)]
    pub unit: Unit,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskFinished {
    pub success: bool,
    pub message: Option<String>,
}

pub trait Ui: Send + Sync {
    fn print_data(&self, text: &str) -> Result<(), CliError>;
    fn status(&self, text: &str);
    fn warning(&self, text: &str);
    fn error(&self, text: &str);
    fn task_started(&self, id: TaskId, title: &str, mode: ProgressMode);
    fn task_progress(&self, id: TaskId, progress: TaskProgress);
    fn task_message(&self, id: TaskId, level: Level, message: &str);
    fn child_task(&self, id: TaskId, task: &Task);
    fn task_finished(&self, id: TaskId, finished: TaskFinished);
    fn enter_stream(&self);
    fn exit_stream(&self);
    fn confirm(&self, confirmation: &Confirmation) -> Result<bool, CliError>;
    fn session_sink(&self, kind: SessionKind) -> Box<dyn SessionSink>;
    fn input(&self) -> Option<mpsc::Receiver<Vec<u8>>>;
    fn render(&self, view: &View) -> Result<(), CliError>;
    fn render_plan(&self, plan: &Plan);
}

pub trait SessionSink: Send {
    fn raw(&mut self, stream: u8, data: &[u8]) -> Result<(), CliError>;
    fn rtt_records(&mut self, stream: u8, records: &[RttRecord]) -> Result<(), CliError>;
    fn input(&mut self, data: &[u8]) -> Result<(), CliError>;
    fn status(&mut self, message: &str);
    fn gap(&mut self, gap: &SessionGap);
}

pub struct HumanUi {
    quiet: bool,
    interactive: bool,
    progress: Mutex<HashMap<usize, ProgressBar>>,
    tasks: Mutex<HashMap<usize, (String, ProgressMode)>>,
}

impl HumanUi {
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            interactive: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            progress: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
        }
    }

    fn finish_all(&self) {
        let mut progress = self.progress.lock().expect("progress");
        for (_, bar) in progress.drain() {
            bar.finish_and_clear();
        }
    }

    fn bar_for(&self, id: TaskId) -> Option<ProgressBar> {
        self.progress.lock().expect("progress").get(&id.0).cloned()
    }
}

impl Ui for HumanUi {
    fn print_data(&self, text: &str) -> Result<(), CliError> {
        print!("{text}");
        std::io::stdout().flush()?;
        Ok(())
    }

    fn status(&self, text: &str) {
        if !self.quiet {
            eprintln!("{text}");
        }
    }

    fn warning(&self, text: &str) {
        if !self.quiet {
            eprintln!("warning: {text}");
        }
    }

    fn error(&self, text: &str) {
        eprintln!("error: {text}");
    }

    fn task_started(&self, id: TaskId, title: &str, mode: ProgressMode) {
        self.tasks
            .lock()
            .expect("tasks")
            .insert(id.0, (title.to_string(), mode));
        if self.quiet || mode == ProgressMode::None {
            return;
        }
        if !self.interactive {
            eprintln!("... {title}");
            return;
        }
        let bar = match mode {
            ProgressMode::Spinner | ProgressMode::ChildTasks | ProgressMode::Stream => {
                ProgressBar::new_spinner()
            }
            ProgressMode::Bar(_) => ProgressBar::new(0),
            ProgressMode::None => return,
        };
        let template = match mode {
            ProgressMode::Bar(Unit::Bytes) => {
                "{spinner:.cyan} {msg} {wide_bar:.cyan/blue} {bytes}/{total_bytes}"
            }
            ProgressMode::Bar(Unit::Count) => {
                "{spinner:.cyan} {msg} {wide_bar:.cyan/blue} {pos}/{len}"
            }
            _ => "{spinner:.cyan} {msg}",
        };
        let style = match mode {
            ProgressMode::Bar(_) => ProgressStyle::default_bar(),
            _ => ProgressStyle::default_spinner(),
        };
        bar.set_style(
            style
                .template(template)
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message(title.to_string());
        self.progress.lock().expect("progress").insert(id.0, bar);
    }

    fn task_progress(&self, id: TaskId, progress: TaskProgress) {
        if let Some(bar) = self.bar_for(id) {
            if let Some(total) = progress.total {
                bar.set_length(total);
            }
            bar.set_position(progress.done);
            if let Some(message) = progress.message {
                bar.set_message(message);
            }
        }
    }

    fn task_message(&self, id: TaskId, level: Level, message: &str) {
        let prefix = match level {
            Level::Info => "",
            Level::Warn => "warning: ",
            Level::Error => "error: ",
        };
        if let Some(bar) = self.bar_for(id) {
            bar.println(format!("{prefix}{message}"));
        } else if !self.quiet || level == Level::Error {
            eprintln!("{prefix}{message}");
        }
    }

    fn child_task(&self, id: TaskId, task: &Task) {
        let percent = task
            .progress
            .as_ref()
            .and_then(|progress| progress.percent)
            .map(|percent| format!(" {percent:.0}%"))
            .unwrap_or_default();
        let line = format!("{} {}{}", task.status, task.summary, percent);
        if let Some(bar) = self.bar_for(id) {
            if matches!(task.status.as_str(), "done" | "failed" | "cancelled") {
                bar.println(format!("  {line}"));
            } else {
                bar.set_message(line);
            }
        } else if self.interactive && !self.quiet {
            eprintln!("  {line}");
        }
    }

    fn task_finished(&self, id: TaskId, finished: TaskFinished) {
        let bar = self.progress.lock().expect("progress").remove(&id.0);
        let task = self.tasks.lock().expect("tasks").remove(&id.0);
        let (title, mode) = task.unwrap_or_else(|| (String::new(), ProgressMode::None));
        let symbol = if finished.success { "✓" } else { "✗" };
        let message = finished.message.unwrap_or_default();
        if let Some(bar) = bar {
            if finished.success {
                let label = if message.is_empty() {
                    format!("{symbol} {title}")
                } else {
                    format!("{symbol} {message}")
                };
                bar.finish_with_message(label.trim_end().to_string());
            } else {
                bar.finish_and_clear();
                eprintln!("{symbol} {title}");
            }
        } else if self.quiet {
            // Errors are still surfaced by the caller; no progress output.
        } else if !self.interactive {
            if mode != ProgressMode::None {
                eprintln!("{symbol} {title}");
            }
        } else if !finished.success {
            eprintln!("{symbol} {title}");
        }
    }

    fn enter_stream(&self) {
        self.finish_all();
    }

    fn exit_stream(&self) {}

    fn confirm(&self, confirmation: &Confirmation) -> Result<bool, CliError> {
        if !confirmation.required {
            return Ok(true);
        }
        if !self.interactive {
            return if confirmation.default {
                Ok(true)
            } else {
                Err(CliError::config(format!(
                    "{}; pass --yes to continue non-interactively",
                    confirmation.prompt
                )))
            };
        }
        let hint = if confirmation.default {
            "[Y/n]"
        } else {
            "[y/N]"
        };
        eprint!("{} {hint} ", confirmation.prompt);
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() {
            Ok(confirmation.default)
        } else {
            Ok(matches!(answer.as_str(), "y" | "yes"))
        }
    }

    fn session_sink(&self, kind: SessionKind) -> Box<dyn SessionSink> {
        Box::new(HumanSessionSink {
            kind,
            quiet: self.quiet,
        })
    }

    fn input(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stdin = tokio::io::stdin();
            let mut buffer = [0u8; 1024];
            loop {
                match stdin.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Some(rx)
    }

    fn render(&self, view: &View) -> Result<(), CliError> {
        match view {
            View::Text(text) => self.print_data(text)?,
            View::Fields(fields) => {
                for field in fields {
                    println!("{:<12}{}", format!("{}:", field.label), field.value);
                }
            }
            View::Table(table) => print_table(table),
        }
        Ok(())
    }

    fn render_plan(&self, plan: &Plan) {
        println!("Plan: {}", plan.title);
        for task in &plan.tasks {
            let marker = match task.policy.visible {
                Visibility::User => "",
                Visibility::Verbose => " (internal)",
            };
            println!("  {}. {}{marker}", task.id.number(), task.title);
            if let Some(detail) = &task.detail {
                println!("       {detail}");
            }
        }
        for warning in &plan.warnings {
            let prefix = match warning.level {
                Level::Info => "",
                Level::Warn => "warning: ",
                Level::Error => "error: ",
            };
            eprintln!("{prefix}{}", warning.message);
        }
        println!("No changes made.");
    }
}

fn print_table(table: &Table) {
    let mut output = ComfyTable::new();
    output.load_style(NOTHING);
    if !table.headers.is_empty() {
        output.set_header(table.headers.clone());
    }
    for row in &table.rows {
        output.add_row(row.clone());
    }
    println!("{output}");
}

struct HumanSessionSink {
    kind: SessionKind,
    quiet: bool,
}

impl SessionSink for HumanSessionSink {
    fn raw(&mut self, _stream: u8, data: &[u8]) -> Result<(), CliError> {
        let mut stdout = std::io::stdout().lock();
        match self.kind {
            SessionKind::Rtt => {
                stdout.write_all(String::from_utf8_lossy(data).as_bytes())?;
            }
            _ => stdout.write_all(data)?,
        }
        stdout.flush()?;
        Ok(())
    }

    fn rtt_records(&mut self, _stream: u8, records: &[RttRecord]) -> Result<(), CliError> {
        let mut stdout = std::io::stdout().lock();
        for record in records {
            stdout.write_all(record.display_line().as_bytes())?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn input(&mut self, data: &[u8]) -> Result<(), CliError> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"> ")?;
        stdout.write_all(data)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    }

    fn status(&mut self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }

    fn gap(&mut self, gap: &SessionGap) {
        if !self.quiet {
            eprintln!(
                "warning: missing session data from {} to {} ({} bytes); continuing",
                gap.from_seq, gap.to_seq, gap.dropped_bytes
            );
        }
    }
}
