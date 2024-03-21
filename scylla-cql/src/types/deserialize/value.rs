//! Provides types for dealing with CQL value deserialization.

use bytes::Bytes;

use std::fmt::Display;

use thiserror::Error;

use super::{DeserializationError, FrameSlice, TypeCheckError};
use crate::frame::frame_errors::ParseError;
use crate::frame::response::result::{deser_cql_value, ColumnType, CqlValue};
use crate::frame::types;
use crate::frame::value::{Counter, CqlDecimal, CqlVarint};

/// A type that can be deserialized from a column value inside a row that was
/// returned from a query.
///
/// For tips on how to write a custom implementation of this trait, see the
/// documentation of the parent module.
///
/// The crate also provides a derive macro which allows to automatically
/// implement the trait for a custom type. For more details on what the macro
/// is capable of, see its documentation.
pub trait DeserializeValue<'frame>
where
    Self: Sized,
{
    /// Checks that the column type matches what this type expects.
    fn type_check(typ: &ColumnType) -> Result<(), TypeCheckError>;

    /// Deserialize a column value from given serialized representation.
    ///
    /// This function can assume that the driver called `type_check` to verify
    /// the column's type. Note that `deserialize` is not an unsafe function,
    /// so it should not use the assumption about `type_check` being called
    /// as an excuse to run `unsafe` code.
    fn deserialize(
        typ: &'frame ColumnType,
        v: Option<FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError>;
}

impl<'frame> DeserializeValue<'frame> for CqlValue {
    fn type_check(_typ: &ColumnType) -> Result<(), TypeCheckError> {
        // CqlValue accepts all possible CQL types
        Ok(())
    }

    fn deserialize(
        typ: &'frame ColumnType,
        v: Option<FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError> {
        let mut val = ensure_not_null_slice::<Self>(typ, v)?;
        let cql = deser_cql_value(typ, &mut val).map_err(|err| {
            mk_deser_err::<Self>(typ, BuiltinDeserializationErrorKind::GenericParseError(err))
        })?;
        Ok(cql)
    }
}

// Option represents nullability of CQL values:
// None corresponds to null,
// Some(val) to non-null values.
impl<'frame, T> DeserializeValue<'frame> for Option<T>
where
    T: DeserializeValue<'frame>,
{
    fn type_check(typ: &ColumnType) -> Result<(), TypeCheckError> {
        T::type_check(typ)
    }

    fn deserialize(
        typ: &'frame ColumnType,
        v: Option<FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError> {
        v.map(|_| T::deserialize(typ, v)).transpose()
    }
}

/// Values that may be empty or not.
///
/// In CQL, some types can have a special value of "empty", represented as
/// a serialized value of length 0. An example of this are integral types:
/// the "int" type can actually hold 2^32 + 1 possible values because of this
/// quirk. Note that this is distinct from being NULL.
///
/// Rust types that cannot represent an empty value (e.g. i32) should implement
/// this trait in order to be deserialized as [MaybeEmpty].
pub trait Emptiable {}

/// A value that may be empty or not.
///
/// `MaybeEmpty` was introduced to help support the quirk described in [Emptiable]
/// for Rust types which can't represent the empty, additional value.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum MaybeEmpty<T: Emptiable> {
    Empty,
    Value(T),
}

impl<'frame, T> DeserializeValue<'frame> for MaybeEmpty<T>
where
    T: DeserializeValue<'frame> + Emptiable,
{
    #[inline]
    fn type_check(typ: &ColumnType) -> Result<(), TypeCheckError> {
        <T as DeserializeValue<'frame>>::type_check(typ)
    }

    fn deserialize(
        typ: &'frame ColumnType,
        v: Option<FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError> {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        if val.is_empty() {
            Ok(MaybeEmpty::Empty)
        } else {
            let v = <T as DeserializeValue<'frame>>::deserialize(typ, v)?;
            Ok(MaybeEmpty::Value(v))
        }
    }
}

macro_rules! impl_strict_type {
    ($t:ty, [$($cql:ident)|+], $conv:expr $(, $l:lifetime)?) => {
        impl<$($l,)? 'frame> DeserializeValue<'frame> for $t
        where
            $('frame: $l)?
        {
            fn type_check(typ: &ColumnType) -> Result<(), TypeCheckError> {
                // TODO: Format the CQL type names in the same notation
                // that ScyllaDB/Cassandra uses internally and include them
                // in such form in the error message
                exact_type_check!(typ, $($cql),*);
                Ok(())
            }

            fn deserialize(
                typ: &'frame ColumnType,
                v: Option<FrameSlice<'frame>>,
            ) -> Result<Self, DeserializationError> {
                $conv(typ, v)
            }
        }
    };

    // Convenience pattern for omitting brackets if type-checking as single types.
    ($t:ty, $cql:ident, $conv:expr $(, $l:lifetime)?) => {
        impl_strict_type!($t, [$cql], $conv $(, $l)*);
    };
}

macro_rules! impl_emptiable_strict_type {
    ($t:ty, [$($cql:ident)|+], $conv:expr $(, $l:lifetime)?) => {
        impl<$($l,)?> Emptiable for $t {}

        impl_strict_type!($t, [$($cql)|*], $conv $(, $l)*);
    };

    // Convenience pattern for omitting brackets if type-checking as single types.
    ($t:ty, $cql:ident, $conv:expr $(, $l:lifetime)?) => {
        impl_emptiable_strict_type!($t, [$cql], $conv $(, $l)*);
    };

}

// fixed numeric types

macro_rules! impl_fixed_numeric_type {
    ($t:ty, [$($cql:ident)|+]) => {
        impl_emptiable_strict_type!(
            $t,
            [$($cql)|*],
            |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
                const SIZE: usize = std::mem::size_of::<$t>();
                let val = ensure_not_null_slice::<Self>(typ, v)?;
                let arr = ensure_exact_length::<Self, SIZE>(typ, val)?;
                Ok(<$t>::from_be_bytes(*arr))
            }
        );
    };

    // Convenience pattern for omitting brackets if type-checking as single types.
    ($t:ty, $cql:ident) => {
        impl_fixed_numeric_type!($t, [$cql]);
    };
}

impl_emptiable_strict_type!(
    bool,
    Boolean,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        let arr = ensure_exact_length::<Self, 1>(typ, val)?;
        Ok(arr[0] != 0x00)
    }
);

impl_fixed_numeric_type!(i8, TinyInt);
impl_fixed_numeric_type!(i16, SmallInt);
impl_fixed_numeric_type!(i32, Int);
impl_fixed_numeric_type!(i64, [BigInt | Counter]);
impl_fixed_numeric_type!(f32, Float);
impl_fixed_numeric_type!(f64, Double);

// other numeric types

impl_emptiable_strict_type!(
    CqlVarint,
    Varint,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        Ok(CqlVarint::from_signed_bytes_be_slice(val))
    }
);

#[cfg(feature = "num-bigint-03")]
impl_emptiable_strict_type!(num_bigint_03::BigInt, Varint, |typ: &'frame ColumnType,
                                                            v: Option<
    FrameSlice<'frame>,
>| {
    let val = ensure_not_null_slice::<Self>(typ, v)?;
    Ok(num_bigint_03::BigInt::from_signed_bytes_be(val))
});

#[cfg(feature = "num-bigint-04")]
impl_emptiable_strict_type!(num_bigint_04::BigInt, Varint, |typ: &'frame ColumnType,
                                                            v: Option<
    FrameSlice<'frame>,
>| {
    let val = ensure_not_null_slice::<Self>(typ, v)?;
    Ok(num_bigint_04::BigInt::from_signed_bytes_be(val))
});

impl_emptiable_strict_type!(
    CqlDecimal,
    Decimal,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let mut val = ensure_not_null_slice::<Self>(typ, v)?;
        let scale = types::read_int(&mut val).map_err(|err| {
            mk_deser_err::<Self>(
                typ,
                BuiltinDeserializationErrorKind::GenericParseError(err.into()),
            )
        })?;
        Ok(CqlDecimal::from_signed_be_bytes_slice_and_exponent(
            val, scale,
        ))
    }
);

