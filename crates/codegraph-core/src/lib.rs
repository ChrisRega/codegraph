//! Shared adapter between the codegraph toolchain and the velr graph database.
//!
//! velr (alpha) does not yet support `$param` placeholders, so this crate
//! provides a Cypher value escaper. It also wraps row results into owned
//! `Cell` values so callers don't have to deal with the borrowed `CellRef`
//! lifetimes.

pub use velr;

use std::collections::BTreeMap;

pub type VelrError = velr::Error;
pub type Result<T> = std::result::Result<T, VelrError>;

/// Owned graph database handle.
pub struct Db {
    inner: velr::Velr,
}

impl Db {
    /// Open or create a velr database at `path` (creates the file if absent).
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            inner: velr::Velr::open(Some(path))?,
        })
    }

    /// In-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            inner: velr::Velr::open(None)?,
        })
    }

    /// Borrow the raw velr handle for advanced operations (transactions, explain, ...).
    pub fn velr(&self) -> &velr::Velr {
        &self.inner
    }

    /// Run a write query and discard any result tables.
    pub fn run(&self, cypher: &str) -> Result<()> {
        self.inner.run(cypher)
    }

    /// Execute a query and collect every row into owned `Cell` values.
    pub fn query(&self, cypher: &str) -> Result<Table> {
        let mut table = self.inner.exec_one(cypher)?;
        let columns: Vec<String> = table.column_names().to_vec();
        let rows =
            table.collect::<Vec<Cell>, _>(|row| Ok(row.iter().map(Cell::from_ref).collect()))?;
        Ok(Table { columns, rows })
    }

    /// Execute a query that may produce multiple result tables (e.g. semicolon-
    /// separated statements, `EXPLAIN`). Each table is materialised into
    /// owned `Cell` values.
    pub fn query_many(&self, cypher: &str) -> Result<Vec<Table>> {
        let mut stream = self.inner.exec(cypher)?;
        let mut out = Vec::new();
        while let Some(mut tr) = stream.next_table()? {
            let columns: Vec<String> = tr.column_names().to_vec();
            let rows =
                tr.collect::<Vec<Cell>, _>(|row| Ok(row.iter().map(Cell::from_ref).collect()))?;
            out.push(Table { columns, rows });
        }
        Ok(out)
    }
}

/// Owned, materialized result table.
#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    /// Returns the cell at `row[col_name]`, or `None` if either is missing.
    pub fn cell(&self, row: usize, col_name: &str) -> Option<&Cell> {
        let c = self.col(col_name)?;
        self.rows.get(row)?.get(c)
    }

    /// Convenience: collect every value of one column as `&str`, skipping non-text/null cells.
    pub fn column_strings(&self, name: &str) -> Vec<String> {
        let Some(idx) = self.col(name) else {
            return vec![];
        };
        self.rows
            .iter()
            .filter_map(|r| r.get(idx).and_then(|c| c.as_str().map(|s| s.to_string())))
            .collect()
    }
}

/// Owned cell value mirroring `velr::CellRef`.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Json(String),
}

impl Cell {
    pub fn from_ref(c: &velr::CellRef<'_>) -> Self {
        match c {
            velr::CellRef::Null => Cell::Null,
            velr::CellRef::Bool(b) => Cell::Bool(*b),
            velr::CellRef::Integer(i) => Cell::Integer(*i),
            velr::CellRef::Float(f) => Cell::Float(*f),
            velr::CellRef::Text(b) => Cell::Text(String::from_utf8_lossy(b).into_owned()),
            velr::CellRef::Json(b) => Cell::Json(String::from_utf8_lossy(b).into_owned()),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Cell::Text(s) | Cell::Json(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Cell::Float(f) => Some(*f),
            Cell::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Cell::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }
}

/// Logical Cypher value used for parameter substitution.
///
/// velr 0.2 has no `$param` support, so we render every value into a Cypher
/// literal via [`escape`] and inline it into the query string.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<usize> for Value {
    fn from(i: usize) -> Self {
        Value::Int(i as i64)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(xs: Vec<T>) -> Self {
        Value::List(xs.into_iter().map(Into::into).collect())
    }
}

/// Escape a value into a Cypher literal that can be inlined into a query string.
pub fn escape(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.is_finite() {
                format!("{f}")
            } else {
                "null".to_string()
            }
        }
        Value::Str(s) => escape_str(s),
        Value::List(xs) => {
            let parts: Vec<String> = xs.iter().map(escape).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Map(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}: {}", escape_ident(k), escape(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Escape a Rust string as a Cypher single-quoted string literal.
pub fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Escape a Cypher identifier (property key, label). Backtick-quotes when needed.
pub fn escape_ident(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if safe {
        s.to_string()
    } else {
        format!("`{}`", s.replace('`', "``"))
    }
}

/// Build a Cypher inline property map: `{k1: v1, k2: v2}`. Useful inside CREATE/MERGE.
pub fn props(items: &[(&str, &Value)]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|(k, v)| format!("{}: {}", escape_ident(k), escape(v)))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_str_basic() {
        assert_eq!(escape_str("hi"), "'hi'");
        assert_eq!(escape_str("it's"), "'it\\'s'");
        assert_eq!(escape_str("a\\b"), "'a\\\\b'");
        assert_eq!(escape_str("a\nb"), "'a\\nb'");
    }

    #[test]
    fn escape_value_variants() {
        assert_eq!(escape(&Value::Null), "null");
        assert_eq!(escape(&Value::Bool(true)), "true");
        assert_eq!(escape(&Value::Int(42)), "42");
        assert_eq!(escape(&Value::Str("x".into())), "'x'");
        assert_eq!(
            escape(&Value::List(vec![Value::Int(1), Value::Str("a".into())])),
            "[1, 'a']"
        );
    }

    #[test]
    fn escape_ident_quotes_when_needed() {
        assert_eq!(escape_ident("name"), "name");
        assert_eq!(escape_ident("first-name"), "`first-name`");
        assert_eq!(escape_ident("a`b"), "`a``b`");
    }
}
