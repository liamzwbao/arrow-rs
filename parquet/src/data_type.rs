// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Data types that connect Parquet physical types with their Rust-specific
//! representations.
use bytes::Bytes;
use half::f16;
use std::cmp::Ordering;
use std::fmt;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::str::from_utf8;

use crate::basic::Type;
use crate::column::reader::{ColumnReader, ColumnReaderImpl};
use crate::column::writer::{ColumnWriter, ColumnWriterImpl};
use crate::errors::{ParquetError, Result};
use crate::util::bit_util::FromBytes;

/// Rust representation for logical type INT96, value is backed by an array of `u32`.
/// The type only takes 12 bytes, without extra padding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Int96 {
    value: [u32; 3],
}

const JULIAN_DAY_OF_EPOCH: i64 = 2_440_588;

/// Number of seconds in a day
const SECONDS_IN_DAY: i64 = 86_400;
/// Number of milliseconds in a second
const MILLISECONDS: i64 = 1_000;
/// Number of microseconds in a second
const MICROSECONDS: i64 = 1_000_000;
/// Number of nanoseconds in a second
const NANOSECONDS: i64 = 1_000_000_000;

/// Number of milliseconds in a day
const MILLISECONDS_IN_DAY: i64 = SECONDS_IN_DAY * MILLISECONDS;
/// Number of microseconds in a day
const MICROSECONDS_IN_DAY: i64 = SECONDS_IN_DAY * MICROSECONDS;
/// Number of nanoseconds in a day
const NANOSECONDS_IN_DAY: i64 = SECONDS_IN_DAY * NANOSECONDS;

impl Int96 {
    /// Creates new INT96 type struct with no data set.
    pub fn new() -> Self {
        Self { value: [0; 3] }
    }

    /// Returns underlying data as slice of [`u32`].
    #[inline]
    pub fn data(&self) -> &[u32] {
        &self.value
    }

    /// Sets data for this INT96 type.
    #[inline]
    pub fn set_data(&mut self, elem0: u32, elem1: u32, elem2: u32) {
        self.value = [elem0, elem1, elem2];
    }

    /// Converts this INT96 into an i64 representing the number of SECONDS since EPOCH
    ///
    /// Will wrap around on overflow
    #[inline]
    pub fn to_seconds(&self) -> i64 {
        let (day, nanos) = self.data_as_days_and_nanos();
        (day as i64 - JULIAN_DAY_OF_EPOCH)
            .wrapping_mul(SECONDS_IN_DAY)
            .wrapping_add(nanos / 1_000_000_000)
    }

    /// Converts this INT96 into an i64 representing the number of MILLISECONDS since EPOCH
    ///
    /// Will wrap around on overflow
    #[inline]
    pub fn to_millis(&self) -> i64 {
        let (day, nanos) = self.data_as_days_and_nanos();
        (day as i64 - JULIAN_DAY_OF_EPOCH)
            .wrapping_mul(MILLISECONDS_IN_DAY)
            .wrapping_add(nanos / 1_000_000)
    }

    /// Converts this INT96 into an i64 representing the number of MICROSECONDS since EPOCH
    ///
    /// Will wrap around on overflow
    #[inline]
    pub fn to_micros(&self) -> i64 {
        let (day, nanos) = self.data_as_days_and_nanos();
        (day as i64 - JULIAN_DAY_OF_EPOCH)
            .wrapping_mul(MICROSECONDS_IN_DAY)
            .wrapping_add(nanos / 1_000)
    }

    /// Converts this INT96 into an i64 representing the number of NANOSECONDS since EPOCH
    ///
    /// Will wrap around on overflow
    #[inline]
    pub fn to_nanos(&self) -> i64 {
        let (day, nanos) = self.data_as_days_and_nanos();
        (day as i64 - JULIAN_DAY_OF_EPOCH)
            .wrapping_mul(NANOSECONDS_IN_DAY)
            .wrapping_add(nanos)
    }

    #[inline]
    fn get_days(&self) -> i32 {
        self.data()[2] as i32
    }

    #[inline]
    fn get_nanos(&self) -> i64 {
        ((self.data()[1] as i64) << 32) + self.data()[0] as i64
    }

    #[inline]
    fn data_as_days_and_nanos(&self) -> (i32, i64) {
        (self.get_days(), self.get_nanos())
    }
}

impl PartialOrd for Int96 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Int96 {
    /// Order `Int96` correctly for (deprecated) timestamp types.
    ///
    /// Note: this is done even though the Int96 type is deprecated and the
    /// [spec does not define the sort order]
    /// because some engines, notably Spark and Databricks Photon still write
    /// Int96 timestamps and rely on their order for optimization.
    ///
    /// [spec does not define the sort order]: https://github.com/apache/parquet-format/blob/cf943c197f4fad826b14ba0c40eb0ffdab585285/src/main/thrift/parquet.thrift#L1079
    fn cmp(&self, other: &Self) -> Ordering {
        match self.get_days().cmp(&other.get_days()) {
            Ordering::Equal => self.get_nanos().cmp(&other.get_nanos()),
            ord => ord,
        }
    }
}
impl From<Vec<u32>> for Int96 {
    fn from(buf: Vec<u32>) -> Self {
        assert_eq!(buf.len(), 3);
        let mut result = Self::new();
        result.set_data(buf[0], buf[1], buf[2]);
        result
    }
}