#[cfg(feature = "bigdecimal-04")]
impl_emptiable_strict_type!(
    bigdecimal_04::BigDecimal,
    Decimal,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let mut val = ensure_not_null_slice::<Self>(typ, v)?;
        let scale = types::read_int(&mut val).map_err(|err| {
            mk_deser_err::<Self>(
                typ,
                BuiltinDeserializationErrorKind::GenericParseError(err.into()),
            )
        })? as i64;
        let int_value = bigdecimal_04::num_bigint::BigInt::from_signed_bytes_be(val);
        Ok(bigdecimal_04::BigDecimal::from((int_value, scale)))
    }
);

// blob

impl_strict_type!(
    &'a [u8],
    Blob,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        Ok(val)
    },
    'a
);
impl_strict_type!(
    Vec<u8>,
    Blob,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        Ok(val.to_vec())
    }
);
impl_strict_type!(
    Bytes,
    Blob,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_owned::<Self>(typ, v)?;
        Ok(val)
    }
);

// string

macro_rules! impl_string_type {
    ($t:ty, $conv:expr $(, $l:lifetime)?) => {
        impl_strict_type!(
            $t,
            [Ascii | Text],
            $conv
            $(, $l)?
        );
    }
}

fn check_ascii<T>(typ: &ColumnType, s: &[u8]) -> Result<(), DeserializationError> {
    if matches!(typ, ColumnType::Ascii) && !s.is_ascii() {
        return Err(mk_deser_err::<T>(
            typ,
            BuiltinDeserializationErrorKind::ExpectedAscii,
        ));
    }
    Ok(())
}

