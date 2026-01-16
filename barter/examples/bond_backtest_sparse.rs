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
            instrument::data::{InstrumentDataState},
            order::in_flight_recorder::InFlightRequestRecorder,
            trading::TradingState,
             instrument::filter::InstrumentFilter,
        },
    },
    risk::DefaultRiskManager,
    statistic::time::Daily,
    strategy::{
        algo::AlgoStrategy,
        close_positions::ClosePositionsStrategy,
        on_disconnect::OnDisconnectStrategy,
        on_trading_disabled::OnTradingDisabled,
    },
    system::config::ExecutionConfig,
};
use barter_data::{
    event::MarketEvent,
    streams::consumer::MarketStreamEvent,
    subscription::{trade::PublicTrade, book::OrderBookL1},
};
use barter_execution::{
    AccountEvent, UnindexedAccountSnapshot,
    balance::{AssetBalance, Balance},
    client::mock::MockExecutionConfig,
    order::{
        request::{OrderRequestCancel, OrderRequestOpen, RequestOpen},
        OrderKind, OrderKey, TimeInForce,
        id::{StrategyId, ClientOrderId},
    },
};
use barter_instrument::{
    index::IndexedInstruments,
    instrument::{
        Instrument, InstrumentIndex, kind::InstrumentKind, 
        spec::{InstrumentSpec, InstrumentSpecPrice, InstrumentSpecQuantity, InstrumentSpecNotional, OrderQuantityUnits}, 
        quote::InstrumentQuoteAsset
    },
    asset::{Asset, name::AssetNameExchange},
    exchange::{ExchangeId, ExchangeIndex},
    Underlying, Side,
    asset::AssetIndex,
};
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::{sync::Arc, fmt::{self, Debug}};
use chrono::{Utc, TimeZone};

// -----------------------------------------------------------
// 1. Bond Event Customization
// -----------------------------------------------------------

#[derive(Clone, PartialEq, Debug, Deserialize, Serialize)]
pub enum BondEvent {
    Trade(PublicTrade),
    AlphaSignal { signal_strength: f64 },
}

impl From<PublicTrade> for BondEvent {
    fn from(trade: PublicTrade) -> Self {
        BondEvent::Trade(trade)
    }
}
impl From<OrderBookL1> for BondEvent {
    fn from(_: OrderBookL1) -> Self {
        panic!("OrderBook not supported in this example")
    }
}

// -----------------------------------------------------------
// 2. Instrument Data
// -----------------------------------------------------------

#[derive(Clone, PartialEq, Debug, Default, Deserialize, Serialize)]
pub struct BondInstrumentData {
    pub last_price: Decimal,
    pub last_signal: Option<f64>,
}

impl InstrumentDataState for BondInstrumentData {
    type MarketEventKind = BondEvent;
    fn price(&self) -> Option<Decimal> {
         if self.last_price.is_zero() { None } else { Some(self.last_price) }
    }
}

// Implement Processor for MarketEvent
impl Processor<&MarketEvent<InstrumentIndex, BondEvent>> for BondInstrumentData {
    type Audit = ();
    fn process(&mut self, event: &MarketEvent<InstrumentIndex, BondEvent>) -> Self::Audit {
        match &event.kind {
            BondEvent::Trade(trade) => {
                self.last_price = Decimal::from_f64(trade.price).unwrap_or_default();
            }
            BondEvent::AlphaSignal { signal_strength } => {
                self.last_signal = Some(*signal_strength);
            }
        }
    }
}

// Implement boilerplate Processor for AccountEvent (required by EngineState)
impl<ExchangeKey, AssetKey, InstrumentKey> Processor<&AccountEvent<ExchangeKey, AssetKey, InstrumentKey>> for BondInstrumentData {
    type Audit = ();
    fn process(&mut self, _event: &AccountEvent<ExchangeKey, AssetKey, InstrumentKey>) -> Self::Audit {}
}

// Implement boilerplate InFlightRequestRecorder (required by EngineState)
impl<ExchangeKey, InstrumentKey> InFlightRequestRecorder<ExchangeKey, InstrumentKey> for BondInstrumentData {
    fn record_in_flight_cancel(&mut self, _request: &OrderRequestCancel<ExchangeKey, InstrumentKey>) {}
    fn record_in_flight_open(&mut self, _request: &OrderRequestOpen<ExchangeKey, InstrumentKey>) {}
}