impl fmt::Display for Int96 {
    #[cold]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.data())
    }
}

/// Rust representation for BYTE_ARRAY and FIXED_LEN_BYTE_ARRAY Parquet physical types.
/// Value is backed by a byte buffer.
#[derive(Clone, Default)]
pub struct ByteArray {
    data: Option<Bytes>,
}

// Special case Debug that prints out byte arrays that are valid utf8 as &str's
impl std::fmt::Debug for ByteArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug_struct = f.debug_struct("ByteArray");
        match self.as_utf8() {
            Ok(s) => debug_struct.field("data", &s),
            Err(_) => debug_struct.field("data", &self.data),
        };
        debug_struct.finish()
    }
}

impl PartialOrd for ByteArray {
    fn partial_cmp(&self, other: &ByteArray) -> Option<Ordering> {
        // sort nulls first (consistent with PartialCmp on Option)
        //
        // Since ByteBuffer doesn't implement PartialOrd, so can't
        // derive an implementation
        match (&self.data, &other.data) {
            (None, None) => Some(Ordering::Equal),
            (None, Some(_)) => Some(Ordering::Less),
            (Some(_), None) => Some(Ordering::Greater),
            (Some(self_data), Some(other_data)) => {
                // compare slices directly
                self_data.partial_cmp(&other_data)
            }
        }
    }
}

impl ByteArray {
    /// Creates new byte array with no data set.
    #[inline]
    pub fn new() -> Self {
        ByteArray { data: None }
    }

    /// Gets length of the underlying byte buffer.
    #[inline]
    pub fn len(&self) -> usize {
        assert!(self.data.is_some());
        self.data.as_ref().unwrap().len()
    }

    /// Checks if the underlying buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns slice of data.
    #[inline]
    pub fn data(&self) -> &[u8] {
        self.data
            .as_ref()
            .expect("set_data should have been called")
            .as_ref()
    }

    /// Set data from another byte buffer.
    #[inline]
    pub fn set_data(&mut self, data: Bytes) {
        self.data = Some(data);
    }

    /// Returns `ByteArray` instance with slice of values for a data.
    #[inline]
    pub fn slice(&self, start: usize, len: usize) -> Self {
        Self::from(
            self.data
                .as_ref()
                .expect("set_data should have been called")
                .slice(start..start + len),
        )
    }

    /// Try to convert the byte array to a utf8 slice
    pub fn as_utf8(&self) -> Result<&str> {
        self.data
            .as_ref()
            .map(|ptr| ptr.as_ref())
            .ok_or_else(|| general_err!("Can't convert empty byte array to utf8"))
            .and_then(|bytes| from_utf8(bytes).map_err(|e| e.into()))
    }
}

impl From<Vec<u8>> for ByteArray {
    fn from(buf: Vec<u8>) -> ByteArray {
        Self {
            data: Some(buf.into()),
        }
    }
}

impl<'a> From<&'a [u8]> for ByteArray {
    fn from(b: &'a [u8]) -> ByteArray {
        let mut v = Vec::new();
        v.extend_from_slice(b);
        Self {
            data: Some(v.into()),
        }
    }
}

impl<'a> From<&'a str> for ByteArray {
    fn from(s: &'a str) -> ByteArray {
        let mut v = Vec::new();
        v.extend_from_slice(s.as_bytes());
        Self {
            data: Some(v.into()),
        }
    }
}

impl From<Bytes> for ByteArray {
    fn from(value: Bytes) -> Self {
        Self { data: Some(value) }
    }
}

impl From<f16> for ByteArray {
    fn from(value: f16) -> Self {
        Self::from(value.to_le_bytes().as_slice())
    }
}

impl PartialEq for ByteArray {
    fn eq(&self, other: &ByteArray) -> bool {
        match (&self.data, &other.data) {
            (Some(d1), Some(d2)) => d1.as_ref() == d2.as_ref(),
            (None, None) => true,
            _ => false,
        }
    }
}

impl fmt::Display for ByteArray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.data())
    }
}

/// Wrapper type for performance reasons, this represents `FIXED_LEN_BYTE_ARRAY` but in all other
/// considerations behaves the same as `ByteArray`
///
/// # Performance notes:
/// This type is a little unfortunate, without it the compiler generates code that takes quite a
/// big hit on the CPU pipeline. Essentially the previous version stalls awaiting the result of
/// `T::get_physical_type() == Type::FIXED_LEN_BYTE_ARRAY`.
///
/// Its debatable if this is wanted, it is out of spec for what parquet documents as its base
/// types, although there are code paths in the Rust (and potentially the C++) versions that
/// warrant this.
///
/// With this wrapper type the compiler generates more targeted code paths matching the higher
/// level logical types, removing the data-hazard from all decoding and encoding paths.
#[repr(transparent)]
#[derive(Clone, Debug, Default)]
pub struct FixedLenByteArray(ByteArray);

