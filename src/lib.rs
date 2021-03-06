#![allow(incomplete_features)]

//! # binserde
//!
//! A crate similar to serde, but specialized for serializing into a compact
//! binary format, including features like string deduplication.
//!
//! *This crate is very WIP.* Features currently not implemented but planned
//! include incremental versioning support so that old formats can still be
//! loaded when the data format changes, deduplication of arbitrary data
//! structures, and explicit tagging (writing a struct or enum as a set of
//! key/value pairs instead of serializing the items in order of declaration,
//! for higher resistance to format changes at the expense of output size)
//!
//! ## Usage
//!
//! ```
//! use std::fs::File;
//! use std::io::BufReader;
//! use binserde_derive::{BinDeserialize, BinSerialize};
//!
//! #[derive(Debug, BinSerialize, BinDeserialize, Eq, PartialEq)]
//! struct MyData {
//!     v1: String,
//!     v2: Option<usize>,
//! }
//!
//! let my_data = MyData {
//!     v1: "Some Text".to_string(),
//!     v2: Some(12415165),
//! };
//!
//! let vec = binserde::serialize(&my_data).unwrap();
//!
//! let copy_of_my_data: MyData = binserde::deserialize(&vec).unwrap();
//!
//! assert_eq!(my_data, copy_of_my_data);
//! ```
//!
//! ## Macro Attributes
//!
//! `#[derive(BinSerialize)]` and `#[derive(BinDeserialize)]` allows using
//! attributes on the type itself and its fields to control (de)serialization.
//!
//! ### `#[binserde(skip)]`
//!
//! Valid for: fields
//!
//! Skips the field when serializing. When deserializing, uses
//! [`Default::default()`] instead of reading from the stream to fill the field.
//!
//! ### `#[binserde(no_dedup)]`
//!
//! Valid for: fields
//!
//! Turns off deduplication for this field. See [Deduplication] for more
//! information about how it works.
//!
//! ### `#[binserde(index = n)]`
//!
//! Valid for: fields
//!
//! **not implemented**
//!
//! Moves the field and all following fields to the specified position `n` when
//! serializing, shifting everything originally after that position to the
//! right.
//!
//! #### Example:
//!
//! ```
//! use binserde_derive::BinSerialize;
//!
//! #[derive(BinSerialize)]
//! struct S {
//!     w: u8,
//!     x: u8,
//!     #[binserde(index = 0)]
//!     y: u8,
//!     z: u8,
//! }
//!
//! let vec = binserde::serialize(&S { w: 0, x: 1, y: 2, z: 3 }).unwrap();
//!
//! assert_eq!(&[2, 3, 0, 1], &*vec);
//! ```
//!
//! The attribute moved `y` and `z` into position 0, pushing `w` and `x` back to
//! positions 2 and 3 respectively.
//!
//! The attribute can be applied on more than one field, in which case moving
//! operations will be evaluated from top to bottom. That means, the following
//! struct serializes in the order z, x, y, w and not x, y, z, w or any other
//! order:
//!
//! ```
//! use binserde_derive::BinSerialize;
//!
//! #[derive(BinSerialize)]
//! struct S {
//!     w: u8,
//!     #[binserde(index = 0)]
//!     x: u8,
//!     y: u8,
//!     #[binserde(index = 0)]
//!     z: u8,
//! }
//! ```
//!
//! # Deduplication
//!
//! Deduplication is currently only implemented for strings. It works by taking
//! any [`String`] or [`str`] that is serialized using its [`BinSerializer`]
//! implementation and adds it to a seperate list which is written to the
//! beginning of the buffer given to [`serialize`] (or an equivalent function,
//! after which the actual data follows. In that data, the string is replaced by
//! a `usize` pointing to the index in the string list. Effectively, a
//! deduplicated data structure gets transformed from this:
//!
//! ```
//! struct S {
//!     s1: String,
//!     s2: String,
//!     strs: Vec<String>,
//!     something_else: u32,
//! }
//! ```
//!
//! to this:
//!
//! ```
//! struct S1 {
//!     strings: Vec<String>,
//!     s1: usize,
//!     s2: usize,
//!     strs: Vec<usize>,
//!     something_else: u32,
//! }
//! ```
//!
//! when serializing. This can have a major impact on the resulting size of the
//! serialized data structure when multiple occurrences of the same string
//! appear.
//!

extern crate self as binserde;

use std::fmt::Display;
use std::io;
use std::io::{Cursor, Read, Write};
use std::num::TryFromIntError;
use std::string::FromUtf8Error;

pub use binserde_derive::{BinDeserialize, BinSerialize};
use thiserror::Error;

use de::BinDeserializeOwned;
pub use de::{BinDeserialize, BinDeserializer};
use dedup::DedupContext;
pub use ser::{BinSerialize, BinSerializer};
pub use serde::Mode;

use crate::de::BinDeserializerBase;
use crate::ser::{BinSerializerBase, PrescanSerializer};

pub mod de;
pub mod dedup;
pub mod ser;
pub mod serde;
mod serdeimpl;
pub mod try_iter;
pub mod util;
mod varint;
mod write_ext;

pub fn serialize<T>(value: &T) -> Result<Vec<u8>>
where
    T: BinSerialize + ?Sized,
{
    serialize_with(value, Mode::default())
}

pub fn serialize_into<W, T>(pipe: W, value: &T) -> Result<()>
where
    W: Write,
    T: BinSerialize + ?Sized,
{
    serialize_with_into(pipe, value, Mode::default())
}

