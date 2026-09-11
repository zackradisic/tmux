//! Wire formats for the per-plugin SQLite database imports (`db_*`).
//!
//! SQLite runs on the host. The guest sends SQL text plus bound
//! parameters and gets back either an exec result (changes, last rowid)
//! or a result set. Every value on the wire is one SQL value: a type
//! byte using SQLite's own fundamental type codes, then the payload. The
//! codes are SQLite's, not the field-block `tag` values, so a host
//! encoder is a direct match on `sqlite3_column_type()`.
//!
//! All integers little-endian, packed. `str` = `u32 len` + UTF-8 bytes,
//! no NUL.
//!
//! ```text
//! value   := u8 ty, payload
//!    1 INTEGER: i64
//!    2 FLOAT:   f64
//!    3 TEXT:    str
//!    4 BLOB:    u32 len, bytes
//!    5 NULL:    (nothing)
//!    6 ZSTD_REF: u32 ptr, u32 len    guest -> host, params only
//!    other ty -> WireError::BadTag
//!    The host emits TEXT that is not valid UTF-8 as BLOB.
//!
//!    ZSTD_REF points at `len` bytes of guest linear memory instead of
//!    carrying them inline. The host reads them in place, compresses
//!    them with zstd on the SQLite worker, and binds the compressed
//!    bytes as a plain BLOB. The buffer must stay valid until the
//!    completion arrives. Rows never carry code 6; `db_decompress`
//!    turns the stored frame back into the original bytes.
//!
//! params  := u16 count, count * value          guest -> host
//!            bound to ?1..?count; a zero-length buffer means count 0
//!            (ptr 0, len 0 is legal on the import)
//! batch   := u16 count, count * { str sql, params }   guest -> host
//!            executed in ONE transaction
//! rows    := u16 ncols, ncols * str name,      host -> guest
//!            u32 nrows, nrows * ncols * value
//!            rectangular; zero rows still carry the column names
//! exec    := i64 changes, i64 last_insert_rowid       16-byte out struct
//! ```

use std::fmt;

use crate::{Cursor, WireError};

/// SQLite fundamental type codes, as they appear in the `ty` byte.
pub mod ty {
    pub const INTEGER: u8 = 1;
    pub const FLOAT: u8 = 2;
    pub const TEXT: u8 = 3;
    pub const BLOB: u8 = 4;
    pub const NULL: u8 = 5;
    /// Not a SQLite type: a reference to guest memory the host compresses
    /// before binding. Legal in params blocks only.
    pub const ZSTD_REF: u8 = 6;
}

/// Size of the `exec` out struct.
pub const EXEC_RESULT_LEN: usize = 16;

/// One SQL value: a bound parameter going in, or a result column coming
/// back.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// `len` bytes at guest address `ptr`, compressed by the host on the
    /// way in and stored as a BLOB. Build one with the SDK's `zstd_ref`.
    /// Never returned in a result set.
    ZstdRef { ptr: u32, len: u32 },
}

impl DbValue {
    pub fn is_null(&self) -> bool {
        matches!(self, DbValue::Null)
    }

    /// The integer, or `None` for any other type. A REAL is not
    /// converted: SQLite's own affinity rules already ran host-side.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            DbValue::Integer(v) => Some(*v),
            _ => None,
        }
    }

    /// The number as f64: REAL as is, INTEGER converted.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            DbValue::Real(v) => Some(*v),
            DbValue::Integer(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            DbValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            DbValue::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// SQLite has no boolean type: an INTEGER is true when nonzero.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            DbValue::Integer(v) => Some(*v != 0),
            _ => None,
        }
    }

    /// The `ty` byte this value travels under.
    pub fn type_code(&self) -> u8 {
        match self {
            DbValue::Null => ty::NULL,
            DbValue::Integer(_) => ty::INTEGER,
            DbValue::Real(_) => ty::FLOAT,
            DbValue::Text(_) => ty::TEXT,
            DbValue::Blob(_) => ty::BLOB,
            DbValue::ZstdRef { .. } => ty::ZSTD_REF,
        }
    }
}

