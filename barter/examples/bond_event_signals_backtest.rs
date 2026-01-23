use chrono::{NaiveDate, NaiveDateTime};
use prettytable::{Cell, Row, Table, format, row};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MarkType {
    CleanPrice,
}

impl MarkType {
    fn parse(s: &str) -> Result<Self, Box<dyn Error>> {
        match s.trim() {
            "CLEAN_PRICE" => Ok(Self::CleanPrice),
            other => Err(format!("unsupported mark_type: {other}").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Ts(NaiveDate);

impl Ts {
    fn parse(s: &str) -> Result<Self, Box<dyn Error>> {
        // Support either YYYY-MM-DD or ISO datetime (we truncate to date).
        if let Ok(d) = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
            return Ok(Self(d));
        }
        let dt = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S"))?;
        Ok(Self(dt.date()))
    }
}

#[derive(Debug, Clone)]
struct MarkRow {
    ts: Ts,
    security_id: String,
    mark_type: MarkType,
    mark: Decimal,
}

#[derive(Debug, Clone)]
struct SignalRow {
    ts: Ts,
    trade_id: String,
    security_id: String,
    delta_weight: Decimal,
    fill_mark_override: Option<Decimal>,
}

#[derive(Debug, Clone)]
struct TradeLegState {
    // Weight currently open for this trade+security.
    weight: Decimal,
    // Average execution mark for the currently-open portion (simple avg weighted by |delta|).
    avg_exec_mark: Decimal,
    // Total absolute executed weight used for avg.
    abs_weight_basis: Decimal,
}

impl TradeLegState {
    fn new() -> Self {
        Self {
            weight: Decimal::ZERO,
            avg_exec_mark: Decimal::ZERO,
            abs_weight_basis: Decimal::ZERO,
        }
    }

    fn apply_delta(&mut self, delta: Decimal, exec_mark: Decimal) {
        // We treat each signal row as an execution at exec_mark.
        // Maintain a simple average execution mark of the *open* position.
        // This is a pragmatic v1; for bonds you might prefer FIFO lots.
        if delta == Decimal::ZERO {
            return;
        }

        let new_weight = self.weight + delta;

        // If position flips sign or is reduced, we don't attempt full lot accounting here.
        // For your stated constraint (trade legs sum to 0 per trade_id), typical flows are
        // open then close, so this is sufficient.
        if self.weight == Decimal::ZERO {
            // Opening fresh.
            self.weight = delta;
            self.avg_exec_mark = exec_mark;
            self.abs_weight_basis = delta.abs();
            return;
        }

        // Same direction add.
        if (self.weight > Decimal::ZERO && delta > Decimal::ZERO)
            || (self.weight < Decimal::ZERO && delta < Decimal::ZERO)
        {
            let abs_add = delta.abs();
            let total_abs = self.abs_weight_basis + abs_add;
            if total_abs > Decimal::ZERO {
                self.avg_exec_mark =
                    (self.avg_exec_mark * self.abs_weight_basis + exec_mark * abs_add) / total_abs;
                self.abs_weight_basis = total_abs;
            }
            self.weight = new_weight;
            return;
        }

        // Opposite direction: reduce/close.
        // We keep avg_exec_mark as-is unless the position crosses through zero.
        self.weight = new_weight;
        if self.weight == Decimal::ZERO {
            self.avg_exec_mark = Decimal::ZERO;
            self.abs_weight_basis = Decimal::ZERO;
        }
    }
}

#[derive(Debug, Clone)]
struct PortfolioState {
    // Current portfolio weights by security.
    weights: HashMap<String, Decimal>,
    // Per-trade per-security state for validation and optional analytics.
    trade_legs: HashMap<(String, String), TradeLegState>,
}

impl PortfolioState {
    fn new() -> Self {
        Self {
            weights: HashMap::new(),
            trade_legs: HashMap::new(),
        }
    }

    fn apply_signal(&mut self, signal: &SignalRow, exec_mark: Decimal) {
        *self
            .weights
            .entry(signal.security_id.clone())
            .or_insert(Decimal::ZERO) += signal.delta_weight;

        let key = (signal.trade_id.clone(), signal.security_id.clone());
        self.trade_legs
            .entry(key)
            .or_insert_with(TradeLegState::new)
            .apply_delta(signal.delta_weight, exec_mark);
    }
}

#[derive(Debug, Clone)]
struct EquityPoint {
    ts: Ts,
    nav: Decimal,
    #[allow(dead_code)]
    pnl: Decimal,
    drawdown: Decimal,
}

fn read_marks_csv(path: &Path) -> Result<Vec<MarkRow>, Box<dyn Error>> {
    let mut rdr = csv::Reader::from_path(path)?;
    let mut out = Vec::new();

    for result in rdr.records() {
        let rec = result?;
        let ts = Ts::parse(rec.get(0).ok_or("missing ts")?)?;
        let security_id = rec.get(1).ok_or("missing security_id")?.to_string();
        let mark_type = MarkType::parse(rec.get(2).ok_or("missing mark_type")?)?;
        let mark: Decimal = rec
            .get(3)
            .ok_or("missing mark")?
            .parse()
            .map_err(|e| format!("invalid mark: {e}"))?;

        out.push(MarkRow {
            ts,
            security_id,
            mark_type,
            mark,
        });
    }

    Ok(out)
}

fn read_signals_csv(path: &Path) -> Result<Vec<SignalRow>, Box<dyn Error>> {
    let mut rdr = csv::Reader::from_path(path)?;
    let mut out = Vec::new();

    for result in rdr.records() {
        let rec = result?;
        let ts = Ts::parse(rec.get(0).ok_or("missing ts")?)?;
        let trade_id = rec.get(1).ok_or("missing trade_id")?.to_string();
        let security_id = rec.get(2).ok_or("missing security_id")?.to_string();
        let delta_weight: Decimal = rec
            .get(3)
            .ok_or("missing delta_weight")?
            .parse()
            .map_err(|e| format!("invalid delta_weight: {e}"))?;

        let fill_mark_override = rec
            .get(4)
            .and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s) }
            })
            .map(|s| s.parse::<Decimal>())
            .transpose()
            .map_err(|e| format!("invalid fill_mark_override: {e}"))?;

