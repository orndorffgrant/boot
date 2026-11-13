use std::ffi::OsString;

use anyhow::Result;
use clap::CommandFactory;

use crate::CLI;

struct OverviewGroup {
    title: &'static str,
    commands: &'static [&'static str],
    compact: bool,
}

const OVERVIEW: &[OverviewGroup] = &[
    OverviewGroup {
        title: "Server service",
        commands: &[
            "start-server",
            "stop-server",
            "restart-server",
            "run-server",
        ],
        compact: false,
    },
    OverviewGroup {
        title: "Server entries",
        commands: &[
            "list-servers",
            "show-server",
            "add-server",
            "edit-server",
            "remove-server",
        ],
        compact: false,
    },
    OverviewGroup {
        title: "Tokens",
        commands: &["create-token", "list-tokens", "revoke-token"],
        compact: false,
    },
    OverviewGroup {
        title: "Probes and firmware",
        commands: &["list-probes", "run-firmware", "attach-probe", "start-dap"],
        compact: false,
    },
    OverviewGroup {
        title: "Serial ports",
        commands: &["list-serial-ports", "attach-serial"],
        compact: false,
    },
    OverviewGroup {
        title: "Sessions",
        commands: &["list-sessions", "replay-session", "export-session"],
        compact: false,
    },
    OverviewGroup {
        title: "System",
        commands: &["update", "show-openapi"],
        compact: false,
    },
    OverviewGroup {
        title: "Help",
        commands: &[
            "help-server",
            "help-token",
            "help-probe",
            "help-serial",
            "help-session",
            "help-firmware",
            "help-dap",
            "help-openapi",
        ],
        compact: true,
    },
];

struct FocusedGroup {
    command: &'static str,
    title: &'static str,
    commands: &'static [&'static str],
}

const FOCUSED: &[FocusedGroup] = &[
    FocusedGroup {
        command: "help-server",
        title: "Server commands",
        commands: &[
            "start-server",
            "stop-server",
            "restart-server",
            "run-server",
            "list-servers",
            "show-server",
            "add-server",
            "edit-server",
            "remove-server",
        ],
    },
    FocusedGroup {
        command: "help-token",
        title: "Token commands",
        commands: &["create-token", "list-tokens", "revoke-token"],
    },
    FocusedGroup {
        command: "help-probe",
        title: "Probe commands",
        commands: &["list-probes", "run-firmware", "attach-probe", "start-dap"],
    },
    FocusedGroup {
        command: "help-serial",
        title: "Serial commands",
        commands: &["list-serial-ports", "attach-serial"],
    },
    FocusedGroup {
        command: "help-session",
        title: "Session commands",
        commands: &[
            "list-sessions",
            "attach-probe",
            "attach-serial",
            "replay-session",
            "export-session",
        ],
    },
    FocusedGroup {
        command: "help-firmware",
        title: "Firmware commands",
        commands: &["run-firmware"],
    },
    FocusedGroup {
        command: "help-dap",
        title: "DAP commands",
        commands: &["start-dap"],
    },
    FocusedGroup {
        command: "help-openapi",
        title: "OpenAPI commands",
        commands: &["show-openapi"],
    },
];

/// Handle `boot`, `boot --help`, `boot help`, and `boot help <command>`
/// before clap parses the command line.
pub fn handle(args: &[OsString]) -> Result<Option<i32>> {
    if args.len() == 1 {
        print_overview()?;
        return Ok(Some(2));
    }
    if args.len() == 2 {
        match args[1].to_str() {
            Some("-h" | "--help" | "help") => {
                print_overview()?;
                return Ok(Some(0));
            }
            _ => {}
        }
    }
    if args.len() >= 3 && args[1] == "help" {
        let name = args[2].to_string_lossy();
        let mut command = CLI::command();
        if let Some(subcommand) = command.find_subcommand_mut(name.as_ref()) {
            subcommand.print_help()?;
            println!();
            return Ok(Some(0));
        }
    }
    Ok(None)
}

pub fn print_overview() -> Result<()> {
    println!("boot — flash, debug, and observe embedded targets");
    println!();
    println!("Usage:");
    println!("  boot <command> [args]");
    println!("  boot help-<object>");
    println!();

    let cli = CLI::command();
    for group in OVERVIEW {
        println!("{}", group.title);
        if group.compact {
            print_compact(group.commands);
        } else {
            print_commands(&cli, group.commands);
        }
        println!();
    }

    println!("Options:");
    println!("  --cli-db <PATH>    Path to the CLI database");
    println!("  --api-db <PATH>    Path to the API database");
    println!("  -h, --help         Print help");
    println!("  -V, --version      Print version");
    Ok(())
}

pub fn print_focused(command: &str) -> Result<()> {
    let group = FOCUSED
        .iter()
        .find(|group| group.command == command)
        .ok_or_else(|| anyhow::anyhow!("Unknown help group '{}'", command))?;
    println!("{}:", group.title);
    let cli = CLI::command();
    print_commands(&cli, group.commands);
    Ok(())
}

fn print_commands(cli: &clap::Command, commands: &[&str]) {
    let width = commands
        .iter()
        .map(|command| command.len())
        .max()
        .unwrap_or(0);
    for name in commands {
        let about = cli
            .get_subcommands()
            .find(|command| command.get_name() == *name)
            .and_then(|command| command.get_about())
            .map(ToString::to_string)
            .unwrap_or_default();
        println!("  {name:<width$}  {about}");
    }
}

fn print_compact(commands: &[&str]) {
    for chunk in commands.chunks(4) {
        let line = chunk
            .iter()
            .map(|command| format!("{command:<18}"))
            .collect::<String>();
        println!("  {}", line.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATIONAL: &[&str] = &[
        "start-server",
        "stop-server",
        "restart-server",
        "run-server",
        "list-servers",
        "show-server",
        "add-server",
        "edit-server",
        "remove-server",
        "create-token",
        "list-tokens",
        "revoke-token",
        "list-probes",
        "run-firmware",
        "attach-probe",
        "start-dap",
        "list-serial-ports",
        "attach-serial",
        "list-sessions",
        "replay-session",
        "export-session",
        "update",
        "show-openapi",
    ];

    fn command_names() -> Vec<String> {
        CLI::command()
            .get_subcommands()
            .map(|command| command.get_name().to_string())
            .collect()
    }

    #[test]
    fn overview_lists_every_operational_command_once() {
        let names = command_names();
        for name in OPERATIONAL {
            assert!(names.contains(&name.to_string()), "missing command {name}");
            let count = OVERVIEW
                .iter()
                .flat_map(|group| group.commands)
                .filter(|command| *command == name)
                .count();
            assert_eq!(count, 1, "{name} should appear once in the overview");
        }
    }

    #[test]
    fn focused_groups_cover_commands_with_objects() {
        let names = command_names();
        for group in FOCUSED {
            assert!(!group.commands.is_empty(), "{} is empty", group.command);
            for command in group.commands {
                assert!(names.contains(&command.to_string()), "missing {command}");
            }
        }
        for name in OPERATIONAL {
            if *name == "update" {
                continue;
            }
            assert!(
                FOCUSED.iter().any(|group| group.commands.contains(name)),
                "{name} should appear in a focused group"
            );
        }
    }

    #[test]
    fn overview_and_focused_groups_reference_known_commands() {
        let names = command_names();
        for group in OVERVIEW {
            for command in group.commands {
                assert!(names.contains(&command.to_string()), "missing {command}");
            }
        }
    }
}