impl fmt::Display for DbValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbValue::Null => write!(f, "NULL"),
            DbValue::Integer(v) => write!(f, "{v}"),
            DbValue::Real(v) => write!(f, "{v}"),
            DbValue::Text(s) => f.write_str(s),
            DbValue::Blob(b) => write!(f, "<blob {} bytes>", b.len()),
            DbValue::ZstdRef { len, .. } => write!(f, "<zstd ref {len} bytes>"),
        }
    }
}

impl From<i64> for DbValue {
    fn from(v: i64) -> Self {
        DbValue::Integer(v)
    }
}

impl From<i32> for DbValue {
    fn from(v: i32) -> Self {
        DbValue::Integer(i64::from(v))
    }
}

impl From<u32> for DbValue {
    fn from(v: u32) -> Self {
        DbValue::Integer(i64::from(v))
    }
}

/// Wraps to the i64 range like SQLite itself would (a u64 above
/// `i64::MAX` becomes negative).
impl From<u64> for DbValue {
    fn from(v: u64) -> Self {
        DbValue::Integer(v as i64)
    }
}

impl From<bool> for DbValue {
    fn from(v: bool) -> Self {
        DbValue::Integer(i64::from(v))
    }
}

impl From<f64> for DbValue {
    fn from(v: f64) -> Self {
        DbValue::Real(v)
    }
}

impl From<&str> for DbValue {
    fn from(v: &str) -> Self {
        DbValue::Text(v.to_string())
    }
}

impl From<String> for DbValue {
    fn from(v: String) -> Self {
        DbValue::Text(v)
    }
}

impl From<&String> for DbValue {
    fn from(v: &String) -> Self {
        DbValue::Text(v.clone())
    }
}

impl From<Vec<u8>> for DbValue {
    fn from(v: Vec<u8>) -> Self {
        DbValue::Blob(v)
    }
}

impl From<&[u8]> for DbValue {
    fn from(v: &[u8]) -> Self {
        DbValue::Blob(v.to_vec())
    }
}

impl<T: Into<DbValue>> From<Option<T>> for DbValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => DbValue::Null,
        }
    }
}

impl From<&DbValue> for DbValue {
    fn from(v: &DbValue) -> Self {
        v.clone()
    }
}

