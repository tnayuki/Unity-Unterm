//! Pure-Rust Mono soft-debugger (SDB) client.
//!
//! Connects to a Unity editor or player's Mono debugger agent over TCP, speaks the
//! debugger wire protocol directly (no .NET runtime, no PDB/mdb parser), and resolves
//! source breakpoints over the wire via `GET_DEBUG_INFO` sequence points.
//!
//! The agent pushes unsolicited composite events (it sends `VM_START` the instant we
//! connect), so the reader always demultiplexes replies from agent commands; events
//! seen while waiting for a reply are queued and drained by the caller.

#![allow(dead_code)]

pub mod value;
pub mod wire;

use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use wire::{cs, kind, vm, Decoder, Encoder, Packet, Result, SdbError};

/// Negotiated protocol/runtime version (e.g. "mono 6.13.0", 2.58 on Unity 6).
#[derive(Debug, Clone)]
pub struct Version {
    pub vm: String,
    pub major: i32,
    pub minor: i32,
}

/// One decoded event out of a composite event packet.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub kind: u8,
    pub request_id: i32,
    pub thread: u32,
    /// For BREAKPOINT/STEP/METHOD_*: the method id.
    pub method: u32,
    /// For BREAKPOINT/STEP: the IL offset.
    pub location: i64,
    /// For TYPE_LOAD: the loaded type id.
    pub type_id: u32,
    /// For ASSEMBLY_LOAD/UNLOAD: the assembly id.
    pub assembly: u32,
    /// For APPDOMAIN_CREATE/UNLOAD: the domain id.
    pub domain: u32,
}

/// A composite event: a suspend policy plus one or more events.
#[derive(Debug, Clone)]
pub struct Composite {
    pub suspend_policy: u8,
    pub events: Vec<Event>,
}

/// Mono's marker line for compiler-generated ("hidden") code; never a real stop.
pub const HIDDEN_LINE: i32 = 0xFEEFEE;

/// One sequence point: an IL offset mapped to a source location.
#[derive(Debug, Clone)]
pub struct SeqPoint {
    pub il_offset: i32,
    pub line: i32,
    pub source_idx: i32,
    pub column: i32,
    pub end_line: i32,
    pub end_column: i32,
}

impl SeqPoint {
    pub fn is_hidden(&self) -> bool {
        self.line == HIDDEN_LINE
    }
}

/// A method's debug info: its source files and the sequence-point table.
#[derive(Debug, Clone)]
pub struct DebugInfo {
    pub max_il: i32,
    pub sources: Vec<String>,
    pub seq_points: Vec<SeqPoint>,
}

/// One call-stack frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: i32,
    pub method: u32,
    pub il_offset: i32,
    pub flags: u8,
}

/// A method local variable's metadata.
#[derive(Debug, Clone)]
pub struct Local {
    pub type_id: u32,
    pub name: String,
    pub live_start: i32,
    pub live_end: i32,
}

/// A type's field metadata.
#[derive(Debug, Clone)]
pub struct Field {
    pub id: u32,
    pub name: String,
    pub type_id: u32,
    pub attrs: i32,
}

impl Field {
    /// FieldAttributes.Static (0x10) — skip these for instance (`this`) values.
    pub fn is_static(&self) -> bool {
        self.attrs & 0x10 != 0
    }
}

/// A modifier attached to an event request.
pub enum Modifier {
    /// Restrict a breakpoint to a method + IL offset.
    LocationOnly { method: u32, il: i64 },
    /// Restrict TYPE_LOAD events to types declared in these source files.
    SourceFileOnly(Vec<String>),
    /// Configure a single-step.
    Step {
        thread: u32,
        size: i32,
        depth: i32,
        filter: i32,
    },
}

/// A live SDB connection.
pub struct Connection {
    stream: TcpStream,
    next_id: u32,
    queued: VecDeque<Composite>,
    version: Option<Version>,
}

impl Connection {
    /// Wrap an already-connected stream and perform the handshake.
    pub fn from_stream(stream: TcpStream) -> Result<Self> {
        let mut c = Connection {
            stream,
            next_id: 0,
            queued: VecDeque::new(),
            version: None,
        };
        c.handshake()?;
        c.version = Some(c.query_version()?);
        Ok(c)
    }

