//! Synchronous SQL on the object's database.
//!
//! A Durable Object's SQLite API (`ctx.storage.sql.exec`) is synchronous, so apps call it
//! from synchronous code and never hold it across an `.await`. The Durable Object host
//! implements [`Sql`] over `sql.exec`; the native harness implements it over `rusqlite`.

use std::fmt;

/// A SQLite value, as a parameter or a column.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `NULL`.
    Null,
    /// An integer. A Durable Object returns integers as JavaScript numbers, so values beyond
    /// ±2^53 lose precision there.
    Integer(i64),
    /// A floating-point number.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Bytes.
    Blob(Vec<u8>),
}

impl Value {
    /// Returns the value as an integer: integers as is, reals truncated, anything else `None`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            Value::Real(f) => Some(*f as i64),
            _ => None,
        }
    }

    /// Returns the value as a non-negative integer; `0` if it is not one.
    pub fn as_u64(&self) -> u64 {
        self.as_i64()
            .and_then(|i| u64::try_from(i).ok())
            .unwrap_or(0)
    }

    /// Returns the value as text, `None` if it is not text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the value as owned text; empty if it is not text.
    pub fn into_string(self) -> String {
        match self {
            Value::Text(s) => s,
            _ => String::new(),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::Integer(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Value::Integer(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Integer(i64::from(v))
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

/// One result row, columns in `SELECT` order.
pub type Row = Vec<Value>;

/// A SQL error, with the database's message.
#[derive(Debug, Clone)]
pub struct SqlError(pub String);

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SqlError {}

/// Synchronous access to the object's SQLite database.
pub trait Sql {
    /// Runs one statement with positional parameters (`?` or `?N`) and returns its rows.
    fn exec(&self, query: &str, params: &[Value]) -> Result<Vec<Row>, SqlError>;

    /// Runs `query` and returns the first column of its first row, or `None` if there is no
    /// row.
    fn value(&self, query: &str, params: &[Value]) -> Result<Option<Value>, SqlError> {
        Ok(self
            .exec(query, params)?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next()))
    }
}