pub fn serialize_with<T>(value: &T, mode: Mode) -> Result<Vec<u8>>
where
    T: BinSerialize + ?Sized,
{
    let mut buf = Cursor::new(Vec::new());
    serialize_with_into(&mut buf, value, mode)?;
    Ok(buf.into_inner())
}

pub fn serialize_with_into<W, T>(mut pipe: W, value: &T, mode: Mode) -> Result<()>
where
    W: Write,
    T: BinSerialize + ?Sized,
{
    if mode.use_dedup {
        let mut ps = PrescanSerializer::new().with_mode(mode);
        value.serialize(&mut ps)?;
        ps.dedup().write_to(&mut pipe)?;
    }
    let mut serializer = BinSerializerBase::new(pipe).with_mode(mode);
    value.serialize(&mut serializer)?;
    Ok(())
}

pub fn deserialize<T>(buf: &[u8]) -> Result<T>
where
    T: BinDeserializeOwned,
{
    deserialize_with(buf, Mode::default())
}

pub fn deserialize_with<T>(buf: &[u8], mode: Mode) -> Result<T>
where
    T: BinDeserializeOwned,
{
    deserialize_with_from(Cursor::new(buf), mode)
}

pub fn deserialize_from<R, T>(pipe: R) -> Result<T>
where
    R: Read,
    T: BinDeserializeOwned,
{
    deserialize_with_from(pipe, Mode::default())
}

pub fn deserialize_with_from<R, T>(mut pipe: R, mode: Mode) -> Result<T>
where
    R: Read,
    T: BinDeserializeOwned,
{
    let context = if mode.use_dedup {
        DedupContext::read_from(&mut pipe)?
    } else {
        DedupContext::new()
    };
    let deserializer = BinDeserializerBase::new(pipe, &context).with_mode(mode);
    T::deserialize(deserializer)
}

pub fn deserialize_in_place<R, T>(target: &mut T, mut pipe: R, mode: Mode) -> Result<()>
where
    R: Read,
    T: BinDeserializeOwned,
{
    let context = if mode.use_dedup {
        DedupContext::read_from(&mut pipe)?
    } else {
        DedupContext::new()
    };
    let deserializer = BinDeserializerBase::new(pipe, &context).with_mode(mode);
    target.deserialize_in_place(deserializer)
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("string too long")]
    TryFromInt(#[from] TryFromIntError),
    #[error("invalid UTF-8 string")]
    InvalidUtf8(#[from] FromUtf8Error),
    #[error("indexed string out of range: {0}")]
    StrOutOfRange(usize),
    #[error("{0}")]
    Custom(String),
}

impl Error {
    pub fn custom<S: Display>(s: S) -> Self {
        Error::Custom(s.to_string())
    }
}

#[cfg(test)]
mod test {
    use binserde_derive::{BinDeserialize, BinSerialize};

    use crate::{deserialize, deserialize_with, serialize, serialize_with, Mode};

    #[test]
    fn serialize_inline_test() {
        use std::collections::{HashMap, HashSet};

        #[derive(Debug, PartialEq, Eq, BinSerialize, BinDeserialize)]
        struct Test {
            vec: Vec<Test1>,
            map_set: HashMap<String, HashSet<String>>,
            test2: Vec<Test2>,
        }

        #[derive(Debug, PartialEq, Eq, BinSerialize, BinDeserialize)]
        struct Test1(String, i32);

        #[derive(Debug, PartialEq, Eq, BinSerialize, BinDeserialize)]
        enum Test2 {
            A,
            B(i32, i32, i32),
            C { thing: i64 },
        }

        let s = Test {
            vec: vec![
                Test1("yyyyyyyyyyyyyyyyyy".to_string(), 4),
                Test1("a".to_string(), 4),
                Test1("yyyyyyyyyyyyyyyyyy".to_string(), 4),
                Test1("ab".to_string(), 4),
                Test1("abc".to_string(), 4),
                Test1("abcd".to_string(), 4),
            ],
            map_set: vec![("a", vec!["a", "b", "c"]), ("a1", vec!["a1", "b1", "c1"])]
                .into_iter()
                .map(|el| {
                    (
                        el.0.to_string(),
                        el.1.into_iter().map(|el| el.to_string()).collect(),
                    )
                })
                .collect(),
            test2: vec![
                Test2::A,
                Test2::B(1, 1992323, 5),
                Test2::C {
                    thing: 23456788765432,
                },
            ],
        };

        {
            let mode = Mode::dedup();

            let buf = serialize_with(&s, mode).expect("failed to serialize");
            println!("{:02X?}", buf);

            let s1: Test = deserialize_with(&buf, mode).expect("failed to deserialize");

            assert_eq!(s, s1);
        }

        {
            let buf = serialize(&s).expect("failed to serialize");
            println!("{:02X?}", buf);

            let s1: Test = deserialize(&buf).expect("failed to deserialize");

            assert_eq!(s, s1);
        }
    }

    #[test]
    fn serialize_constant_output() {
        assert_eq!(&[3, 97, 98, 99], &*serialize(&"abc").unwrap());

        assert_eq!(&[0xFF], &*serialize(&true).unwrap());
        assert_eq!(&[0x00], &*serialize(&false).unwrap());

        assert_eq!(
            &[0x03, 0x02, 0x05, 0x45],
            &*serialize_with(
                &[1i16, -3i16, -35i16] as &[i16],
                Mode::default().with_fixed_size_use_varint(true)
            )
            .unwrap()
        );
    }
}
