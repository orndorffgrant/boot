// SPDX-License-Identifier: AGPL-3.0-or-later
//! Functional workflows: turn a command intent into a linear CLI plan.

use boot_protocol::FlashOptions;

use crate::domain::{
    ArtifactSource, AttachPolicy, CommandIntent, FollowCursor, ServerSelector, SessionAccess,
    SessionAcquire, SessionKind, TokenSource,
};
use crate::error::CliError;
use crate::plan::{
    Action, ArtifactRef, ChangeKind, CollectRequest, Plan, PlanBuilder, ProgressMode, ResourceKind,
    ServiceAction, TokenChange,
};

pub fn plan(intent: CommandIntent) -> Result<Plan, CliError> {
    match intent {
        CommandIntent::StartServer(options) => Ok(start_server(options)),
        CommandIntent::StopServer => Ok(service_plan(ServiceAction::Stop, "stop the local server")),
        CommandIntent::RestartServer => Ok(service_plan(
            ServiceAction::Restart,
            "restart the local server",
        )),
        CommandIntent::RunServer(options) => Ok(run_server(options)),
        CommandIntent::ListServers => Ok(collect_plan("list servers", CollectRequest::Servers)),
        CommandIntent::ShowServer(selector) => Ok(show_server(selector)),
        CommandIntent::AddServer(intent) => add_server(intent),
        CommandIntent::EditServer(intent) => edit_server(intent),
        CommandIntent::RemoveServer(name) => Ok(remove_server(name)),
        CommandIntent::CreateToken(intent) => Ok(one_action(
            "create an API token",
            Action::CreateToken(intent),
            ProgressMode::Spinner,
        )),
        CommandIntent::ListTokens { server, direct } => Ok(collect_plan(
            "list API tokens",
            CollectRequest::Tokens { server, direct },
        )),
        CommandIntent::RevokeToken(intent) => Ok(one_action(
            "revoke API tokens",
            Action::RevokeToken(intent),
            ProgressMode::Spinner,
        )),
        CommandIntent::ListProbes { server } => Ok(collect_plan(
            "list debug probes",
            CollectRequest::Probes { server },
        )),
        CommandIntent::RunFirmware(intent) => Ok(run_firmware(intent)),
        CommandIntent::AttachProbe(intent) => Ok(attach_probe(intent)),
        CommandIntent::StartDap { target, port } => Ok(start_dap(target, port)),
        CommandIntent::ListSerialPorts { server } => Ok(collect_plan(
            "list serial ports",
            CollectRequest::SerialPorts { server },
        )),
        CommandIntent::AttachSerial(intent) => Ok(attach_serial(intent)),
        CommandIntent::ListSessions { server, select } => Ok(collect_plan(
            "list sessions",
            CollectRequest::Sessions { server, select },
        )),
        CommandIntent::ReplaySession(intent) => Ok(replay_session(intent)),
        CommandIntent::ExportSession(intent) => Ok(export_session(intent)),
        CommandIntent::Update(intent) => Ok(update(intent)),
        CommandIntent::ShowOpenApi => Ok(one_action(
            "show the OpenAPI document",
            Action::ShowOpenApi,
            ProgressMode::None,
        )),
    }
}

fn one_action(title: &str, action: Action, progress: ProgressMode) -> Plan {
    let mut builder = PlanBuilder::new(title);
    builder.push(title, action, progress);
    builder.finish()
}

fn collect_plan(title: &str, request: CollectRequest) -> Plan {
    let mut builder = PlanBuilder::new(title);
    builder.push(title, Action::Collect(request), ProgressMode::Spinner);
    builder.finish()
}

