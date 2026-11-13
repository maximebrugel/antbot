//! CSV journals in the data dir:
//!
//! * `signals.csv`: one premium sample every `timing.signal_sample_secs`
//!   (oracle, mark, impact bid / ask, mids, premium, strategy state);
//! * `orders.csv`: every order command sent to the executor and every
//!   order feedback (resting, fill, terminal state);
//! * `trades.csv`: one row per closed round trip.
//!
//! The header is written once when a file is created; a restart appends
//! to the existing file. A file whose header differs (older column layout)
//! is moved aside to `<name>.<unix_ms>.old` and a new one is started, so
//! columns never get mixed. A row left half-written by a crash is
//! terminated before the next row is appended; the scan is quote-aware,
//! so a row cut inside a quoted field (even right after an embedded
//! newline) gets its quote closed first and cannot swallow later rows.
//!
//! Write errors never stop the bot: they are logged (throttled) and the
//! row is lost. After a failed (possibly partial) write, the next row
//! first terminates a half-written line, so two rows are never glued.
//!
//! Writes are synchronous on the main loop (rows are rare and small; the
//! state save blocks the same way) and go straight to the OS with no
//! user-space buffer, but are not fsynced: rows survive a process crash,
//! not necessarily a power loss.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::hl::market::MarketState;
use crate::monitor::fmt_utc_ms;
use crate::strategy::signal::{Command, DoneReason, OrderIntent, OrderUpdate, TradeRecord};

pub const SIGNALS_FILE: &str = "signals.csv";
pub const ORDERS_FILE: &str = "orders.csv";
pub const TRADES_FILE: &str = "trades.csv";

/// Remembered intents (for side / kind on later order events) are bounded.
const MAX_TRACKED_INTENTS: usize = 256;

/// Quote a CSV field if needed (RFC 4180).
pub fn escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Join fields into one CSV line (no trailing newline).
pub fn line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| escape(f))
        .collect::<Vec<_>>()
        .join(",")
}