        out.push(SignalRow {
            ts,
            trade_id,
            security_id,
            delta_weight,
            fill_mark_override,
        });
    }

    Ok(out)
}

#[derive(Debug)]
struct PreparedData {
    calendar: Vec<Ts>,
    marks: HashMap<(Ts, String), Decimal>,
    // signals by date
    signals_by_ts: BTreeMap<Ts, Vec<SignalRow>>,
    securities: HashSet<String>,
}

fn prepare_data(
    marks: Vec<MarkRow>,
    signals: Vec<SignalRow>,
    mark_type: MarkType,
) -> Result<PreparedData, Box<dyn Error>> {
    let mut marks_map: HashMap<(Ts, String), Decimal> = HashMap::new();
    let mut calendar_set: HashSet<Ts> = HashSet::new();
    let mut securities: HashSet<String> = HashSet::new();

    for row in marks {
        if row.mark_type != mark_type {
            continue;
        }
        calendar_set.insert(row.ts);
        securities.insert(row.security_id.clone());
        let key = (row.ts, row.security_id);
        if marks_map.insert(key, row.mark).is_some() {
            return Err("duplicate mark row for (ts,security_id,mark_type)".into());
        }
    }

    let mut signals_by_ts: BTreeMap<Ts, Vec<SignalRow>> = BTreeMap::new();
    for s in signals {
        securities.insert(s.security_id.clone());
        signals_by_ts.entry(s.ts).or_default().push(s);
    }

    let mut calendar: Vec<Ts> = calendar_set.into_iter().collect();
    calendar.sort();

    Ok(PreparedData {
        calendar,
        marks: marks_map,
        signals_by_ts,
        securities,
    })
}

