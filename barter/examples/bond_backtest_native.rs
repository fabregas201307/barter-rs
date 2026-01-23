use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    error::Error,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use barter::{
    backtest::{
        BacktestArgsConstant, BacktestArgsDynamic,
        market_data::{BacktestMarketData, MarketDataInMemory},
        run_backtests,
    },
    engine::{
        Engine, Processor,
        state::{
            EngineState,
            builder::EngineStateBuilder,
            global::DefaultGlobalData,
            instrument::{data::InstrumentDataState, filter::InstrumentFilter},
            order::in_flight_recorder::InFlightRequestRecorder,
            trading::TradingState,
        },
    },
    risk::DefaultRiskManager,
    statistic::{summary::TradingSummary, time::Daily},
    strategy::{
        algo::AlgoStrategy, close_positions::ClosePositionsStrategy,
        on_disconnect::OnDisconnectStrategy, on_trading_disabled::OnTradingDisabled,
    },
    system::config::ExecutionConfig,
};
use barter_data::{event::MarketEvent, streams::consumer::MarketStreamEvent};
use barter_execution::{
    AccountEvent, AccountEventKind, UnindexedAccountSnapshot,
    balance::{AssetBalance, Balance},
    client::mock::MockExecutionConfig,
    order::{
        OrderKey, OrderKind, TimeInForce,
        id::{ClientOrderId, StrategyId},
        request::{OrderRequestCancel, OrderRequestOpen, RequestOpen},
    },
};
use barter_instrument::{
    Side, Underlying, Keyed,
    asset::{Asset, AssetIndex, name::AssetNameExchange},
    exchange::{ExchangeId, ExchangeIndex},
    index::IndexedInstruments,
    instrument::{
        Instrument, InstrumentIndex,
        kind::InstrumentKind,
        quote::InstrumentQuoteAsset,
        spec::{
            InstrumentSpec, InstrumentSpecNotional, InstrumentSpecPrice, InstrumentSpecQuantity,
            OrderQuantityUnits,
        },
    },
};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use smol_str::SmolStr;
use tracing::info;

const DEFAULT_MARKS_PATH: &str = "barter/examples/data/bond_marks_shares.csv";
const DEFAULT_SIGNALS_PATH: &str = "barter/examples/data/bond_signals_shares.csv";
const MARK_TYPE_CLEAN_PRICE: &str = "CLEAN_PRICE";
const SIGNAL_EVENT_HOUR: u32 = 12;
const MARK_EVENT_HOUR: u32 = 21;
const MOCK_EXCHANGE: ExchangeId = ExchangeId::Mock;