/// Price or size without binary float noise: rounded to 8 decimals with
/// trailing zeros trimmed, so a VWAP of fills at 111.93 prints as `111.93`,
/// not `111.93000000000002`. Empty when not finite.
fn num(v: f64) -> String {
    if !v.is_finite() {
        return String::new();
    }
    let s = format!("{v:.8}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Optional float, empty when unknown.
fn opt(v: Option<f64>) -> String {
    v.map_or_else(String::new, num)
}

/// Float rounded to `dp` decimals.
fn fixed(v: f64, dp: usize) -> String {
    if v.is_finite() {
        format!("{v:.dp$}")
    } else {
        String::new()
    }
}

fn opt_fixed(v: Option<f64>, dp: usize) -> String {
    v.map_or_else(String::new, |x| fixed(x, dp))
}

/// Append-only CSV file with a fixed header.
#[derive(Debug)]
pub struct CsvAppender {
    path: PathBuf,
    file: File,
    errors: u64,
    /// The last write failed: check the line ending before the next row.
    check_line_end: bool,
}

impl CsvAppender {
    /// Open (or create) `path`, writing `header` if the file is new or
    /// empty. An existing file with another header is moved aside.
    pub fn open(path: &Path, header: &str) -> std::io::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        if let Some(first) = first_line(path)? {
            if first.trim_end() != header {
                let aside = path.with_file_name(format!(
                    "{}.{}.old",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("journal"),
                    crate::hl::market::now_ms()
                ));
                std::fs::rename(path, &aside)?;
                tracing::warn!(
                    file = %path.display(),
                    moved_to = %aside.display(),
                    "CSV header changed: previous file moved aside"
                );
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            writeln!(file, "{header}")?;
        } else {
            // A row cut by a crash: terminate it so the next row is clean.
            file.write_all(repair_suffix(path)?.as_bytes())?;
        }
        file.flush()?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            errors: 0,
            check_line_end: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one row (unbuffered: it reaches the OS right away and
    /// survives a process crash).
    pub fn append(&mut self, fields: &[String]) {
        let mut s = String::new();
        if self.check_line_end {
            // A previous write may have stopped mid-row (e.g. ENOSPC).
            match repair_suffix(&self.path) {
                Ok(suffix) => s.push_str(suffix),
                // Unknown: an empty line is harmless, a glued row is not.
                Err(_) => s.push('\n'),
            }
        }
        s.push_str(&line(fields));
        s.push('\n');
        let res = self
            .file
            .write_all(s.as_bytes())
            .and_then(|_| self.file.flush());
        self.check_line_end = res.is_err();
        if let Err(e) = res {
            self.errors += 1;
            if self.errors == 1 || self.errors % 100 == 0 {
                tracing::warn!(
                    file = %self.path.display(),
                    error = %e,
                    errors = self.errors,
                    "CSV write failed (row lost)"
                );
            }
        }
    }
}

fn first_line(path: &Path) -> std::io::Result<Option<String>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut l = String::new();
    BufReader::new(f).read_line(&mut l)?;
    Ok((!l.is_empty()).then_some(l))
}

/// What to append so the file ends on a complete record: `""` (empty file
/// or clean end), `"\n"` (a row cut outside quotes) or `"\"\n"` (a row
/// cut inside a quoted field: close the quote, then end the row).
///
/// Quote-aware scan of the whole file (RFC 4180: inside a quoted field an
/// escaped quote is `""`, so the parity of `"` tells whether the end is
/// inside quotes). Journals are small (a row every few seconds at most)
/// and this only runs at open and after a failed write.
fn repair_suffix(path: &Path) -> std::io::Result<&'static str> {
    let mut f = File::open(path)?;
    let mut in_quote = false;
    let mut last = None;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        in_quote ^= buf[..n].iter().filter(|&&b| b == b'"').count() % 2 == 1;
        last = Some(buf[n - 1]);
    }
    Ok(match last {
        _ if in_quote => "\"\n",
        None | Some(b'\n') => "",
        Some(_) => "\n",
    })
}

/// One `signals.csv` row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SignalRow {
    pub ts_ms: u64,
    pub oracle_px: Option<f64>,
    pub mark_px: Option<f64>,
    pub impact_bid_px: Option<f64>,
    pub impact_ask_px: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// Premium used by the strategy (rolling mean when averaging is on).
    pub premium_bps: Option<f64>,
    /// Instantaneous premium of the configured source.
    pub raw_premium_bps: Option<f64>,
    /// Exchange-published premium (ctx).
    pub api_premium_bps: Option<f64>,
    pub avg_ready: bool,
    pub stale: bool,
    pub state: String,
    pub position: f64,
    pub killed: bool,
}

impl SignalRow {
    pub const HEADER: &'static str = "ts_ms,ts_utc,oracle_px,mark_px,impact_bid_px,impact_ask_px,impact_mid_px,best_bid,best_ask,book_mid_px,premium_bps,raw_premium_bps,api_premium_bps,avg_ready,stale,state,position,killed";

    /// Build from the market snapshot; strategy-side values are filled by
    /// the caller.
    pub fn from_market(ts_ms: u64, m: &MarketState) -> Self {
        Self {
            ts_ms,
            oracle_px: m.oracle_px,
            mark_px: m.mark_px,
            impact_bid_px: m.impact_bid_px,
            impact_ask_px: m.impact_ask_px,
            best_bid: m.best_bid.map(|l| l.px),
            best_ask: m.best_ask.map(|l| l.px),
            api_premium_bps: m.api_premium.map(|p| p * 1e4),
            ..Self::default()
        }
    }

    fn mid(a: Option<f64>, b: Option<f64>) -> Option<f64> {
        Some(0.5 * (a? + b?))
    }

    pub fn impact_mid_px(&self) -> Option<f64> {
        Self::mid(self.impact_bid_px, self.impact_ask_px)
    }

    pub fn book_mid_px(&self) -> Option<f64> {
        Self::mid(self.best_bid, self.best_ask)
    }