fn validate_trade_closure(signals: &BTreeMap<Ts, Vec<SignalRow>>) -> Result<(), Box<dyn Error>> {
    let mut sums: HashMap<(String, String), Decimal> = HashMap::new();

    for rows in signals.values() {
        for s in rows {
            *sums
                .entry((s.trade_id.clone(), s.security_id.clone()))
                .or_insert(Decimal::ZERO) += s.delta_weight;
        }
    }

    let tolerance = dec!(0.00000001);
    let mut bad = Vec::new();
    for ((trade_id, security_id), sum) in sums {
        if sum.abs() > tolerance {
            bad.push((trade_id, security_id, sum));
        }
    }

    if !bad.is_empty() {
        let mut msg = String::from("trade closure validation failed (sum delta_weight != 0):\n");
        for (trade_id, security_id, sum) in bad {
            msg.push_str(&format!(
                "  trade_id={trade_id} security_id={security_id} sum={sum}\n"
            ));
        }
        return Err(msg.into());
    }

    Ok(())
}

fn validate_marks_coverage(
    prepared: &PreparedData,
    mark_type: MarkType,
) -> Result<(), Box<dyn Error>> {
    // Conservative validation: every security referenced in signals must have marks on every date
    // in calendar (for drawdown). You can relax this later.
    for ts in &prepared.calendar {
        for sec in &prepared.securities {
            let key = (*ts, sec.clone());
            if !prepared.marks.contains_key(&key) {
                return Err(format!(
                    "missing mark for ts={:?} security_id={sec} mark_type={:?}",
                    ts.0, mark_type
                )
                .into());
            }
        }
    }
    Ok(())
}

fn compute_equity_curve(
    prepared: &PreparedData,
    mark_type: MarkType,
) -> Result<Vec<EquityPoint>, Box<dyn Error>> {
    let mut portfolio = PortfolioState::new();
    let mut equity = Vec::new();

    // NAV starts at 1.0
    let mut nav = dec!(1.0);
    let mut peak = nav;

    // We model signals as executed intraday on `ts`, then valued at the close mark on `ts`.
    // After that, close-to-close returns from `ts` to `ts_next` use the post-signal weights.
    //
    // Need at least 2 dates to compute close-to-close returns.
    if prepared.calendar.len() < 2 {
        return Err("need at least 2 mark dates to compute returns".into());
    }

    for (idx, ts) in prepared.calendar.iter().enumerate() {
        // 1) Apply any intraday executions on `ts`, then value them at the close mark on `ts`.
        //    This captures fill->close PnL via fill_mark_override without mutating marks.
        if let Some(rows) = prepared.signals_by_ts.get(ts) {
            let mut immediate_return = Decimal::ZERO;

            for s in rows {
                let close_mark = *prepared
                    .marks
                    .get(&(s.ts, s.security_id.clone()))
                    .ok_or_else(|| {
                        format!("missing close mark on signal day for {}", s.security_id)
                    })?;

                let exec_mark = s.fill_mark_override.unwrap_or(close_mark);

                // If exec_mark == close_mark, this contributes 0.
                // Works for both long and short deltas.
                if exec_mark > Decimal::ZERO {
                    immediate_return += s.delta_weight * ((close_mark / exec_mark) - dec!(1.0));
                }

                portfolio.apply_signal(s, exec_mark);
            }

            nav = nav * (dec!(1.0) + immediate_return);
        }

        // Snapshot NAV at the close of ts (after executions on ts).
        if nav > peak {
            peak = nav;
        }
        let drawdown_at_close = if peak == Decimal::ZERO {
            Decimal::ZERO
        } else {
            (nav / peak) - dec!(1.0)
        };

        equity.push(EquityPoint {
            ts: *ts,
            nav,
            pnl: nav - dec!(1.0),
            drawdown: drawdown_at_close,
        });

        // 2) Close-to-close return from ts -> next_ts uses post-signal weights.
        if idx + 1 >= prepared.calendar.len() {
            break;
        }

        let ts_next = prepared.calendar[idx + 1];

        let mut interval_return = Decimal::ZERO;
        for (sec, w) in portfolio.weights.iter() {
            if *w == Decimal::ZERO {
                continue;
            }

            let m_now = *prepared
                .marks
                .get(&(*ts, sec.clone()))
                .ok_or_else(|| format!("missing current mark for {sec}"))?;
            let m_next = *prepared
                .marks
                .get(&(ts_next, sec.clone()))
                .ok_or_else(|| format!("missing next mark for {sec}"))?;

            let r = (m_next / m_now) - dec!(1.0);
            interval_return += *w * r;
        }

        nav = nav * (dec!(1.0) + interval_return);
    }

    // We don't currently use mark_type directly (it is enforced upstream), but keep the arg to
    // make the API explicit.
    let _ = mark_type;

    Ok(equity)
}