impl PartialEq for FixedLenByteArray {
    fn eq(&self, other: &FixedLenByteArray) -> bool {
        self.0.eq(&other.0)
    }
}

impl PartialEq<ByteArray> for FixedLenByteArray {
    fn eq(&self, other: &ByteArray) -> bool {
        self.0.eq(other)
    }
}

impl PartialEq<FixedLenByteArray> for ByteArray {
    fn eq(&self, other: &FixedLenByteArray) -> bool {
        self.eq(&other.0)
    }
}

impl fmt::Display for FixedLenByteArray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl PartialOrd for FixedLenByteArray {
    fn partial_cmp(&self, other: &FixedLenByteArray) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl PartialOrd<FixedLenByteArray> for ByteArray {
    fn partial_cmp(&self, other: &FixedLenByteArray) -> Option<Ordering> {
        self.partial_cmp(&other.0)
    }
}

impl PartialOrd<ByteArray> for FixedLenByteArray {
    fn partial_cmp(&self, other: &ByteArray) -> Option<Ordering> {
        self.0.partial_cmp(other)
    }
}

impl Deref for FixedLenByteArray {
    type Target = ByteArray;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FixedLenByteArray {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<ByteArray> for FixedLenByteArray {
    fn from(other: ByteArray) -> Self {
        Self(other)
    }
}

impl From<Vec<u8>> for FixedLenByteArray {
    fn from(buf: Vec<u8>) -> FixedLenByteArray {
        FixedLenByteArray(ByteArray::from(buf))
    }
}

impl From<FixedLenByteArray> for ByteArray {
    fn from(other: FixedLenByteArray) -> Self {
        other.0
    }
}

/// Rust representation for Decimal values.
///
/// This is not a representation of Parquet physical type, but rather a wrapper for
/// DECIMAL logical type, and serves as container for raw parts of decimal values:
/// unscaled value in bytes, precision and scale.
#[derive(Clone, Debug)]
pub enum Decimal {
    /// Decimal backed by `i32`.
    Int32 {
        /// The underlying value
        value: [u8; 4],
        /// The total number of digits in the number
        precision: i32,
        /// The number of digits to the right of the decimal point
        scale: i32,
    },
    /// Decimal backed by `i64`.
    Int64 {
        /// The underlying value
        value: [u8; 8],
        /// The total number of digits in the number
        precision: i32,
        /// The number of digits to the right of the decimal point
        scale: i32,
    },
    /// Decimal backed by byte array.
    Bytes {
        /// The underlying value
        value: ByteArray,
        /// The total number of digits in the number
        precision: i32,
        /// The number of digits to the right of the decimal point
        scale: i32,
    },
}

impl Decimal {
    /// Creates new decimal value from `i32`.
    pub fn from_i32(value: i32, precision: i32, scale: i32) -> Self {
        let bytes = value.to_be_bytes();
        Decimal::Int32 {
            value: bytes,
            precision,
            scale,
        }
    }

    /// Creates new decimal value from `i64`.
    pub fn from_i64(value: i64, precision: i32, scale: i32) -> Self {
        let bytes = value.to_be_bytes();
        Decimal::Int64 {
            value: bytes,
            precision,
            scale,
        }
    }

    /// Creates new decimal value from `ByteArray`.
    pub fn from_bytes(value: ByteArray, precision: i32, scale: i32) -> Self {
        Decimal::Bytes {
            value,
            precision,
            scale,
        }
    }

    /// Returns bytes of unscaled value.
    pub fn data(&self) -> &[u8] {
        match *self {
            Decimal::Int32 { ref value, .. } => value,
            Decimal::Int64 { ref value, .. } => value,
            Decimal::Bytes { ref value, .. } => value.data(),
        }
    }

    /// Returns decimal precision.
    pub fn precision(&self) -> i32 {
        match *self {
            Decimal::Int32 { precision, .. } => precision,
            Decimal::Int64 { precision, .. } => precision,
            Decimal::Bytes { precision, .. } => precision,
        }
    }

    /// Returns decimal scale.
    pub fn scale(&self) -> i32 {
        match *self {
            Decimal::Int32 { scale, .. } => scale,
            Decimal::Int64 { scale, .. } => scale,
            Decimal::Bytes { scale, .. } => scale,
        }
    }
}

impl Default for Decimal {
    fn default() -> Self {
        Self::from_i32(0, 0, 0)
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Decimal) -> bool {
        self.precision() == other.precision()
            && self.scale() == other.scale()
            && self.data() == other.data()
    }
}

/// Converts an instance of data type to a slice of bytes as `u8`.
pub trait AsBytes {
    /// Returns slice of bytes for this data type.
    fn as_bytes(&self) -> &[u8];
}

/// Converts an slice of a data type to a slice of bytes.
pub trait SliceAsBytes: Sized {
    /// Returns slice of bytes for a slice of this data type.
    fn slice_as_bytes(self_: &[Self]) -> &[u8];
    /// Return the internal representation as a mutable slice
    ///
    /// # Safety
    /// If modified you are _required_ to ensure the internal representation
    /// is valid and correct for the actual raw data
    unsafe fn slice_as_bytes_mut(self_: &mut [Self]) -> &mut [u8];
}

impl AsBytes for [u8] {
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

macro_rules! gen_as_bytes {
    ($source_ty:ident) => {
        impl AsBytes for $source_ty {
            #[allow(clippy::size_of_in_element_count)]
            fn as_bytes(&self) -> &[u8] {
                // SAFETY: macro is only used with primitive types that have no padding, so the
                // resulting slice always refers to initialized memory.
                unsafe {
                    std::slice::from_raw_parts(
                        self as *const $source_ty as *const u8,
                        std::mem::size_of::<$source_ty>(),
                    )
                }
            }
        }

        impl SliceAsBytes for $source_ty {
            #[inline]
            #[allow(clippy::size_of_in_element_count)]
            fn slice_as_bytes(self_: &[Self]) -> &[u8] {
                // SAFETY: macro is only used with primitive types that have no padding, so the
                // resulting slice always refers to initialized memory.
                unsafe {
                    std::slice::from_raw_parts(
                        self_.as_ptr() as *const u8,
                        std::mem::size_of_val(self_),
                    )
                }
            }

            #[inline]
            #[allow(clippy::size_of_in_element_count)]
            unsafe fn slice_as_bytes_mut(self_: &mut [Self]) -> &mut [u8] {
                // SAFETY: macro is only used with primitive types that have no padding, so the
                // resulting slice always refers to initialized memory. Moreover, self has no
                // invalid bit patterns, so all writes to the resulting slice will be valid.
                unsafe {
                    std::slice::from_raw_parts_mut(
                        self_.as_mut_ptr() as *mut u8,
                        std::mem::size_of_val(self_),
                    )
                }
            }
        }
    };
}

gen_as_bytes!(i8);
gen_as_bytes!(i16);
gen_as_bytes!(i32);
gen_as_bytes!(i64);
gen_as_bytes!(u8);
gen_as_bytes!(u16);
gen_as_bytes!(u32);
gen_as_bytes!(u64);
gen_as_bytes!(f32);
gen_as_bytes!(f64);

macro_rules! unimplemented_slice_as_bytes {
    ($ty: ty) => {
        impl SliceAsBytes for $ty {
            fn slice_as_bytes(_self: &[Self]) -> &[u8] {
                unimplemented!()
            }

            unsafe fn slice_as_bytes_mut(_self: &mut [Self]) -> &mut [u8] {
                unimplemented!()
            }
        }
    };
}

// TODO - Can Int96 and bool be implemented in these terms?
unimplemented_slice_as_bytes!(Int96);
unimplemented_slice_as_bytes!(bool);
unimplemented_slice_as_bytes!(ByteArray);
unimplemented_slice_as_bytes!(FixedLenByteArray);

impl AsBytes for bool {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: a bool is guaranteed to be either 0x00 or 0x01 in memory, so the memory is
        // valid.
        unsafe { std::slice::from_raw_parts(self as *const bool as *const u8, 1) }
    }
}

impl AsBytes for Int96 {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: Int96::data is a &[u32; 3].
        unsafe { std::slice::from_raw_parts(self.data() as *const [u32] as *const u8, 12) }
    }
}

impl AsBytes for ByteArray {
    fn as_bytes(&self) -> &[u8] {
        self.data()
    }
}

impl AsBytes for FixedLenByteArray {
    fn as_bytes(&self) -> &[u8] {
        self.data()
    }
}

impl AsBytes for Decimal {
    fn as_bytes(&self) -> &[u8] {
        self.data()
    }
}

impl AsBytes for Vec<u8> {
    fn as_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsBytes for &str {
    fn as_bytes(&self) -> &[u8] {
        (self as &str).as_bytes()
    }
}

impl AsBytes for str {
    fn as_bytes(&self) -> &[u8] {
        (self as &str).as_bytes()
    }
}

pub(crate) mod private {
    use bytes::Bytes;