fn start_server(options: crate::domain::ServerOptions) -> Plan {
    let mut builder = PlanBuilder::new("start the local server");
    let local = builder.slot();
    builder.push(
        "Prepare local server",
        Action::PrepareLocalServer {
            options: options.clone(),
            out: local,
        },
        ProgressMode::Spinner,
    );
    builder.push(
        "Start server service",
        Action::Service {
            action: ServiceAction::Start,
            options: Some(options),
            local: Some(local),
        },
        ProgressMode::Spinner,
    );
    builder.push(
        "Show server status",
        Action::InspectServer {
            selector: ServerSelector::default(),
            include_service: true,
        },
        ProgressMode::None,
    );
    builder.finish()
}

fn service_plan(action: ServiceAction, title: &str) -> Plan {
    let mut builder = PlanBuilder::new(title);
    builder.push(
        title,
        Action::Service {
            action,
            options: None,
            local: None,
        },
        ProgressMode::Spinner,
    );
    builder.finish()
}

fn run_server(options: crate::domain::ServerOptions) -> Plan {
    let mut builder = PlanBuilder::new("run the local server in the foreground");
    let local = builder.slot();
    builder.push(
        "Prepare local server",
        Action::PrepareLocalServer {
            options: options.clone(),
            out: local,
        },
        ProgressMode::Spinner,
    );
    builder.push(
        "Run server",
        Action::RunServerForeground { options, local },
        ProgressMode::None,
    );
    builder.finish()
}

fn show_server(selector: ServerSelector) -> Plan {
    let mut builder = PlanBuilder::new("show server status");
    let include_service = selector.as_str() == "local";
    builder.push(
        "Inspect server",
        Action::InspectServer {
            selector,
            include_service,
        },
        ProgressMode::None,
    );
    builder.finish()
}

fn add_server(intent: crate::domain::AddServerIntent) -> Result<Plan, CliError> {
    intent.token.validate()?;
    let mut builder = PlanBuilder::new("add a server");
    let token = add_read_token(&mut builder, &intent.token);
    if token.is_none() {
        builder.warn(format!(
            "no token supplied for '{}'; protected commands will fail until you add one",
            intent.name
        ));
    }
    builder.push(
        format!("Add server {}", intent.name),
        Action::AddServer {
            name: intent.name,
            url: intent.url,
            ca_cert: intent.ca_cert.map(|path| path.display().to_string()),
            token,
        },
        ProgressMode::None,
    );
    Ok(builder.finish())
}

fn edit_server(intent: crate::domain::EditServerIntent) -> Result<Plan, CliError> {
    intent.token.validate()?;
    let has_edits = intent.url.is_some()
        || intent.ca_cert.is_some()
        || !intent.token.is_empty()
        || intent.clear_token;
    let mut builder = PlanBuilder::new("edit a server");
    if !has_edits {
        let server = builder.slot();
        builder.push_hidden(
            "Load server",
            Action::LoadServer {
                selector: ServerSelector::named(intent.name.clone()),
                out: server,
            },
            ProgressMode::None,
        );
        let document = builder.slot();
        builder.push(
            "Edit server document",
            Action::OpenServerEditor {
                server,
                out: document,
            },
            ProgressMode::None,
        );
        builder.push(
            "Apply server document",
            Action::ApplyServerDocument { server, document },
            ProgressMode::None,
        );
        return Ok(builder.finish());
    }
    let token = if intent.clear_token {
        TokenChange::Clear
    } else {
        match add_read_token(&mut builder, &intent.token) {
            Some(key) => TokenChange::Set(key),
            None => TokenChange::Keep,
        }
    };
    builder.push(
        format!("Update server {}", intent.name),
        Action::UpdateServer {
            name: intent.name,
            url: intent.url,
            ca_cert: intent
                .ca_cert
                .map(|value| value.map(|path| path.display().to_string())),
            token,
        },
        ProgressMode::None,
    );
    Ok(builder.finish())
}

fn remove_server(name: String) -> Plan {
    let mut builder = PlanBuilder::new(format!("remove server {name}"));
    builder.push(
        format!("Remove server {name}"),
        Action::RemoveServer { name },
        ProgressMode::None,
    );
    builder.finish()
}