    pub fn fields(&self) -> Vec<String> {
        vec![
            self.ts_ms.to_string(),
            fmt_utc_ms(self.ts_ms),
            opt(self.oracle_px),
            opt(self.mark_px),
            opt(self.impact_bid_px),
            opt(self.impact_ask_px),
            opt_fixed(self.impact_mid_px(), 6),
            opt(self.best_bid),
            opt(self.best_ask),
            opt_fixed(self.book_mid_px(), 6),
            opt_fixed(self.premium_bps, 3),
            opt_fixed(self.raw_premium_bps, 3),
            opt_fixed(self.api_premium_bps, 3),
            self.avg_ready.to_string(),
            self.stale.to_string(),
            self.state.clone(),
            num(self.position),
            self.killed.to_string(),
        ]
    }
}

/// One `orders.csv` row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OrderRow {
    pub ts_ms: u64,
    pub mode: String,
    /// place | cancel | resting | fill | done
    pub event: &'static str,
    pub tag: Option<u64>,
    /// buy | sell
    pub side: Option<&'static str>,
    /// alo | ioc
    pub kind: Option<&'static str>,
    /// entry | exit
    pub purpose: Option<&'static str>,
    pub reduce_only: Option<bool>,
    pub px: Option<f64>,
    pub sz: Option<f64>,
    pub fee_usd: Option<f64>,
    pub crossed: Option<bool>,
    /// Terminal status (done rows).
    pub status: Option<&'static str>,
    pub filled_sz: Option<f64>,
    pub detail: String,
}

impl OrderRow {
    pub const HEADER: &'static str = "ts_ms,ts_utc,mode,event,tag,side,kind,purpose,reduce_only,px,sz,fee_usd,crossed,status,filled_sz,detail";

    pub fn fields(&self) -> Vec<String> {
        let b = |v: Option<bool>| v.map_or_else(String::new, |x| x.to_string());
        vec![
            self.ts_ms.to_string(),
            fmt_utc_ms(self.ts_ms),
            self.mode.clone(),
            self.event.to_string(),
            self.tag.map_or_else(String::new, |t| t.to_string()),
            self.side.unwrap_or_default().to_string(),
            self.kind.unwrap_or_default().to_string(),
            self.purpose.unwrap_or_default().to_string(),
            b(self.reduce_only),
            opt(self.px),
            opt(self.sz),
            opt_fixed(self.fee_usd, 6),
            b(self.crossed),
            self.status.unwrap_or_default().to_string(),
            opt(self.filled_sz),
            self.detail.clone(),
        ]
    }

    fn with_intent(mut self, i: &OrderIntent) -> Self {
        self.side = Some(side_str(i.is_buy));
        self.kind = Some(match i.kind {
            crate::strategy::signal::OrderKind::Alo => "alo",
            crate::strategy::signal::OrderKind::Ioc => "ioc",
        });
        self.purpose = Some(match i.purpose {
            crate::strategy::signal::Purpose::Entry => "entry",
            crate::strategy::signal::Purpose::Exit => "exit",
        });
        self.reduce_only = Some(i.reduce_only);
        self
    }
}

fn side_str(is_buy: bool) -> &'static str {
    if is_buy {
        "buy"
    } else {
        "sell"
    }
}

/// Short status name of a terminal state, plus its detail text.
pub fn done_status(r: &DoneReason) -> (&'static str, String) {
    match r {
        DoneReason::Filled => ("filled", String::new()),
        DoneReason::Canceled => ("canceled", String::new()),
        DoneReason::PostOnlyRejected => ("post_only_rejected", String::new()),
        DoneReason::Rejected(m) => ("rejected", m.clone()),
        DoneReason::IocExpired => ("ioc_expired", String::new()),
    }
}

/// One `trades.csv` row.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeRow<'a> {
    pub mode: &'a str,
    pub t: &'a TradeRecord,
}