    use crate::encodings::decoding::PlainDecoderDetails;
    use crate::util::bit_util::{read_num_bytes, BitReader, BitWriter};

    use super::{ParquetError, Result, SliceAsBytes};
    use crate::basic::Type;
    use crate::file::metadata::HeapSize;

    /// Sealed trait to start to remove specialisation from implementations
    ///
    /// This is done to force the associated value type to be unimplementable outside of this
    /// crate, and thus hint to the type system (and end user) traits are public for the contract
    /// and not for extension.
    pub trait ParquetValueType:
        PartialEq
        + std::fmt::Debug
        + std::fmt::Display
        + Default
        + Clone
        + super::AsBytes
        + super::FromBytes
        + SliceAsBytes
        + PartialOrd
        + Send
        + HeapSize
        + crate::encodings::decoding::private::GetDecoder
        + crate::file::statistics::private::MakeStatistics
    {
        const PHYSICAL_TYPE: Type;

        /// Encode the value directly from a higher level encoder
        fn encode<W: std::io::Write>(
            values: &[Self],
            writer: &mut W,
            bit_writer: &mut BitWriter,
        ) -> Result<()>;

        /// Establish the data that will be decoded in a buffer
        fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize);

        /// Decode the value from a given buffer for a higher level decoder
        fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize>;

        fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize>;