impl_string_type!(
    &'a str,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        check_ascii::<&str>(typ, val)?;
        let s = std::str::from_utf8(val).map_err(|err| {
            mk_deser_err::<Self>(typ, BuiltinDeserializationErrorKind::InvalidUtf8(err))
        })?;
        Ok(s)
    },
    'a
);
impl_string_type!(
    String,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        check_ascii::<String>(typ, val)?;
        let s = std::str::from_utf8(val).map_err(|err| {
            mk_deser_err::<Self>(typ, BuiltinDeserializationErrorKind::InvalidUtf8(err))
        })?;
        Ok(s.to_string())
    }
);

// TODO: Consider support for deserialization of string::String<Bytes>

// counter

impl_strict_type!(
    Counter,
    Counter,
    |typ: &'frame ColumnType, v: Option<FrameSlice<'frame>>| {
        let val = ensure_not_null_slice::<Self>(typ, v)?;
        let arr = ensure_exact_length::<Self, 8>(typ, val)?;
        let counter = i64::from_be_bytes(*arr);
        Ok(Counter(counter))
    }
);

// Utilities

fn ensure_not_null_frame_slice<'frame, T>(
    typ: &ColumnType,
    v: Option<FrameSlice<'frame>>,
) -> Result<FrameSlice<'frame>, DeserializationError> {
    v.ok_or_else(|| mk_deser_err::<T>(typ, BuiltinDeserializationErrorKind::ExpectedNonNull))
}

fn ensure_not_null_slice<'frame, T>(
    typ: &ColumnType,
    v: Option<FrameSlice<'frame>>,
) -> Result<&'frame [u8], DeserializationError> {
    ensure_not_null_frame_slice::<T>(typ, v).map(|frame_slice| frame_slice.as_slice())
}

fn ensure_not_null_owned<T>(
    typ: &ColumnType,
    v: Option<FrameSlice>,
) -> Result<Bytes, DeserializationError> {
    ensure_not_null_frame_slice::<T>(typ, v).map(|frame_slice| frame_slice.to_bytes())
}

fn ensure_exact_length<'frame, T, const SIZE: usize>(
    typ: &ColumnType,
    v: &'frame [u8],
) -> Result<&'frame [u8; SIZE], DeserializationError> {
    v.try_into().map_err(|_| {
        mk_deser_err::<T>(
            typ,
            BuiltinDeserializationErrorKind::ByteLengthMismatch {
                expected: SIZE,
                got: v.len(),
            },
        )
    })
}

// Error facilities

/// Type checking of one of the built-in types failed.
#[derive(Debug, Error, Clone)]
#[error("Failed to type check Rust type {rust_name} against CQL type {cql_type:?}: {kind}")]
pub struct BuiltinTypeCheckError {
    /// Name of the Rust type being deserialized.
    pub rust_name: &'static str,

    /// The CQL type that the Rust type was being deserialized from.
    pub cql_type: ColumnType,

    /// Detailed information about the failure.
    pub kind: BuiltinTypeCheckErrorKind,
}

