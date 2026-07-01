//! Mono soft-debugger (SDB) wire protocol: framing, primitives, and constants.
//!
//! Verified against a live Unity 6000.3.18f1 editor (mono 6.13.0, protocol 2.58):
//! object/type/method/etc. ids are 4-byte big-endian ints; the 11-byte header is
//! big-endian; replies carry a 2-byte error code where a command carries cmdset/cmd.
//! See `~/.claude/plans/unterm-debugger-mono-soft-quokka.md` for the full map.

use std::io::{self, Read, Write};

/// The 13-byte literal both sides exchange right after the TCP connect.
pub const HANDSHAKE: &[u8] = b"DWP-Handshake";

/// Fixed header length for every command and reply packet.
pub const HEADER_LEN: usize = 11;

/// `flags` byte value identifying a reply (vs. a command/event from the agent).
pub const FLAG_REPLY: u8 = 0x80;

/// Command sets (`debugger-agent.c` `CommandSet`).
pub mod cs {
    pub const VM: u8 = 1;
    pub const OBJECT_REF: u8 = 9;
    pub const STRING_REF: u8 = 10;
    pub const THREAD: u8 = 11;
    pub const ARRAY_REF: u8 = 13;
    pub const EVENT_REQUEST: u8 = 15;
    pub const STACK_FRAME: u8 = 16;
    pub const APPDOMAIN: u8 = 20;
    pub const ASSEMBLY: u8 = 21;
    pub const METHOD: u8 = 22;
    pub const TYPE: u8 = 23;
    pub const MODULE: u8 = 24;
    pub const FIELD: u8 = 25;
    pub const EVENT: u8 = 64;
}

/// VM command set.
pub mod vm {
    pub const VERSION: u8 = 1;
    pub const ALL_THREADS: u8 = 2;
    pub const SUSPEND: u8 = 3;
    pub const RESUME: u8 = 4;
    pub const EXIT: u8 = 5;
    pub const DISPOSE: u8 = 6;
    pub const INVOKE_METHOD: u8 = 7;
    pub const SET_PROTOCOL_VERSION: u8 = 8;
    pub const GET_TYPES_FOR_SOURCE_FILE: u8 = 11;
    pub const GET_TYPES: u8 = 12;
}

/// AppDomain command set.
pub mod appdomain {
    pub const GET_ROOT_DOMAIN: u8 = 1;
    pub const GET_FRIENDLY_NAME: u8 = 2;
    pub const GET_ASSEMBLIES: u8 = 3;
    pub const GET_CORLIB: u8 = 6;
}

/// Assembly command set.
pub mod assembly {
    pub const GET_LOCATION: u8 = 1;
    pub const GET_ENTRY_POINT: u8 = 2;
    pub const GET_MANIFEST_MODULE: u8 = 3;
    pub const GET_OBJECT: u8 = 4;
    pub const GET_TYPE: u8 = 5;
    pub const GET_NAME: u8 = 6;
}

/// Type command set.
pub mod ty {
    pub const GET_INFO: u8 = 1;
    pub const GET_METHODS: u8 = 2;
    pub const GET_FIELDS: u8 = 3;
    pub const GET_SOURCE_FILES: u8 = 6;
    pub const GET_SOURCE_FILES_2: u8 = 13;
    pub const GET_METHODS_BY_NAME_FLAGS: u8 = 15;
}

/// ObjectRef command set.
pub mod object {
    pub const GET_TYPE: u8 = 1;
    pub const GET_VALUES: u8 = 2;
}

/// StringRef command set.
pub mod string_ref {
    pub const GET_VALUE: u8 = 1;
}

/// ArrayRef command set.
pub mod array {
    pub const GET_LENGTH: u8 = 1;
    pub const GET_VALUES: u8 = 2;
}

/// Method command set.
pub mod method {
    pub const GET_NAME: u8 = 1;
    pub const GET_DECLARING_TYPE: u8 = 2;
    pub const GET_DEBUG_INFO: u8 = 3;
    pub const GET_PARAM_INFO: u8 = 4;
    pub const GET_LOCALS_INFO: u8 = 5;
    pub const GET_INFO: u8 = 6;
}

/// Thread command set.
pub mod thread {
    pub const GET_FRAME_INFO: u8 = 1;
    pub const GET_NAME: u8 = 2;
    pub const GET_STATE: u8 = 3;
    pub const GET_INFO: u8 = 4;
    pub const GET_ID: u8 = 5;
}

/// StackFrame command set.
pub mod frame {
    pub const GET_VALUES: u8 = 1;
    pub const GET_THIS: u8 = 2;
    pub const SET_VALUES: u8 = 3;
    pub const GET_DOMAIN: u8 = 4;
}

/// EventRequest command set.
pub mod evreq {
    pub const SET: u8 = 1;
    pub const CLEAR: u8 = 2;
    pub const CLEAR_ALL_BREAKPOINTS: u8 = 3;
}

/// Event command set (agent -> client composite events).
pub mod event {
    pub const COMPOSITE: u8 = 100;
}

/// `EventKind` values (used in EVENT_REQUEST.SET and inside composite events).
pub mod kind {
    pub const VM_START: u8 = 0;
    pub const VM_DEATH: u8 = 1;
    pub const THREAD_START: u8 = 2;
    pub const THREAD_DEATH: u8 = 3;
    pub const APPDOMAIN_CREATE: u8 = 4;
    pub const APPDOMAIN_UNLOAD: u8 = 5;
    pub const METHOD_ENTRY: u8 = 6;
    pub const METHOD_EXIT: u8 = 7;
    pub const ASSEMBLY_LOAD: u8 = 8;
    pub const ASSEMBLY_UNLOAD: u8 = 9;
    pub const BREAKPOINT: u8 = 10;
    pub const STEP: u8 = 11;
    pub const TYPE_LOAD: u8 = 12;
    pub const EXCEPTION: u8 = 13;
    pub const USER_BREAK: u8 = 15;
    pub const USER_LOG: u8 = 16;
}