// ---------------------------------------------------------------------------
// value
// ---------------------------------------------------------------------------

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Append one value.
pub fn write_value(out: &mut Vec<u8>, v: &DbValue) {
    out.push(v.type_code());
    match v {
        DbValue::Null => {}
        DbValue::Integer(i) => out.extend_from_slice(&i.to_le_bytes()),
        DbValue::Real(f) => out.extend_from_slice(&f.to_le_bytes()),
        DbValue::Text(s) => write_str(out, s),
        DbValue::Blob(b) => {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        DbValue::ZstdRef { ptr, len } => {
            out.extend_from_slice(&ptr.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
    }
}

/// Read one value.
pub fn read_value(c: &mut Cursor<'_>) -> Result<DbValue, WireError> {
    Ok(match c.u8()? {
        ty::INTEGER => DbValue::Integer(c.i64()?),
        ty::FLOAT => DbValue::Real(c.f64()?),
        ty::TEXT => DbValue::Text(c.str()?.to_string()),
        ty::BLOB => DbValue::Blob(c.bytes()?.to_vec()),
        ty::NULL => DbValue::Null,
        ty::ZSTD_REF => {
            let ptr = c.u32()?;
            let len = c.u32()?;
            DbValue::ZstdRef { ptr, len }
        }
        other => return Err(WireError::BadTag(other)),
    })
}

// ---------------------------------------------------------------------------
// params
// ---------------------------------------------------------------------------

/// Encode bound parameters. An empty slice encodes to an empty buffer,
/// which the host reads as "no parameters" (and allows a script).
pub fn encode_params(params: &[DbValue]) -> Vec<u8> {
    if params.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(2 + params.len() * 9);
    out.extend_from_slice(&(params.len() as u16).to_le_bytes());
    for p in params {
        write_value(&mut out, p);
    }
    out
}

/// Decode a params block. An empty buffer is zero parameters.
pub fn decode_params(buf: &[u8]) -> Result<Vec<DbValue>, WireError> {
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let mut c = Cursor::new(buf);
    let params = read_params(&mut c)?;
    Ok(params)
}

fn read_params(c: &mut Cursor<'_>) -> Result<Vec<DbValue>, WireError> {
    let n = c.u16()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_value(c)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// batch
// ---------------------------------------------------------------------------

/// One statement of a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStmt {
    pub sql: String,
    pub params: Vec<DbValue>,
}

/// Encode a batch block: every statement runs in one transaction.
pub fn encode_batch(stmts: &[BatchStmt]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(stmts.len() as u16).to_le_bytes());
    for s in stmts {
        write_str(&mut out, &s.sql);
        // Inline params: always with the count header, so a statement
        // without parameters is `u16 0`, not an absent block.
        out.extend_from_slice(&(s.params.len() as u16).to_le_bytes());
        for p in &s.params {
            write_value(&mut out, p);
        }
    }
    out
}

/// Decode a batch block.
pub fn decode_batch(buf: &[u8]) -> Result<Vec<BatchStmt>, WireError> {
    let mut c = Cursor::new(buf);
    let n = c.u16()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let sql = c.str()?.to_string();
        let params = read_params(&mut c)?;
        out.push(BatchStmt { sql, params });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

/// Streaming encoder for a result set: the host writes column names
/// first, then values row by row, and `finish` patches the row count.
pub struct RowsWriter {
    buf: Vec<u8>,
    ncols: usize,
    nrows: u32,
    nrows_at: usize,
    /// Values written in the row being built.
    in_row: usize,
}

impl RowsWriter {
    pub fn new(columns: &[&str]) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
        for name in columns {
            write_str(&mut buf, name);
        }
        let nrows_at = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes());
        Self { buf, ncols: columns.len(), nrows: 0, nrows_at, in_row: 0 }
    }

    pub fn value(&mut self, v: &DbValue) {
        write_value(&mut self.buf, v);
        self.in_row += 1;
    }

    pub fn null(&mut self) {
        self.buf.push(ty::NULL);
        self.in_row += 1;
    }

    pub fn integer(&mut self, v: i64) {
        self.buf.push(ty::INTEGER);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self.in_row += 1;
    }

    pub fn real(&mut self, v: f64) {
        self.buf.push(ty::FLOAT);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self.in_row += 1;
    }

    pub fn text(&mut self, v: &str) {
        self.buf.push(ty::TEXT);
        write_str(&mut self.buf, v);
        self.in_row += 1;
    }

    pub fn blob(&mut self, v: &[u8]) {
        self.buf.push(ty::BLOB);
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v);
        self.in_row += 1;
    }

    /// Close the current row. Panics in debug builds if the row is not
    /// rectangular; the host encoder drives this from SQLite's column
    /// count, so a mismatch is a host bug.
    pub fn end_row(&mut self) {
        debug_assert_eq!(self.in_row, self.ncols, "row is not rectangular");
        self.in_row = 0;
        self.nrows += 1;
    }

    /// Bytes written so far (for size caps).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn nrows(&self) -> u32 {
        self.nrows
    }

    pub fn ncols(&self) -> usize {
        self.ncols
    }

    pub fn finish(mut self) -> Vec<u8> {
        let at = self.nrows_at;
        self.buf[at..at + 4].copy_from_slice(&self.nrows.to_le_bytes());
        self.buf
    }
}

/// A decoded result set: column names plus values, row-major, flat.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub values: Vec<DbValue>,
}

