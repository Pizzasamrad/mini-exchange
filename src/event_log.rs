//! Line-oriented event log for deterministic replay.
//!
//! The file is CSV with a fixed header. A reader pulls one event at a time
//! from any [`BufRead`], and a writer appends to any [`Write`], so a
//! multi-million-event log never has to sit in memory as a `Vec`.
//!
//! ```text
//! # mini-exchange event log v1
//! seq,market,cmd,id,side,price,qty
//! 1,DEFAULT,limit,1,buy,10000,25
//! 2,YES,market,2,sell,,3
//! 3,NO,cancel,1,,,
//! ```
//!
//! `seq` is the logical timestamp. It is strictly increasing and is copied
//! onto limit and market commands. `market` is a symbol such as `DEFAULT`,
//! `YES`, or `NO`; an empty field means [`MarketId::default_market`]. Limit
//! rows carry a side, a tick price, and a quantity. Market rows leave the
//! price empty. Cancel rows carry only the order id.

use std::borrow::Cow;
use std::fmt;
use std::io::{BufRead, Write};
use std::str::FromStr;

use crate::types::{Command, OrderId, Price, Qty, Side, Timestamp};

/// First line of every file written by [`EventWriter`].
pub const VERSION_COMMENT: &str = "# mini-exchange event log v1";

/// Required header. The reader skips blank lines and `#` comments, then
/// demands this row before any event.
pub const HEADER: &str = "seq,market,cmd,id,side,price,qty";

/// Lines longer than this are rejected. Real rows are a few dozen bytes.
const MAX_LINE_LEN: usize = 4096;

/// Maximum length of a market symbol, in bytes.
const MAX_MARKET_LEN: usize = 32;

/// Symbol that names which book an event belongs to.
///
/// Single-book streams use [`MarketId::default_market`]. A binary contract
/// uses [`MarketId::yes`] and [`MarketId::no`]. Symbols are ASCII, so the
/// column needs no quoting.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarketId {
    inner: Cow<'static, str>,
}

impl MarketId {
    /// The symbol written for a single-book stream.
    pub const DEFAULT_NAME: &'static str = "DEFAULT";

    /// Single-book symbol `DEFAULT`.
    pub fn default_market() -> Self {
        Self::from_static(Self::DEFAULT_NAME)
    }

    /// Binary-contract leg `YES`.
    pub fn yes() -> Self {
        Self::from_static("YES")
    }

    /// Binary-contract leg `NO`.
    pub fn no() -> Self {
        Self::from_static("NO")
    }

    /// Parse a symbol. Empty input is rejected here; the CSV reader treats an
    /// empty market column as [`MarketId::default_market`].
    pub fn parse(name: &str) -> Result<Self, MarketIdError> {
        validate_market(name)?;
        Ok(Self {
            inner: Cow::Owned(name.to_string()),
        })
    }

    /// Borrowed symbol. `name` must already be a valid market id.
    fn from_static(name: &'static str) -> Self {
        validate_market(name).expect("static market id is valid");
        Self {
            inner: Cow::Borrowed(name),
        }
    }

    /// Symbol text.
    pub fn as_str(&self) -> &str {
        &self.inner
    }
}

fn validate_market(name: &str) -> Result<(), MarketIdError> {
    if name.is_empty() {
        return Err(MarketIdError::new("market id is empty"));
    }
    if name.len() > MAX_MARKET_LEN {
        return Err(MarketIdError::new("market id is longer than 32 bytes"));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(MarketIdError::new("market id is empty"));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(MarketIdError::new(
            "market id must start with an ASCII letter or digit",
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        return Err(MarketIdError::new(
            "market id must be ASCII letters, digits, '_' or '-'",
        ));
    }
    Ok(())
}

impl fmt::Debug for MarketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MarketId({})", self.as_str())
    }
}

impl fmt::Display for MarketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl AsRef<str> for MarketId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A market symbol failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketIdError {
    message: String,
}

impl MarketIdError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MarketIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MarketIdError {}

/// One recorded command.
///
/// `seq` is both the file order and the logical timestamp stored on limit and
/// market commands. [`EventWriter`] rejects an event whose command timestamp
/// disagrees with `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Strictly increasing logical timestamp.
    pub seq: u64,
    /// Book this command applies to.
    pub market: MarketId,
    /// Limit, market, or cancel command.
    pub command: Command,
}

impl Event {
    /// Event whose limit timestamp is `seq`.
    pub fn limit(
        seq: u64,
        market: MarketId,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    ) -> Self {
        Self {
            seq,
            market,
            command: Command::limit(id, side, price, qty, Timestamp(seq)),
        }
    }