// -----------------------------------------------------------
// 3. Strategy
// -----------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct BondStrategy { 
}

impl AlgoStrategy for BondStrategy {
    type State = EngineState<DefaultGlobalData, BondInstrumentData>;

    fn generate_algo_orders(
        &self,
        state: &Self::State,
    ) -> (
        impl IntoIterator<Item = OrderRequestCancel<ExchangeIndex, InstrumentIndex>>,
        impl IntoIterator<Item = OrderRequestOpen<ExchangeIndex, InstrumentIndex>>,
    ) {
         let mut open = Vec::new();
         // Use enumerate to get valid InstrumentIndex
         for (i, data) in state.instruments.0.values().enumerate() {
             if let Some(signal) = data.data.last_signal {
                 if signal > 0.9 {
                     
                     let request_state = RequestOpen {
                         side: Side::Buy,
                         price: data.data.last_price,
                         quantity: Decimal::from(1),
                         kind: OrderKind::Market,
                         time_in_force: TimeInForce::FillOrKill, 
                     };
                     
                     open.push(OrderRequestOpen {
                         key: OrderKey {
                            exchange: ExchangeIndex(0), 
                            instrument: InstrumentIndex(i),
                            strategy: StrategyId::new("bond_strat"),
                            cid: ClientOrderId::random(),
                         }, 
                         state: request_state,
                     });
                 }
             }
         }
         (vec![], open)
    }
}

impl ClosePositionsStrategy for BondStrategy {
    type State = EngineState<DefaultGlobalData, BondInstrumentData>;
    fn close_positions_requests<'a>(
        &'a self,
        _state: &'a Self::State,
        _filter: &'a InstrumentFilter<ExchangeIndex, AssetIndex, InstrumentIndex>,
    ) -> (
        impl IntoIterator<Item = OrderRequestCancel<ExchangeIndex, InstrumentIndex>> + 'a,
        impl IntoIterator<Item = OrderRequestOpen<ExchangeIndex, InstrumentIndex>> + 'a,
    ) 
    where ExchangeIndex: 'a, AssetIndex: 'a, InstrumentIndex: 'a
    {
        (vec![], vec![])
    }
}

impl<Clock, ExecutionTxs, Risk> OnDisconnectStrategy<Clock, EngineState<DefaultGlobalData, BondInstrumentData>, ExecutionTxs, Risk> for BondStrategy {
    type OnDisconnect = ();
    fn on_disconnect(
        _engine: &mut Engine<Clock, EngineState<DefaultGlobalData, BondInstrumentData>, ExecutionTxs, Self, Risk>, 
        _exchange: ExchangeId
    ) -> Self::OnDisconnect {}
}

impl<Clock, ExecutionTxs, Risk> OnTradingDisabled<Clock, EngineState<DefaultGlobalData, BondInstrumentData>, ExecutionTxs, Risk> for BondStrategy {
    type OnTradingDisabled = ();
    fn on_trading_disabled(
        _engine: &mut Engine<Clock, EngineState<DefaultGlobalData, BondInstrumentData>, ExecutionTxs, Self, Risk>
    ) -> Self::OnTradingDisabled {}
}