/// Suspend policies for an event request / a composite event.
pub mod suspend {
    pub const NONE: u8 = 0;
    pub const EVENT_THREAD: u8 = 1;
    pub const ALL: u8 = 2;
}

/// Modifier kinds for EVENT_REQUEST.SET.
pub mod modifier {
    pub const COUNT: u8 = 1;
    pub const THREAD_ONLY: u8 = 3;
    pub const LOCATION_ONLY: u8 = 7;
    pub const EXCEPTION_ONLY: u8 = 8;
    pub const STEP: u8 = 10;
    pub const ASSEMBLY_ONLY: u8 = 11;
    pub const SOURCE_FILE_ONLY: u8 = 12;
    pub const TYPE_NAME_ONLY: u8 = 13;
}

/// Step size / depth for a STEP modifier.
pub mod step {
    pub const SIZE_MIN: i32 = 0;
    pub const SIZE_LINE: i32 = 1;
    pub const DEPTH_INTO: i32 = 0;
    pub const DEPTH_OVER: i32 = 1;
    pub const DEPTH_OUT: i32 = 2;
}

/// Errors surfaced by the wire layer.
#[derive(Debug)]
pub enum SdbError {
    Io(io::Error),
    /// The agent returned a non-zero error code for a command.
    Agent(u16),
    /// A reply/packet was malformed or truncated.
    Protocol(String),
    /// The connection closed.
    Eof,
}

impl std::fmt::Display for SdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SdbError::Io(e) => write!(f, "io: {e}"),
            SdbError::Agent(c) => write!(f, "agent error {c}"),
            SdbError::Protocol(m) => write!(f, "protocol: {m}"),
            SdbError::Eof => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for SdbError {}

impl From<io::Error> for SdbError {
    fn from(e: io::Error) -> Self {
        SdbError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, SdbError>;

/// A packet read off the wire: either a reply to one of our commands, or an
/// unsolicited command from the agent (composite events arrive this way).
#[derive(Debug)]
pub enum Packet {
    Reply { id: u32, error: u16, data: Vec<u8> },
    Command { id: u32, cmd_set: u8, cmd: u8, data: Vec<u8> },
}

/// Big-endian payload builder. Ids are encoded as 4-byte ints (verified on Unity).
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn byte(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn int(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn long(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn id(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn string(&mut self, v: &str) -> &mut Self {
        let b = v.as_bytes();
        self.int(b.len() as i32);
        self.buf.extend_from_slice(b);
        self
    }

    /// Take the built payload, leaving the encoder empty. Takes `&mut self` so it
    /// can terminate a fluent chain over a temporary (`Encoder::new().id(x).finish()`).
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// Big-endian payload reader over a borrowed slice.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| overflow())?;
        if end > self.buf.len() {
            return Err(SdbError::Protocol(format!(
                "short read: need {n} at {} of {}",
                self.pos,
                self.buf.len()
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn int(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn uint(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn long(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn id(&mut self) -> Result<u32> {
        self.uint()
    }

    pub fn string(&mut self) -> Result<String> {
        let n = self.uint()? as usize;
        let b = self.take(n)?;
        Ok(String::from_utf8_lossy(b).into_owned())
    }

    /// Skip `n` bytes (e.g. a source-file hash we don't use).
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n)?;
        Ok(())
    }

    /// Bytes left unread.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

fn overflow() -> SdbError {
    SdbError::Protocol("length overflow".into())
}

/// Send a command packet. Returns the number of bytes written for the header+body.
pub fn write_command<W: Write>(
    w: &mut W,
    id: u32,
    cmd_set: u8,
    cmd: u8,
    payload: &[u8],
) -> Result<()> {
    let len = (HEADER_LEN + payload.len()) as u32;
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0..4].copy_from_slice(&len.to_be_bytes());
    hdr[4..8].copy_from_slice(&id.to_be_bytes());
    hdr[8] = 0; // flags: command from client
    hdr[9] = cmd_set;
    hdr[10] = cmd;
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read exactly one packet (blocking).
pub fn read_packet<R: Read>(r: &mut R) -> Result<Packet> {
    let mut hdr = [0u8; HEADER_LEN];
    read_exact(r, &mut hdr)?;
    let len = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as usize;
    let id = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
    let flags = hdr[8];
    if len < HEADER_LEN {
        return Err(SdbError::Protocol(format!("bad packet length {len}")));
    }
    let mut data = vec![0u8; len - HEADER_LEN];
    read_exact(r, &mut data)?;
    if flags == FLAG_REPLY {
        let error = u16::from_be_bytes(hdr[9..11].try_into().unwrap());
        Ok(Packet::Reply { id, error, data })
    } else {
        Ok(Packet::Command {
            id,
            cmd_set: hdr[9],
            cmd: hdr[10],
            data,
        })
    }
}

/// Like `Read::read_exact` but maps a clean EOF (0 bytes at a boundary) to `Eof`.
fn read_exact<R: Read>(r: &mut R, mut buf: &mut [u8]) -> Result<()> {
    let want = buf.len();
    let mut got = 0;
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => {
                return if got == 0 {
                    Err(SdbError::Eof)
                } else {
                    Err(SdbError::Protocol(format!(
                        "truncated packet: {got}/{want} bytes"
                    )))
                };
            }
            Ok(n) => {
                got += n;
                buf = &mut buf[n..];
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(SdbError::Io(e)),
        }
    }
    Ok(())
}