        /// Return the encoded size for a type
        fn dict_encoding_size(&self) -> (usize, usize) {
            (std::mem::size_of::<Self>(), 1)
        }

        /// Return the number of variable length bytes in a given slice of data
        ///
        /// Returns the sum of lengths for BYTE_ARRAY data, and None for all other data types
        fn variable_length_bytes(_: &[Self]) -> Option<i64> {
            None
        }

        /// Return the value as i64 if possible
        ///
        /// This is essentially the same as `std::convert::TryInto<i64>` but can't be
        /// implemented for `f32` and `f64`, types that would fail orphan rules
        fn as_i64(&self) -> Result<i64> {
            Err(general_err!("Type cannot be converted to i64"))
        }

        /// Return the value as u64 if possible
        ///
        /// This is essentially the same as `std::convert::TryInto<u64>` but can't be
        /// implemented for `f32` and `f64`, types that would fail orphan rules
        fn as_u64(&self) -> Result<u64> {
            self.as_i64()
                .map_err(|_| general_err!("Type cannot be converted to u64"))
                .map(|x| x as u64)
        }

        /// Return the value as an Any to allow for downcasts without transmutation
        fn as_any(&self) -> &dyn std::any::Any;

        /// Return the value as an mutable Any to allow for downcasts without transmutation
        fn as_mut_any(&mut self) -> &mut dyn std::any::Any;

        /// Sets the value of this object from the provided [`Bytes`]
        ///
        /// Only implemented for `ByteArray` and `FixedLenByteArray`. Will panic for other types.
        fn set_from_bytes(&mut self, _data: Bytes) {
            unimplemented!();
        }
    }

    impl ParquetValueType for bool {
        const PHYSICAL_TYPE: Type = Type::BOOLEAN;

        #[inline]
        fn encode<W: std::io::Write>(
            values: &[Self],
            _: &mut W,
            bit_writer: &mut BitWriter,
        ) -> Result<()> {
            for value in values {
                bit_writer.put_value(*value as u64, 1)
            }
            Ok(())
        }

        #[inline]
        fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize) {
            decoder.bit_reader.replace(BitReader::new(data));
            decoder.num_values = num_values;
        }

        #[inline]
        fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize> {
            let bit_reader = decoder.bit_reader.as_mut().unwrap();
            let num_values = std::cmp::min(buffer.len(), decoder.num_values);
            let values_read = bit_reader.get_batch(&mut buffer[..num_values], 1);
            decoder.num_values -= values_read;
            Ok(values_read)
        }

        fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize> {
            let bit_reader = decoder.bit_reader.as_mut().unwrap();
            let num_values = std::cmp::min(num_values, decoder.num_values);
            let values_read = bit_reader.skip(num_values, 1);
            decoder.num_values -= values_read;
            Ok(values_read)
        }

        #[inline]
        fn as_i64(&self) -> Result<i64> {
            Ok(*self as i64)
        }

        #[inline]
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        #[inline]
        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    macro_rules! impl_from_raw {
        ($ty: ty, $physical_ty: expr, $self: ident => $as_i64: block) => {
            impl ParquetValueType for $ty {
                const PHYSICAL_TYPE: Type = $physical_ty;

                #[inline]
                fn encode<W: std::io::Write>(values: &[Self], writer: &mut W, _: &mut BitWriter) -> Result<()> {
                    // SAFETY: Self is one of i32, i64, f32, f64, which have no padding.
                    let raw = unsafe {
                        std::slice::from_raw_parts(
                            values.as_ptr() as *const u8,
                            std::mem::size_of_val(values),
                        )
                    };
                    writer.write_all(raw)?;

                    Ok(())
                }

                #[inline]
                fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize) {
                    decoder.data.replace(data);
                    decoder.start = 0;
                    decoder.num_values = num_values;
                }

                #[inline]
                fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize> {
                    let data = decoder.data.as_ref().expect("set_data should have been called");
                    let num_values = std::cmp::min(buffer.len(), decoder.num_values);
                    let bytes_left = data.len() - decoder.start;
                    let bytes_to_decode = std::mem::size_of::<Self>() * num_values;

                    if bytes_left < bytes_to_decode {
                        return Err(eof_err!("Not enough bytes to decode"));
                    }

                    {
                        // SAFETY: Self has no invalid bit patterns, so writing to the slice
                        // obtained with slice_as_bytes_mut is always safe.
                        let raw_buffer = &mut unsafe { Self::slice_as_bytes_mut(buffer) }[..bytes_to_decode];
                        raw_buffer.copy_from_slice(data.slice(
                            decoder.start..decoder.start + bytes_to_decode
                        ).as_ref());
                    };
                    decoder.start += bytes_to_decode;
                    decoder.num_values -= num_values;

                    Ok(num_values)
                }

                #[inline]
                fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize> {
                    let data = decoder.data.as_ref().expect("set_data should have been called");
                    let num_values = num_values.min(decoder.num_values);
                    let bytes_left = data.len() - decoder.start;
                    let bytes_to_skip = std::mem::size_of::<Self>() * num_values;

                    if bytes_left < bytes_to_skip {
                        return Err(eof_err!("Not enough bytes to skip"));
                    }

                    decoder.start += bytes_to_skip;
                    decoder.num_values -= num_values;

                    Ok(num_values)
                }

                #[inline]
                fn as_i64(&$self) -> Result<i64> {
                    $as_i64
                }

                #[inline]
                fn as_any(&self) -> &dyn std::any::Any {
                    self
                }

                #[inline]
                fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
                    self
                }
            }
        }
    }