impl Rows {
    pub fn decode(buf: &[u8]) -> Result<Rows, WireError> {
        let mut c = Cursor::new(buf);
        let ncols = c.u16()? as usize;
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            columns.push(c.str()?.to_string());
        }
        let nrows = c.u32()? as usize;
        let total = nrows.checked_mul(ncols).ok_or(WireError::Truncated)?;
        // Cap the preallocation by what the buffer can physically hold
        // (one byte per value at least), so a corrupt count cannot ask
        // for gigabytes before the first read fails.
        let mut values = Vec::with_capacity(total.min(c.remaining()));
        for _ in 0..total {
            values.push(read_value(&mut c)?);
        }
        Ok(Rows { columns, values })
    }

    pub fn encode(&self) -> Vec<u8> {
        let names: Vec<&str> = self.columns.iter().map(String::as_str).collect();
        let mut w = RowsWriter::new(&names);
        if !self.columns.is_empty() {
            for row in self.values.chunks(self.columns.len()) {
                for v in row {
                    w.value(v);
                }
                w.end_row();
            }
        }
        w.finish()
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        if self.columns.is_empty() {
            0
        } else {
            self.values.len() / self.columns.len()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ncols(&self) -> usize {
        self.columns.len()
    }

    /// Index of a column by name.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    pub fn row(&self, i: usize) -> Option<Row<'_>> {
        if i >= self.len() {
            return None;
        }
        Some(Row { rows: self, index: i })
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        (0..self.len()).map(move |i| Row { rows: self, index: i })
    }

    /// The value at (row, column).
    pub fn get(&self, row: usize, col: usize) -> Option<&DbValue> {
        if col >= self.ncols() {
            return None;
        }
        self.values.get(row * self.ncols() + col)
    }

    /// The single value of a one-row, one-column result (`SELECT
    /// count(*)`), or `None` if the shape differs.
    pub fn scalar(&self) -> Option<&DbValue> {
        if self.len() == 1 && self.ncols() == 1 {
            self.values.first()
        } else {
            None
        }
    }
}

/// One row of a [`Rows`], borrowing it.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    rows: &'a Rows,
    index: usize,
}

impl<'a> Row<'a> {
    pub fn get(&self, col: usize) -> Option<&'a DbValue> {
        self.rows.get(self.index, col)
    }

    pub fn get_named(&self, name: &str) -> Option<&'a DbValue> {
        self.rows.column(name).and_then(|c| self.get(c))
    }

    /// Every value of the row, in column order.
    pub fn values(&self) -> &'a [DbValue] {
        let n = self.rows.ncols();
        &self.rows.values[self.index * n..(self.index + 1) * n]
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

/// Result of a statement that returns no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecResult {
    /// Rows changed by the statement (the sum over a script or batch).
    pub changes: i64,
    /// `last_insert_rowid()` after the (last) statement.
    pub last_insert_rowid: i64,
}

