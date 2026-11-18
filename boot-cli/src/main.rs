// SPDX-License-Identifier: AGPL-3.0-or-later
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio_util::sync::CancellationToken;

mod command;
mod domain;
mod error;
mod execute;
mod follow;
mod local;
mod plan;
mod ports;
mod protocol;
mod ui;
mod workflow;

use command::{CLI, help};
use error::CliError;
use execute::Executor;
use plan::Outcome;
use ports::Ports;
use ui::{HumanUi, Ui};

#[tokio::main]
async fn main() -> ExitCode {
    let args = std::env::args_os().collect::<Vec<_>>();
    match help::handle(&args) {
        Ok(Some(code)) => return ExitCode::from(code as u8),
        Ok(None) => {}
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    }

    let cli = CLI::parse();
    if let Some(group) = cli.command.help_group() {
        if let Err(error) = help::print_focused(group) {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let intent = match cli.command.intent() {
        Ok(intent) => intent,
        Err(error) => return report_error(&error),
    };
    if cli.api_db.is_some() && intent.token_api_without_direct() {
        return report_error(&CliError::usage("--api-db requires --direct"));
    }
    let plan = match workflow::plan(intent) {
        Ok(plan) => plan,
        Err(error) => return report_error(&error),
    };
    let ui: Arc<dyn Ui> = Arc::new(HumanUi::new(cli.quiet));
    if cli.dry_run {
        ui.render_plan(&plan);
        return ExitCode::SUCCESS;
    }

    let ports = Ports::new(cli.cli_db, cli.api_db);
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if signal_cancel.is_cancelled() {
                std::process::exit(130);
            }
            signal_cancel.cancel();
        }
    });
    let executor = Executor::new(&ports, ui.clone(), cancel);
    match executor.execute(plan).await {
        Ok(outcome) => report_outcome(ui.as_ref(), outcome),
        Err(error) => report_error(&error),
    }
}

fn report_outcome(ui: &dyn Ui, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Done => ExitCode::SUCCESS,
        Outcome::View(view) => match ui.render(&view) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => report_error(&error),
        },
        Outcome::Streamed { summary } => match summary {
            Some(view) => match ui.render(&view) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => report_error(&error),
            },
            None => ExitCode::SUCCESS,
        },
        Outcome::Partial { view, failures } => {
            if let Err(error) = ui.render(&view) {
                return report_error(&error);
            }
            for failure in &failures {
                ui.error(&failure.to_string());
            }
            ExitCode::FAILURE
        }
    }
}

fn report_error(error: &CliError) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(error.exit_code() as u8)
}