    impl_from_raw!(i32, Type::INT32, self => { Ok(*self as i64) });
    impl_from_raw!(i64, Type::INT64, self => { Ok(*self) });
    impl_from_raw!(f32, Type::FLOAT, self => { Err(general_err!("Type cannot be converted to i64")) });
    impl_from_raw!(f64, Type::DOUBLE, self => { Err(general_err!("Type cannot be converted to i64")) });

    impl ParquetValueType for super::Int96 {
        const PHYSICAL_TYPE: Type = Type::INT96;

        #[inline]
        fn encode<W: std::io::Write>(
            values: &[Self],
            writer: &mut W,
            _: &mut BitWriter,
        ) -> Result<()> {
            for value in values {
                let raw = SliceAsBytes::slice_as_bytes(value.data());
                writer.write_all(raw)?;
            }
            Ok(())
        }

        #[inline]
        fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize) {
            decoder.data.replace(data);
            decoder.start = 0;
            decoder.num_values = num_values;
        }

        #[inline]
        fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize> {
            // TODO - Remove the duplication between this and the general slice method
            let data = decoder
                .data
                .as_ref()
                .expect("set_data should have been called");
            let num_values = std::cmp::min(buffer.len(), decoder.num_values);
            let bytes_left = data.len() - decoder.start;
            let bytes_to_decode = 12 * num_values;

            if bytes_left < bytes_to_decode {
                return Err(eof_err!("Not enough bytes to decode"));
            }

            let data_range = data.slice(decoder.start..decoder.start + bytes_to_decode);
            let bytes: &[u8] = &data_range;
            decoder.start += bytes_to_decode;

            let mut pos = 0; // position in byte array
            for item in buffer.iter_mut().take(num_values) {
                let elem0 = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                let elem1 = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
                let elem2 = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());

                item.set_data(elem0, elem1, elem2);
                pos += 12;
            }
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize> {
            let data = decoder
                .data
                .as_ref()
                .expect("set_data should have been called");
            let num_values = std::cmp::min(num_values, decoder.num_values);
            let bytes_left = data.len() - decoder.start;
            let bytes_to_skip = 12 * num_values;

            if bytes_left < bytes_to_skip {
                return Err(eof_err!("Not enough bytes to skip"));
            }
            decoder.start += bytes_to_skip;
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        #[inline]
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        #[inline]
        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    impl HeapSize for super::Int96 {
        fn heap_size(&self) -> usize {
            0 // no heap allocations
        }
    }

    impl ParquetValueType for super::ByteArray {
        const PHYSICAL_TYPE: Type = Type::BYTE_ARRAY;

        #[inline]
        fn encode<W: std::io::Write>(
            values: &[Self],
            writer: &mut W,
            _: &mut BitWriter,
        ) -> Result<()> {
            for value in values {
                let len: u32 = value.len().try_into().unwrap();
                writer.write_all(&len.to_ne_bytes())?;
                let raw = value.data();
                writer.write_all(raw)?;
            }
            Ok(())
        }

        #[inline]
        fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize) {
            decoder.data.replace(data);
            decoder.start = 0;
            decoder.num_values = num_values;
        }

        #[inline]
        fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize> {
            let data = decoder
                .data
                .as_mut()
                .expect("set_data should have been called");
            let num_values = std::cmp::min(buffer.len(), decoder.num_values);
            for val_array in buffer.iter_mut().take(num_values) {
                let len: usize =
                    read_num_bytes::<u32>(4, data.slice(decoder.start..).as_ref()) as usize;
                decoder.start += std::mem::size_of::<u32>();

                if data.len() < decoder.start + len {
                    return Err(eof_err!("Not enough bytes to decode"));
                }

                val_array.set_data(data.slice(decoder.start..decoder.start + len));
                decoder.start += len;
            }
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        fn variable_length_bytes(values: &[Self]) -> Option<i64> {
            Some(values.iter().map(|x| x.len() as i64).sum())
        }

        fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize> {
            let data = decoder
                .data
                .as_mut()
                .expect("set_data should have been called");
            let num_values = num_values.min(decoder.num_values);

            for _ in 0..num_values {
                let len: usize =
                    read_num_bytes::<u32>(4, data.slice(decoder.start..).as_ref()) as usize;
                decoder.start += std::mem::size_of::<u32>() + len;
            }
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        #[inline]
        fn dict_encoding_size(&self) -> (usize, usize) {
            (std::mem::size_of::<u32>(), self.len())
        }

        #[inline]
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        #[inline]
        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }

        #[inline]
        fn set_from_bytes(&mut self, data: Bytes) {
            self.set_data(data);
        }
    }

    impl HeapSize for super::ByteArray {
        fn heap_size(&self) -> usize {
            // note: this is an estimate, not exact, so just return the size
            // of the actual data used, don't try to handle the fact that it may
            // be shared.
            self.data.as_ref().map(|data| data.len()).unwrap_or(0)
        }
    }

    impl ParquetValueType for super::FixedLenByteArray {
        const PHYSICAL_TYPE: Type = Type::FIXED_LEN_BYTE_ARRAY;

        #[inline]
        fn encode<W: std::io::Write>(
            values: &[Self],
            writer: &mut W,
            _: &mut BitWriter,
        ) -> Result<()> {
            for value in values {
                let raw = value.data();
                writer.write_all(raw)?;
            }
            Ok(())
        }

        #[inline]
        fn set_data(decoder: &mut PlainDecoderDetails, data: Bytes, num_values: usize) {
            decoder.data.replace(data);
            decoder.start = 0;
            decoder.num_values = num_values;
        }

        #[inline]
        fn decode(buffer: &mut [Self], decoder: &mut PlainDecoderDetails) -> Result<usize> {
            assert!(decoder.type_length > 0);

            let data = decoder
                .data
                .as_mut()
                .expect("set_data should have been called");
            let num_values = std::cmp::min(buffer.len(), decoder.num_values);

            for item in buffer.iter_mut().take(num_values) {
                let len = decoder.type_length as usize;

                if data.len() < decoder.start + len {
                    return Err(eof_err!("Not enough bytes to decode"));
                }

                item.set_data(data.slice(decoder.start..decoder.start + len));
                decoder.start += len;
            }
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        fn skip(decoder: &mut PlainDecoderDetails, num_values: usize) -> Result<usize> {
            assert!(decoder.type_length > 0);

            let data = decoder
                .data
                .as_mut()
                .expect("set_data should have been called");
            let num_values = std::cmp::min(num_values, decoder.num_values);
            for _ in 0..num_values {
                let len = decoder.type_length as usize;

                if data.len() < decoder.start + len {
                    return Err(eof_err!("Not enough bytes to skip"));
                }

                decoder.start += len;
            }
            decoder.num_values -= num_values;

            Ok(num_values)
        }

        #[inline]
        fn dict_encoding_size(&self) -> (usize, usize) {
            (std::mem::size_of::<u32>(), self.len())
        }

        #[inline]
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        #[inline]
        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }

        #[inline]
        fn set_from_bytes(&mut self, data: Bytes) {
            self.set_data(data);
        }
    }

    impl HeapSize for super::FixedLenByteArray {
        fn heap_size(&self) -> usize {
            self.0.heap_size()
        }
    }
}

/// Contains the Parquet physical type information as well as the Rust primitive type
/// presentation.
pub trait DataType: 'static + Send {
    /// The physical type of the Parquet data type.
    type T: private::ParquetValueType;