fn mk_typck_err<T>(
    cql_type: &ColumnType,
    kind: impl Into<BuiltinTypeCheckErrorKind>,
) -> TypeCheckError {
    mk_typck_err_named(std::any::type_name::<T>(), cql_type, kind)
}

fn mk_typck_err_named(
    name: &'static str,
    cql_type: &ColumnType,
    kind: impl Into<BuiltinTypeCheckErrorKind>,
) -> TypeCheckError {
    TypeCheckError::new(BuiltinTypeCheckError {
        rust_name: name,
        cql_type: cql_type.clone(),
        kind: kind.into(),
    })
}

macro_rules! exact_type_check {
    ($typ:ident, $($cql:tt),*) => {
        match $typ {
            $(ColumnType::$cql)|* => {},
            _ => return Err(mk_typck_err::<Self>(
                $typ,
                BuiltinTypeCheckErrorKind::MismatchedType {
                    expected: &[$(ColumnType::$cql),*],
                }
            ))
        }
    };
}
use exact_type_check;

/// Describes why type checking some of the built-in types failed.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum BuiltinTypeCheckErrorKind {
    /// Expected one from a list of particular types.
    MismatchedType {
        /// The list of types that the Rust type can deserialize from.
        expected: &'static [ColumnType],
    },
}

impl Display for BuiltinTypeCheckErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuiltinTypeCheckErrorKind::MismatchedType { expected } => {
                write!(f, "expected one of the CQL types: {expected:?}")
            }
        }
    }
}

/// Deserialization of one of the built-in types failed.
#[derive(Debug, Error)]
#[error("Failed to deserialize Rust type {rust_name} from CQL type {cql_type:?}: {kind}")]
pub struct BuiltinDeserializationError {
    /// Name of the Rust type being deserialized.
    pub rust_name: &'static str,

    /// The CQL type that the Rust type was being deserialized from.
    pub cql_type: ColumnType,

    /// Detailed information about the failure.
    pub kind: BuiltinDeserializationErrorKind,
}

fn mk_deser_err<T>(
    cql_type: &ColumnType,
    kind: impl Into<BuiltinDeserializationErrorKind>,
) -> DeserializationError {
    mk_deser_err_named(std::any::type_name::<T>(), cql_type, kind)
}

fn mk_deser_err_named(
    name: &'static str,
    cql_type: &ColumnType,
    kind: impl Into<BuiltinDeserializationErrorKind>,
) -> DeserializationError {
    DeserializationError::new(BuiltinDeserializationError {
        rust_name: name,
        cql_type: cql_type.clone(),
        kind: kind.into(),
    })
}

/// Describes why deserialization of some of the built-in types failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum BuiltinDeserializationErrorKind {
    /// A generic deserialization failure - legacy error type.
    GenericParseError(ParseError),

    /// Expected non-null value, got null.
    ExpectedNonNull,

    /// The length of read value in bytes is different than expected for the Rust type.
    ByteLengthMismatch { expected: usize, got: usize },

    /// Expected valid ASCII string.
    ExpectedAscii,

    /// Invalid UTF-8 string.
    InvalidUtf8(std::str::Utf8Error),
}