fn add_read_token(
    builder: &mut PlanBuilder,
    source: &TokenSource,
) -> Option<crate::plan::Key<crate::plan::TokenSecret>> {
    if source.is_empty() {
        return None;
    }
    let key = builder.slot();
    builder.push_hidden(
        "Read token",
        Action::ReadToken {
            source: source.clone(),
            out: key,
        },
        ProgressMode::None,
    );
    Some(key)
}

fn run_firmware(intent: crate::domain::RunFirmwareIntent) -> Plan {
    let mut builder = PlanBuilder::new("flash firmware and attach");
    let server = builder.slot();
    builder.push_hidden(
        "Load server",
        Action::LoadServer {
            selector: ServerSelector::named(intent.target.server.clone()),
            out: server,
        },
        ProgressMode::None,
    );
    let probe = builder.slot();
    builder.push_hidden(
        "Resolve probe",
        Action::ResolveResource {
            server,
            selector: intent.target.selector.clone(),
            kind: ResourceKind::Probe,
            out: probe,
        },
        ProgressMode::None,
    );
    let artifact = builder.slot();
    builder.push(
        format!("Upload {}", intent.firmware.display()),
        Action::UploadArtifact {
            server,
            path: intent.firmware,
            out: artifact,
        },
        ProgressMode::Bar(crate::plan::Unit::Bytes),
    );
    builder.push(
        "Flash firmware",
        Action::RunChange {
            server,
            resource: probe,
            kind: ChangeKind::Flash {
                artifact: Some(artifact),
                options: FlashOptions::default(),
            },
        },
        ProgressMode::ChildTasks,
    );
    match intent.attach {
        AttachPolicy::None => {}
        AttachPolicy::Rtt => {
            builder.push(
                "Attach to RTT and stream output",
                Action::AttachSession {
                    server,
                    resource: Some(probe),
                    kind: SessionKind::Rtt,
                    acquire: SessionAcquire::AttachOrCreate,
                    explicit_session: None,
                    artifact: ArtifactRef::Inherit,
                    baud_rate: None,
                    policy: follow_policy(
                        FollowCursor::Latest,
                        SessionAccess::Write,
                        false,
                        false,
                        crate::plan::ReconnectMode::UntilCancelled,
                        true,
                    ),
                },
                ProgressMode::Stream,
            );
        }
        AttachPolicy::Serial(target) => {
            let serial_server = builder.slot();
            builder.push_hidden(
                "Load serial server",
                Action::LoadServer {
                    selector: ServerSelector::named(target.server.clone()),
                    out: serial_server,
                },
                ProgressMode::None,
            );
            let port = builder.slot();
            builder.push_hidden(
                "Resolve serial port",
                Action::ResolveResource {
                    server: serial_server,
                    selector: target.selector,
                    kind: ResourceKind::SerialPort,
                    out: port,
                },
                ProgressMode::None,
            );
            builder.push(
                "Attach to serial and stream output",
                Action::AttachSession {
                    server: serial_server,
                    resource: Some(port),
                    kind: SessionKind::Serial,
                    acquire: SessionAcquire::AttachOrCreate,
                    explicit_session: None,
                    artifact: ArtifactRef::None,
                    baud_rate: Some(115200),
                    policy: follow_policy(
                        FollowCursor::Latest,
                        SessionAccess::Write,
                        false,
                        true,
                        crate::plan::ReconnectMode::UntilCancelled,
                        true,
                    ),
                },
                ProgressMode::Stream,
            );
        }
    }
    builder.finish()
}