    /// Returns Parquet physical type.
    fn get_physical_type() -> Type {
        <Self::T as private::ParquetValueType>::PHYSICAL_TYPE
    }

    /// Returns size in bytes for Rust representation of the physical type.
    fn get_type_size() -> usize;

    /// Returns the underlying [`ColumnReaderImpl`] for the given [`ColumnReader`].
    fn get_column_reader(column_writer: ColumnReader) -> Option<ColumnReaderImpl<Self>>
    where
        Self: Sized;

    /// Returns the underlying [`ColumnWriterImpl`] for the given [`ColumnWriter`].
    fn get_column_writer(column_writer: ColumnWriter<'_>) -> Option<ColumnWriterImpl<'_, Self>>
    where
        Self: Sized;

    /// Returns a reference to the underlying [`ColumnWriterImpl`] for the given [`ColumnWriter`].
    fn get_column_writer_ref<'a, 'b: 'a>(
        column_writer: &'b ColumnWriter<'a>,
    ) -> Option<&'b ColumnWriterImpl<'a, Self>>
    where
        Self: Sized;

    /// Returns a mutable reference to the underlying [`ColumnWriterImpl`] for the given
    fn get_column_writer_mut<'a, 'b: 'a>(
        column_writer: &'a mut ColumnWriter<'b>,
    ) -> Option<&'a mut ColumnWriterImpl<'b, Self>>
    where
        Self: Sized;
}

macro_rules! make_type {
    ($name:ident, $reader_ident: ident, $writer_ident: ident, $native_ty:ty, $size:expr) => {
        #[doc = concat!("Parquet physical type: ", stringify!($name))]
        #[derive(Clone)]
        pub struct $name {}

        impl DataType for $name {
            type T = $native_ty;

            fn get_type_size() -> usize {
                $size
            }

            fn get_column_reader(column_reader: ColumnReader) -> Option<ColumnReaderImpl<Self>> {
                match column_reader {
                    ColumnReader::$reader_ident(w) => Some(w),
                    _ => None,
                }
            }

            fn get_column_writer(
                column_writer: ColumnWriter<'_>,
            ) -> Option<ColumnWriterImpl<'_, Self>> {
                match column_writer {
                    ColumnWriter::$writer_ident(w) => Some(w),
                    _ => None,
                }
            }

            fn get_column_writer_ref<'a, 'b: 'a>(
                column_writer: &'a ColumnWriter<'b>,
            ) -> Option<&'a ColumnWriterImpl<'b, Self>> {
                match column_writer {
                    ColumnWriter::$writer_ident(w) => Some(w),
                    _ => None,
                }
            }

            fn get_column_writer_mut<'a, 'b: 'a>(
                column_writer: &'a mut ColumnWriter<'b>,
            ) -> Option<&'a mut ColumnWriterImpl<'b, Self>> {
                match column_writer {
                    ColumnWriter::$writer_ident(w) => Some(w),
                    _ => None,
                }
            }
        }
    };
}