// -----------------------------------------------------------
// 4. Main
// -----------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    barter::logging::init_logging();

    // A. Instrument
    let bond_asset = Asset::new("US_TREASURY_10Y", "US_TREASURY_10Y");
    let quote_asset = Asset::new("USD", "USD");
    
    // Manual String conversion for name since From not implemented
    let name_internal = bond_asset.name_internal.to_string(); 
    let name_exchange = bond_asset.name_exchange.to_string();

    let bond_instrument = Instrument::new(
        ExchangeId::BinanceSpot, 
        name_internal,
        name_exchange,
        Underlying::new(bond_asset.clone(), quote_asset.clone()), 
        InstrumentQuoteAsset::UnderlyingQuote,
        InstrumentKind::Spot,
        Some(InstrumentSpec::new(
            InstrumentSpecPrice::new(Decimal::from(0), Decimal::from(1)),
            InstrumentSpecQuantity::new(OrderQuantityUnits::Asset(bond_asset.clone()), Decimal::from(0), Decimal::from(1)),
            InstrumentSpecNotional::new(Decimal::from(0)),
        )), 
    );
    let instruments = IndexedInstruments::new(vec![bond_instrument]);

    // B. Data
    let t0 = Utc.with_ymd_and_hms(2023, 1, 1, 10, 0, 0).unwrap();
    let t1 = Utc.with_ymd_and_hms(2023, 1, 5, 10, 0, 0).unwrap();
    
    // Synthetic Events
    let events = vec![
        MarketStreamEvent::Item(MarketEvent {
            time_exchange: t0,
            time_received: t0,
            exchange: ExchangeId::BinanceSpot,
            instrument: InstrumentIndex(0),
            kind: BondEvent::Trade(PublicTrade {
                id: "1".to_string(),
                price: 100.0,
                amount: 1000.0,
                side: Side::Buy,
            })
        }),
         MarketStreamEvent::Item(MarketEvent {
            time_exchange: t1,
            time_received: t1,
            exchange: ExchangeId::BinanceSpot,
            instrument: InstrumentIndex(0),
            kind: BondEvent::AlphaSignal { signal_strength: 0.95 },
        }),
    ];

    let market_data = MarketDataInMemory::new(Arc::new(events));
    let time_engine_start = market_data.time_first_event().await.unwrap();

    // C. Engine State
    let engine_state = EngineStateBuilder::new(&instruments, DefaultGlobalData::default(), |_| {
        BondInstrumentData::default()
    })
    .time_engine_start(time_engine_start)
    .trading_state(TradingState::Enabled)
    .build();

    // D. Execution Config
    // We need to provide initial balances for the Mock Exchange.
    // Since we are BUYING, we need Quote Asset (USD) balance.
    // The MockExchange also validates that we have balance entries for ALL assets in the instruments.
    // IMPORTANT: The MockExchange uses these balances to initialize its state.
    // However, when the Engine starts, the MockExchange connects and sends an initial AccountSnapshot.
    // The Engine's Clock processes this Snapshot.
    // If we set the balance `time_exchange` to `Utc::now()` (which is year 2026), it will be WAY ahead
    // of our historical backtest data (which is year 2023).
    // This causes the "HistoricalClock received out-of-order events" error because the Engine thinks time has jumped to 2026,
    // and then it tries to process 2023 market data and fails/skips it.
    // To fix this, we set the initial balance time to be BEFORE our backtest start time (t0).
    let time_balance_init = t0 - chrono::Duration::hours(1);
    let balances = vec![
        AssetBalance {
            asset: AssetNameExchange::from("USD"),
            balance: Balance {
                total: Decimal::from(100_000),
                free: Decimal::from(100_000),
            },
            time_exchange: time_balance_init,
        },
        AssetBalance {
            asset: AssetNameExchange::from("US_TREASURY_10Y"),
            balance: Balance {
                total: Decimal::from(0),
                free: Decimal::from(0),
            },
            time_exchange: time_balance_init,
        }
    ];

    let execution_config = ExecutionConfig::Mock(MockExecutionConfig::new(
        ExchangeId::BinanceSpot,
        UnindexedAccountSnapshot::new(ExchangeId::BinanceSpot, balances, vec![]),
        100, // latency
        Decimal::ZERO, // fees
    ));

    // E. Run Backtest
    let args_constant = Arc::new(BacktestArgsConstant {
        instruments,
        executions: vec![execution_config],
        market_data,
        summary_interval: Daily,
        engine_state,
    });

    let args_dynamic = BacktestArgsDynamic {
        id: SmolStr::new("bond_test"),
        risk_free_return: Decimal::ZERO,
        strategy: BondStrategy::default(),
        risk: DefaultRiskManager::default(),
    };

    println!("Starting Sparse Bond Backtest...");
    let summary = run_backtests(args_constant, std::iter::once(args_dynamic)).await?;
    
    println!("Backtest Success! Duration: {:?}", summary.duration);
    for backtest_summary in summary.summaries {
        backtest_summary.trading_summary.print_summary();
    }
    
    Ok(())
}