impl Display for BuiltinDeserializationErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuiltinDeserializationErrorKind::GenericParseError(err) => err.fmt(f),
            BuiltinDeserializationErrorKind::ExpectedNonNull => {
                f.write_str("expected a non-null value, got null")
            }
            BuiltinDeserializationErrorKind::ByteLengthMismatch { expected, got } => write!(
                f,
                "the CQL type requires {} bytes, but got {}",
                expected, got,
            ),
            BuiltinDeserializationErrorKind::ExpectedAscii => {
                f.write_str("expected a valid ASCII string")
            }
            BuiltinDeserializationErrorKind::InvalidUtf8(err) => err.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, Bytes, BytesMut};

    use std::fmt::Debug;

    use crate::frame::response::cql_to_rust::FromCqlVal;
    use crate::frame::response::result::{deser_cql_value, ColumnType, CqlValue};
    use crate::frame::types;
    use crate::frame::value::{Counter, CqlDecimal, CqlVarint};
    use crate::types::deserialize::{DeserializationError, FrameSlice};
    use crate::types::serialize::value::SerializeValue;
    use crate::types::serialize::CellWriter;

    use super::{mk_deser_err, BuiltinDeserializationErrorKind, DeserializeValue};

    #[test]
    fn test_deserialize_bytes() {
        const ORIGINAL_BYTES: &[u8] = &[1, 5, 2, 4, 3];

        let bytes = make_bytes(ORIGINAL_BYTES);

        let decoded_slice = deserialize::<&[u8]>(&ColumnType::Blob, &bytes).unwrap();
        let decoded_vec = deserialize::<Vec<u8>>(&ColumnType::Blob, &bytes).unwrap();
        let decoded_bytes = deserialize::<Bytes>(&ColumnType::Blob, &bytes).unwrap();

        assert_eq!(decoded_slice, ORIGINAL_BYTES);
        assert_eq!(decoded_vec, ORIGINAL_BYTES);
        assert_eq!(decoded_bytes, ORIGINAL_BYTES);
    }

    #[test]
    fn test_deserialize_ascii() {
        const ASCII_TEXT: &str = "The quick brown fox jumps over the lazy dog";

        let ascii = make_bytes(ASCII_TEXT.as_bytes());

        let decoded_ascii_str = deserialize::<&str>(&ColumnType::Ascii, &ascii).unwrap();
        let decoded_ascii_string = deserialize::<String>(&ColumnType::Ascii, &ascii).unwrap();
        let decoded_text_str = deserialize::<&str>(&ColumnType::Text, &ascii).unwrap();
        let decoded_text_string = deserialize::<String>(&ColumnType::Text, &ascii).unwrap();

        assert_eq!(decoded_ascii_str, ASCII_TEXT);
        assert_eq!(decoded_ascii_string, ASCII_TEXT);
        assert_eq!(decoded_text_str, ASCII_TEXT);
        assert_eq!(decoded_text_string, ASCII_TEXT);
    }

    #[test]
    fn test_deserialize_text() {
        const UNICODE_TEXT: &str = "Zażółć gęślą jaźń";

        let unicode = make_bytes(UNICODE_TEXT.as_bytes());

        // Should fail because it's not an ASCII string
        deserialize::<&str>(&ColumnType::Ascii, &unicode).unwrap_err();
        deserialize::<String>(&ColumnType::Ascii, &unicode).unwrap_err();

        let decoded_text_str = deserialize::<&str>(&ColumnType::Text, &unicode).unwrap();
        let decoded_text_string = deserialize::<String>(&ColumnType::Text, &unicode).unwrap();
        assert_eq!(decoded_text_str, UNICODE_TEXT);
        assert_eq!(decoded_text_string, UNICODE_TEXT);
    }

    #[test]
    fn test_integral() {
        let tinyint = make_bytes(&[0x01]);
        let decoded_tinyint = deserialize::<i8>(&ColumnType::TinyInt, &tinyint).unwrap();
        assert_eq!(decoded_tinyint, 0x01);

        let smallint = make_bytes(&[0x01, 0x02]);
        let decoded_smallint = deserialize::<i16>(&ColumnType::SmallInt, &smallint).unwrap();
        assert_eq!(decoded_smallint, 0x0102);

        let int = make_bytes(&[0x01, 0x02, 0x03, 0x04]);
        let decoded_int = deserialize::<i32>(&ColumnType::Int, &int).unwrap();
        assert_eq!(decoded_int, 0x01020304);

        let bigint = make_bytes(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let decoded_bigint = deserialize::<i64>(&ColumnType::BigInt, &bigint).unwrap();
        assert_eq!(decoded_bigint, 0x0102030405060708);
    }

    #[test]
    fn test_bool() {
        for boolean in [true, false] {
            let boolean_bytes = make_bytes(&[boolean as u8]);
            let decoded_bool = deserialize::<bool>(&ColumnType::Boolean, &boolean_bytes).unwrap();
            assert_eq!(decoded_bool, boolean);
        }
    }

    #[test]
    fn test_floating_point() {
        let float = make_bytes(&[63, 0, 0, 0]);
        let decoded_float = deserialize::<f32>(&ColumnType::Float, &float).unwrap();
        assert_eq!(decoded_float, 0.5);

        let double = make_bytes(&[64, 0, 0, 0, 0, 0, 0, 0]);
        let decoded_double = deserialize::<f64>(&ColumnType::Double, &double).unwrap();
        assert_eq!(decoded_double, 2.0);
    }

    #[test]
    fn test_from_cql_value_compatibility() {
        // This test should have a sub-case for each type
        // that implements FromCqlValue

        // fixed size integers
        for i in 0..7 {
            let v: i8 = 1 << i;
            compat_check::<i8>(&ColumnType::TinyInt, make_bytes(&v.to_be_bytes()));
            compat_check::<i8>(&ColumnType::TinyInt, make_bytes(&(-v).to_be_bytes()));
        }
        for i in 0..15 {
            let v: i16 = 1 << i;
            compat_check::<i16>(&ColumnType::SmallInt, make_bytes(&v.to_be_bytes()));
            compat_check::<i16>(&ColumnType::SmallInt, make_bytes(&(-v).to_be_bytes()));
        }
        for i in 0..31 {
            let v: i32 = 1 << i;
            compat_check::<i32>(&ColumnType::Int, make_bytes(&v.to_be_bytes()));
            compat_check::<i32>(&ColumnType::Int, make_bytes(&(-v).to_be_bytes()));
        }
        for i in 0..63 {
            let v: i64 = 1 << i;
            compat_check::<i64>(&ColumnType::BigInt, make_bytes(&v.to_be_bytes()));
            compat_check::<i64>(&ColumnType::BigInt, make_bytes(&(-v).to_be_bytes()));
        }

        // bool
        compat_check::<bool>(&ColumnType::Boolean, make_bytes(&[0]));
        compat_check::<bool>(&ColumnType::Boolean, make_bytes(&[1]));

        // fixed size floating point types
        compat_check::<f32>(&ColumnType::Float, make_bytes(&123f32.to_be_bytes()));
        compat_check::<f32>(&ColumnType::Float, make_bytes(&(-123f32).to_be_bytes()));
        compat_check::<f64>(&ColumnType::Double, make_bytes(&123f64.to_be_bytes()));
        compat_check::<f64>(&ColumnType::Double, make_bytes(&(-123f64).to_be_bytes()));

        // big integers
        const PI_STR: &[u8] = b"3.1415926535897932384626433832795028841971693993751058209749445923";
        let num1 = &PI_STR[2..];
        let num2 = [b'-']
            .into_iter()
            .chain(PI_STR[2..].iter().copied())
            .collect::<Vec<_>>();
        let num3 = &b"0"[..];

        // native - CqlVarint
        {
            let num1 = CqlVarint::from_signed_bytes_be_slice(num1);
            let num2 = CqlVarint::from_signed_bytes_be_slice(&num2);
            let num3 = CqlVarint::from_signed_bytes_be_slice(num3);
            compat_check_serialized::<CqlVarint>(&ColumnType::Varint, &num1);
            compat_check_serialized::<CqlVarint>(&ColumnType::Varint, &num2);
            compat_check_serialized::<CqlVarint>(&ColumnType::Varint, &num3);
        }

        #[cfg(feature = "num-bigint-03")]
        {
            use num_bigint_03::BigInt;

            let num1 = BigInt::parse_bytes(num1, 10).unwrap();
            let num2 = BigInt::parse_bytes(&num2, 10).unwrap();
            let num3 = BigInt::parse_bytes(num3, 10).unwrap();
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num1);
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num2);
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num3);
        }

        #[cfg(feature = "num-bigint-04")]
        {
            use num_bigint_04::BigInt;

            let num1 = BigInt::parse_bytes(num1, 10).unwrap();
            let num2 = BigInt::parse_bytes(&num2, 10).unwrap();
            let num3 = BigInt::parse_bytes(num3, 10).unwrap();
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num1);
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num2);
            compat_check_serialized::<BigInt>(&ColumnType::Varint, &num3);
        }

        // big decimals
        {
            let scale1 = 0;
            let scale2 = -42;
            let scale3 = 2137;
            let num1 = CqlDecimal::from_signed_be_bytes_slice_and_exponent(num1, scale1);
            let num2 = CqlDecimal::from_signed_be_bytes_and_exponent(num2, scale2);
            let num3 = CqlDecimal::from_signed_be_bytes_slice_and_exponent(num3, scale3);
            compat_check_serialized::<CqlDecimal>(&ColumnType::Decimal, &num1);
            compat_check_serialized::<CqlDecimal>(&ColumnType::Decimal, &num2);
            compat_check_serialized::<CqlDecimal>(&ColumnType::Decimal, &num3);
        }

        // native - CqlDecimal

        #[cfg(feature = "bigdecimal-04")]
        {
            use bigdecimal_04::BigDecimal;

            let num1 = PI_STR.to_vec();
            let num2 = vec![b'-']
                .into_iter()
                .chain(PI_STR.iter().copied())
                .collect::<Vec<_>>();
            let num3 = b"0.0".to_vec();

            let num1 = BigDecimal::parse_bytes(&num1, 10).unwrap();
            let num2 = BigDecimal::parse_bytes(&num2, 10).unwrap();
            let num3 = BigDecimal::parse_bytes(&num3, 10).unwrap();
            compat_check_serialized::<BigDecimal>(&ColumnType::Decimal, &num1);
            compat_check_serialized::<BigDecimal>(&ColumnType::Decimal, &num2);
            compat_check_serialized::<BigDecimal>(&ColumnType::Decimal, &num3);
        }

        // blob
        compat_check::<Vec<u8>>(&ColumnType::Blob, make_bytes(&[]));
        compat_check::<Vec<u8>>(&ColumnType::Blob, make_bytes(&[1, 9, 2, 8, 3, 7, 4, 6, 5]));

        // text types
        for typ in &[ColumnType::Ascii, ColumnType::Text] {
            compat_check::<String>(typ, make_bytes("".as_bytes()));
            compat_check::<String>(typ, make_bytes("foo".as_bytes()));
            compat_check::<String>(typ, make_bytes("superfragilisticexpialidocious".as_bytes()));
        }

        // counters
        for i in 0..63 {
            let v: i64 = 1 << i;
            compat_check::<Counter>(&ColumnType::Counter, make_bytes(&v.to_be_bytes()));
        }
    }

    // Checks that both new and old serialization framework
    // produces the same results in this case
    fn compat_check<T>(typ: &ColumnType, raw: Bytes)
    where
        T: for<'f> DeserializeValue<'f>,
        T: FromCqlVal<Option<CqlValue>>,
        T: Debug + PartialEq,
    {
        let mut slice = raw.as_ref();
        let mut cell = types::read_bytes_opt(&mut slice).unwrap();
        let old = T::from_cql(
            cell.as_mut()
                .map(|c| deser_cql_value(typ, c))
                .transpose()
                .unwrap(),
        )
        .unwrap();
        let new = deserialize::<T>(typ, &raw).unwrap();
        assert_eq!(old, new);
    }

    fn compat_check_serialized<T>(typ: &ColumnType, val: &dyn SerializeValue)
    where
        T: for<'f> DeserializeValue<'f>,
        T: FromCqlVal<Option<CqlValue>>,
        T: Debug + PartialEq,
    {
        let raw = serialize(typ, val);
        compat_check::<T>(typ, raw);
    }

    fn deserialize<'frame, T>(
        typ: &'frame ColumnType,
        bytes: &'frame Bytes,
    ) -> Result<T, DeserializationError>
    where
        T: DeserializeValue<'frame>,
    {
        <T as DeserializeValue<'frame>>::type_check(typ)
            .map_err(|typecheck_err| DeserializationError(typecheck_err.0))?;
        let mut frame_slice = FrameSlice::new(bytes);
        let value = frame_slice.read_cql_bytes().map_err(|err| {
            mk_deser_err::<T>(typ, BuiltinDeserializationErrorKind::GenericParseError(err))
        })?;
        <T as DeserializeValue<'frame>>::deserialize(typ, value)
    }

    fn make_bytes(cell: &[u8]) -> Bytes {
        let mut b = BytesMut::new();
        append_bytes(&mut b, cell);
        b.freeze()
    }

    fn serialize(typ: &ColumnType, value: &dyn SerializeValue) -> Bytes {
        let mut bytes = Bytes::new();
        serialize_to_buf(typ, value, &mut bytes);
        bytes
    }

    fn serialize_to_buf(typ: &ColumnType, value: &dyn SerializeValue, buf: &mut Bytes) {
        let mut v = Vec::new();
        let writer = CellWriter::new(&mut v);
        value.serialize(typ, writer).unwrap();
        *buf = v.into();
    }

    fn append_bytes(b: &mut impl BufMut, cell: &[u8]) {
        b.put_i32(cell.len() as i32);
        b.put_slice(cell);
    }
}