fn attach_probe(intent: crate::domain::AttachProbeIntent) -> Plan {
    let mut builder = PlanBuilder::new("attach to a probe session");
    let server = builder.slot();
    builder.push_hidden(
        "Load server",
        Action::LoadServer {
            selector: ServerSelector::named(intent.target.server.clone()),
            out: server,
        },
        ProgressMode::None,
    );
    let probe = builder.slot();
    builder.push_hidden(
        "Resolve probe",
        Action::ResolveResource {
            server,
            selector: intent.target.selector,
            kind: ResourceKind::Probe,
            out: probe,
        },
        ProgressMode::None,
    );
    let artifact = match intent.artifact {
        ArtifactSource::Upload(path) => {
            let key = builder.slot();
            builder.push(
                format!("Upload {}", path.display()),
                Action::UploadArtifact {
                    server,
                    path,
                    out: key,
                },
                ProgressMode::Bar(crate::plan::Unit::Bytes),
            );
            ArtifactRef::Uploaded(key)
        }
        ArtifactSource::Existing(id) => ArtifactRef::Id(id),
        ArtifactSource::Clear => ArtifactRef::Clear,
        ArtifactSource::Inherit => ArtifactRef::Inherit,
    };
    builder.push(
        "Attach to RTT and stream output",
        Action::AttachSession {
            server,
            resource: Some(probe),
            kind: SessionKind::Rtt,
            acquire: intent.acquire,
            explicit_session: intent.session,
            artifact,
            baud_rate: None,
            policy: follow_policy(
                FollowCursor::Latest,
                intent.access,
                false,
                false,
                crate::plan::ReconnectMode::UntilCancelled,
                true,
            ),
        },
        ProgressMode::Stream,
    );
    builder.finish()
}

fn start_dap(target: crate::domain::ResourceSelector, _port: u16) -> Plan {
    let mut builder = PlanBuilder::new("start a DAP session");
    let server = builder.slot();
    builder.push_hidden(
        "Load server",
        Action::LoadServer {
            selector: ServerSelector::named(target.server),
            out: server,
        },
        ProgressMode::None,
    );
    let probe = builder.slot();
    builder.push_hidden(
        "Resolve probe",
        Action::ResolveResource {
            server,
            selector: target.selector,
            kind: ResourceKind::Probe,
            out: probe,
        },
        ProgressMode::None,
    );
    builder.push(
        "Start DAP session",
        Action::AttachSession {
            server,
            resource: Some(probe),
            kind: SessionKind::Dap,
            acquire: SessionAcquire::AttachOrCreate,
            explicit_session: None,
            artifact: ArtifactRef::None,
            baud_rate: None,
            policy: follow_policy(
                FollowCursor::Latest,
                SessionAccess::Write,
                false,
                false,
                crate::plan::ReconnectMode::Bounded(0),
                false,
            ),
        },
        ProgressMode::Stream,
    );
    builder.finish()
}

fn attach_serial(intent: crate::domain::AttachSerialIntent) -> Plan {
    let mut builder = PlanBuilder::new("attach to a serial session");
    let server = builder.slot();
    builder.push_hidden(
        "Load server",
        Action::LoadServer {
            selector: ServerSelector::named(intent.target.server),
            out: server,
        },
        ProgressMode::None,
    );
    let port = builder.slot();
    builder.push_hidden(
        "Resolve serial port",
        Action::ResolveResource {
            server,
            selector: intent.target.selector,
            kind: ResourceKind::SerialPort,
            out: port,
        },
        ProgressMode::None,
    );
    builder.push(
        "Attach to serial and stream output",
        Action::AttachSession {
            server,
            resource: Some(port),
            kind: SessionKind::Serial,
            acquire: intent.acquire,
            explicit_session: intent.session,
            artifact: ArtifactRef::None,
            baud_rate: Some(intent.baud_rate),
            policy: follow_policy(
                FollowCursor::Latest,
                intent.access,
                false,
                intent.access == SessionAccess::Write,
                crate::plan::ReconnectMode::UntilCancelled,
                true,
            ),
        },
        ProgressMode::Stream,
    );
    builder.finish()
}

