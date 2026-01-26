use chrono::{NaiveDate, NaiveDateTime};
use prettytable::{Cell, Row, Table, format, row};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
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

#[derive(Debug, Clone, Copy)]
struct ExposureStats {
    avg_gross: Decimal,
    max_gross: Decimal,
    avg_net: Decimal,
    max_net: Decimal,
    max_abs_pos: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct TurnoverStats {
    total: Decimal,
    avg_daily: Decimal,
    max_daily: Decimal,
}

#[derive(Debug, Clone)]
struct DrawdownStats {
    start: Ts,
    trough: Ts,
    recovery: Option<Ts>,
    duration_days: i64,
    recovery_days: Option<i64>,
}

#[derive(Debug, Clone)]
struct TradeResult {
    trade_id: String,
    entry: Ts,
    exit: Ts,
    trade_return: Decimal,
    holding_days: i64,
}

#[derive(Debug, Clone, Copy)]
struct TradeStats {
    total: usize,
    wins: usize,
    losses: usize,
    win_rate: Decimal,
    avg_win: Decimal,
    avg_loss: Decimal,
    payoff_ratio: Decimal,
    profit_factor: Decimal,
    mean_return: Decimal,
    std_return: Decimal,
    avg_hold_days: Decimal,
    median_hold_days: Decimal,
    long_hit_rate: Decimal,
    short_hit_rate: Decimal,
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
    trade_count_total: usize,
    trade_count_dropped: usize,
}

fn prepare_data(
    marks: Vec<MarkRow>,
    signals: Vec<SignalRow>,
    mark_type: MarkType,
) -> Result<PreparedData, Box<dyn Error>> {
    let mut marks_map: HashMap<(Ts, String), Decimal> = HashMap::new();
    let mut calendar_set: HashSet<Ts> = HashSet::new();

    for row in marks {
        if row.mark_type != mark_type {
            continue;
        }
        calendar_set.insert(row.ts);
        let key = (row.ts, row.security_id);
        if marks_map.insert(key, row.mark).is_some() {
            return Err("duplicate mark row for (ts,security_id,mark_type)".into());
        }
    }

    let mut signals_by_trade: HashMap<String, Vec<SignalRow>> = HashMap::new();
    for s in signals {
        signals_by_trade.entry(s.trade_id.clone()).or_default().push(s);
    }

    let trade_count_total = signals_by_trade.len();
    let mut valid_signals = Vec::new();
    let mut dropped_trades = HashSet::new();

    for (trade_id, mut rows) in signals_by_trade {
        // Sort rows by ts for tenure check
        rows.sort_by_key(|r| r.ts);

        let mut has_all_marks = true;

        // 1) Every signal row must have a mark on its ts
        for r in &rows {
            if !marks_map.contains_key(&(r.ts, r.security_id.clone())) {
                has_all_marks = false;
                break;
            }
        }

        if has_all_marks {
            // 2) Tenure coverage: for each security in the trade, we need marks for every 
            // calendar date from its first signal to its last signal in this trade_id.
            let mut sec_tenures: HashMap<String, (Ts, Ts)> = HashMap::new();
            for r in &rows {
                let entry = sec_tenures
                    .entry(r.security_id.clone())
                    .or_insert((r.ts, r.ts));
                if r.ts < entry.0 { entry.0 = r.ts; }
                if r.ts > entry.1 { entry.1 = r.ts; }
            }

            for (sec, (start, end)) in sec_tenures {
                for ts in &calendar_set {
                    if *ts >= start && *ts <= end {
                        if !marks_map.contains_key(&(*ts, sec.clone())) {
                            has_all_marks = false;
                            break;
                        }
                    }
                }
                if !has_all_marks { break; }
            }
        }

        if has_all_marks {
            valid_signals.extend(rows);
        } else {
            dropped_trades.insert(trade_id);
        }
    }

    let trade_count_dropped = dropped_trades.len();
    if !dropped_trades.is_empty() {
        println!(
            "WARNING: dropped {} trade_ids due to missing marks (e.g. {:?})",
            trade_count_dropped,
            dropped_trades.iter().take(3).collect::<Vec<_>>()
        );
    }

    let mut signals_by_ts: BTreeMap<Ts, Vec<SignalRow>> = BTreeMap::new();
    for s in valid_signals {
        signals_by_ts.entry(s.ts).or_default().push(s);
    }

    let mut calendar: Vec<Ts> = calendar_set.into_iter().collect();
    calendar.sort();

    Ok(PreparedData {
        calendar,
        marks: marks_map,
        signals_by_ts,
        trade_count_total,
        trade_count_dropped,
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

fn compute_equity_curve(
    prepared: &PreparedData,
    mark_type: MarkType,
) -> Result<(Vec<EquityPoint>, ExposureStats), Box<dyn Error>> {
    let mut portfolio = PortfolioState::new();
    let mut equity = Vec::new();

    let mut sum_gross = Decimal::ZERO;
    let mut sum_net = Decimal::ZERO;
    let mut max_gross = Decimal::ZERO;
    let mut max_net = Decimal::ZERO;
    let mut max_abs_pos = Decimal::ZERO;
    let mut exposure_points = 0usize;

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

        // Exposure stats at close of ts
        let mut gross = Decimal::ZERO;
        let mut net = Decimal::ZERO;
        for w in portfolio.weights.values() {
            gross += w.abs();
            net += *w;
            if w.abs() > max_abs_pos {
                max_abs_pos = w.abs();
            }
        }
        if gross > max_gross {
            max_gross = gross;
        }
        if net.abs() > max_net.abs() {
            max_net = net;
        }
        sum_gross += gross;
        sum_net += net;
        exposure_points += 1;

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

    let avg_gross = if exposure_points > 0 {
        sum_gross / Decimal::from(exposure_points)
    } else {
        Decimal::ZERO
    };
    let avg_net = if exposure_points > 0 {
        sum_net / Decimal::from(exposure_points)
    } else {
        Decimal::ZERO
    };

    let exposure = ExposureStats {
        avg_gross,
        max_gross,
        avg_net,
        max_net,
        max_abs_pos,
    };

    Ok((equity, exposure))
}

fn compute_turnover_stats(prepared: &PreparedData) -> TurnoverStats {
    let mut total = Decimal::ZERO;
    let mut max_daily = Decimal::ZERO;

    for rows in prepared.signals_by_ts.values() {
        let mut daily = Decimal::ZERO;
        for s in rows {
            daily += s.delta_weight.abs();
        }
        if daily > max_daily {
            max_daily = daily;
        }
        total += daily;
    }

    let avg_daily = if prepared.calendar.is_empty() {
        Decimal::ZERO
    } else {
        total / Decimal::from(prepared.calendar.len())
    };

    TurnoverStats {
        total,
        avg_daily,
        max_daily,
    }
}

fn compute_drawdown_stats(equity: &[EquityPoint]) -> DrawdownStats {
    let first = equity.first().expect("equity not empty");
    let mut peak_nav = first.nav;
    let mut peak_ts = first.ts;

    let mut trough_nav = first.nav;
    let mut trough_ts = first.ts;

    let mut max_drawdown = Decimal::ZERO;
    let mut max_start = first.ts;
    let mut max_trough = first.ts;
    let mut max_recovery: Option<Ts> = None;
    let mut max_peak_ts = first.ts;

    for p in equity.iter().skip(1) {
        if p.nav > peak_nav {
            // Recovery from current peak
            if max_recovery.is_none() && peak_ts == max_peak_ts {
                max_recovery = Some(p.ts);
            }
            peak_nav = p.nav;
            peak_ts = p.ts;
            trough_nav = p.nav;
            trough_ts = p.ts;
            continue;
        }

        if p.nav < trough_nav {
            trough_nav = p.nav;
            trough_ts = p.ts;
        }

        let dd = (p.nav / peak_nav) - dec!(1.0);
        if dd < max_drawdown {
            max_drawdown = dd;
            max_start = peak_ts;
            max_trough = trough_ts;
            max_peak_ts = peak_ts;
            max_recovery = None;
        }
    }

    let duration_days = (max_trough.0 - max_start.0).num_days();
    let recovery_days = max_recovery.map(|r| (r.0 - max_start.0).num_days());

    let _ = max_drawdown;
    DrawdownStats {
        start: max_start,
        trough: max_trough,
        recovery: max_recovery,
        duration_days,
        recovery_days,
    }
}

fn compute_trade_stats(prepared: &PreparedData) -> (TradeStats, Vec<TradeResult>) {
    let mut trades: HashMap<String, Vec<SignalRow>> = HashMap::new();
    for rows in prepared.signals_by_ts.values() {
        for s in rows {
            trades.entry(s.trade_id.clone()).or_default().push(s.clone());
        }
    }

    let mut calendar_index: HashMap<Ts, usize> = HashMap::new();
    for (i, ts) in prepared.calendar.iter().enumerate() {
        calendar_index.insert(*ts, i);
    }

    let mut results: Vec<TradeResult> = Vec::new();
    let mut holding_days: Vec<i64> = Vec::new();
    let mut win_sum = Decimal::ZERO;
    let mut loss_sum = Decimal::ZERO;
    let mut wins = 0usize;
    let mut losses = 0usize;

    let mut long_wins = 0usize;
    let mut long_total = 0usize;
    let mut short_wins = 0usize;
    let mut short_total = 0usize;

    for (trade_id, mut rows) in trades {
        rows.sort_by_key(|r| r.ts);
        let entry = rows.first().unwrap().ts;
        let exit = rows.last().unwrap().ts;

        let entry_idx = calendar_index.get(&entry).cloned().unwrap_or(0);
        let exit_idx = calendar_index
            .get(&exit)
            .cloned()
            .unwrap_or(entry_idx);

        let hold_days = (exit_idx as i64 - entry_idx as i64) + 1;

        let mut rows_by_ts: BTreeMap<Ts, Vec<SignalRow>> = BTreeMap::new();
        for r in &rows {
            rows_by_ts.entry(r.ts).or_default().push(r.clone());
        }

        // Leg hit ratios (entry to exit)
        let mut sec_rows: HashMap<String, Vec<SignalRow>> = HashMap::new();
        for r in &rows {
            sec_rows.entry(r.security_id.clone()).or_default().push(r.clone());
        }
        for (_sec, mut srows) in sec_rows {
            srows.sort_by_key(|r| r.ts);
            let entry_row = srows.first().unwrap();
            let exit_row = srows.last().unwrap();
            if entry_row.delta_weight == Decimal::ZERO {
                continue;
            }
            let entry_close = match prepared
                .marks
                .get(&(entry_row.ts, entry_row.security_id.clone()))
            {
                Some(m) => *m,
                None => continue,
            };
            let entry_exec = entry_row.fill_mark_override.unwrap_or(entry_close);
            let exit_close = match prepared
                .marks
                .get(&(exit_row.ts, exit_row.security_id.clone()))
            {
                Some(m) => *m,
                None => continue,
            };

            if entry_row.delta_weight > Decimal::ZERO {
                long_total += 1;
                let leg_ret = (exit_close / entry_exec) - dec!(1.0);
                if leg_ret > Decimal::ZERO {
                    long_wins += 1;
                }
            } else {
                short_total += 1;
                let leg_ret = (entry_exec / exit_close) - dec!(1.0);
                if leg_ret > Decimal::ZERO {
                    short_wins += 1;
                }
            }
        }

        // Simulate trade return
        let mut nav = dec!(1.0);
        let mut portfolio = PortfolioState::new();

        let end_idx = exit_idx.min(prepared.calendar.len().saturating_sub(1));
        for idx in entry_idx..=end_idx {
            let ts = prepared.calendar[idx];

            if let Some(ts_rows) = rows_by_ts.get(&ts) {
                let mut immediate_return = Decimal::ZERO;
                for s in ts_rows {
                    let close_mark = match prepared
                        .marks
                        .get(&(s.ts, s.security_id.clone()))
                    {
                        Some(m) => *m,
                        None => continue,
                    };
                    let exec_mark = s.fill_mark_override.unwrap_or(close_mark);
                    if exec_mark > Decimal::ZERO {
                        immediate_return += s.delta_weight * ((close_mark / exec_mark) - dec!(1.0));
                    }
                    portfolio.apply_signal(s, exec_mark);
                }
                nav = nav * (dec!(1.0) + immediate_return);
            }

            if idx >= end_idx {
                break;
            }

            let ts_next = prepared.calendar[idx + 1];
            let mut interval_return = Decimal::ZERO;
            for (sec, w) in portfolio.weights.iter() {
                if *w == Decimal::ZERO {
                    continue;
                }
                let m_now = match prepared.marks.get(&(ts, sec.clone())) {
                    Some(m) => *m,
                    None => continue,
                };
                let m_next = match prepared.marks.get(&(ts_next, sec.clone())) {
                    Some(m) => *m,
                    None => continue,
                };
                interval_return += *w * ((m_next / m_now) - dec!(1.0));
            }
            nav = nav * (dec!(1.0) + interval_return);
        }

        let trade_return = nav - dec!(1.0);
        results.push(TradeResult {
            trade_id,
            entry,
            exit,
            trade_return,
            holding_days: hold_days,
        });
        holding_days.push(hold_days);

        if trade_return > Decimal::ZERO {
            wins += 1;
            win_sum += trade_return;
        } else if trade_return < Decimal::ZERO {
            losses += 1;
            loss_sum += trade_return;
        }
    }

    let total = results.len();
    let win_rate = if total > 0 {
        Decimal::from_f64_retain(wins as f64 / total as f64).unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let avg_win = if wins > 0 {
        win_sum / Decimal::from(wins)
    } else {
        Decimal::ZERO
    };
    let avg_loss = if losses > 0 {
        loss_sum / Decimal::from(losses)
    } else {
        Decimal::ZERO
    };
    let payoff_ratio = if avg_loss != Decimal::ZERO {
        avg_win / avg_loss.abs()
    } else {
        Decimal::ZERO
    };
    let profit_factor = if loss_sum != Decimal::ZERO {
        win_sum / loss_sum.abs()
    } else {
        Decimal::ZERO
    };

    let mut returns_f64 = Vec::new();
    for r in &results {
        if let Some(v) = r.trade_return.to_f64() {
            returns_f64.push(v);
        }
    }

    let mean_return = if !returns_f64.is_empty() {
        let sum: f64 = returns_f64.iter().sum();
        Decimal::from_f64_retain(sum / returns_f64.len() as f64).unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let std_return = if returns_f64.len() > 1 {
        let mean = mean_return.to_f64().unwrap_or(0.0);
        let var = returns_f64
            .iter()
            .map(|v| (v - mean).powi(2))
            .sum::<f64>()
            / returns_f64.len() as f64;
        Decimal::from_f64_retain(var.sqrt()).unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };

    holding_days.sort();
    let avg_hold_days = if !holding_days.is_empty() {
        let sum: i64 = holding_days.iter().sum();
        Decimal::from_i64(sum).unwrap_or(Decimal::ZERO)
            / Decimal::from(holding_days.len())
    } else {
        Decimal::ZERO
    };
    let median_hold_days = if holding_days.is_empty() {
        Decimal::ZERO
    } else if holding_days.len() % 2 == 1 {
        Decimal::from_i64(holding_days[holding_days.len() / 2]).unwrap_or(Decimal::ZERO)
    } else {
        let mid = holding_days.len() / 2;
        let a = holding_days[mid - 1];
        let b = holding_days[mid];
        Decimal::from_i64(a + b).unwrap_or(Decimal::ZERO) / dec!(2.0)
    };

    let long_hit_rate = if long_total > 0 {
        Decimal::from_f64_retain(long_wins as f64 / long_total as f64)
            .unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let short_hit_rate = if short_total > 0 {
        Decimal::from_f64_retain(short_wins as f64 / short_total as f64)
            .unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };

    let stats = TradeStats {
        total,
        wins,
        losses,
        win_rate,
        avg_win,
        avg_loss,
        payoff_ratio,
        profit_factor,
        mean_return,
        std_return,
        avg_hold_days,
        median_hold_days,
        long_hit_rate,
        short_hit_rate,
    };

    (stats, results)
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

fn compute_worst_day(equity: &[EquityPoint]) -> Option<(Ts, Decimal)> {
    if equity.len() < 2 {
        return None;
    }
    let mut worst = Decimal::ZERO;
    let mut worst_ts = equity[1].ts;
    for i in 1..equity.len() {
        let prev = equity[i - 1].nav;
        let cur = equity[i].nav;
        if prev == Decimal::ZERO {
            continue;
        }
        let r = (cur / prev) - dec!(1.0);
        if i == 1 || r < worst {
            worst = r;
            worst_ts = equity[i].ts;
        }
    }
    Some((worst_ts, worst))
}

fn print_summary(
    equity: &[EquityPoint],
    trade_total: usize,
    trade_dropped: usize,
    exposure: ExposureStats,
    turnover: TurnoverStats,
    trade_stats: TradeStats,
    drawdown: DrawdownStats,
    worst_trades: &[TradeResult],
) {
    let stats = BacktestStats::calculate(equity);
    let trade_executed = trade_total.saturating_sub(trade_dropped);
    let execution_rate = if trade_total > 0 {
        (trade_executed as f64 / trade_total as f64) * 100.0
    } else {
        0.0
    };
    let worst_day = compute_worst_day(equity);

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
        Cell::new("Total Signals (unique trade_id)"),
        Cell::new(&format!("{}", trade_total)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Dropped trades (missing marks)"),
        Cell::new(&format!("{}", trade_dropped)).style_spec("Fr"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Executed trades"),
        Cell::new(&format!("{}", trade_executed)).style_spec("Fg"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Execution Rate"),
        Cell::new(&format!("{:.2}%", execution_rate)),
    ]));

    table.add_row(row!["", ""]);

    table.add_row(Row::new(vec![
        Cell::new("Total Turnover (sum |delta_weight|)"),
        Cell::new(&format!("{:.4}", turnover.total)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Daily Turnover"),
        Cell::new(&format!("{:.4}", turnover.avg_daily)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max Daily Turnover"),
        Cell::new(&format!("{:.4}", turnover.max_daily)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Gross Exposure"),
        Cell::new(&format!("{:.4}", exposure.avg_gross)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max Gross Exposure"),
        Cell::new(&format!("{:.4}", exposure.max_gross)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Net Exposure"),
        Cell::new(&format!("{:.4}", exposure.avg_net)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max Net Exposure"),
        Cell::new(&format!("{:.4}", exposure.max_net)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max Abs Position"),
        Cell::new(&format!("{:.4}", exposure.max_abs_pos)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Holding Period (days)"),
        Cell::new(&format!("{:.2}", trade_stats.avg_hold_days)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Median Holding Period (days)"),
        Cell::new(&format!("{:.2}", trade_stats.median_hold_days)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trade Count (executed)"),
        Cell::new(&format!("{}", trade_stats.total)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Win Rate"),
        Cell::new(&format!("{:.2}%", trade_stats.win_rate * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trade Wins"),
        Cell::new(&format!("{}", trade_stats.wins)).style_spec("Fg"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trade Losses"),
        Cell::new(&format!("{}", trade_stats.losses)).style_spec("Fr"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Win"),
        Cell::new(&format!("{:.4}%", trade_stats.avg_win * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Avg Loss"),
        Cell::new(&format!("{:.4}%", trade_stats.avg_loss * dec!(100.0))).style_spec("Fr"),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Payoff Ratio"),
        Cell::new(&format!("{:.4}", trade_stats.payoff_ratio)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Profit Factor"),
        Cell::new(&format!("{:.4}", trade_stats.profit_factor)),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trade Return Mean"),
        Cell::new(&format!("{:.4}%", trade_stats.mean_return * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Trade Return StdDev"),
        Cell::new(&format!("{:.4}%", trade_stats.std_return * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Long Leg Hit Rate"),
        Cell::new(&format!("{:.2}%", trade_stats.long_hit_rate * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Short Leg Hit Rate"),
        Cell::new(&format!("{:.2}%", trade_stats.short_hit_rate * dec!(100.0))),
    ]));

    table.add_row(Row::new(vec![
        Cell::new("Max DD Duration"),
        Cell::new(&format!(
            "{} -> {} ({}d)",
            drawdown.start.0, drawdown.trough.0, drawdown.duration_days
        )),
    ]));

    let recovery_str = match (drawdown.recovery, drawdown.recovery_days) {
        (Some(ts), Some(days)) => format!("{} ({}d)", ts.0, days),
        _ => "unrecovered".to_string(),
    };
    table.add_row(Row::new(vec![
        Cell::new("Max DD Recovery"),
        Cell::new(&recovery_str),
    ]));

    if let Some((ts, r)) = worst_day {
        table.add_row(Row::new(vec![
            Cell::new("Worst Day Return"),
            Cell::new(&format!("{} ({:.4}%)", ts.0, r * dec!(100.0))).style_spec("Fr"),
        ]));
    }

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

    if !worst_trades.is_empty() {
        let mut sorted = worst_trades.to_vec();
        sorted.sort_by(|a, b| a.trade_return.cmp(&b.trade_return));

        println!("\nWorst 5 Trades:");
        let mut worst_table = Table::new();
        worst_table.set_format(*format::consts::FORMAT_BOX_CHARS);
        worst_table.add_row(row![bFc => "Trade ID", "Entry", "Exit", "Hold (d)", "Return"]);

        for t in sorted.into_iter().take(5) {
            worst_table.add_row(Row::new(vec![
                Cell::new(&t.trade_id),
                Cell::new(&t.entry.0.to_string()),
                Cell::new(&t.exit.0.to_string()),
                Cell::new(&format!("{}", t.holding_days)),
                Cell::new(&format!("{:.4}%", t.trade_return * dec!(100.0))).style_spec("Fr"),
            ]));
        }

        worst_table.printstd();
    }

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

    let turnover = compute_turnover_stats(&prepared);
    let (trade_stats, trade_results) = compute_trade_stats(&prepared);
    let (equity, exposure) = compute_equity_curve(&prepared, mark_type)?;
    let drawdown = compute_drawdown_stats(&equity);

    print_summary(
        &equity,
        prepared.trade_count_total,
        prepared.trade_count_dropped,
        exposure,
        turnover,
        trade_stats,
        drawdown,
        &trade_results,
    );

    Ok(())
}