impl ExecResult {
    pub fn to_bytes(self) -> [u8; EXEC_RESULT_LEN] {
        let mut out = [0u8; EXEC_RESULT_LEN];
        out[0..8].copy_from_slice(&self.changes.to_le_bytes());
        out[8..16].copy_from_slice(&self.last_insert_rowid.to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, WireError> {
        let mut c = Cursor::new(buf);
        Ok(Self { changes: c.i64()?, last_insert_rowid: c.i64()? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_types() -> Vec<DbValue> {
        vec![
            DbValue::Integer(-42),
            DbValue::Real(2.5),
            DbValue::Text("héllo".into()),
            DbValue::Blob(vec![0, 1, 2, 255]),
            DbValue::Null,
            DbValue::ZstdRef { ptr: 4096, len: 70_000 },
        ]
    }

    #[test]
    fn params_round_trip() {
        let p = all_types();
        let buf = encode_params(&p);
        assert_eq!(buf[0..2], 6u16.to_le_bytes());
        assert_eq!(decode_params(&buf).unwrap(), p);
        assert!(encode_params(&[]).is_empty());
        assert_eq!(decode_params(&[]).unwrap(), Vec::<DbValue>::new());
    }

    #[test]
    fn params_truncation_is_an_error() {
        let buf = encode_params(&all_types());
        // Cut 0 is the empty buffer, which legally means "no params".
        for cut in 1..buf.len() {
            assert!(decode_params(&buf[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn bad_type_code_is_rejected() {
        for bad in [0u8, 7, 8, 255] {
            let mut buf = 1u16.to_le_bytes().to_vec();
            buf.push(bad);
            assert_eq!(decode_params(&buf), Err(WireError::BadTag(bad)));
        }
    }

    #[test]
    fn zstd_ref_is_nine_bytes() {
        let buf = encode_params(&[DbValue::ZstdRef { ptr: 0x1000, len: 3 }]);
        assert_eq!(buf.len(), 2 + 1 + 8);
        assert_eq!(buf[2], ty::ZSTD_REF);
        assert_eq!(&buf[3..7], &0x1000u32.to_le_bytes());
        assert_eq!(&buf[7..11], &3u32.to_le_bytes());
    }

    #[test]
    fn batch_round_trip() {
        let stmts = vec![
            BatchStmt { sql: "INSERT INTO t VALUES (?1)".into(), params: all_types() },
            BatchStmt { sql: "DELETE FROM t".into(), params: vec![] },
        ];
        let buf = encode_batch(&stmts);
        assert_eq!(decode_batch(&buf).unwrap(), stmts);
        for cut in 0..buf.len() {
            assert!(decode_batch(&buf[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn rows_round_trip() {
        let mut w = RowsWriter::new(&["id", "name", "score", "raw", "gone"]);
        w.integer(1);
        w.text("a");
        w.real(0.5);
        w.blob(b"xy");
        w.null();
        w.end_row();
        w.value(&DbValue::Integer(2));
        w.text("");
        w.real(-1.0);
        w.blob(b"");
        w.null();
        w.end_row();
        assert_eq!(w.nrows(), 2);
        let buf = w.finish();

        let rows = Rows::decode(&buf).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.ncols(), 5);
        assert_eq!(rows.columns[1], "name");
        assert_eq!(rows.get(0, 0), Some(&DbValue::Integer(1)));
        assert_eq!(rows.get(1, 1), Some(&DbValue::Text(String::new())));
        assert_eq!(rows.get(1, 3), Some(&DbValue::Blob(Vec::new())));
        assert_eq!(rows.get(0, 4), Some(&DbValue::Null));
        assert_eq!(rows.get(0, 5), None);
        assert_eq!(rows.get(2, 0), None);
        let r = rows.row(1).unwrap();
        assert_eq!(r.get_named("score"), Some(&DbValue::Real(-1.0)));
        assert_eq!(r.values().len(), 5);
        assert_eq!(rows.iter().count(), 2);
        assert!(rows.scalar().is_none());
        assert_eq!(rows.encode(), buf);

        for cut in 0..buf.len() {
            assert!(Rows::decode(&buf[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn zero_rows_keep_column_names() {
        let buf = RowsWriter::new(&["n"]).finish();
        let rows = Rows::decode(&buf).unwrap();
        assert_eq!(rows.columns, vec!["n".to_string()]);
        assert!(rows.is_empty());
        assert_eq!(rows.iter().count(), 0);
        assert!(rows.row(0).is_none());
    }

    #[test]
    fn scalar_shape() {
        let mut w = RowsWriter::new(&["count(*)"]);
        w.integer(7);
        w.end_row();
        let rows = Rows::decode(&w.finish()).unwrap();
        assert_eq!(rows.scalar().and_then(DbValue::as_i64), Some(7));
    }

    #[test]
    fn exec_result_round_trip() {
        let r = ExecResult { changes: 3, last_insert_rowid: -9 };
        assert_eq!(ExecResult::from_bytes(&r.to_bytes()), Ok(r));
        assert!(ExecResult::from_bytes(&r.to_bytes()[..15]).is_err());
    }

    #[test]
    fn conversions() {
        assert_eq!(DbValue::from(true), DbValue::Integer(1));
        assert_eq!(DbValue::from(3u32), DbValue::Integer(3));
        assert_eq!(DbValue::from(None::<i64>), DbValue::Null);
        assert_eq!(DbValue::from(Some("x")), DbValue::Text("x".into()));
        assert_eq!(DbValue::from(&b"ab"[..]), DbValue::Blob(vec![b'a', b'b']));
        assert_eq!(DbValue::Integer(2).as_f64(), Some(2.0));
        assert_eq!(DbValue::Integer(0).as_bool(), Some(false));
        assert_eq!(DbValue::Text("t".into()).as_i64(), None);
    }
}