fn replay_session(intent: crate::domain::ReplaySessionIntent) -> Plan {
    let mut builder = PlanBuilder::new("replay a retained session");
    let (server_key, session_key) = load_server_and_session(&mut builder, &intent.target);
    let mut policy = follow_policy(
        FollowCursor::After(intent.from),
        SessionAccess::Read,
        intent.include_input,
        false,
        crate::plan::ReconnectMode::None,
        false,
    );
    policy.strict_replay = true;
    let _ = server_key;
    builder.push(
        "Replay session",
        Action::FollowSession {
            session: session_key,
            policy,
        },
        ProgressMode::Stream,
    );
    builder.finish()
}

fn export_session(intent: crate::domain::ExportSessionIntent) -> Plan {
    let mut builder = PlanBuilder::new("export a retained session");
    let (server_key, session_key) = load_server_and_session(&mut builder, &intent.target);
    builder.push(
        format!("Export session to {}", intent.out.display()),
        Action::ExportSession {
            server: server_key,
            session: session_key,
            out_path: intent.out,
            format: intent.format,
            include_input: intent.include_input,
        },
        ProgressMode::Spinner,
    );
    builder.finish()
}

fn load_server_and_session(
    builder: &mut PlanBuilder,
    target: &crate::domain::ResourceSelector,
) -> (
    crate::plan::Key<crate::domain::ServerConfig>,
    crate::plan::Key<crate::plan::SessionHandle>,
) {
    let server = builder.slot();
    builder.push_hidden(
        "Load server",
        Action::LoadServer {
            selector: ServerSelector::named(target.server.clone()),
            out: server,
        },
        ProgressMode::None,
    );
    let session = builder.slot();
    builder.push_hidden(
        "Load session",
        Action::LoadSession {
            server,
            session_id: target.selector.clone(),
            out: session,
        },
        ProgressMode::None,
    );
    (server, session)
}

fn update(intent: crate::domain::UpdateIntent) -> Plan {
    let mut builder = PlanBuilder::new(if intent.check {
        "check for a boot update"
    } else {
        "update the installed boot binary"
    });
    let release = builder.slot();
    builder.push(
        "Fetch release",
        Action::FetchRelease {
            repo: intent.repo,
            version: intent.version,
            out: release,
        },
        ProgressMode::Spinner,
    );
    if !intent.check {
        builder.push(
            "Install release",
            Action::InstallRelease {
                release,
                force: intent.force,
                no_restart: intent.no_restart,
                yes: intent.yes,
            },
            ProgressMode::Bar(crate::plan::Unit::Bytes),
        );
    }
    builder.finish()
}