impl TradeRow<'_> {
    pub const HEADER: &'static str = "entry_ts_ms,entry_ts_utc,exit_ts_ms,exit_ts_utc,mode,side,size,entry_px,exit_px,notional_usd,fees_usd,funding_usd,gross_pnl_usd,pnl_usd,pnl_bps,reason,entry_premium_bps,exit_premium_bps,hold_secs,exit_taker_sz,signal_ts_ms";

    pub fn fields(&self) -> Vec<String> {
        let t = self.t;
        vec![
            t.opened_ms.to_string(),
            fmt_utc_ms(t.opened_ms),
            t.closed_ms.to_string(),
            fmt_utc_ms(t.closed_ms),
            self.mode.to_string(),
            t.side.as_str().to_string(),
            num(t.size),
            num(t.entry_px),
            num(t.exit_px),
            fixed(t.entry_notional_usd(), 2),
            fixed(t.fees_usd, 6),
            fixed(t.funding_usd, 6),
            fixed(t.gross_pnl_usd, 6),
            fixed(t.pnl_usd, 6),
            fixed(t.pnl_bps, 3),
            t.exit_reason.as_str().to_string(),
            opt_fixed(t.entry_premium_bps, 3),
            opt_fixed(t.exit_premium_bps, 3),
            fixed(t.hold_secs(), 0),
            num(t.exit_taker_sz),
            t.signal_ms.to_string(),
        ]
    }
}

/// The three journals of a run. Each one is optional: a file that cannot
/// be opened is logged and skipped (the bot keeps running).
#[derive(Debug, Default)]
pub struct Journal {
    mode: String,
    signals: Option<CsvAppender>,
    orders: Option<CsvAppender>,
    trades: Option<CsvAppender>,
    /// Intents of orders sent (tag -> intent), to describe later events.
    intents: HashMap<u64, OrderIntent>,
}