#[derive(Debug, Clone)]
struct BacktestStats {
    total_return: Decimal,
    max_drawdown: Decimal,
    annualized_return: Decimal,
    #[allow(dead_code)]
    daily_sharpe: Decimal,
    annualized_sharpe: Decimal,
    annualized_volatility: Decimal,
    trading_days: usize,
}

impl BacktestStats {
    fn calculate(equity: &[EquityPoint]) -> Self {
        if equity.is_empty() {
            return Self {
                total_return: Decimal::ZERO,
                max_drawdown: Decimal::ZERO,
                annualized_return: Decimal::ZERO,
                daily_sharpe: Decimal::ZERO,
                annualized_sharpe: Decimal::ZERO,
                annualized_volatility: Decimal::ZERO,
                trading_days: 0,
            };
        }

        let first = equity.first().unwrap();
        let last = equity.last().unwrap();
        let total_return = (last.nav / first.nav) - dec!(1.0);

        let max_drawdown = equity
            .iter()
            .map(|p| p.drawdown)
            .min()
            .unwrap_or(Decimal::ZERO);

        let trading_days = equity.len();

        // Calculate daily returns for Sharpe/Vol
        let mut daily_returns: Vec<Decimal> = Vec::with_capacity(trading_days - 1);
        for i in 1..trading_days {
            let prev_nav = equity[i - 1].nav;
            let curr_nav = equity[i].nav;
            if prev_nav != Decimal::ZERO {
                let r = (curr_nav / prev_nav) - dec!(1.0);
                daily_returns.push(r);
            }
        }

        let (mean_ret, std_dev) = if !daily_returns.is_empty() {
            let sum: Decimal = daily_returns.iter().sum();
            let count = Decimal::from(daily_returns.len());
            let mean = sum / count;

            let variance_sum: Decimal =
                daily_returns.iter().map(|&r| (r - mean) * (r - mean)).sum();

            // Sample std dev (n-1)
            let std_dev = if count > dec!(1.0) {
                (variance_sum / (count - dec!(1.0)))
                    .sqrt()
                    .unwrap_or(Decimal::ZERO)
            } else {
                Decimal::ZERO
            };
            (mean, std_dev)
        } else {
            (Decimal::ZERO, Decimal::ZERO)
        };

        // Annualization factors (assuming 365 days for this crypto/synthetic calendar)
        // If this were pure biz days, we'd use ~252 or similar.
        let days_in_year = dec!(365.0);
        let sqrt_days = days_in_year.sqrt().unwrap_or(dec!(19.1));

        let annualized_volatility = std_dev * sqrt_days;

        let daily_sharpe = if std_dev != Decimal::ZERO {
            mean_ret / std_dev
        } else {
            Decimal::ZERO
        };

        let annualized_sharpe = daily_sharpe * sqrt_days;

        // CAGR / Annualized Return
        // (1 + total)^ (365 / days) - 1
        let days_dec = Decimal::from(trading_days);
        // Using f64 for power because rust_decimal power is limited or tricky?
        // actually rust_decimal has `powf` via `MathematicalOps` via feature "maths".
        // Dependencies usually enable features. I'll use f64 conversion for safety/ease if needed,
        // but let's try to stick to Decimal or approximate.
        // Actually, let's use f64 for the exponentiation to be safe.

        let total_ret_f64 = total_return.to_f64().unwrap_or(0.0);
        let year_fraction = days_dec.to_f64().unwrap_or(1.0) / 365.0;

        let annualized_return = if year_fraction > 0.0 {
            let val = (1.0_f64 + total_ret_f64).powf(1.0_f64 / year_fraction) - 1.0_f64;
            Decimal::from_f64_retain(val).unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        Self {
            total_return,
            max_drawdown,
            annualized_return,
            daily_sharpe,
            annualized_sharpe,
            annualized_volatility,
            trading_days,
        }
    }
}

fn print_summary(equity: &[EquityPoint]) {
    let stats = BacktestStats::calculate(equity);

    println!();

    // Title Table
    let mut title_table = Table::new();
    title_table.set_format(*format::consts::FORMAT_CLEAN);
    title_table.add_row(Row::new(vec![
        Cell::new("BOND EVENT BACKTEST SUMMARY").style_spec("bB"),
    ]));
    title_table.printstd();

    // Metrics Table
    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_BOX_CHARS);