    /// Event whose market-order timestamp is `seq`.
    pub fn market(seq: u64, market: MarketId, id: OrderId, side: Side, qty: Qty) -> Self {
        Self {
            seq,
            market,
            command: Command::market(id, side, qty, Timestamp(seq)),
        }
    }

    /// Cancel event. Cancels do not carry a timestamp in [`Command`].
    pub fn cancel(seq: u64, market: MarketId, id: OrderId) -> Self {
        Self {
            seq,
            market,
            command: Command::cancel(id),
        }
    }
}

/// Why one line could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    line: u64,
    message: String,
}

impl ParseError {
    fn new(line: u64, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    /// 1-based line number in the file, counting comments and blank lines.
    pub fn line(&self) -> u64 {
        self.line
    }

    /// What was wrong on that line, without the line-number prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "event log line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// A failure while streaming a log: I/O, or a line that failed validation.
#[derive(Debug)]
pub enum ReadError {
    /// The underlying reader failed.
    Io(std::io::Error),
    /// A line was missing, malformed, or out of order.
    Parse(ParseError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "event log read failed: {err}"),
            Self::Parse(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Parse(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for ReadError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Streaming reader. Each [`Iterator`] item is one event or a line-numbered
/// error. After an error, the iterator ends.
pub struct EventReader<R> {
    reader: R,
    buf: String,
    line_no: u64,
    prev_seq: Option<u64>,
    header_seen: bool,
    done: bool,
}

impl<R: BufRead> EventReader<R> {
    /// Read `reader` from the start. The header is checked on the first event.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: String::new(),
            line_no: 0,
            prev_seq: None,
            header_seen: false,
            done: false,
        }
    }

    fn pull(&mut self) -> Result<Option<Event>, ReadError> {
        loop {
            self.buf.clear();
            let read = self.reader.read_line(&mut self.buf)?;
            if read == 0 {
                if !self.header_seen {
                    return Err(ReadError::Parse(ParseError::new(
                        self.line_no.max(1),
                        format!("missing header `{HEADER}`"),
                    )));
                }
                return Ok(None);
            }
            self.line_no += 1;
            if self.buf.len() > MAX_LINE_LEN {
                return Err(ReadError::Parse(ParseError::new(
                    self.line_no,
                    format!("line exceeds {MAX_LINE_LEN} bytes"),
                )));
            }
            let line = self.buf.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !self.header_seen {
                if line != HEADER {
                    return Err(ReadError::Parse(ParseError::new(
                        self.line_no,
                        format!("expected header `{HEADER}`, found `{line}`"),
                    )));
                }
                self.header_seen = true;
                continue;
            }
            let event = parse_record(self.line_no, line, self.prev_seq)?;
            self.prev_seq = Some(event.seq);
            return Ok(Some(event));
        }
    }
}

impl<R: BufRead> Iterator for EventReader<R> {
    type Item = Result<Event, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.pull() {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(err) => {
                self.done = true;
                Some(Err(err))
            }
        }
    }
}

fn parse_record(line_no: u64, line: &str, prev_seq: Option<u64>) -> Result<Event, ReadError> {
    let fields = split_fields(line).map_err(|count| {
        ReadError::Parse(ParseError::new(
            line_no,
            format!("expected 7 fields, found {count}"),
        ))
    })?;
    let [seq_field, market_field, cmd, id_field, side_field, price_field, qty_field] = fields;
    let seq = parse_u64(line_no, seq_field, "sequence")?;
    if let Some(prev) = prev_seq {
        if seq <= prev {
            return Err(ReadError::Parse(ParseError::new(
                line_no,
                format!("sequence {seq} is not greater than previous sequence {prev}"),
            )));
        }
    }
    let market = if market_field.is_empty() {
        MarketId::default_market()
    } else {
        MarketId::parse(market_field).map_err(|err| {
            ReadError::Parse(ParseError::new(
                line_no,
                format!("invalid market id `{market_field}`: {err}"),
            ))
        })?
    };
    let id = OrderId(parse_u64(line_no, id_field, "order id")?);
    let command = match cmd {
        "limit" => {
            let side = parse_side(line_no, side_field)?;
            let price = Price(parse_u64(line_no, price_field, "price")?);
            let qty = Qty(parse_u64(line_no, qty_field, "quantity")?);
            Command::limit(id, side, price, qty, Timestamp(seq))
        }
        "market" => {
            if !price_field.is_empty() {
                return Err(ReadError::Parse(ParseError::new(
                    line_no,
                    "market order price must be empty",
                )));
            }
            let side = parse_side(line_no, side_field)?;
            let qty = Qty(parse_u64(line_no, qty_field, "quantity")?);
            Command::market(id, side, qty, Timestamp(seq))
        }
        "cancel" => {
            if !side_field.is_empty() || !price_field.is_empty() || !qty_field.is_empty() {
                return Err(ReadError::Parse(ParseError::new(
                    line_no,
                    "cancel must leave side, price, and quantity empty",
                )));
            }
            Command::cancel(id)
        }
        other => {
            return Err(ReadError::Parse(ParseError::new(
                line_no,
                format!("unknown command `{other}`"),
            )));
        }
    };
    Ok(Event {
        seq,
        market,
        command,
    })
}

fn split_fields(line: &str) -> Result<[&str; 7], usize> {
    let mut out = [""; 7];
    let mut count = 0usize;
    for field in line.split(',') {
        if count < out.len() {
            out[count] = field.trim();
        }
        count += 1;
    }
    if count == out.len() {
        Ok(out)
    } else {
        Err(count)
    }
}

fn parse_u64(line_no: u64, field: &str, name: &str) -> Result<u64, ReadError> {
    if field.is_empty() {
        return Err(ReadError::Parse(ParseError::new(
            line_no,
            format!("missing {name}"),
        )));
    }
    u64::from_str(field).map_err(|_| {
        ReadError::Parse(ParseError::new(
            line_no,
            format!("invalid {name} `{field}`"),
        ))
    })
}

fn parse_side(line_no: u64, field: &str) -> Result<Side, ReadError> {
    match field {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        "" => Err(ReadError::Parse(ParseError::new(line_no, "missing side"))),
        other => Err(ReadError::Parse(ParseError::new(
            line_no,
            format!("unknown side `{other}`"),
        ))),
    }
}

/// Streaming writer. [`EventWriter::create`] emits the version comment and
/// the header before any event.
pub struct EventWriter<W> {
    inner: W,
    events: u64,
}

impl<W: Write> EventWriter<W> {
    /// Write the version comment, each extra comment, and the header.
    ///
    /// A comment that already starts with `#` is not prefixed twice. Comments
    /// may not contain newlines.
    pub fn create<I, S>(inner: W, comments: I) -> std::io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut writer = Self { inner, events: 0 };
        writeln!(writer.inner, "{VERSION_COMMENT}")?;
        for comment in comments {
            write_comment(&mut writer.inner, comment.as_ref())?;
        }
        writeln!(writer.inner, "{HEADER}")?;
        Ok(writer)
    }

    /// Append one event.
    pub fn write_event(&mut self, event: &Event) -> std::io::Result<()> {
        check_timestamp(event)?;
        match event.command {
            Command::Limit {
                id,
                side,
                price,
                qty,
                ..
            } => {
                writeln!(
                    self.inner,
                    "{seq},{market},limit,{id},{side},{price},{qty}",
                    seq = event.seq,
                    market = event.market,
                )?;
            }
            Command::Market { id, side, qty, .. } => {
                writeln!(
                    self.inner,
                    "{seq},{market},market,{id},{side},,{qty}",
                    seq = event.seq,
                    market = event.market,
                )?;
            }
            Command::Cancel { id } => {
                writeln!(
                    self.inner,
                    "{seq},{market},cancel,{id},,,",
                    seq = event.seq,
                    market = event.market,
                )?;
            }
        }
        self.events += 1;
        Ok(())
    }

    /// Append every event. Returns how many were written.
    pub fn write_events<I>(&mut self, events: I) -> std::io::Result<u64>
    where
        I: IntoIterator<Item = Event>,
    {
        let mut written = 0u64;
        for event in events {
            self.write_event(&event)?;
            written += 1;
        }
        Ok(written)
    }

    /// Events successfully written.
    pub fn events_written(&self) -> u64 {
        self.events
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }

    /// Flush and return the underlying writer.
    pub fn into_inner(mut self) -> std::io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

fn write_comment<W: Write>(writer: &mut W, comment: &str) -> std::io::Result<()> {
    if comment.contains('\n') || comment.contains('\r') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "event log comment contains a newline",
        ));
    }
    if let Some(stripped) = comment.strip_prefix("# ") {
        writeln!(writer, "# {stripped}")?;
    } else if let Some(stripped) = comment.strip_prefix('#') {
        writeln!(writer, "# {}", stripped.trim_start())?;
    } else {
        writeln!(writer, "# {comment}")?;
    }
    Ok(())
}

fn check_timestamp(event: &Event) -> std::io::Result<()> {
    let ts = match event.command {
        Command::Limit { ts, .. } | Command::Market { ts, .. } => ts.get(),
        Command::Cancel { .. } => return Ok(()),
    };
    if ts != event.seq {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "event sequence {} and command timestamp {ts} differ",
                event.seq
            ),
        ));
    }
    Ok(())
}