    fn handshake(&mut self) -> Result<()> {
        self.stream.write_all(wire::HANDSHAKE)?;
        self.stream.flush()?;
        // Bound the handshake read so a second debugger connecting to an agent that's
        // already serving one (the agent accepts the TCP but never replies) fails fast
        // instead of hanging forever on "connecting".
        self.stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .ok();
        let mut buf = [0u8; wire::HANDSHAKE.len()];
        use std::io::Read;
        let mut got = 0;
        while got < buf.len() {
            let n = self.stream.read(&mut buf[got..])?;
            if n == 0 {
                return Err(SdbError::Eof);
            }
            got += n;
        }
        self.stream.set_read_timeout(None).ok();
        if buf != wire::HANDSHAKE {
            return Err(SdbError::Protocol("bad handshake reply".into()));
        }
        Ok(())
    }

    /// The negotiated version (always set after construction).
    pub fn version(&self) -> &Version {
        self.version.as_ref().expect("version queried at connect")
    }

    /// Send a command and block until its reply, queueing any events that arrive first.
    /// Returns the reply payload, or `SdbError::Agent` for a non-zero error code.
    pub fn request(&mut self, cmd_set: u8, cmd: u8, payload: &[u8]) -> Result<Vec<u8>> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        wire::write_command(&mut self.stream, id, cmd_set, cmd, payload)?;
        loop {
            match wire::read_packet(&mut self.stream)? {
                Packet::Reply {
                    id: rid,
                    error,
                    data,
                } if rid == id => {
                    if error != 0 {
                        return Err(SdbError::Agent(error));
                    }
                    return Ok(data);
                }
                Packet::Reply { id: rid, .. } => {
                    return Err(SdbError::Protocol(format!(
                        "reply id {rid} != expected {id}"
                    )));
                }
                Packet::Command { cmd_set, cmd, data, .. } => {
                    self.absorb_command(cmd_set, cmd, &data);
                }
            }
        }
    }

    /// Block for the next composite event (draining the queue first). Set a read
    /// timeout via [`set_read_timeout`] to bound the wait.
    pub fn next_event(&mut self) -> Result<Composite> {
        if let Some(ev) = self.queued.pop_front() {
            return Ok(ev);
        }
        loop {
            match wire::read_packet(&mut self.stream)? {
                Packet::Command { cmd_set, cmd, data, .. } => {
                    if let Some(ev) = decode_command(cmd_set, cmd, &data) {
                        return Ok(ev);
                    }
                }
                Packet::Reply { id, .. } => {
                    return Err(SdbError::Protocol(format!(
                        "unexpected reply id {id} while awaiting event"
                    )));
                }
            }
        }
    }

    /// Pop a queued event without blocking, if one was buffered during a `request`.
    pub fn take_queued(&mut self) -> Option<Composite> {
        self.queued.pop_front()
    }

    fn absorb_command(&mut self, cmd_set: u8, cmd: u8, data: &[u8]) {
        if let Some(ev) = decode_command(cmd_set, cmd, data) {
            self.queued.push_back(ev);
        }
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(t)?;
        Ok(())
    }

    // --- typed VM helpers ---

    fn query_version(&mut self) -> Result<Version> {
        let data = self.request(cs::VM, vm::VERSION, &[])?;
        let mut d = Decoder::new(&data);
        let vm = d.string()?;
        let major = d.int()?;
        let minor = d.int()?;
        Ok(Version { vm, major, minor })
    }

    /// All managed thread ids.
    pub fn all_threads(&mut self) -> Result<Vec<u32>> {
        let data = self.request(cs::VM, vm::ALL_THREADS, &[])?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        (0..n).map(|_| d.id()).collect()
    }

    pub fn resume(&mut self) -> Result<()> {
        self.request(cs::VM, vm::RESUME, &[]).map(|_| ())
    }

    pub fn suspend(&mut self) -> Result<()> {
        self.request(cs::VM, vm::SUSPEND, &[]).map(|_| ())
    }

    /// Root AppDomain id. Note: on Unity this domain holds only mscorlib; user
    /// assemblies live in a reloadable child domain surfaced via load events.
    pub fn root_domain(&mut self) -> Result<u32> {
        let data = self.request(cs::APPDOMAIN, wire::appdomain::GET_ROOT_DOMAIN, &[])?;
        Decoder::new(&data).id()
    }

    /// Assembly ids loaded in a domain.
    pub fn domain_assemblies(&mut self, domain: u32) -> Result<Vec<u32>> {
        let payload = Encoder::new().id(domain).finish();
        let data = self.request(cs::APPDOMAIN, wire::appdomain::GET_ASSEMBLIES, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        (0..n).map(|_| d.id()).collect()
    }

    pub fn assembly_name(&mut self, assembly: u32) -> Result<String> {
        let payload = Encoder::new().id(assembly).finish();
        let data = self.request(cs::ASSEMBLY, wire::assembly::GET_NAME, &payload)?;
        Decoder::new(&data).string()
    }

    // --- type / method introspection ---

    /// Type ids declaring code in `file`. Returns empty until those types are loaded
    /// (on Unity, user types load into a child domain at play time — prefer the
    /// TYPE_LOAD event path for arming pending breakpoints).
    pub fn types_for_source_file(&mut self, file: &str, ignore_case: bool) -> Result<Vec<u32>> {
        let payload = Encoder::new()
            .string(file)
            .byte(ignore_case as u8)
            .finish();
        let data = self.request(cs::VM, vm::GET_TYPES_FOR_SOURCE_FILE, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        (0..n).map(|_| d.id()).collect()
    }

    /// Resolve a type by name within an assembly (`GET_TYPE`). Returns 0 if absent.
    pub fn assembly_type(&mut self, assembly: u32, name: &str) -> Result<u32> {
        let payload = Encoder::new().id(assembly).string(name).byte(0).finish();
        let data = self.request(cs::ASSEMBLY, wire::assembly::GET_TYPE, &payload)?;
        Decoder::new(&data).id()
    }

    pub fn type_methods(&mut self, type_id: u32) -> Result<Vec<u32>> {
        let payload = Encoder::new().id(type_id).finish();
        let data = self.request(cs::TYPE, wire::ty::GET_METHODS, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        (0..n).map(|_| d.id()).collect()
    }

    pub fn method_name(&mut self, method: u32) -> Result<String> {
        let payload = Encoder::new().id(method).finish();
        let data = self.request(cs::METHOD, wire::method::GET_NAME, &payload)?;
        Decoder::new(&data).string()
    }

    /// The sequence-point table for a method (used to map file/line <-> IL offset).
    ///
    /// Unity's mono agent (verified on 6000.3.18f1 / mono 6.13.0 / protocol 2.58)
    /// sends the *simple* debug-info format regardless of the reported version:
    /// `int max_il`, a single length-prefixed source path, `int n_seq`, then
    /// `n_seq` × (`int il_offset`, `int line`). There is no source-count prefix,
    /// no source hash, no per-point source index, and no column/end fields.
    pub fn debug_info(&mut self, method: u32) -> Result<DebugInfo> {
        let payload = Encoder::new().id(method).finish();
        let data = self.request(cs::METHOD, wire::method::GET_DEBUG_INFO, &payload)?;
        let mut d = Decoder::new(&data);
        let max_il = d.int()?;
        let source = d.string()?;
        let nsp = d.uint()? as usize;
        let mut seq_points = Vec::with_capacity(nsp);
        for _ in 0..nsp {
            let il_offset = d.int()?;
            let line = d.int()?;
            seq_points.push(SeqPoint {
                il_offset,
                line,
                source_idx: 0,
                column: 0,
                end_line: line,
                end_column: 0,
            });
        }
        Ok(DebugInfo {
            max_il,
            sources: vec![source],
            seq_points,
        })
    }

    /// Local variable metadata. Like `debug_info`, Unity's agent uses the simple
    /// format (verified on mono 6.13.0 / 2.58): `int n`, `n` × type id, `n` × name
    /// string, then `n` × (`int live_start`, `int live_end`). No scope-table prefix.
    pub fn locals_info(&mut self, method: u32) -> Result<Vec<Local>> {
        let payload = Encoder::new().id(method).finish();
        let data = self.request(cs::METHOD, wire::method::GET_LOCALS_INFO, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        let types: Vec<u32> = (0..n).map(|_| d.id()).collect::<Result<_>>()?;
        let names: Vec<String> = (0..n).map(|_| d.string()).collect::<Result<_>>()?;
        let mut locals = Vec::with_capacity(n);
        for i in 0..n {
            let live_start = d.int()?;
            let live_end = d.int()?;
            locals.push(Local {
                type_id: types[i],
                name: names[i].clone(),
                live_start,
                live_end,
            });
        }
        Ok(locals)
    }

    // --- event requests ---

    /// Register an event request. Returns its request id (match it in events / pass
    /// it to `clear_event`).
    pub fn set_event(&mut self, kind: u8, suspend: u8, modifiers: &[Modifier]) -> Result<i32> {
        let minor = self.version().minor;
        let mut enc = Encoder::new();
        enc.byte(kind).byte(suspend).byte(modifiers.len() as u8);
        for m in modifiers {
            match m {
                Modifier::LocationOnly { method, il } => {
                    enc.byte(wire::modifier::LOCATION_ONLY).id(*method).long(*il);
                }
                Modifier::SourceFileOnly(files) => {
                    enc.byte(wire::modifier::SOURCE_FILE_ONLY)
                        .int(files.len() as i32);
                    for f in files {
                        enc.string(f);
                    }
                }
                Modifier::Step {
                    thread,
                    size,
                    depth,
                    filter,
                } => {
                    enc.byte(wire::modifier::STEP)
                        .id(*thread)
                        .int(*size)
                        .int(*depth);
                    if minor >= 16 {
                        enc.int(*filter);
                    }
                }
            }
        }
        let payload = enc.finish();
        let data = self.request(cs::EVENT_REQUEST, wire::evreq::SET, &payload)?;
        Decoder::new(&data).int()
    }

    pub fn clear_event(&mut self, kind: u8, request_id: i32) -> Result<()> {
        let payload = Encoder::new().byte(kind).int(request_id).finish();
        self.request(cs::EVENT_REQUEST, wire::evreq::CLEAR, &payload)
            .map(|_| ())
    }

    /// Subscribe to TYPE_LOAD events for types in the given source files. Suspends the
    /// VM on load (policy ALL) so the caller can arm a breakpoint on the freshly-loaded
    /// type BEFORE its methods run, then resume. Returns the request id.
    pub fn watch_source_files(&mut self, files: &[String]) -> Result<i32> {
        self.set_event(
            kind::TYPE_LOAD,
            wire::suspend::ALL,
            &[Modifier::SourceFileOnly(files.to_vec())],
        )
    }

    /// Set a breakpoint at a method + IL offset (suspend policy ALL). Returns its id.
    pub fn set_breakpoint(&mut self, method: u32, il: i64) -> Result<i32> {
        self.set_event(
            kind::BREAKPOINT,
            wire::suspend::ALL,
            &[Modifier::LocationOnly { method, il }],
        )
    }

    /// Arm a single step on `thread`. Returns the step request id (clear it after the
    /// STEP event fires, or it keeps firing).
    pub fn step(&mut self, thread: u32, size: i32, depth: i32) -> Result<i32> {
        self.set_event(
            kind::STEP,
            wire::suspend::ALL,
            &[Modifier::Step {
                thread,
                size,
                depth,
                filter: 0,
            }],
        )
    }

    // --- stop-state inspection ---

    /// A thread's name (empty for unnamed threads).
    pub fn thread_name(&mut self, thread: u32) -> Result<String> {
        let payload = Encoder::new().id(thread).finish();
        let data = self.request(cs::THREAD, wire::thread::GET_NAME, &payload)?;
        Decoder::new(&data).string()
    }

    pub fn frames(&mut self, thread: u32) -> Result<Vec<Frame>> {
        let payload = Encoder::new().id(thread).int(0).int(-1).finish();
        let data = self.request(cs::THREAD, wire::thread::GET_FRAME_INFO, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            let id = d.int()?;
            let method = d.id()?;
            let il_offset = d.int()?;
            let flags = d.byte()?;
            frames.push(Frame {
                id,
                method,
                il_offset,
                flags,
            });
        }
        Ok(frames)
    }

    pub fn frame_values(
        &mut self,
        thread: u32,
        frame: i32,
        positions: &[i32],
    ) -> Result<Vec<value::Value>> {
        let mut enc = Encoder::new();
        enc.id(thread).id(frame as u32).int(positions.len() as i32);
        for p in positions {
            enc.int(*p);
        }
        let payload = enc.finish();
        let data = self.request(cs::STACK_FRAME, wire::frame::GET_VALUES, &payload)?;
        let mut d = Decoder::new(&data);
        positions.iter().map(|_| value::decode_value(&mut d)).collect()
    }

    pub fn frame_this(&mut self, thread: u32, frame: i32) -> Result<value::Value> {
        let payload = Encoder::new().id(thread).id(frame as u32).finish();
        let data = self.request(cs::STACK_FRAME, wire::frame::GET_THIS, &payload)?;
        value::decode_value(&mut Decoder::new(&data))
    }

    // --- object / field inspection ---

    /// The runtime type id of an object.
    pub fn object_type(&mut self, obj: u32) -> Result<u32> {
        let payload = Encoder::new().id(obj).finish();
        let data = self.request(cs::OBJECT_REF, wire::object::GET_TYPE, &payload)?;
        Decoder::new(&data).id()
    }

    /// The base (parent) type id of a type, or 0 for `System.Object`. From
    /// `TYPE.GET_INFO`, whose stable prefix is: namespace, name, full-name (strings),
    /// assembly, module, then the parent type id.
    pub fn type_base(&mut self, type_id: u32) -> Result<u32> {
        let payload = Encoder::new().id(type_id).finish();
        let data = self.request(cs::TYPE, wire::ty::GET_INFO, &payload)?;
        let mut d = Decoder::new(&data);
        let _ns = d.string()?;
        let _name = d.string()?;
        let _full = d.string()?;
        let _assembly = d.id()?;
        let _module = d.id()?;
        d.id() // parent
    }

    /// A type's declared fields (id, name, type, attrs). Verified layout on Unity:
    /// `int count`, then per field `id`, `string name`, `id type`, `int attrs`.
    pub fn type_fields(&mut self, type_id: u32) -> Result<Vec<Field>> {
        let payload = Encoder::new().id(type_id).finish();
        let data = self.request(cs::TYPE, wire::ty::GET_FIELDS, &payload)?;
        let mut d = Decoder::new(&data);
        let n = d.uint()? as usize;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let id = d.id()?;
            let name = d.string()?;
            let type_id = d.id()?;
            let attrs = d.int()?;
            fields.push(Field {
                id,
                name,
                type_id,
                attrs,
            });
        }
        Ok(fields)
    }

    /// Read the given fields of an object (parallel to `field_ids`).
    pub fn object_values(&mut self, obj: u32, field_ids: &[u32]) -> Result<Vec<value::Value>> {
        let mut enc = Encoder::new();
        enc.id(obj).int(field_ids.len() as i32);
        for f in field_ids {
            enc.id(*f);
        }
        let payload = enc.finish();
        let data = self.request(cs::OBJECT_REF, wire::object::GET_VALUES, &payload)?;
        let mut d = Decoder::new(&data);
        field_ids.iter().map(|_| value::decode_value(&mut d)).collect()
    }

    /// The element count of an array (first dimension; SZARRAYs are one-dimensional).
    pub fn array_length(&mut self, arr: u32) -> Result<i32> {
        let payload = Encoder::new().id(arr).finish();
        let data = self.request(cs::ARRAY_REF, wire::array::GET_LENGTH, &payload)?;
        let mut d = Decoder::new(&data);
        let rank = d.uint()? as usize;
        let mut len = 0;
        for i in 0..rank {
            let dim = d.int()?;
            let _lower_bound = d.int()?;
            if i == 0 {
                len = dim;
            }
        }
        Ok(len)
    }

    /// Read `count` array elements starting at `index`.
    pub fn array_values(&mut self, arr: u32, index: i32, count: i32) -> Result<Vec<value::Value>> {
        let payload = Encoder::new().id(arr).int(index).int(count).finish();
        let data = self.request(cs::ARRAY_REF, wire::array::GET_VALUES, &payload)?;
        let mut d = Decoder::new(&data);
        (0..count).map(|_| value::decode_value(&mut d)).collect()
    }

    /// The contents of a managed string by its object id.
    pub fn string_value(&mut self, string_id: u32) -> Result<String> {
        let payload = Encoder::new().id(string_id).finish();
        let data = self.request(cs::STRING_REF, wire::string_ref::GET_VALUE, &payload)?;
        Decoder::new(&data).string()
    }

    /// A type's name (`Namespace.Name`, or just `Name`).
    pub fn type_name(&mut self, type_id: u32) -> Result<String> {
        let payload = Encoder::new().id(type_id).finish();
        let data = self.request(cs::TYPE, wire::ty::GET_INFO, &payload)?;
        let mut d = Decoder::new(&data);
        let ns = d.string()?;
        let name = d.string()?;
        Ok(if ns.is_empty() { name } else { format!("{ns}.{name}") })
    }
}

/// Pick the IL location a source breakpoint should bind to, across all candidate
/// methods for a file. Snaps forward to the nearest non-hidden sequence point at or
/// after `line`, preferring the lowest line then the lowest IL offset.
pub fn resolve_line(candidates: &[(u32, DebugInfo)], file: &str, line: i32) -> Option<(u32, i64)> {
    let want = basename(file).to_ascii_lowercase();
    let mut best: Option<(i32, i32, u32, i64)> = None; // (line, il, method, il64)
    for (method, info) in candidates {
        for sp in &info.seq_points {
            if sp.is_hidden() || sp.line < line {
                continue;
            }
            let src = info
                .sources
                .get(sp.source_idx as usize)
                .map(|s| basename(s).to_ascii_lowercase());
            if src.as_deref() != Some(want.as_str()) {
                continue;
            }
            let cand = (sp.line, sp.il_offset, *method, sp.il_offset as i64);
            if best.map_or(true, |b| (cand.0, cand.1) < (b.0, b.1)) {
                best = Some(cand);
            }
        }
    }
    best.map(|(_, _, m, il)| (m, il))
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A human-readable name for an event kind (for logging).
pub fn kind_name(k: u8) -> &'static str {
    match k {
        kind::VM_START => "VM_START",
        kind::VM_DEATH => "VM_DEATH",
        kind::THREAD_START => "THREAD_START",
        kind::THREAD_DEATH => "THREAD_DEATH",
        kind::APPDOMAIN_CREATE => "APPDOMAIN_CREATE",
        kind::APPDOMAIN_UNLOAD => "APPDOMAIN_UNLOAD",
        kind::METHOD_ENTRY => "METHOD_ENTRY",
        kind::METHOD_EXIT => "METHOD_EXIT",
        kind::ASSEMBLY_LOAD => "ASSEMBLY_LOAD",
        kind::ASSEMBLY_UNLOAD => "ASSEMBLY_UNLOAD",
        kind::BREAKPOINT => "BREAKPOINT",
        kind::STEP => "STEP",
        kind::TYPE_LOAD => "TYPE_LOAD",
        kind::EXCEPTION => "EXCEPTION",
        kind::USER_BREAK => "USER_BREAK",
        kind::USER_LOG => "USER_LOG",
        _ => "UNKNOWN",
    }
}

/// Decode an agent command packet into a composite event, or `None` if it is not a
/// composite event we model.
fn decode_command(cmd_set: u8, cmd: u8, data: &[u8]) -> Option<Composite> {
    if cmd_set != cs::EVENT || cmd != wire::event::COMPOSITE {
        return None;
    }
    decode_composite(data).ok()
}

fn decode_composite(data: &[u8]) -> Result<Composite> {
    let mut d = Decoder::new(data);
    let suspend_policy = d.byte()?;
    let n = d.uint()? as usize;
    let mut events = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = d.byte()?;
        let request_id = d.int()?;
        let mut ev = Event {
            kind,
            request_id,
            ..Default::default()
        };
        match kind {
            kind::VM_START | kind::THREAD_START | kind::THREAD_DEATH => {
                ev.thread = d.id()?;
            }
            kind::APPDOMAIN_CREATE | kind::APPDOMAIN_UNLOAD => {
                ev.thread = d.id()?;
                ev.domain = d.id()?;
            }
            kind::ASSEMBLY_LOAD | kind::ASSEMBLY_UNLOAD => {
                ev.thread = d.id()?;
                ev.assembly = d.id()?;
            }
            kind::TYPE_LOAD => {
                ev.thread = d.id()?;
                ev.type_id = d.id()?;
            }
            kind::BREAKPOINT | kind::STEP => {
                ev.thread = d.id()?;
                ev.method = d.id()?;
                ev.location = d.long()?;
            }
            kind::METHOD_ENTRY | kind::METHOD_EXIT => {
                ev.thread = d.id()?;
                ev.method = d.id()?;
            }
            kind::VM_DEATH => {
                // No reliably-present tail across versions; this is terminal.
            }
            other => {
                // Unknown kind: we cannot know its tail length to keep parsing a
                // multi-event composite safely, so stop after recording it.
                events.push(Event {
                    kind: other,
                    request_id,
                    ..Default::default()
                });
                break;
            }
        }
        events.push(ev);
    }
    Ok(Composite {
        suspend_policy,
        events,
    })
}

/// Walk up from `start` looking for `Library/EditorInstance.json`.
pub fn find_editor_instance(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("Library").join("EditorInstance.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// The Unity editor's SDB agent listens on `56000 + (pid % 1000)` on loopback.
pub fn editor_port(process_id: u32) -> u16 {
    (56000 + (process_id % 1000)) as u16
}

/// A Unity player's SDB agent port. Like the editor's, but keyed by the player's
/// multicast Guid instead of a pid (the pid isn't advertised). Verified on a live
/// 6000.x OSXPlayer: Guid 2625555618 → 56618.
pub fn player_sdb_port(guid: u32) -> u16 {
    (56000 + (guid % 1000)) as u16
}

/// Open an SDB connection to an agent at `ip:port` (editor on loopback, or a player).
pub fn connect(ip: &str, port: u16) -> Result<Connection> {
    let stream = TcpStream::connect((ip, port))?;
    stream.set_nodelay(true).ok();
    Connection::from_stream(stream)
}

/// Read `Library/EditorInstance.json` and connect to that editor's Mono agent.
pub fn connect_editor(project_root: &Path) -> Result<Connection> {
    let (ip, port) = editor_endpoint(project_root)?;
    connect(&ip, port)
}

/// The (loopback ip, port) of this project's editor agent, from EditorInstance.json.
pub fn editor_endpoint(project_root: &Path) -> Result<(String, u16)> {
    let path = find_editor_instance(project_root).ok_or_else(|| {
        SdbError::Protocol(format!(
            "no Library/EditorInstance.json under {}",
            project_root.display()
        ))
    })?;
    let text = std::fs::read_to_string(&path)?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| SdbError::Protocol(format!("parse {}: {e}", path.display())))?;
    let pid = json
        .get("process_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| SdbError::Protocol("EditorInstance.json missing process_id".into()))?
        as u32;
    Ok(("127.0.0.1".to_string(), editor_port(pid)))
}

// --- Unity player discovery (multicast) ---

/// The multicast group + port Unity players broadcast their presence on.
const DISCOVERY_GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(225, 0, 0, 222);
const DISCOVERY_PORT: u16 = 54997;

/// A debuggable Unity player found via multicast discovery.
#[derive(Debug, Clone)]
pub struct Player {
    pub guid: u32,
    pub ip: String,
    pub port: u16,
    pub project: String,
    /// The `[Id]` field, e.g. "OSXPlayer(1,MacBook-Air.local)".
    pub label: String,
}

/// Bind a UDP socket joined to Unity's player-discovery multicast group.
pub fn discovery_socket() -> std::io::Result<std::net::UdpSocket> {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    // Bind with SO_REUSEADDR so we can share the port with the editor's own listener.
    let sock = {
        let s = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        s.set_reuse_address(true)?;
        #[cfg(unix)]
        s.set_reuse_port(true)?;
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).into())?;
        let s: UdpSocket = s.into();
        s
    };
    // Join the group on EVERY local IPv4 interface. On macOS, joining on INADDR_ANY
    // alone binds only the default-route interface, so a player reachable over a
    // VM/secondary interface is never received; join each interface explicitly.
    let _ = sock.join_multicast_v4(&DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED);
    for ifip in local_ipv4_interfaces() {
        let _ = sock.join_multicast_v4(&DISCOVERY_GROUP, &ifip);
    }
    sock.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    Ok(sock)
}

/// Local IPv4 interface addresses (for per-interface multicast joins).
#[cfg(unix)]
fn local_ipv4_interfaces() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let addr = (*p).ifa_addr;
            if !addr.is_null() && (*addr).sa_family as i32 == libc::AF_INET {
                let sin = addr as *const libc::sockaddr_in;
                // s_addr is stored in network byte order, i.e. the [a,b,c,d] octets.
                out.push(std::net::Ipv4Addr::from((*sin).sin_addr.s_addr.to_ne_bytes()));
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

#[cfg(not(unix))]
fn local_ipv4_interfaces() -> Vec<std::net::Ipv4Addr> {
    Vec::new()
}

/// Parse a player announcement (`[IP] .. [Guid] .. [Debug] 1 ..`). Returns `None` for
/// non-debuggable players or malformed data.
pub fn parse_player(data: &[u8]) -> Option<Player> {
    let text = std::str::from_utf8(data).ok()?;
    let field = |name: &str| -> Option<String> {
        let tag = format!("[{name}] ");
        let start = text.find(&tag)? + tag.len();
        let rest = &text[start..];
        let end = rest.find('[').unwrap_or(rest.len());
        Some(rest[..end].trim_end_matches('\0').trim().to_string())
    };
    if field("Debug").as_deref() != Some("1") {
        return None; // script debugging not enabled → not attachable
    }
    let guid: u32 = field("Guid")?.parse().ok()?;
    let ip = field("IP")?;
    if ip.is_empty() {
        return None;
    }
    Some(Player {
        guid,
        port: player_sdb_port(guid),
        ip,
        project: field("ProjectName").unwrap_or_default(),
        label: field("Id").unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_port_formula() {
        assert_eq!(editor_port(46462), 56462);
        assert_eq!(editor_port(13136), 56136);
        assert_eq!(editor_port(56000), 56000);
    }

    #[test]
    fn decode_vm_start_composite() {
        // suspend_policy=0, n=1, kind=VM_START(0), request_id=0, thread=1
        let mut e = Encoder::new();
        e.byte(0).int(1).byte(kind::VM_START).int(0).id(1);
        let c = decode_composite(&e.finish()).unwrap();
        assert_eq!(c.suspend_policy, 0);
        assert_eq!(c.events.len(), 1);
        assert_eq!(c.events[0].kind, kind::VM_START);
        assert_eq!(c.events[0].thread, 1);
    }

    #[test]
    fn decode_breakpoint_composite() {
        // suspend_policy=2(ALL), n=1, kind=BREAKPOINT, req=7, thread=3, method=9, loc=42
        let mut e = Encoder::new();
        e.byte(2)
            .int(1)
            .byte(kind::BREAKPOINT)
            .int(7)
            .id(3)
            .id(9)
            .long(42);
        let c = decode_composite(&e.finish()).unwrap();
        let ev = &c.events[0];
        assert_eq!(ev.kind, kind::BREAKPOINT);
        assert_eq!(ev.request_id, 7);
        assert_eq!(ev.thread, 3);
        assert_eq!(ev.method, 9);
        assert_eq!(ev.location, 42);
    }

    #[test]
    fn decoder_rejects_short_read() {
        let mut d = Decoder::new(&[0, 0, 0]);
        assert!(d.int().is_err());
    }

    fn sp(il: i32, line: i32) -> SeqPoint {
        SeqPoint {
            il_offset: il,
            line,
            source_idx: 0,
            column: 0,
            end_line: line,
            end_column: 0,
        }
    }

    #[test]
    fn resolve_snaps_forward_past_blank_and_hidden() {
        // Rotator.cs Update(): line 35 decl, 36 `{` (no sp), 37 first statement.
        let info = DebugInfo {
            max_il: 100,
            sources: vec!["/proj/Assets/Rotator.cs".into()],
            seq_points: vec![
                sp(0, 35),
                sp(2, HIDDEN_LINE), // compiler-generated, must be skipped
                sp(6, 37),
                sp(20, 41),
            ],
        };
        let c = [(9u32, info)];
        // Asking for line 36 (the brace) snaps forward to 37 -> il 6.
        assert_eq!(resolve_line(&c, "Rotator.cs", 36), Some((9, 6)));
        // Exact line 37 resolves to its own sp.
        assert_eq!(resolve_line(&c, "Rotator.cs", 37), Some((9, 6)));
        // A wrong file name resolves to nothing.
        assert_eq!(resolve_line(&c, "Other.cs", 37), None);
    }

    #[test]
    fn resolve_prefers_lowest_line_then_il_across_methods() {
        let a = DebugInfo {
            max_il: 50,
            sources: vec!["Rotator.cs".into()],
            seq_points: vec![sp(10, 49), sp(4, 49)],
        };
        let b = DebugInfo {
            max_il: 50,
            sources: vec!["Rotator.cs".into()],
            seq_points: vec![sp(0, 51)],
        };
        let c = [(1u32, a), (2u32, b)];
        // line 48 -> nearest is 49, lowest il on that line is 4 in method 1.
        assert_eq!(resolve_line(&c, "Rotator.cs", 48), Some((1, 4)));
    }
}