impl Journal {
    /// Open `signals.csv`, `orders.csv` and `trades.csv` in `data_dir`.
    pub fn open(data_dir: &Path, mode: &str) -> Self {
        let open = |name: &str, header: &str| match CsvAppender::open(&data_dir.join(name), header)
        {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::error!(file = name, error = %e, "cannot open CSV journal (disabled)");
                None
            }
        };
        let j = Self {
            mode: mode.to_string(),
            signals: open(SIGNALS_FILE, SignalRow::HEADER),
            orders: open(ORDERS_FILE, OrderRow::HEADER),
            trades: open(TRADES_FILE, TradeRow::HEADER),
            intents: HashMap::new(),
        };
        tracing::info!(dir = %data_dir.display(), "CSV journals: signals.csv, orders.csv, trades.csv");
        j
    }

    /// A journal that writes nothing (tests, tools).
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn signal(&mut self, row: &SignalRow) {
        if let Some(a) = &mut self.signals {
            a.append(&row.fields());
        }
    }

    pub fn trade(&mut self, t: &TradeRecord) {
        if let Some(a) = &mut self.trades {
            a.append(
                &TradeRow {
                    mode: &self.mode,
                    t,
                }
                .fields(),
            );
        }
    }

    fn order(&mut self, row: OrderRow) {
        if let Some(a) = &mut self.orders {
            a.append(&row.fields());
        }
    }

    fn base(&self, ts_ms: u64, event: &'static str, tag: Option<u64>) -> OrderRow {
        let row = OrderRow {
            ts_ms,
            mode: self.mode.clone(),
            event,
            tag,
            ..OrderRow::default()
        };
        match tag.and_then(|t| self.intents.get(&t)) {
            Some(i) => row.with_intent(i),
            None => row,
        }
    }

    /// A command handed to the executor. `touch` is the best bid / ask the
    /// command was decided on: it goes in the `detail` of `place` rows
    /// (`book <bid>/<ask>`) so the post-only and IOC prices can be audited
    /// against the book from the journal alone.
    pub fn command(&mut self, ts_ms: u64, c: &Command, touch: (Option<f64>, Option<f64>)) {
        match c {
            Command::Place(i) => {
                if self.intents.len() >= MAX_TRACKED_INTENTS {
                    // Oldest tags first (tags increase).
                    if let Some(&min) = self.intents.keys().min() {
                        self.intents.remove(&min);
                    }
                }
                self.intents.insert(i.tag, *i);
                let mut row = self.base(ts_ms, "place", Some(i.tag));
                row.px = Some(i.px);
                row.sz = Some(i.sz);
                let side = |v: Option<f64>| v.map_or_else(|| "-".to_string(), num);
                row.detail = format!("book {}/{}", side(touch.0), side(touch.1));
                self.order(row);
            }
            Command::Cancel { tag } => {
                let row = self.base(ts_ms, "cancel", Some(*tag));
                self.order(row);
            }
        }
    }

    /// Order feedback from the executor.
    pub fn order_update(&mut self, ts_ms: u64, u: &OrderUpdate) {
        match u {
            OrderUpdate::Resting { tag } => {
                let row = self.base(ts_ms, "resting", Some(*tag));
                self.order(row);
            }
            OrderUpdate::Filled {
                tag,
                is_buy,
                px,
                sz,
                fee_usd,
                crossed,
                time_ms,
            } => {
                let mut row = self.base(*time_ms, "fill", *tag);
                row.side = Some(side_str(*is_buy));
                row.px = Some(*px);
                row.sz = Some(*sz);
                row.fee_usd = Some(*fee_usd);
                row.crossed = Some(*crossed);
                self.order(row);
            }
            OrderUpdate::Done {
                tag,
                reason,
                filled_sz,
            } => {
                let mut row = self.base(ts_ms, "done", Some(*tag));
                let (status, detail) = done_status(reason);
                row.status = Some(status);
                row.detail = detail;
                row.filled_sz = *filled_sz;
                self.order(row);
                // Late fills of this tag are logged without the intent.
                self.intents.remove(tag);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::premium::Side;
    use crate::strategy::signal::{ExitReason, OrderKind, Purpose};

    fn read(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn prices_and_sizes_print_without_float_noise() {
        assert_eq!(num(111.93000000000002), "111.93");
        assert_eq!(num(0.1 + 0.2), "0.3");
        assert_eq!(num(22.33), "22.33");
        assert_eq!(num(100.0), "100");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(-1e-12), "0");
        assert_eq!(num(-21.94), "-21.94");
        assert_eq!(num(0.000_012_346), "0.00001235");
        assert_eq!(num(f64::NAN), "");
        assert_eq!(opt(None), "");
        let mut t = trade();
        t.entry_px = (111.93 * 10.0 + 111.93 * 12.33) / 22.33;
        t.size = 0.1 + 0.2;
        let f = TradeRow {
            mode: "paper",
            t: &t,
        }
        .fields();
        assert_eq!(f[6], "0.3");
        assert_eq!(f[7], "111.93");
    }

    fn trade() -> TradeRecord {
        TradeRecord {
            side: Side::Short,
            size: 24.24,
            entry_px: 103.11,
            exit_px: 102.5,
            signal_ms: 1_794_500_000_000,
            opened_ms: 1_794_500_060_000,
            closed_ms: 1_794_528_860_000,
            entry_premium_bps: Some(62.345),
            exit_premium_bps: Some(4.1),
            fees_usd: 0.15,
            funding_usd: -0.02,
            gross_pnl_usd: 14.786,
            pnl_usd: 14.616,
            pnl_bps: 58.48,
            exit_reason: ExitReason::Converged,
            exit_taker_sz: 0.0,
        }
    }

    #[test]
    fn escaping() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a,b"), "\"a,b\"");
        assert_eq!(escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(escape("x\ny"), "\"x\ny\"");
        assert_eq!(
            line(&["1".into(), "a,b".into(), String::new()]),
            "1,\"a,b\","
        );
    }

    #[test]
    fn headers_match_field_counts() {
        let n = |h: &str| h.split(',').count();
        assert_eq!(SignalRow::default().fields().len(), n(SignalRow::HEADER));
        assert_eq!(OrderRow::default().fields().len(), n(OrderRow::HEADER));
        let t = trade();
        assert_eq!(
            TradeRow {
                mode: "paper",
                t: &t
            }
            .fields()
            .len(),
            n(TradeRow::HEADER)
        );
    }

    #[test]
    fn header_once_and_append_on_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        {
            let mut a = CsvAppender::open(&p, "a,b").unwrap();
            a.append(&["1".into(), "2".into()]);
        }
        {
            let mut a = CsvAppender::open(&p, "a,b").unwrap();
            a.append(&["3".into(), "4".into()]);
        }
        assert_eq!(read(&p), vec!["a,b", "1,2", "3,4"]);
    }

    #[test]
    fn changed_header_moves_old_file_aside() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        std::fs::write(&p, "old,cols\n1,2\n").unwrap();
        let mut a = CsvAppender::open(&p, "a,b,c").unwrap();
        a.append(&["1".into(), "2".into(), "3".into()]);
        assert_eq!(read(&p), vec!["a,b,c", "1,2,3"]);
        let olds: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("x.csv.") && n.ends_with(".old"))
            .collect();
        assert_eq!(olds.len(), 1);
        assert_eq!(read(&d.path().join(&olds[0])), vec!["old,cols", "1,2"]);
    }

    #[test]
    fn truncated_row_is_terminated() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        std::fs::write(&p, "a,b\n1,2\n3,").unwrap();
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.append(&["5".into(), "6".into()]);
        assert_eq!(read(&p), vec!["a,b", "1,2", "3,", "5,6"]);
    }

    #[test]
    fn row_after_failed_partial_write_starts_on_a_new_line() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.append(&["1".into(), "2".into()]);
        // Simulate a short write that failed mid-row (e.g. ENOSPC).
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"3,").unwrap();
        a.check_line_end = true;
        a.append(&["5".into(), "6".into()]);
        assert!(!a.check_line_end);
        a.append(&["7".into(), "8".into()]);
        assert_eq!(read(&p), vec!["a,b", "1,2", "3,", "5,6", "7,8"]);
        // A failed write that wrote nothing adds no blank line.
        a.check_line_end = true;
        a.append(&["9".into(), "0".into()]);
        assert_eq!(read(&p), vec!["a,b", "1,2", "3,", "5,6", "7,8", "9,0"]);
    }

    #[test]
    fn row_cut_inside_a_quoted_field_is_closed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        std::fs::write(&p, "a,b\n1,\"partial").unwrap();
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.append(&["2".into(), "valid".into()]);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "a,b\n1,\"partial\"\n2,valid\n"
        );
    }

    #[test]
    fn row_cut_after_an_embedded_newline_is_closed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        // The file ends with a newline, but inside an open quote.
        std::fs::write(&p, "a,b\n1,\"line \"\"one\"\"\n").unwrap();
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.append(&["2".into(), "valid".into()]);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "a,b\n1,\"line \"\"one\"\"\n\"\n2,valid\n"
        );
    }

    #[test]
    fn complete_quoted_rows_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        {
            let mut a = CsvAppender::open(&p, "a,b").unwrap();
            a.append(&["1".into(), "say \"hi\",\nbye".into()]);
        }
        let before = std::fs::read_to_string(&p).unwrap();
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.check_line_end = true;
        a.append(&["2".into(), "x".into()]);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            format!("{before}2,x\n")
        );
    }

    #[test]
    fn failed_write_inside_a_quoted_field_is_closed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.csv");
        let mut a = CsvAppender::open(&p, "a,b").unwrap();
        a.append(&["1".into(), "2".into()]);
        // Simulate a short write that stopped inside a quoted field.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"3,\"rejected: bad\n").unwrap();
        a.check_line_end = true;
        a.append(&["5".into(), "6".into()]);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "a,b\n1,2\n3,\"rejected: bad\n\"\n5,6\n"
        );
    }

    #[test]
    fn empty_file_gets_header() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/x.csv");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
        CsvAppender::open(&p, "a,b").unwrap();
        assert_eq!(read(&p), vec!["a,b"]);
    }

    #[test]
    fn signal_row_values() {
        let r = SignalRow {
            ts_ms: 1_794_511_665_989,
            oracle_px: Some(104.29),
            mark_px: Some(103.63),
            impact_bid_px: Some(103.4983),
            impact_ask_px: Some(103.5681),
            best_bid: Some(103.54),
            best_ask: Some(103.55),
            premium_bps: Some(-72.5712),
            raw_premium_bps: Some(-72.5712),
            api_premium_bps: None,
            avg_ready: true,
            stale: false,
            state: "flat".into(),
            position: 0.0,
            killed: false,
        };
        assert_eq!(
            line(&r.fields()),
            "1794511665989,2026-11-12T19:27:45.989Z,104.29,103.63,103.4983,103.5681,103.533200,103.54,103.55,103.545000,-72.571,-72.571,,true,false,flat,0,false"
        );
    }

    #[test]
    fn trade_row_values() {
        let t = trade();
        assert_eq!(
            line(&TradeRow { mode: "paper", t: &t }.fields()),
            "1794500060000,2026-11-12T16:14:20.000Z,1794528860000,2026-11-13T00:14:20.000Z,paper,short,24.24,103.11,102.5,2499.39,0.150000,-0.020000,14.786000,14.616000,58.480,converged,62.345,4.100,28800,0,1794500000000"
        );
    }

    #[test]
    fn journal_writes_order_lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let mut j = Journal::open(d.path(), "paper");
        let i = OrderIntent {
            tag: 7,
            is_buy: false,
            px: 103.2,
            sz: 24.24,
            kind: OrderKind::Alo,
            reduce_only: false,
            purpose: Purpose::Entry,
        };
        j.command(1_000, &Command::Place(i), (Some(103.19), None));
        j.order_update(1_100, &OrderUpdate::Resting { tag: 7 });
        j.order_update(
            1_200,
            &OrderUpdate::Filled {
                tag: Some(7),
                is_buy: false,
                px: 103.2,
                sz: 10.0,
                fee_usd: 0.03096,
                crossed: false,
                time_ms: 1_190,
            },
        );
        j.command(1_300, &Command::Cancel { tag: 7 }, (None, None));
        j.order_update(
            1_400,
            &OrderUpdate::Done {
                tag: 7,
                reason: DoneReason::Rejected("margin, try again".into()),
                filled_sz: Some(10.0),
            },
        );
        j.trade(&trade());
        let rows = read(&d.path().join(ORDERS_FILE));
        assert_eq!(rows[0], OrderRow::HEADER);
        assert_eq!(
            rows[1],
            "1000,1970-01-01T00:00:01.000Z,paper,place,7,sell,alo,entry,false,103.2,24.24,,,,,book 103.19/-"
        );
        assert_eq!(
            rows[2],
            "1100,1970-01-01T00:00:01.100Z,paper,resting,7,sell,alo,entry,false,,,,,,,"
        );
        assert_eq!(
            rows[3],
            "1190,1970-01-01T00:00:01.190Z,paper,fill,7,sell,alo,entry,false,103.2,10,0.030960,false,,,"
        );
        assert_eq!(
            rows[4],
            "1300,1970-01-01T00:00:01.300Z,paper,cancel,7,sell,alo,entry,false,,,,,,,"
        );
        assert_eq!(
            rows[5],
            "1400,1970-01-01T00:00:01.400Z,paper,done,7,sell,alo,entry,false,,,,,rejected,10,\"margin, try again\""
        );
        assert!(j.intents.is_empty());
        let trades = read(&d.path().join(TRADES_FILE));
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0], TradeRow::HEADER);
        assert!(read(&d.path().join(SIGNALS_FILE)) == vec![SignalRow::HEADER.to_string()]);
    }

    #[test]
    fn disabled_journal_is_a_noop() {
        let mut j = Journal::disabled();
        j.signal(&SignalRow::default());
        j.trade(&trade());
        j.command(1, &Command::Cancel { tag: 1 }, (None, None));
    }
}
