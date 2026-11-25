// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native DAP adapter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use probe_rs::flashing;
use probe_rs::probe::DebugProbeSelector;
use probe_rs::probe::list::Lister;
use probe_rs::{CoreStatus, MemoryInterface, Permissions};
use probe_rs_debug::{
    DebugInfo, DebugRegisters, ObjectRef, StackFrame, VariableCache, exception_handler_for_core,
    stack_frame::StackFrameInfo,
};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use typed_path::TypedPath;

use crate::codec::{self, DapReader, Next};
use crate::svd::{self, SvdModel};
use crate::{DapBackend, DapError, DapIo, DapLaunch};

const THREAD_ID: i64 = 1;

pub struct NativeDapBackend;

impl DapBackend for NativeDapBackend {
    fn run(
        &mut self,
        io: DapIo,
        launch: DapLaunch,
        shutdown: CancellationToken,
    ) -> Result<(), DapError> {
        let mut session = AdapterSession::new(io.output.clone(), launch);
        let mut reader = DapReader::new(io.input);
        let mut last_status = CoreStatus::Unknown;
        let mut configured = false;

        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }

            match reader.next(Duration::from_millis(20)) {
                Ok(Next::Message(message)) => {
                    if !session.handle(&message, &mut configured)? {
                        return Ok(());
                    }
                    last_status = session.status().unwrap_or(CoreStatus::Unknown);
                }
                Ok(Next::Timeout) => {
                    if configured {
                        session.poll(&mut last_status)?;
                    }
                }
                Ok(Next::Ended) => return Ok(()),
                Err(error) => return Err(DapError::Other(error)),
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ScopeRef {
    Locals(usize),
    Registers(usize),
    Peripherals,
}

#[derive(Debug, Clone)]
enum VarRef {
    Locals { frame: usize, key: ObjectRef },
    SvdPeripheral(usize),
    SvdRegister(usize, usize),
}

struct FrameData {
    frame: StackFrame,
    locals: Option<VariableCache>,
}

struct AdapterSession {
    output: Sender<Vec<u8>>,
    launch: DapLaunch,
    probe_session: Option<probe_rs::Session>,
    program: Option<PathBuf>,
    debug_info: Option<DebugInfo>,
    svd: Option<SvdModel>,
    svd_path: Option<PathBuf>,
    core_index: usize,
    configured: bool,
    halted: bool,
    frames: Vec<FrameData>,
    breakpoints: Vec<(u64, String, i64)>,
    scope_refs: HashMap<i64, ScopeRef>,
    var_refs: HashMap<i64, VarRef>,
    next_ref: i64,
    adapter_seq: i64,
    #[allow(dead_code)]
    rtt: Option<probe_rs::rtt::Rtt>,
}

impl AdapterSession {
    fn new(output: Sender<Vec<u8>>, launch: DapLaunch) -> Self {
        Self {
            output,
            launch,
            probe_session: None,
            program: None,
            debug_info: None,
            svd: None,
            svd_path: None,
            core_index: 0,
            configured: false,
            halted: false,
            frames: Vec::new(),
            breakpoints: Vec::new(),
            scope_refs: HashMap::new(),
            var_refs: HashMap::new(),
            next_ref: 10_000,
            adapter_seq: 1,
            rtt: None,
        }
    }

    fn handle(&mut self, message: &Value, configured: &mut bool) -> Result<bool, DapError> {
        if message.get("type").and_then(Value::as_str) != Some("request") {
            return Ok(true);
        }
        let command = message
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| DapError::Other("DAP request has no command".into()))?;

        match command {
            "initialize" => {
                self.respond(message, Some(capabilities()))?;
                self.event("initialized", None)?;
            }
            "launch" | "attach" => match self.start(command, message) {
                Ok(()) => self.respond(message, None)?,
                Err(error) => {
                    self.error(message, &error.to_string())?;
                    return Err(error);
                }
            },
            "configurationDone" => {
                *configured = true;
                self.configured = true;
                if let Some(session) = self.probe_session.as_mut() {
                    if let Ok(mut core) = session.core(self.core_index) {
                        let _ = core.run();
                    }
                }
                self.halted = false;
                self.respond(message, None)?;
                self.event(
                    "continued",
                    Some(json!({"threadId": THREAD_ID, "allThreadsContinued": true})),
                )?;
            }
            "setBreakpoints" => self.set_breakpoints(message)?,
            "setFunctionBreakpoints" | "setInstructionBreakpoints" => {
                self.respond(message, Some(json!({"breakpoints": []})))?;
            }
            "threads" => {
                self.respond(
                    message,
                    Some(json!({"threads": [{"id": THREAD_ID, "name": "Core 0"}]})),
                )?;
            }
            "stackTrace" => self.stack_trace(message)?,
            "scopes" => self.scopes(message)?,
            "variables" => self.variables(message)?,
            "continue" => {
                if let Some(session) = self.probe_session.as_mut() {
                    let mut core = session
                        .core(self.core_index)
                        .map_err(|error| DapError::Other(error.to_string()))?;
                    core.run()
                        .map_err(|error| DapError::Other(error.to_string()))?;
                }
                self.halted = false;
                self.respond(message, Some(json!({"allThreadsContinued": true})))?;
                self.event(
                    "continued",
                    Some(json!({"threadId": THREAD_ID, "allThreadsContinued": true})),
                )?;
            }
            "pause" => {
                if let Some(session) = self.probe_session.as_mut() {
                    let mut core = session
                        .core(self.core_index)
                        .map_err(|error| DapError::Other(error.to_string()))?;
                    core.halt(Duration::from_millis(500))
                        .map_err(|error| DapError::Other(error.to_string()))?;
                }
                self.halted = true;
                self.refresh_frames()?;
                self.respond(message, None)?;
                self.stopped("pause")?;
            }
            "next" | "stepIn" | "stepOut" => {
                if let Some(session) = self.probe_session.as_mut() {
                    let mut core = session
                        .core(self.core_index)
                        .map_err(|error| DapError::Other(error.to_string()))?;
                    core.step()
                        .map_err(|error| DapError::Other(error.to_string()))?;
                }
                self.halted = true;
                self.refresh_frames()?;
                self.respond(message, None)?;
                self.stopped("step")?;
            }
            "disconnect" | "terminate" => {
                self.respond(message, None)?;
                return Ok(false);
            }
            "evaluate" => self.evaluate(message)?,
            "setVariable" => self.set_variable(message)?,
            "completions" => {
                self.respond(
                    message,
                    Some(json!({"targets": [
                        {"label":"help"}, {"label":"backtrace"}, {"label":"registers"},
                        {"label":"info"}, {"label":"inspect"}, {"label":"cpu"},
                        {"label":"breakpoint"}, {"label":"rtt"}
                    ]})),
                )?;
            }
            "readMemory" => self.read_memory(message)?,
            "writeMemory" => self.write_memory(message)?,
            "disassemble" => {
                self.respond(message, Some(json!({"instructions": []})))?;
            }
            "modules" | "loadedSources" | "source" => {
                self.respond(message, Some(json!({})))?;
            }
            _ => self.error(message, &format!("unsupported DAP command '{command}'"))?,
        }
        Ok(true)
    }

    fn start(&mut self, command: &str, message: &Value) -> Result<(), DapError> {
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let config = LaunchConfig::parse(args, &self.launch)?;
        self.program = config.program.clone();
        self.svd_path = config.svd.clone();
        self.core_index = config.core_index;
        self.debug_info = config
            .program
            .as_ref()
            .and_then(|path| DebugInfo::from_file(path).ok());
        self.svd = config
            .svd
            .as_ref()
            .and_then(|path| SvdModel::parse(path).ok());

        let selector: DebugProbeSelector = self
            .launch
            .probe_selector
            .parse()
            .map_err(|error| DapError::Other(format!("invalid probe selector: {error}")))?;
        let lister = Lister::new();
        let probe = lister
            .open(selector)
            .map_err(|error| DapError::Other(format!("failed to open probe: {error}")))?;
        let target = config
            .chip
            .as_deref()
            .map(probe_rs::config::TargetSelector::from)
            .unwrap_or(probe_rs::config::TargetSelector::Auto);
        let mut session = if config.connect_under_reset {
            probe.attach_under_reset(target, Permissions::default())
        } else {
            probe.attach(target, Permissions::default())
        }
        .map_err(|error| DapError::Other(format!("failed to attach target: {error}")))?;

        if command == "launch" && config.flashing {
            if let Some(program) = config.program.as_ref() {
                flashing::download_file(
                    &mut session,
                    program,
                    flashing::ElfLoader(flashing::ElfOptions::default()),
                )
                .map_err(|error| DapError::Other(format!("flash failed: {error}")))?;
                if let Ok(mut core) = session.core(self.core_index) {
                    let _ = core.reset_and_halt(Duration::from_millis(500));
                }
                self.halted = true;
            }
        }

        self.probe_session = Some(session);
        if self.halted {
            self.refresh_frames()?;
        }
        self.try_attach_rtt();
        Ok(())
    }

    fn try_attach_rtt(&mut self) {
        let Some(session) = self.probe_session.as_mut() else {
            return;
        };
        let Ok(mut core) = session.core(self.core_index) else {
            return;
        };
        if let Ok(rtt) = probe_rs::rtt::Rtt::attach(&mut core) {
            self.rtt = Some(rtt);
        }
    }

    fn status(&mut self) -> Result<CoreStatus, DapError> {
        let Some(session) = self.probe_session.as_mut() else {
            return Ok(CoreStatus::Unknown);
        };
        let mut core = session
            .core(self.core_index)
            .map_err(|error| DapError::Other(error.to_string()))?;
        core.status()
            .map_err(|error| DapError::Other(error.to_string()))
    }

    fn poll(&mut self, last: &mut CoreStatus) -> Result<(), DapError> {
        self.poll_rtt();
        let status = self.status()?;
        if status.is_halted() && !last.is_halted() {
            self.halted = true;
            self.refresh_frames()?;
            self.stopped("exception")?;
        }
        *last = status;
        Ok(())
    }

    fn poll_rtt(&mut self) {
        let mut chunks = Vec::new();
        {
            let Some(rtt) = self.rtt.as_mut() else {
                return;
            };
            let Some(session) = self.probe_session.as_mut() else {
                return;
            };
            let Ok(mut core) = session.core(self.core_index) else {
                return;
            };
            let mut buffer = [0u8; 1024];
            for channel in rtt.up_channels().iter_mut() {
                if let Ok(count) = channel.read(&mut core, &mut buffer) {
                    if count > 0 {
                        chunks.push(String::from_utf8_lossy(&buffer[..count]).to_string());
                    }
                }
            }
        }
        for text in chunks {
            let _ = self.event(
                "output",
                Some(json!({"category": "stdout", "output": text})),
            );
        }
    }

    fn refresh_frames(&mut self) -> Result<(), DapError> {
        self.frames.clear();
        self.scope_refs.clear();
        self.var_refs.clear();
        let Some(debug_info) = self.debug_info.as_ref() else {
            return Ok(());
        };
        let Some(session) = self.probe_session.as_mut() else {
            return Ok(());
        };
        let mut core = session
            .core(self.core_index)
            .map_err(|error| DapError::Other(error.to_string()))?;
        let registers = DebugRegisters::from_core(&mut core);
        let handler = exception_handler_for_core(core.core_type());
        let instruction_set = core.instruction_set().ok();
        let frames = debug_info
            .unwind(&mut core, registers, handler.as_ref(), instruction_set, 128)
            .map_err(|error| DapError::Other(format!("stack unwind failed: {error}")))?;
        self.frames = frames
            .into_iter()
            .map(|frame| FrameData {
                locals: frame.local_variables.clone(),
                frame,
            })
            .collect();
        Ok(())
    }

    fn set_breakpoints(&mut self, message: &Value) -> Result<(), DapError> {
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let source = args
            .get("source")
            .and_then(|source| source.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(session) = self.probe_session.as_mut() else {
            self.error(message, "target is not attached")?;
            return Ok(());
        };
        let mut core = session
            .core(self.core_index)
            .map_err(|error| DapError::Other(error.to_string()))?;

        let old: Vec<u64> = self
            .breakpoints
            .iter()
            .filter(|(_, path, _)| path == source)
            .map(|(address, _, _)| *address)
            .collect();
        for address in old {
            let _ = core.clear_hw_breakpoint(address);
        }
        self.breakpoints.retain(|(_, path, _)| path != source);

        let requested = args
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut response = Vec::new();
        for breakpoint in requested {
            let line = breakpoint.get("line").and_then(Value::as_i64).unwrap_or(0);
            let column = breakpoint.get("column").and_then(Value::as_u64);
            let mut verified = false;
            if let (Some(debug_info), false) = (self.debug_info.as_ref(), source.is_empty()) {
                let path = TypedPath::derive(source);
                if let Ok(location) =
                    debug_info.get_breakpoint_location(path, line.max(1) as u64, column)
                {
                    if core.set_hw_breakpoint(location.address).is_ok() {
                        self.breakpoints
                            .push((location.address, source.to_string(), line));
                        verified = true;
                    }
                }
            }
            response.push(json!({"verified": verified, "line": line}));
        }
        drop(core);
        self.respond(message, Some(json!({"breakpoints": response})))
    }

    fn stack_trace(&mut self, message: &Value) -> Result<(), DapError> {
        if self.frames.is_empty() {
            let _ = self.refresh_frames();
        }
        let frames: Vec<Value> = self
            .frames
            .iter()
            .enumerate()
            .map(|(index, frame_data)| {
                let frame = &frame_data.frame;
                let source = frame.source_location.as_ref().map(|location| {
                    let path = location.path.to_path().display().to_string();
                    let name = path
                        .rsplit('/')
                        .next()
                        .unwrap_or(path.as_str())
                        .to_string();
                    json!({
                        "name": name,
                        "path": path,
                    })
                });
                json!({
                    "id": index + 1,
                    "name": frame.function_name,
                    "source": source,
                    "line": frame.source_location.as_ref().and_then(|location| location.line).map(|line| line + 1).unwrap_or(0),
                    "column": 1,
                    "instructionPointerReference": format!("0x{:x}", register_value_to_u64(frame.pc)),
                })
            })
            .collect();
        self.respond(message, Some(json!({"stackFrames": frames})))
    }

    fn scopes(&mut self, message: &Value) -> Result<(), DapError> {
        let frame_id = message
            .get("arguments")
            .and_then(|args| args.get("frameId"))
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let frame = frame_id.saturating_sub(1) as usize;
        let locals = self
            .next_ref
            .checked_add(1)
            .ok_or_else(|| DapError::Other("variable reference overflow".into()))?;
        self.next_ref = locals + 1;
        self.scope_refs.insert(locals, ScopeRef::Locals(frame));
        let registers = self.next_ref;
        self.next_ref += 1;
        self.scope_refs
            .insert(registers, ScopeRef::Registers(frame));
        let mut scopes = vec![
            json!({"name": "Locals", "variablesReference": locals, "expensive": false}),
            json!({"name": "Registers", "variablesReference": registers, "expensive": false}),
        ];
        if self.svd.is_some() {
            let reference = self.next_ref;
            self.next_ref += 1;
            self.scope_refs.insert(reference, ScopeRef::Peripherals);
            scopes.push(
                json!({"name": "Peripherals", "variablesReference": reference, "expensive": true}),
            );
        }
        self.respond(message, Some(json!({"scopes": scopes})))
    }

    fn variables(&mut self, message: &Value) -> Result<(), DapError> {
        let reference = message
            .get("arguments")
            .and_then(|args| args.get("variablesReference"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let scope = self.scope_refs.get(&reference).cloned();
        let var = self.var_refs.get(&reference).cloned();
        let mut variables = Vec::new();
        match (scope, var) {
            (Some(ScopeRef::Locals(frame)), _) => {
                variables = self.local_variables(frame, None)?;
            }
            (Some(ScopeRef::Registers(frame)), _) => {
                if let Some(frame_data) = self.frames.get(frame) {
                    for register in &frame_data.frame.registers.0 {
                        let value = register
                            .value
                            .map(|value| format!("{value:?}"))
                            .unwrap_or_else(|| "<unavailable>".to_string());
                        variables.push(json!({
                            "name": register.core_register.name(),
                            "value": value,
                            "type": "register",
                            "variablesReference": 0
                        }));
                    }
                }
            }
            (Some(ScopeRef::Peripherals), _) => {
                let peripherals: Vec<(usize, String, String, u64)> = self
                    .svd
                    .as_ref()
                    .map(|svd| {
                        svd.peripherals
                            .iter()
                            .enumerate()
                            .map(|(index, peripheral)| {
                                (
                                    index,
                                    peripheral.name.clone(),
                                    peripheral.description.clone().unwrap_or_default(),
                                    peripheral.base_address,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (index, name, description, base_address) in peripherals {
                    let reference = self.reference_for(VarRef::SvdPeripheral(index));
                    variables.push(json!({
                        "name": name,
                        "value": description,
                        "type": format!("peripheral @ 0x{base_address:08x}"),
                        "variablesReference": reference
                    }));
                }
            }
            (_, Some(VarRef::Locals { frame, key })) => {
                variables = self.local_variables(frame, Some(key))?;
            }
            (_, Some(VarRef::SvdPeripheral(index))) => {
                let registers: Vec<(usize, String, String, u64)> = self
                    .svd
                    .as_ref()
                    .and_then(|svd| svd.peripherals.get(index))
                    .map(|peripheral| {
                        peripheral
                            .registers
                            .iter()
                            .enumerate()
                            .map(|(register_index, register)| {
                                (
                                    register_index,
                                    register.name.clone(),
                                    register.description.clone().unwrap_or_default(),
                                    register.address,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (register_index, name, description, address) in registers {
                    let reference = self.reference_for(VarRef::SvdRegister(index, register_index));
                    variables.push(json!({
                        "name": name,
                        "value": description,
                        "type": format!("register @ 0x{address:08x}"),
                        "variablesReference": reference
                    }));
                }
            }
            (_, Some(VarRef::SvdRegister(p, r))) => {
                if let Some(svd) = self.svd.as_ref()
                    && let Some(peripheral) = svd.peripherals.get(p)
                    && let Some(register) = peripheral.registers.get(r)
                {
                    let value = if let Some(session) = self.probe_session.as_mut() {
                        session
                            .core(self.core_index)
                            .map(|mut core| svd::read_register(&mut core, register))
                            .unwrap_or_else(|error| format!("<unreadable: {error}>"))
                    } else {
                        "<not attached>".to_string()
                    };
                    variables.push(json!({
                        "name": register.name,
                        "value": value,
                        "type": format!("u{}", register.size),
                        "variablesReference": 0
                    }));
                    if let Some(fields) = Some(&register.fields) {
                        for field in fields {
                            variables.push(json!({
                                "name": field.name,
                                "value": format!("bits {}:{}", field.offset + field.width - 1, field.offset),
                                "type": "field",
                                "variablesReference": 0
                            }));
                        }
                    }
                }
            }
            _ => {}
        }
        self.respond(message, Some(json!({"variables": variables})))
    }

    fn local_variables(
        &mut self,
        frame: usize,
        parent: Option<ObjectRef>,
    ) -> Result<Vec<Value>, DapError> {
        let Some(mut cache) = self
            .frames
            .get_mut(frame)
            .and_then(|frame_data| frame_data.locals.take())
        else {
            return Ok(Vec::new());
        };
        let frame_info = self.frames.get(frame).map(|frame_data| {
            (
                frame_data.frame.registers.clone(),
                frame_data.frame.frame_base,
                frame_data.frame.canonical_frame_address,
            )
        });
        let root = parent.unwrap_or_else(|| cache.root_variable().variable_key());
        if let (Some((registers, frame_base, canonical_frame_address)), Some(debug_info)) =
            (frame_info, self.debug_info.as_ref())
            && let Some(session) = self.probe_session.as_mut()
            && let Ok(mut core) = session.core(self.core_index)
            && let Some(mut variable) = cache.get_variable_by_key(root)
        {
            let info = StackFrameInfo {
                registers: &registers,
                frame_base,
                canonical_frame_address,
            };
            let _ = debug_info.cache_deferred_variables(&mut cache, &mut core, &mut variable, info);
            let _ = cache.update_variable(&variable);
        }
        let children: Vec<_> = cache.get_children(root).cloned().collect();
        let mut out = Vec::new();
        for mut child in children {
            let key = child.variable_key();
            let has_children = child.variable_node_type.is_deferred() || cache.has_children(&child);
            let reference = if has_children {
                self.reference_for(VarRef::Locals { frame, key })
            } else {
                0
            };
            if let Some(session) = self.probe_session.as_mut()
                && let Ok(mut core) = session.core(self.core_index)
            {
                child.extract_value(&mut core, &cache);
            }
            out.push(json!({
                "name": child.name.to_string(),
                "value": child.to_string(&cache),
                "type": child.type_name(),
                "variablesReference": reference
            }));
        }
        if let Some(frame_data) = self.frames.get_mut(frame) {
            frame_data.locals = Some(cache);
        }
        Ok(out)
    }

    fn reference_for(&mut self, value: VarRef) -> i64 {
        let reference = self.next_ref;
        self.next_ref += 1;
        self.var_refs.insert(reference, value);
        reference
    }

    fn evaluate(&mut self, message: &Value) -> Result<(), DapError> {
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let expression = args
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("repl");
        if context == "repl" {
            let result = self.repl(expression)?;
            self.respond(
                message,
                Some(json!({"result": result, "variablesReference": 0})),
            )?;
            return Ok(());
        }
        if let Some(value) = self.evaluate_expression(expression) {
            self.respond(
                message,
                Some(json!({"result": value, "variablesReference": 0})),
            )?;
        } else {
            self.error(message, &format!("could not evaluate '{expression}'"))?;
        }
        Ok(())
    }

    fn evaluate_expression(&mut self, expression: &str) -> Option<String> {
        let expression = expression.trim();
        if let Some(rest) = expression.strip_prefix("0x") {
            if let Ok(address) = u64::from_str_radix(rest, 16) {
                if let Some(session) = self.probe_session.as_mut()
                    && let Ok(mut core) = session.core(self.core_index)
                    && let Ok(value) = core.read_word_32(address)
                {
                    return Some(format!("0x{value:08x}"));
                }
            }
        }
        if let Ok(address) = expression.parse::<u64>() {
            if let Some(session) = self.probe_session.as_mut()
                && let Ok(mut core) = session.core(self.core_index)
                && let Ok(value) = core.read_word_32(address)
            {
                return Some(format!("0x{value:08x}"));
            }
        }
        for frame in &self.frames {
            if let Some(cache) = frame.locals.as_ref() {
                if let Some(variable) =
                    find_variable(cache, cache.root_variable().variable_key(), expression)
                {
                    let mut variable = variable;
                    if let Some(session) = self.probe_session.as_mut()
                        && let Ok(mut core) = session.core(self.core_index)
                    {
                        variable.extract_value(&mut core, cache);
                    }
                    return Some(variable.to_string(cache));
                }
            }
        }
        None
    }

    fn repl(&mut self, expression: &str) -> Result<String, DapError> {
        let mut parts = expression.split_whitespace();
        let command = parts.next().unwrap_or("help");
        match command {
            "help" => Ok(
                "commands: help, backtrace, registers, info, inspect <var>, cpu, breakpoint, rtt\n"
                    .into(),
            ),
            "backtrace" => Ok(self
                .frames
                .iter()
                .enumerate()
                .map(|(index, frame)| {
                    let location = frame
                        .frame
                        .source_location
                        .as_ref()
                        .map(|location| {
                            format!(
                                "{}:{}",
                                location.path.to_path().display(),
                                location.line.map(|line| line + 1).unwrap_or(0)
                            )
                        })
                        .unwrap_or_else(|| "<unknown>".into());
                    format!("#{index} {} at {location}\n", frame.frame.function_name)
                })
                .collect::<String>()),
            "registers" => Ok(self
                .frames
                .first()
                .map(|frame| {
                    frame
                        .frame
                        .registers
                        .0
                        .iter()
                        .map(|register| {
                            format!(
                                "{} = {}\n",
                                register.core_register.name(),
                                register
                                    .value
                                    .map(|value| format!("{value:?}"))
                                    .unwrap_or_else(|| "<unavailable>".into())
                            )
                        })
                        .collect::<String>()
                })
                .unwrap_or_else(|| "no halted frame\n".into())),
            "info" => Ok(format!(
                "program: {}\nsvd: {}\nbreakpoints: {}\n",
                self.program
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
                self.svd_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
                self.breakpoints.len()
            )),
            "inspect" => {
                let name = parts.next().unwrap_or_default();
                self.evaluate_expression(name)
                    .ok_or_else(|| DapError::Other(format!("variable '{name}' not found")))
            }
            "cpu" => Ok(format!(
                "thread 1: {}\n",
                if self.halted { "halted" } else { "running" }
            )),
            "breakpoint" => Ok(self
                .breakpoints
                .iter()
                .map(|(address, source, line)| format!("* 0x{address:08x} {source}:{line}\n"))
                .collect()),
            "rtt" => Ok("RTT is streamed as DAP output when available\n".into()),
            other => Err(DapError::Other(format!("unknown REPL command '{other}'"))),
        }
    }

    fn set_variable(&mut self, message: &Value) -> Result<(), DapError> {
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let reference = args
            .get("variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        let value = args
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(VarRef::SvdRegister(p, r)) = self.var_refs.get(&reference).cloned() {
            if let Some(svd) = self.svd.as_ref()
                && let Some(peripheral) = svd.peripherals.get(p)
                && let Some(register) = peripheral.registers.get(r)
            {
                let parsed = parse_number(value)
                    .ok_or_else(|| DapError::Other(format!("invalid numeric value '{value}'")))?;
                if let Some(session) = self.probe_session.as_mut() {
                    let mut core = session
                        .core(self.core_index)
                        .map_err(|error| DapError::Other(error.to_string()))?;
                    core.write_word_32(register.address, parsed as u32)
                        .map_err(|error| DapError::Other(error.to_string()))?;
                }
                self.respond(
                    message,
                    Some(json!({"value": format!("0x{parsed:x}"), "variablesReference": 0})),
                )?;
                return Ok(());
            }
        }
        self.error(
            message,
            &format!("setting variable '{name}' is not supported for this reference"),
        )
    }

    fn read_memory(&mut self, message: &Value) -> Result<(), DapError> {
        use base64::Engine;
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let reference = args
            .get("memoryReference")
            .and_then(Value::as_str)
            .unwrap_or("0");
        let address = parse_number(reference)
            .ok_or_else(|| DapError::Other("invalid memory reference".into()))?;
        let count = args.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
        let mut data = vec![0u8; count];
        let read = if let Some(session) = self.probe_session.as_mut() {
            session
                .core(self.core_index)
                .ok()
                .and_then(|mut core| core.read_8(address, &mut data).ok())
                .map(|_| data)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.respond(
            message,
            Some(json!({
                "address": format!("0x{address:x}"),
                "data": base64::prelude::BASE64_STANDARD.encode(&read),
                "unreadableBytes": count.saturating_sub(read.len())
            })),
        )
    }

    fn write_memory(&mut self, message: &Value) -> Result<(), DapError> {
        use base64::Engine;
        let args = message.get("arguments").unwrap_or(&Value::Null);
        let address = parse_number(
            args.get("memoryReference")
                .and_then(Value::as_str)
                .unwrap_or("0"),
        )
        .ok_or_else(|| DapError::Other("invalid memory reference".into()))?;
        let data = base64::prelude::BASE64_STANDARD
            .decode(args.get("data").and_then(Value::as_str).unwrap_or_default())
            .map_err(|error| DapError::Other(format!("invalid base64: {error}")))?;
        if let Some(session) = self.probe_session.as_mut() {
            let mut core = session
                .core(self.core_index)
                .map_err(|error| DapError::Other(error.to_string()))?;
            core.write_8(address, &data)
                .map_err(|error| DapError::Other(error.to_string()))?;
        }
        self.respond(message, Some(json!({"bytesWritten": data.len()})))
    }

    fn respond(&mut self, request: &Value, body: Option<Value>) -> Result<(), DapError> {
        let mut message = json!({
            "type": "response",
            "request_seq": request.get("seq").and_then(Value::as_i64).unwrap_or(0),
            "success": true,
            "command": request.get("command").and_then(Value::as_str).unwrap_or_default(),
            "seq": self.adapter_seq,
        });
        self.adapter_seq += 1;
        if let Some(body) = body {
            message["body"] = body;
        }
        codec::send(&self.output, &message).map_err(DapError::Other)
    }

    fn error(&mut self, request: &Value, message: &str) -> Result<(), DapError> {
        let response = json!({
            "type": "response",
            "request_seq": request.get("seq").and_then(Value::as_i64).unwrap_or(0),
            "success": false,
            "command": request.get("command").and_then(Value::as_str).unwrap_or_default(),
            "seq": self.adapter_seq,
            "message": message,
            "body": {"error": {"id": 1, "format": message}}
        });
        self.adapter_seq += 1;
        codec::send(&self.output, &response).map_err(DapError::Other)
    }

    fn event(&mut self, event: &str, body: Option<Value>) -> Result<(), DapError> {
        let mut message = json!({
            "type": "event",
            "event": event,
            "seq": self.adapter_seq,
        });
        self.adapter_seq += 1;
        if let Some(body) = body {
            message["body"] = body;
        }
        codec::send(&self.output, &message).map_err(DapError::Other)
    }

    fn stopped(&mut self, reason: &str) -> Result<(), DapError> {
        self.event(
            "stopped",
            Some(json!({
                "reason": reason,
                "threadId": THREAD_ID,
                "allThreadsStopped": true
            })),
        )
    }
}

#[derive(Debug)]
struct LaunchConfig {
    program: Option<PathBuf>,
    svd: Option<PathBuf>,
    chip: Option<String>,
    core_index: usize,
    flashing: bool,
    connect_under_reset: bool,
}

impl LaunchConfig {
    fn parse(args: &Value, launch: &DapLaunch) -> Result<Self, DapError> {
        let boot = args.get("bootArtifacts");
        let program = if let Some(path) = launch.program_override.as_ref() {
            Some(path.clone())
        } else if let Some(id) = boot
            .and_then(|value| value.get("program"))
            .and_then(Value::as_str)
        {
            Some(launch.artifacts.resolve(id)?)
        } else if let Some(id) = args
            .get("coreConfigs")
            .and_then(Value::as_array)
            .and_then(|cores| cores.first())
            .and_then(|core| core.get("bootProgram"))
            .and_then(Value::as_str)
        {
            Some(launch.artifacts.resolve(id)?)
        } else {
            args.get("coreConfigs")
                .and_then(Value::as_array)
                .and_then(|cores| cores.first())
                .and_then(|core| core.get("programBinary"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| {
                    args.get("program")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                })
        };

        let svd = if let Some(id) = boot
            .and_then(|value| value.get("svd"))
            .and_then(Value::as_str)
        {
            Some(launch.artifacts.resolve(id)?)
        } else if let Some(id) = args
            .get("coreConfigs")
            .and_then(Value::as_array)
            .and_then(|cores| cores.first())
            .and_then(|core| core.get("bootSvd"))
            .and_then(Value::as_str)
        {
            Some(launch.artifacts.resolve(id)?)
        } else {
            args.get("coreConfigs")
                .and_then(Value::as_array)
                .and_then(|cores| cores.first())
                .and_then(|core| core.get("svdFile"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| {
                    args.get("svdFile")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                })
        };

        let flashing = args
            .get("flashingConfig")
            .and_then(|config| config.get("flashingEnabled"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let connect_under_reset = args
            .get("connectUnderReset")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let core_index = args
            .get("coreConfigs")
            .and_then(Value::as_array)
            .and_then(|cores| cores.first())
            .and_then(|core| core.get("coreIndex"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        Ok(Self {
            program,
            svd,
            chip: args.get("chip").and_then(Value::as_str).map(str::to_string),
            core_index,
            flashing,
            connect_under_reset,
        })
    }
}

fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsFunctionBreakpoints": true,
        "supportsConditionalBreakpoints": false,
        "supportsHitConditionalBreakpoints": false,
        "supportsEvaluateForHovers": true,
        "supportsSetVariable": true,
        "supportsTerminateRequest": true,
        "supportsRestartRequest": true,
        "supportsInstructionBreakpoints": true,
        "supportsReadMemoryRequest": true,
        "supportsWriteMemoryRequest": true,
        "supportsDisassembleRequest": true,
        "supportsSteppingGranularity": true,
    })
}

fn find_variable(
    cache: &VariableCache,
    root: ObjectRef,
    name: &str,
) -> Option<probe_rs_debug::Variable> {
    for child in cache.get_children(root) {
        if child.name.to_string() == name {
            return Some(child.clone());
        }
        if let Some(found) = find_variable(cache, child.variable_key(), name) {
            return Some(found);
        }
    }
    None
}

fn parse_number(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(value, 16).ok()
    } else {
        value.parse().ok()
    }
}

fn register_value_to_u64(value: probe_rs::RegisterValue) -> u64 {
    match value {
        probe_rs::RegisterValue::U32(value) => u64::from(value),
        probe_rs::RegisterValue::U64(value) => value,
        probe_rs::RegisterValue::U128(value) => value as u64,
    }
}