// Generate struct definitions for all physical types

make_type!(BoolType, BoolColumnReader, BoolColumnWriter, bool, 1);
make_type!(Int32Type, Int32ColumnReader, Int32ColumnWriter, i32, 4);
make_type!(Int64Type, Int64ColumnReader, Int64ColumnWriter, i64, 8);
make_type!(
    Int96Type,
    Int96ColumnReader,
    Int96ColumnWriter,
    Int96,
    mem::size_of::<Int96>()
);
make_type!(FloatType, FloatColumnReader, FloatColumnWriter, f32, 4);
make_type!(DoubleType, DoubleColumnReader, DoubleColumnWriter, f64, 8);
make_type!(
    ByteArrayType,
    ByteArrayColumnReader,
    ByteArrayColumnWriter,
    ByteArray,
    mem::size_of::<ByteArray>()
);
make_type!(
    FixedLenByteArrayType,
    FixedLenByteArrayColumnReader,
    FixedLenByteArrayColumnWriter,
    FixedLenByteArray,
    mem::size_of::<FixedLenByteArray>()
);

impl AsRef<[u8]> for ByteArray {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for FixedLenByteArray {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Macro to reduce repetition in making type assertions on the physical type against `T`
macro_rules! ensure_phys_ty {
    ($($ty:pat_param)|+ , $err: literal) => {
        match T::get_physical_type() {
            $($ty => (),)*
            _ => panic!($err),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_as_bytes() {
        // Test Int96
        let i96 = Int96::from(vec![1, 2, 3]);
        assert_eq!(i96.as_bytes(), &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);

        // Test ByteArray
        let ba = ByteArray::from(vec![1, 2, 3]);
        assert_eq!(ba.as_bytes(), &[1, 2, 3]);

        // Test Decimal
        let decimal = Decimal::from_i32(123, 5, 2);
        assert_eq!(decimal.as_bytes(), &[0, 0, 0, 123]);
        let decimal = Decimal::from_i64(123, 5, 2);
        assert_eq!(decimal.as_bytes(), &[0, 0, 0, 0, 0, 0, 0, 123]);
        let decimal = Decimal::from_bytes(ByteArray::from(vec![1, 2, 3]), 5, 2);
        assert_eq!(decimal.as_bytes(), &[1, 2, 3]);
    }

    #[test]
    fn test_int96_from() {
        assert_eq!(
            Int96::from(vec![1, 12345, 1234567890]).data(),
            &[1, 12345, 1234567890]
        );
    }

    #[test]
    fn test_byte_array_from() {
        assert_eq!(ByteArray::from(b"ABC".to_vec()).data(), b"ABC");
        assert_eq!(ByteArray::from("ABC").data(), b"ABC");
        assert_eq!(
            ByteArray::from(Bytes::from(vec![1u8, 2u8, 3u8, 4u8, 5u8])).data(),
            &[1u8, 2u8, 3u8, 4u8, 5u8]
        );
        let buf = vec![6u8, 7u8, 8u8, 9u8, 10u8];
        assert_eq!(ByteArray::from(buf).data(), &[6u8, 7u8, 8u8, 9u8, 10u8]);
    }

    #[test]
    fn test_decimal_partial_eq() {
        assert_eq!(Decimal::default(), Decimal::from_i32(0, 0, 0));
        assert_eq!(Decimal::from_i32(222, 5, 2), Decimal::from_i32(222, 5, 2));
        assert_eq!(
            Decimal::from_bytes(ByteArray::from(vec![0, 0, 0, 3]), 5, 2),
            Decimal::from_i32(3, 5, 2)
        );

        assert!(Decimal::from_i32(222, 5, 2) != Decimal::from_i32(111, 5, 2));
        assert!(Decimal::from_i32(222, 5, 2) != Decimal::from_i32(222, 6, 2));
        assert!(Decimal::from_i32(222, 5, 2) != Decimal::from_i32(222, 5, 3));

        assert!(Decimal::from_i64(222, 5, 2) != Decimal::from_i32(222, 5, 2));
    }

    #[test]
    fn test_byte_array_ord() {
        let ba1 = ByteArray::from(vec![1, 2, 3]);
        let ba11 = ByteArray::from(vec![1, 2, 3]);
        let ba2 = ByteArray::from(vec![3, 4]);
        let ba3 = ByteArray::from(vec![1, 2, 4]);
        let ba4 = ByteArray::from(vec![]);
        let ba5 = ByteArray::from(vec![2, 2, 3]);

        assert!(ba1 < ba2);
        assert!(ba3 > ba1);
        assert!(ba1 > ba4);
        assert_eq!(ba1, ba11);
        assert!(ba5 > ba1);
    }
}
