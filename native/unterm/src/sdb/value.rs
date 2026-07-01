//! Decoding of SDB tagged values (locals, `this`, fields).
//!
//! Mirrors mono's `buffer_add_value`: a leading element-type/value tag byte, then
//! the payload. Integer-family primitives (incl. bool/char) are written as 4-byte
//! ints; I8/U8 and native int/ptr as 8 bytes; R4/R8 as their bit patterns.

use super::wire::{Decoder, Result};

/// Element type / value tag bytes (`MonoTypeEnum` + SDB-specific value tags).
pub mod tag {
    pub const BOOLEAN: u8 = 0x02;
    pub const CHAR: u8 = 0x03;
    pub const I1: u8 = 0x04;
    pub const U1: u8 = 0x05;
    pub const I2: u8 = 0x06;
    pub const U2: u8 = 0x07;
    pub const I4: u8 = 0x08;
    pub const U4: u8 = 0x09;
    pub const I8: u8 = 0x0a;
    pub const U8: u8 = 0x0b;
    pub const R4: u8 = 0x0c;
    pub const R8: u8 = 0x0d;
    pub const STRING: u8 = 0x0e;
    pub const PTR: u8 = 0x0f;
    pub const VALUETYPE: u8 = 0x11;
    pub const CLASS: u8 = 0x12;
    pub const ARRAY: u8 = 0x14;
    pub const I: u8 = 0x18;
    pub const U: u8 = 0x19;
    pub const OBJECT: u8 = 0x1c;
    pub const SZARRAY: u8 = 0x1d;
    // SDB-specific value ids.
    pub const ID_NULL: u8 = 0xf0;
    pub const ID_TYPE: u8 = 0xf1;
    pub const ID_PARENT_VTYPE: u8 = 0xf2;
    pub const ID_FIXED_ARRAY: u8 = 0xf3;
}

/// A decoded value. Object-like values carry the opaque object id for further
/// inspection (string contents, fields, array elements) via the *_REF command sets.
#[derive(Debug, Clone)]
pub enum Value {
    Bool(bool),
    Char(u16),
    Int(i64),
    UInt(u64),
    F32(f32),
    F64(f64),
    /// A managed string object id (fetch contents via STRING_REF.GET_VALUE).
    String(u32),
    /// A reference object id (Class/Object/Array/SzArray). `tag` keeps the kind.
    Object { tag: u8, id: u32 },
    /// A boxed/inline value type with its fields.
    ValueType {
        klass: u32,
        is_enum: bool,
        fields: Vec<Value>,
    },
    /// A null reference.
    Null,
    /// A type token (System.Type).
    Type(u32),
    /// Tag we don't model; decoding cannot continue past this.
    Unknown(u8),
}

impl Value {
    /// A short, human-readable rendering (object ids shown as handles).
    pub fn summary(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Char(c) => format!("'{}'", char::from_u32(*c as u32).unwrap_or('?')),
            Value::Int(v) => v.to_string(),
            Value::UInt(v) => v.to_string(),
            Value::F32(v) => v.to_string(),
            Value::F64(v) => v.to_string(),
            Value::String(id) => format!("string#{id}"),
            Value::Object { tag, id } => {
                if *id == 0 {
                    "null".into()
                } else {
                    format!("obj#{id} (tag 0x{tag:02x})")
                }
            }
            Value::ValueType { klass, fields, .. } => {
                format!("struct klass#{klass} ({} fields)", fields.len())
            }
            Value::Null => "null".into(),
            Value::Type(id) => format!("Type#{id}"),
            Value::Unknown(t) => format!("<unknown tag 0x{t:02x}>"),
        }
    }
}

/// Decode one tagged value from the cursor.
pub fn decode_value(d: &mut Decoder) -> Result<Value> {
    let t = d.byte()?;
    Ok(match t {
        tag::BOOLEAN => Value::Bool(d.int()? != 0),
        tag::CHAR => Value::Char(d.int()? as u16),
        tag::I1 | tag::I2 | tag::I4 => Value::Int(d.int()? as i64),
        tag::U1 | tag::U2 | tag::U4 => Value::UInt(d.int()? as u32 as u64),
        tag::I8 => Value::Int(d.long()?),
        tag::U8 => Value::UInt(d.long()? as u64),
        tag::R4 => Value::F32(f32::from_bits(d.int()? as u32)),
        tag::R8 => Value::F64(f64::from_bits(d.long()? as u64)),
        tag::I | tag::U | tag::PTR => Value::Int(d.long()?),
        tag::STRING => Value::String(d.id()?),
        tag::CLASS | tag::OBJECT | tag::ARRAY | tag::SZARRAY => Value::Object {
            tag: t,
            id: d.id()?,
        },
        tag::VALUETYPE => {
            let is_enum = d.byte()? != 0;
            let klass = d.id()?;
            let n = d.uint()? as usize;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                fields.push(decode_value(d)?);
            }
            Value::ValueType {
                klass,
                is_enum,
                fields,
            }
        }
        tag::ID_NULL => {
            // tag, then the element type byte, then the klass id (modern mono).
            let _elem = d.byte()?;
            let _klass = d.id()?;
            Value::Null
        }
        tag::ID_TYPE => Value::Type(d.id()?),
        // Unknown tag: we can't know its payload size, so the caller must stop
        // decoding further values in this batch (the cursor is now unaligned).
        other => Value::Unknown(other),
    })
}
