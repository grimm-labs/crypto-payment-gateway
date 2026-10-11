//! Base-unit amounts of the withdrawal tables (`NUMERIC(78,0)`).
//!
//! sqlx's `Decimal` cannot hold them (96-bit mantissa, while wei amounts
//! need more), so they cross the wire as text — `"col"::text` when read,
//! `$n::numeric` when written — and are `u128` in Rust. Never floats.

use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef};
use sqlx::{Decode, Encode, Postgres, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RawAmount(pub u128);

impl From<u128> for RawAmount {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl From<RawAmount> for u128 {
    fn from(value: RawAmount) -> Self {
        value.0
    }
}

impl std::fmt::Display for RawAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Type<Postgres> for RawAmount {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for RawAmount {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let text = <&str as Decode<Postgres>>::decode(value)?;
        Ok(Self(text.parse()?))
    }
}

impl Encode<'_, Postgres> for RawAmount {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Postgres>>::encode_by_ref(&self.0.to_string(), buf)
    }
}