type SecurityId = SmolStr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    barter::logging::init_logging();

    let mut args = env::args().skip(1);
    let marks_path = PathBuf::from(
        args.next()
            .unwrap_or_else(|| DEFAULT_MARKS_PATH.to_string()),
    );
    let signals_path = PathBuf::from(
        args.next()
            .unwrap_or_else(|| DEFAULT_SIGNALS_PATH.to_string()),
    );

    println!(
        "Using marks CSV: {} | signals CSV: {}",
        marks_path.display(),
        signals_path.display()
    );

    let marks = read_mark_rows(&marks_path)?;
    let mark_lookup = build_mark_lookup(&marks);
    let signals = read_signal_rows(&signals_path, &mark_lookup)?;

    let security_ids = collect_security_ids(&marks, &signals);
    let (instruments, instrument_lookup) = build_instruments(&security_ids);

    let events = build_market_events(&marks, &signals, &instrument_lookup)?;
    let last_event_time = events
        .iter()
        .filter_map(|event| match event {
            MarketStreamEvent::Item(event) => Some(event.time_exchange),
            _ => None,
        })
        .max()
        .ok_or_else(|| "no market events were generated".to_string())?;
    let market_data = MarketDataInMemory::new(Arc::new(events));
    let engine_start = market_data.time_first_event().await?;

    let engine_state = EngineStateBuilder::new(&instruments, DefaultGlobalData::default(), |item: &Keyed<InstrumentIndex, Instrument<Keyed<ExchangeIndex, ExchangeId>, AssetIndex>>| {
        BondInstrumentData {
            id: SmolStr::new(&item.value.name_internal),
            ..Default::default()
        }
    })
    .time_engine_start(engine_start)
    .trading_state(TradingState::Enabled)
    .build();

    let execution_config =
        build_mock_execution(&security_ids, last_event_time + Duration::milliseconds(1));

    let args_constant = Arc::new(BacktestArgsConstant {
        instruments,
        executions: vec![execution_config],
        market_data,
        summary_interval: Daily,
        engine_state,
    });

    let args_dynamic = BacktestArgsDynamic {
        id: SmolStr::new("bond_native"),
        risk_free_return: Decimal::ZERO,
        strategy: BondSignalStrategy::default(),
        risk: DefaultRiskManager::default(),
    };

    println!(
        "Starting native bond backtest with {} instruments...",
        security_ids.len()
    );
    let summaries = run_backtests(args_constant, std::iter::once(args_dynamic)).await?;

    println!("Backtest duration: {:?}", summaries.duration);
    for summary in summaries.summaries {
        print_compact_summary(&summary.trading_summary);
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct MarkRecord {
    day: NaiveDate,
    time: DateTime<Utc>,
    security_id: SecurityId,
    price: Decimal,
}

#[derive(Debug, Clone)]
struct SignalRecord {
    ordinal: usize,
    time: DateTime<Utc>,
    trade_id: SmolStr,
    security_id: SecurityId,
    delta_shares: i64,
    fill_price: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
struct MarkCsvRow {
    ts: String,
    security_id: String,
    mark_type: String,
    mark: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
struct SignalCsvRow {
    ts: String,
    trade_id: String,
    security_id: String,
    delta_shares: i64,
    #[serde(default)]
    fill_price: Option<Decimal>,
}

#[derive(Debug, Clone)]
struct BondMarkEvent {
    clean_price: Decimal,
}

#[derive(Debug, Clone)]
struct BondSignalEvent {
    signal_id: SmolStr,
    trade_id: SmolStr,
    delta_shares: i64,
    fill_price: Decimal,
}

#[derive(Debug, Clone)]
enum BondEvent {
    Mark(BondMarkEvent),
    Signal(BondSignalEvent),
}

#[derive(Debug, Clone, Default)]
struct BondInstrumentData {
    id: SmolStr,
    last_price: Decimal,
    last_signal: Option<BondSignalEvent>,
}

impl InstrumentDataState for BondInstrumentData {
    type MarketEventKind = BondEvent;

    fn price(&self) -> Option<Decimal> {
         // Debug log occasionally? No, might span too much.
         if self.last_price.is_zero() {
             None
         } else {
             Some(self.last_price)
         }
    }
}

impl Processor<&MarketEvent<InstrumentIndex, BondEvent>> for BondInstrumentData {
    type Audit = ();

    fn process(&mut self, event: &MarketEvent<InstrumentIndex, BondEvent>) -> Self::Audit {
        match &event.kind {
            BondEvent::Mark(mark) => {
                if self.id == "440625658" || self.id == "002007565" {
                    info!(target: "bond_native", "MARK UPDATE for {}: {} -> {}", self.id, self.last_price, mark.clean_price);
                }
                // Info log for first few instruments to verify
                if self.last_price.is_zero() {
                     info!(target: "bond_native", "PRICE UPDATE FIRST TIME: {} -> {}", self.last_price, mark.clean_price);
                }
                self.last_price = mark.clean_price;
            }
            BondEvent::Signal(signal) => {
                if self.id == "440625658" || self.id == "002007565" {
                    info!(target: "bond_native", "SIGNAL RECV for {}: trade_id={}, shares={}", self.id, signal.trade_id, signal.delta_shares);
                }
                self.last_signal = Some(signal.clone());
            }
        }
    }
}

impl<ExchangeKey, AssetKey, InstrumentKey>
    Processor<&AccountEvent<ExchangeKey, AssetKey, InstrumentKey>> for BondInstrumentData
where
    ExchangeKey: Clone,
    AssetKey: Clone,
    InstrumentKey: Clone,
{
    type Audit = ();

    fn process(
        &mut self,
        _event: &AccountEvent<ExchangeKey, AssetKey, InstrumentKey>,
    ) -> Self::Audit {
        if let AccountEventKind::Trade(trade) = &_event.kind {
            if self.id == "440625658" || self.id == "002007565" {
                info!(target: "bond_native", "TRADE FILL for {}: {:?} {} @ {}", self.id, trade.side, trade.quantity, trade.price);
            }
            info!(target: "bond_native", "received trade fill: {:?} {} @ {}", trade.side, trade.quantity, trade.price);
        }
    }
}

impl<ExchangeKey, InstrumentKey> InFlightRequestRecorder<ExchangeKey, InstrumentKey>
    for BondInstrumentData
{
    fn record_in_flight_cancel(
        &mut self,
        _request: &barter_execution::order::request::OrderRequestCancel<ExchangeKey, InstrumentKey>,
    ) {
    }

    fn record_in_flight_open(
        &mut self,
        _request: &barter_execution::order::request::OrderRequestOpen<ExchangeKey, InstrumentKey>,
    ) {
    }
}

#[derive(Clone)]
struct BondSignalStrategy {
    processed: Arc<Mutex<HashSet<SmolStr>>>,
}

impl Default for BondSignalStrategy {
    fn default() -> Self {
        Self {
            processed: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl std::fmt::Debug for BondSignalStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondSignalStrategy").finish_non_exhaustive()
    }
}

impl AlgoStrategy for BondSignalStrategy {
    type State = EngineState<DefaultGlobalData, BondInstrumentData>;

    fn generate_algo_orders(
        &self,
        state: &Self::State,
    ) -> (
        impl IntoIterator<Item = OrderRequestCancel<ExchangeIndex, InstrumentIndex>>,
        impl IntoIterator<Item = OrderRequestOpen<ExchangeIndex, InstrumentIndex>>,
    ) {
        let mut open_requests = Vec::new();
        let mut processed = self.processed.lock().expect("strategy state poisoned");

        for instrument_state in state.instruments.0.values() {
            let Some(signal) = &instrument_state.data.last_signal else {
                continue;
            };

            let is_target = instrument_state.data.id == "440625658" || instrument_state.data.id == "002007565";

            if !processed.insert(signal.signal_id.clone()) {
                // if is_target {
                //     info!(target: "bond_native", "SKIPPING signal for {}: duplicate signal_id={}", instrument_state.data.id, signal.signal_id);
                // }
                continue;
            }

            if signal.delta_shares == 0 {
                if is_target {
                    info!(target: "bond_native", "SKIPPING signal for {}: delta_shares is 0", instrument_state.data.id);
                }
                continue;
            }

            let side = if signal.delta_shares.is_positive() {
                Side::Buy
            } else {
                Side::Sell
            };
            let quantity = Decimal::from(signal.delta_shares.abs());
            if quantity.is_zero() {
                 if is_target {
                    info!(target: "bond_native", "SKIPPING signal for {}: quantity is 0 from delta_shares={}", instrument_state.data.id, signal.delta_shares);
                }
                continue;
            }

            if signal.fill_price <= Decimal::ZERO {
                 if is_target {
                    info!(target: "bond_native", "SKIPPING signal for {}: fill_price <= 0 ({})", instrument_state.data.id, signal.fill_price);
                }
                continue;
            }

            open_requests.push(OrderRequestOpen {
                key: OrderKey {
                    exchange: ExchangeIndex(0),
                    instrument: instrument_state.key,
                    strategy: StrategyId::new(signal.trade_id.clone()),
                    cid: ClientOrderId::random(),
                },
                state: RequestOpen {
                    side,
                    price: signal.fill_price,
                    quantity,
                    kind: OrderKind::Market,
                    time_in_force: TimeInForce::ImmediateOrCancel,
                },
            });
            info!(
                target: "bond_native",
                "emitting {:?} order for {} shares of {} at {}",
                side,
                quantity,
                signal.trade_id,
                signal.fill_price
            );
        }

        drop(processed);

        (
            Vec::<OrderRequestCancel<ExchangeIndex, InstrumentIndex>>::new(),
            open_requests,
        )
    }
}

impl ClosePositionsStrategy for BondSignalStrategy {
    type State = EngineState<DefaultGlobalData, BondInstrumentData>;

    fn close_positions_requests<'a>(
        &'a self,
        _state: &'a Self::State,
        _filter: &'a InstrumentFilter<ExchangeIndex, AssetIndex, InstrumentIndex>,
    ) -> (
        impl IntoIterator<Item = OrderRequestCancel<ExchangeIndex, InstrumentIndex>> + 'a,
        impl IntoIterator<Item = OrderRequestOpen<ExchangeIndex, InstrumentIndex>> + 'a,
    )
    where
        ExchangeIndex: 'a,
        AssetIndex: 'a,
        InstrumentIndex: 'a,
    {
        (
            Vec::<OrderRequestCancel<ExchangeIndex, InstrumentIndex>>::new(),
            Vec::<OrderRequestOpen<ExchangeIndex, InstrumentIndex>>::new(),
        )
    }
}

impl<Clock, ExecutionTxs, Risk>
    OnDisconnectStrategy<
        Clock,
        EngineState<DefaultGlobalData, BondInstrumentData>,
        ExecutionTxs,
        Risk,
    > for BondSignalStrategy
{
    type OnDisconnect = ();

    fn on_disconnect(
        _engine: &mut Engine<
            Clock,
            EngineState<DefaultGlobalData, BondInstrumentData>,
            ExecutionTxs,
            Self,
            Risk,
        >,
        _exchange: ExchangeId,
    ) -> Self::OnDisconnect {
    }
}

impl<Clock, ExecutionTxs, Risk>
    OnTradingDisabled<Clock, EngineState<DefaultGlobalData, BondInstrumentData>, ExecutionTxs, Risk>
    for BondSignalStrategy
{
    type OnTradingDisabled = ();

    fn on_trading_disabled(
        _engine: &mut Engine<
            Clock,
            EngineState<DefaultGlobalData, BondInstrumentData>,
            ExecutionTxs,
            Self,
            Risk,
        >,
    ) -> Self::OnTradingDisabled {
    }
}

fn read_mark_rows(path: &Path) -> Result<Vec<MarkRecord>, Box<dyn Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut rows = Vec::new();

    for row in reader.deserialize::<MarkCsvRow>() {
        let row = row?;
        if row.mark_type.trim() != MARK_TYPE_CLEAN_PRICE {
            continue;
        }
        let day = parse_ts_to_date(&row.ts)?;
        let time = to_utc(day, MARK_EVENT_HOUR);
        rows.push(MarkRecord {
            day,
            time,
            security_id: SmolStr::new(row.security_id),
            price: row.mark,
        });
    }

    println!("Read {} mark rows from CSV", rows.len());

    Ok(rows)
}

fn read_signal_rows(
    path: &Path,
    mark_lookup: &HashMap<(NaiveDate, SecurityId), Decimal>,
) -> Result<Vec<SignalRecord>, Box<dyn Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut rows = Vec::new();

    for (ordinal, row) in reader.deserialize::<SignalCsvRow>().enumerate() {
        let row = row?;
        let day = parse_ts_to_date(&row.ts)?;
        let time = to_utc(day, SIGNAL_EVENT_HOUR);
        let security_id = SmolStr::new(row.security_id);
        let fill_price = row
            .fill_price
            .or_else(|| mark_lookup.get(&(day, security_id.clone())).cloned())
            .ok_or_else(|| format!("missing reference mark for {} on {}", security_id, day))?;

        rows.push(SignalRecord {
            ordinal,
            time,
            trade_id: SmolStr::new(row.trade_id),
            security_id,
            delta_shares: row.delta_shares,
            fill_price,
        });
    }

    Ok(rows)
}

fn build_mark_lookup(marks: &[MarkRecord]) -> HashMap<(NaiveDate, SecurityId), Decimal> {
    let mut lookup = HashMap::new();
    for mark in marks {
        lookup.insert((mark.day, mark.security_id.clone()), mark.price);
    }
    lookup
}

fn collect_security_ids(marks: &[MarkRecord], signals: &[SignalRecord]) -> BTreeSet<SecurityId> {
    let mut ids = BTreeSet::new();
    for mark in marks {
        ids.insert(mark.security_id.clone());
    }
    for signal in signals {
        ids.insert(signal.security_id.clone());
    }
    ids
}

fn build_instruments(
    security_ids: &BTreeSet<SecurityId>,
) -> (IndexedInstruments, HashMap<SecurityId, InstrumentIndex>) {
    let quote_asset = Asset::new("USD", "USD");
    let mut instruments = Vec::with_capacity(security_ids.len());
    let mut lookup = HashMap::new();

    for (index, security_id) in security_ids.iter().cloned().enumerate() {
        let asset = Asset::new(security_id.as_str(), security_id.as_str());
        let instrument = Instrument::new(
            MOCK_EXCHANGE,
            security_id.to_string(),
            security_id.to_string(),
            Underlying::new(asset.clone(), quote_asset.clone()),
            InstrumentQuoteAsset::UnderlyingQuote,
            InstrumentKind::Spot,
            Some(InstrumentSpec::new(
                InstrumentSpecPrice::new(Decimal::ZERO, Decimal::from(1)),
                InstrumentSpecQuantity::new(
                    OrderQuantityUnits::Asset(asset.clone()),
                    Decimal::ZERO,
                    Decimal::from(1),
                ),
                InstrumentSpecNotional::new(Decimal::ZERO),
            )),
        );
        lookup.insert(security_id.clone(), InstrumentIndex(index));
        instruments.push(instrument);
    }

    (IndexedInstruments::new(instruments), lookup)
}

fn build_market_events(
    marks: &[MarkRecord],
    signals: &[SignalRecord],
    instrument_lookup: &HashMap<SecurityId, InstrumentIndex>,
) -> Result<Vec<MarketStreamEvent<InstrumentIndex, BondEvent>>, Box<dyn Error>> {
    let mut events = Vec::with_capacity(marks.len() + signals.len());

    for mark in marks {
        let instrument = *instrument_lookup
            .get(&mark.security_id)
            .ok_or_else(|| format!("missing instrument for {}", mark.security_id))?;
        let event = MarketStreamEvent::Item(MarketEvent {
            time_exchange: mark.time,
            time_received: mark.time,
            exchange: MOCK_EXCHANGE,
            instrument,
            kind: BondEvent::Mark(BondMarkEvent {
                clean_price: mark.price,
            }),
        });
        events.push((mark.time, event));
    }
    
    println!("Built {} market events from marks", marks.len());

    for signal in signals {
        let instrument = *instrument_lookup
            .get(&signal.security_id)
            .ok_or_else(|| format!("missing instrument for {}", signal.security_id))?;
        let signal_id = format!(
            "{}_{}_{}",
            signal.trade_id, signal.security_id, signal.ordinal
        );
        let event = MarketStreamEvent::Item(MarketEvent {
            time_exchange: signal.time,
            time_received: signal.time,
            exchange: MOCK_EXCHANGE,
            instrument,
            kind: BondEvent::Signal(BondSignalEvent {
                signal_id: SmolStr::new(signal_id),
                trade_id: signal.trade_id.clone(),
                delta_shares: signal.delta_shares,
                fill_price: signal.fill_price,
            }),
        });
        events.push((signal.time, event));
    }

    events.sort_by(|(a_time, _), (b_time, _)| a_time.cmp(b_time));
    Ok(events.into_iter().map(|(_, evt)| evt).collect())
}

fn build_mock_execution(
    security_ids: &BTreeSet<SecurityId>,
    snapshot_time: DateTime<Utc>,
) -> ExecutionConfig {
    // Account snapshots may be delivered after market ticks, so timestamp them after the
    // final market event to avoid HistoricalClock ordering violations.
    let balance_time = snapshot_time;
    let mut balances = Vec::new();

    balances.push(AssetBalance {
        asset: AssetNameExchange::from("USD"),
        balance: Balance {
            total: dec!(100_000_000),
            free: dec!(100_000_000),
        },
        time_exchange: balance_time,
    });

    for security_id in security_ids {
        balances.push(AssetBalance {
            asset: AssetNameExchange::from(security_id.as_str()),
            balance: Balance {
                total: Decimal::ZERO,
                free: Decimal::ZERO,
            },
            time_exchange: balance_time,
        });
    }

    ExecutionConfig::Mock(MockExecutionConfig::new(
        MOCK_EXCHANGE,
        UnindexedAccountSnapshot::new(MOCK_EXCHANGE, balances, vec![]),
        0,
        Decimal::ZERO,
    ))
}

fn parse_ts_to_date(ts: &str) -> Result<NaiveDate, Box<dyn Error>> {
    if let Ok(date) = NaiveDate::parse_from_str(ts.trim(), "%Y-%m-%d") {
        return Ok(date);
    }

    let dt = NaiveDateTime::parse_from_str(ts.trim(), "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(ts.trim(), "%Y-%m-%d %H:%M:%S"))?;
    Ok(dt.date())
}

fn to_utc(day: NaiveDate, hour: u32) -> DateTime<Utc> {
    let datetime = day
        .and_hms_opt(hour, 0, 0)
        .expect("invalid hour provided for naive date");
    Utc.from_utc_datetime(&datetime)
}

fn print_compact_summary<Interval>(summary: &TradingSummary<Interval>) {
    let duration = summary.trading_duration();
    let days = duration.num_days();
    let hours = duration.num_hours() % 24;
    let minutes = duration.num_minutes() % 60;
    let instrument_count = summary.instruments.len();
    let asset_count = summary.assets.len();
    let total_pnl: Decimal = summary.instruments.values().map(|sheet| sheet.pnl).sum();
    let winners = summary
        .instruments
        .values()
        .filter(|sheet| sheet.pnl > Decimal::ZERO)
        .count();
    let losers = instrument_count.saturating_sub(winners);

    println!("\n===== Bond Native Trading Summary =====");
    println!(
        "Window: {} -> {} ({}d {}h {}m)",
        summary.time_engine_start, summary.time_engine_end, days, hours, minutes
    );
    println!(
        "Coverage: {} instruments | {} assets | aggregate PnL = {:+.4}",
        instrument_count, asset_count, total_pnl
    );
    println!("Split: {winners} winners / {losers} losers");

    if instrument_count == 0 {
        println!("No instrument tear sheets were produced.");
        return;
    }

    let mut sorted: Vec<_> = summary.instruments.iter().collect();
    sorted.sort_by(|(_, a), (_, b)| b.pnl.cmp(&a.pnl));

    println!("Top 5 PnL movers:");
    for (name, sheet) in sorted.iter().take(5) {
        println!(
            "  {:<32} pnl={:+.4} sharpe={} win_rate={} pf={}",
            name,
            sheet.pnl,
            format_ratio(sheet.sharpe_ratio.value),
            format_percent_option(sheet.win_rate.as_ref().map(|w| w.value)),
            sheet
                .profit_factor
                .as_ref()
                .map(|pf| format!("{:.2}", pf.value))
                .unwrap_or_else(|| "n/a".to_string())
        );
    }

    println!("Bottom 5 PnL movers:");
    for (name, sheet) in sorted.iter().rev().take(5) {
        println!(
            "  {:<32} pnl={:+.4} sharpe={} win_rate={} pf={}",
            name,
            sheet.pnl,
            format_ratio(sheet.sharpe_ratio.value),
            format_percent_option(sheet.win_rate.as_ref().map(|w| w.value)),
            sheet
                .profit_factor
                .as_ref()
                .map(|pf| format!("{:.2}", pf.value))
                .unwrap_or_else(|| "n/a".to_string())
        );
    }

    let mut balances: Vec<_> = summary
        .assets
        .iter()
        .filter_map(|(asset, sheet)| sheet.balance_end.as_ref().map(|balance| (asset, balance)))
        .collect();
    if balances.is_empty() {
        println!("No asset balances available from mock execution.");
        return;
    }

    balances.sort_by(|(_, a), (_, b)| b.total.cmp(&a.total));
    println!("Cash balances (top 5):");
    for (asset, balance) in balances.iter().take(5) {
        println!(
            "  {:<24} total={:.2} free={:.2}",
            format!("{}:{}", asset.exchange.as_str(), asset.asset),
            balance.total,
            balance.free
        );
    }
}

fn format_percent_option(option: Option<Decimal>) -> String {
    option
        .map(|value| format!("{:.1}%", value * dec!(100)))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_ratio(value: Decimal) -> String {
    if value == Decimal::MAX {
        "inf".to_string()
    } else if value == Decimal::MIN {
        "-inf".to_string()
    } else {
        format!("{value:.2}")
    }
}