    table.add_row(row![bFc => "Metric", "Value"]);

    table.add_row(Row::new(vec![
        Cell::new("Total Return"),
        Cell::new(&format!("{:.4}%", stats.total_return * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Annualized Return (CAGR)"),
        Cell::new(&format!("{:.4}%", stats.annualized_return * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max Drawdown"),
        Cell::new(&format!("{:.4}%", stats.max_drawdown * dec!(100.0))).style_spec("Fr"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Annualized Volatility"),
        Cell::new(&format!(
            "{:.4}%",
            stats.annualized_volatility * dec!(100.0)
        )),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Sharpe Ratio (Ann.)"),
        Cell::new(&format!("{:.4}", stats.annualized_sharpe)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trading Days"),
        Cell::new(&format!("{}", stats.trading_days)),
    ]));

    table.printstd();

    println!("\nFirst 5 points:");
    for p in equity.iter().take(5) {
        println!("  {} nav={:.6} dd={:.6}", p.ts.0, p.nav, p.drawdown);
    }
    println!("\nLast 5 points:");
    for p in equity.iter().rev().take(5).rev() {
        println!("  {} nav={:.6} dd={:.6}", p.ts.0, p.nav, p.drawdown);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Collect command line arguments
    let args: Vec<String> = std::env::args().collect();

    // Default paths
    let default_marks = "barter/examples/data/bond_marks_template.csv";
    let default_signals = "barter/examples/data/bond_signals_template.csv";

    // Usage: cargo run ... -- <marks_path> <signals_path>
    let (marks_str, signals_str) = if args.len() >= 3 {
        println!(
            "Using provided paths:\n  Marks: {}\n  Signals: {}",
            args[1], args[2]
        );
        (args[1].as_str(), args[2].as_str())
    } else {
        println!("Usage: <binary> <marks_csv> <signals_csv>");
        println!(
            "No arguments provided. Using defaults:\n  Marks: {}\n  Signals: {}",
            default_marks, default_signals
        );
        (default_marks, default_signals)
    };

    let marks_path = Path::new(marks_str);
    let signals_path = Path::new(signals_str);

    if !marks_path.exists() {
        return Err(format!("missing marks file: {}", marks_path.display()).into());
    }
    if !signals_path.exists() {
        return Err(format!("missing signals file: {}", signals_path.display()).into());
    }

    let marks = read_marks_csv(marks_path)?;
    let signals = read_signals_csv(signals_path)?;

    let mark_type = MarkType::CleanPrice;

    let prepared = prepare_data(marks, signals, mark_type.clone())?;

    validate_trade_closure(&prepared.signals_by_ts)?;
    validate_marks_coverage(&prepared, mark_type.clone())?;

    let equity = compute_equity_curve(&prepared, mark_type)?;
    print_summary(&equity);

    Ok(())
}