fn follow_policy(
    cursor: FollowCursor,
    access: SessionAccess,
    include_input: bool,
    pump_input: bool,
    reconnect: crate::plan::ReconnectMode,
    resume_on_suspend: bool,
) -> crate::plan::FollowSessionPolicy {
    crate::plan::FollowSessionPolicy {
        cursor,
        access,
        include_input,
        reconnect,
        resume_on_suspend,
        strict_replay: cursor == FollowCursor::Start,
        pump_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ArtifactSource, AttachProbeIntent, AttachSerialIntent, EditServerIntent,
        ReplaySessionIntent, ServerOptions, TokenSource, UpdateIntent,
    };
    use crate::plan::Action;

    fn resource(value: &str) -> crate::domain::ResourceSelector {
        crate::domain::ResourceSelector::parse(value).unwrap()
    }

    #[test]
    fn run_firmware_without_attach_has_flash_tasks_only() {
        let plan = run_firmware(crate::domain::RunFirmwareIntent {
            target: resource("local/probe"),
            firmware: "firmware.elf".into(),
            attach: AttachPolicy::None,
        });
        let actions: Vec<_> = plan.tasks.iter().map(|task| &task.action).collect();
        assert!(matches!(actions[0], Action::LoadServer { .. }));
        assert!(matches!(actions[1], Action::ResolveResource { .. }));
        assert!(matches!(actions[2], Action::UploadArtifact { .. }));
        assert!(matches!(actions[3], Action::RunChange { .. }));
        assert_eq!(actions.len(), 4);
    }

    #[test]
    fn run_firmware_with_rtt_attach_adds_stream_task() {
        let plan = run_firmware(crate::domain::RunFirmwareIntent {
            target: resource("local/probe"),
            firmware: "firmware.elf".into(),
            attach: AttachPolicy::Rtt,
        });
        assert!(matches!(
            plan.tasks.last().unwrap().action,
            Action::AttachSession {
                kind: SessionKind::Rtt,
                ..
            }
        ));
        assert_eq!(plan.tasks.last().unwrap().progress, ProgressMode::Stream);
    }

    #[test]
    fn attach_probe_upload_and_acquire_policy_are_planned() {
        let plan = attach_probe(AttachProbeIntent {
            target: resource("local/probe"),
            artifact: ArtifactSource::Upload("firmware.elf".into()),
            acquire: SessionAcquire::AttachOnly,
            access: SessionAccess::Read,
            session: None,
        });
        assert!(
            plan.tasks
                .iter()
                .any(|task| matches!(task.action, Action::UploadArtifact { .. }))
        );
        let attach = plan.tasks.last().unwrap();
        match &attach.action {
            Action::AttachSession {
                acquire, artifact, ..
            } => {
                assert_eq!(*acquire, SessionAcquire::AttachOnly);
                assert!(matches!(artifact, ArtifactRef::Uploaded(_)));
            }
            other => panic!("expected attach task, got {other:?}"),
        }
    }

    #[test]
    fn attach_serial_pumps_input_for_write_access() {
        let plan = attach_serial(AttachSerialIntent {
            target: resource("local/tty"),
            acquire: SessionAcquire::AttachOrCreate,
            access: SessionAccess::Write,
            session: None,
            baud_rate: 115200,
        });
        let attach = plan.tasks.last().unwrap();
        match &attach.action {
            Action::AttachSession {
                policy, baud_rate, ..
            } => {
                assert!(policy.pump_input);
                assert_eq!(*baud_rate, Some(115200));
            }
            other => panic!("expected attach task, got {other:?}"),
        }
    }

    #[test]
    fn edit_server_without_flags_opens_the_editor() {
        let plan = edit_server(EditServerIntent {
            name: "pi".into(),
            url: None,
            ca_cert: None,
            token: TokenSource::default(),
            clear_token: false,
        })
        .unwrap();
        assert!(matches!(
            plan.tasks[1].action,
            Action::OpenServerEditor { .. }
        ));
        assert!(matches!(
            plan.tasks[2].action,
            Action::ApplyServerDocument { .. }
        ));
    }

    #[test]
    fn update_check_does_not_install() {
        let plan = update(UpdateIntent {
            check: true,
            version: None,
            force: false,
            yes: false,
            no_restart: false,
            repo: None,
        });
        assert_eq!(plan.tasks.len(), 1);
        assert!(matches!(plan.tasks[0].action, Action::FetchRelease { .. }));
    }

    #[test]
    fn start_server_prepares_then_starts() {
        let plan = start_server(ServerOptions::default());
        assert_eq!(plan.tasks.len(), 3);
        assert!(matches!(
            plan.tasks[0].action,
            Action::PrepareLocalServer { .. }
        ));
        assert!(matches!(plan.tasks[1].action, Action::Service { .. }));
    }

    #[test]
    fn replay_session_uses_strict_replay() {
        let plan = replay_session(ReplaySessionIntent {
            target: resource("local/ses_1"),
            from: 4,
            include_input: true,
        });
        match &plan.tasks.last().unwrap().action {
            Action::FollowSession { policy, .. } => {
                assert!(policy.strict_replay);
                assert_eq!(policy.cursor, FollowCursor::After(4));
                assert!(policy.include_input);
            }
            other => panic!("expected follow task, got {other:?}"),
        }
    }
}
