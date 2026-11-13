use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::hl::types::{AssetPosition, Leverage, MarginSummary, Position};
use crate::strategy::signal::Purpose;

const T0: u64 = 1_800_000_000_000;

#[derive(Default)]
struct Fake {
    place_replies: Mutex<VecDeque<Result<OrderStatus, ExchangeError>>>,
    cancel_replies: Mutex<VecDeque<Result<CancelStatus, ExchangeError>>>,
    statuses: Mutex<HashMap<String, RemoteStatus>>,
    open: Mutex<Vec<OpenOrder>>,
    fills: Mutex<Vec<Fill>>,
    szi: Mutex<f64>,
    placed: Mutex<Vec<OrderRequest>>,
    canceled: Mutex<Vec<Cloid>>,
    leverage_calls: Mutex<Vec<u32>>,
    funding: Mutex<Vec<(u64, f64)>>,
    /// Coin of the fake position (default xyz:ANT).
    pos_coin: Mutex<Option<String>>,
    /// Filled size reported by orderStatus, by cloid.
    filled: Mutex<HashMap<String, f64>>,
    /// Margin mode / leverage of the fake position (default isolated 1x).
    pos_leverage: Mutex<Option<(String, u32)>>,
    /// `clearinghouseState` fails while set.
    fail_reads: Mutex<bool>,
    /// `updateLeverage` fails while set.
    fail_leverage: Mutex<bool>,
}

impl LiveApi for Fake {
    async fn place(&self, req: &OrderRequest) -> Result<OrderStatus, ExchangeError> {
        self.placed.lock().unwrap().push(req.clone());
        self.place_replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(OrderStatus::Resting {
                oid: 1,
                cloid: req.cloid,
            }))
    }
    async fn cancel_by_cloid(&self, cloid: Cloid) -> Result<CancelStatus, ExchangeError> {
        self.canceled.lock().unwrap().push(cloid);
        self.cancel_replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(CancelStatus::Success))
    }
    async fn update_leverage_isolated(&self, leverage: u32) -> Result<(), ExchangeError> {
        self.leverage_calls.lock().unwrap().push(leverage);
        if *self.fail_leverage.lock().unwrap() {
            return Err(ExchangeError::Http {
                status: 500,
                body: String::new(),
            });
        }
        Ok(())
    }
    async fn clearinghouse(&self) -> anyhow::Result<ClearinghouseState> {
        if *self.fail_reads.lock().unwrap() {
            anyhow::bail!("mock clearinghouseState failure");
        }
        let szi = *self.szi.lock().unwrap();
        let coin = self
            .pos_coin
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "xyz:ANT".into());
        let ms = MarginSummary {
            account_value: 5000.0,
            total_ntl_pos: 0.0,
            total_raw_usd: 5000.0,
            total_margin_used: 0.0,
        };
        Ok(ClearinghouseState {
            margin_summary: ms.clone(),
            cross_margin_summary: None,
            withdrawable: Some(4000.0),
            asset_positions: if szi == 0.0 {
                vec![]
            } else {
                vec![AssetPosition {
                    kind: Some("oneWay".into()),
                    position: Position {
                        coin,
                        szi,
                        leverage: {
                            let (kind, value) = self
                                .pos_leverage
                                .lock()
                                .unwrap()
                                .clone()
                                .unwrap_or(("isolated".into(), 1));
                            Leverage {
                                kind,
                                value,
                                raw_usd: None,
                            }
                        },
                        entry_px: Some(100.0),
                        position_value: szi.abs() * 100.0,
                        unrealized_pnl: 0.0,
                        liquidation_px: None,
                        margin_used: 0.0,
                        max_leverage: Some(10),
                    },
                }]
            },
            time: T0,
        })
    }
    async fn open_orders(&self) -> anyhow::Result<Vec<OpenOrder>> {
        Ok(self.open.lock().unwrap().clone())
    }
    async fn user_fills(&self) -> anyhow::Result<Vec<Fill>> {
        Ok(self.fills.lock().unwrap().clone())
    }
    async fn order_status(&self, cloid: Cloid) -> anyhow::Result<RemoteOrder> {
        let key = cloid.to_string();
        Ok(RemoteOrder {
            status: self
                .statuses
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or(RemoteStatus::Unknown),
            filled_sz: self.filled.lock().unwrap().get(&key).copied(),
        })
    }
    async fn funding_since(&self, start_ms: u64) -> anyhow::Result<Vec<(u64, f64)>> {
        Ok(self
            .funding
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(t, _)| *t >= start_ms)
            .collect())
    }
}

fn params() -> LiveParams {
    LiveParams::from_config(&Config::default(), 2, 10)
}

fn exec(fake: Fake) -> LiveExecutor<Fake> {
    LiveExecutor::new(params(), fake, None, T0).unwrap()
}

fn alo(tag: u64) -> OrderIntent {
    OrderIntent {
        tag,
        is_buy: true,
        px: 100.0,
        sz: 1.0,
        kind: OrderKind::Alo,
        reduce_only: false,
        purpose: Purpose::Entry,
    }
}

fn fill(cloid: Option<Cloid>, oid: u64, tid: u64, time: u64, sz: f64) -> Fill {
    Fill {
        coin: "xyz:ANT".into(),
        px: 100.0,
        sz,
        side: "B".into(),
        time,
        start_position: None,
        dir: None,
        closed_pnl: None,
        hash: None,
        oid,
        crossed: false,
        fee: 0.003,
        tid,
        fee_token: Some("USDC".into()),
        builder_fee: None,
        cloid: cloid.map(|c| c.to_string()),
    }
}

fn m() -> MarketState {
    MarketState::new("xyz:ANT")
}

fn dones(ev: &[ExecEvent]) -> Vec<(u64, DoneReason)> {
    ev.iter()
        .filter_map(|e| match e {
            ExecEvent::Order(OrderUpdate::Done { tag, reason, .. }) => Some((*tag, reason.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn place_resting_then_fill_via_ws_is_counted_once() {
    let mut x = exec(Fake::default());
    let ev = x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 1 })]);
    let o = x.state().orders[0].clone();
    assert_eq!(o.oid, Some(1));
    // ws fill (by cloid), then the same fill again in a resubscription snapshot.
    let f = fill(Some(o.cloid), 1, 77, T0 + 5, 1.0);
    let ev = x
        .on_user_event(
            T0 + 5,
            UserEvent::Fills {
                is_snapshot: false,
                fills: vec![f.clone()],
            },
        )
        .await;
    assert!(matches!(
        ev[0],
        ExecEvent::Order(OrderUpdate::Filled { tag: Some(1), sz, fee_usd, .. }) if sz == 1.0 && fee_usd == 0.003
    ));
    let ev = x
        .on_user_event(
            T0 + 6,
            UserEvent::Fills {
                is_snapshot: true,
                fills: vec![f],
            },
        )
        .await;
    assert!(ev.is_empty(), "duplicate tid ignored");
    // orderUpdates "filled": terminal Done, once.
    let up = crate::hl::types::OrderUpdate {
        order: OpenOrder {
            coin: "xyz:ANT".into(),
            side: "B".into(),
            limit_px: 100.0,
            sz: 0.0,
            oid: 1,
            timestamp: T0,
            orig_sz: Some(1.0),
            cloid: Some(o.cloid.to_string()),
        },
        status: "filled".into(),
        status_timestamp: T0 + 5,
    };
    let ev = x
        .on_user_event(T0 + 6, UserEvent::OrderUpdates(vec![up.clone()]))
        .await;
    assert_eq!(dones(&ev), vec![(1, DoneReason::Filled)]);
    let ev = x
        .on_user_event(T0 + 7, UserEvent::OrderUpdates(vec![up]))
        .await;
    assert!(ev.is_empty());
}

#[tokio::test]
async fn order_errors_map_to_done_reasons() {
    let fake = Fake::default();
    fake.place_replies.lock().unwrap().extend([
        Ok(OrderStatus::Error(
            "Post only order would have immediately matched, bbo was 100.01@100.02. asset=110138"
                .into(),
        )),
        Ok(OrderStatus::Error(
            "Order could not immediately match against any resting orders. asset=110138".into(),
        )),
        Ok(OrderStatus::Error(
            "Insufficient margin to place order.".into(),
        )),
    ]);
    let mut x = exec(fake);
    let ev = x
        .execute(
            T0,
            vec![
                Command::Place(alo(1)),
                Command::Place(alo(2)),
                Command::Place(alo(3)),
            ],
            &m(),
        )
        .await;
    let d = dones(&ev);
    assert_eq!(d[0], (1, DoneReason::PostOnlyRejected));
    assert_eq!(d[1], (2, DoneReason::IocExpired));
    assert!(matches!(&d[2].1, DoneReason::Rejected(s) if s.contains("margin")));
}

#[tokio::test]
async fn rate_limit_pauses_new_orders_and_alerts() {
    let fake = Fake::default();
    fake.place_replies
        .lock()
        .unwrap()
        .push_back(Ok(OrderStatus::Error(
            "Too many cumulative requests sent (10001 > 10000) for cumulative volume traded $0.00."
                .into(),
        )));
    let mut x = exec(fake);
    let ev = x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    assert!(ev
        .iter()
        .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("rate limit"))));
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(_)));
    // Paused: refused locally, nothing sent.
    let ev = x
        .execute(T0 + 1_000, vec![Command::Place(alo(2))], &m())
        .await;
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(s) if s.contains("paused")));
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
    // After the backoff: allowed again.
    let ev = x
        .execute(T0 + 61_000, vec![Command::Place(alo(3))], &m())
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);
}

#[tokio::test]
async fn local_action_budget_caps_orders_but_not_cancels() {
    let mut p = params();
    p.live.max_exchange_actions_per_min = 2;
    let mut x = LiveExecutor::new(p, Fake::default(), None, T0).unwrap();
    x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    x.execute(T0, vec![Command::Cancel { tag: 1 }], &m()).await;
    let ev = x.execute(T0, vec![Command::Place(alo(2))], &m()).await;
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(s) if s.contains("budget")));
    x.execute(T0 + 1, vec![Command::Place(alo(3))], &m()).await; // refused too
                                                                 // A cancel always goes out even over budget.
    let mut p = params();
    p.live.max_exchange_actions_per_min = 1;
    let mut y = LiveExecutor::new(p, Fake::default(), None, T0).unwrap();
    y.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    let ev = y.execute(T0, vec![Command::Cancel { tag: 1 }], &m()).await;
    assert_eq!(dones(&ev), vec![(1, DoneReason::Canceled)]);
    // A minute later the budget is free again.
    let ev = x
        .execute(T0 + 60_001, vec![Command::Place(alo(4))], &m())
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 4 })]);
}

#[tokio::test]
async fn transport_error_is_resolved_by_cloid_on_reconcile() {
    let fake = Fake::default();
    fake.place_replies
        .lock()
        .unwrap()
        .push_back(Err(ExchangeError::Http {
            status: 502,
            body: "bad gateway".into(),
        }));
    let mut x = exec(fake);
    let ev = x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    assert!(
        dones(&ev).is_empty(),
        "no Done while the outcome is unknown"
    );
    assert!(x.state().orders[0].uncertain);
    let cloid = x.state().orders[0].cloid;
    // The exchange actually has it resting.
    x.api()
        .statuses
        .lock()
        .unwrap()
        .insert(cloid.to_string(), RemoteStatus::Open);
    let ev = x.reconcile(T0 + 1_000, false).await;
    assert!(ev.contains(&ExecEvent::Order(OrderUpdate::Resting { tag: 1 })));
    assert!(!x.state().orders[0].uncertain);
    // Later it is canceled by the exchange (e.g. margin): resync says so.
    x.api().statuses.lock().unwrap().insert(
        cloid.to_string(),
        RemoteStatus::Canceled("marginCanceled".into()),
    );
    let ev = x.reconcile(T0 + 2_000, true).await;
    assert_eq!(dones(&ev), vec![(1, DoneReason::Canceled)]);
}

#[tokio::test]
async fn forced_resync_picks_up_dropped_fills() {
    let mut x = exec(Fake::default());
    x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    let cloid = x.state().orders[0].cloid;
    x.api()
        .fills
        .lock()
        .unwrap()
        .push(fill(Some(cloid), 1, 5, T0 + 10, 0.4));
    // Unforced reconcile does not fetch fills.
    let ev = x.reconcile(T0 + 20, false).await;
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ExecEvent::Order(OrderUpdate::Filled { .. }))));
    x.api().open.lock().unwrap().push(OpenOrder {
        coin: "xyz:ANT".into(),
        side: "B".into(),
        limit_px: 100.0,
        sz: 0.6,
        oid: 1,
        timestamp: T0,
        orig_sz: Some(1.0),
        cloid: Some(cloid.to_string()),
    });
    let ev = x.reconcile(T0 + 30, true).await;
    assert!(ev.iter().any(|e| matches!(
        e,
        ExecEvent::Order(OrderUpdate::Filled { tag: Some(1), sz, .. }) if *sz == 0.4
    )));
    assert!(dones(&ev).is_empty(), "still open");
}

#[tokio::test]
async fn startup_adopts_own_orders_cancels_strays_and_replays_missed_fills() {
    // Previous run: order 7 resting, order 8 resting (filled while down).
    let pf = Fake::default();
    pf.place_replies.lock().unwrap().extend([
        Ok(OrderStatus::Resting {
            oid: 1,
            cloid: None,
        }),
        Ok(OrderStatus::Resting {
            oid: 2,
            cloid: None,
        }),
    ]);
    let mut prev = exec(pf);
    prev.execute(T0, vec![Command::Place(alo(7))], &m()).await;
    prev.execute(T0, vec![Command::Place(alo(8))], &m()).await;
    let saved = prev.save_state();
    let c7 = prev.state().orders[0].cloid;
    let c8 = prev.state().orders[1].cloid;

    let fake = Fake::default();
    let open_of = |cloid: Option<String>, oid| OpenOrder {
        coin: "xyz:ANT".into(),
        side: "B".into(),
        limit_px: 99.0,
        sz: 1.0,
        oid,
        timestamp: T0,
        orig_sz: None,
        cloid,
    };
    fake.open.lock().unwrap().extend([
        open_of(Some(c7.to_string()), 1),
        open_of(Some("0x000000000000000000000000000000ff".into()), 99), // stray
    ]);
    fake.statuses
        .lock()
        .unwrap()
        .insert(c8.to_string(), RemoteStatus::Filled);
    fake.fills.lock().unwrap().extend([
        fill(Some(c8), 2, 11, T0 + 100, 1.0),
        // Old unrelated fill from before the bot existed: history.
        fill(None, 555, 12, T0 - 10_000_000, 3.0),
    ]);
    let mut x = LiveExecutor::new(params(), fake, Some(saved), T0 + 1_000_000).unwrap();
    let ev = x.startup(T0 + 1_000_000).await.unwrap();
    // Stray canceled with an alert; own order not canceled.
    let canceled = x.api().canceled.lock().unwrap().clone();
    assert_eq!(canceled.len(), 1);
    assert_ne!(canceled[0], c7);
    assert!(ev
        .iter()
        .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("stray"))));
    // Missed fill of order 8 replayed, then its Done.
    assert!(ev.iter().any(|e| matches!(
        e,
        ExecEvent::Order(OrderUpdate::Filled { tag: Some(8), .. })
    )));
    assert_eq!(dones(&ev), vec![(8, DoneReason::Filled)]);
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ExecEvent::Order(OrderUpdate::Filled { tag: None, .. }))));
    assert_eq!(x.api().leverage_calls.lock().unwrap().clone(), vec![1]);
    assert_eq!(x.available_margin_usd(), Some(4000.0));
    assert_eq!(x.venue().unwrap().szi, 0.0);
}

#[tokio::test]
async fn startup_with_position_keeps_leverage_and_reports_it() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = -2.5;
    let mut x = exec(fake);
    let ev = x.startup(T0).await.unwrap();
    assert!(x.api().leverage_calls.lock().unwrap().is_empty());
    assert!(ev.iter().any(|e| matches!(e, ExecEvent::Alert(_))));
    let v = x.venue().unwrap();
    assert_eq!((v.szi, v.entry_px), (-2.5, Some(100.0)));
}

#[tokio::test]
async fn failed_cancel_is_retried_by_reconcile() {
    let fake = Fake::default();
    fake.cancel_replies
        .lock()
        .unwrap()
        .push_back(Err(ExchangeError::Http {
            status: 503,
            body: String::new(),
        }));
    let mut x = exec(fake);
    x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    let cloid = x.state().orders[0].cloid;
    let ev = x
        .execute(T0 + 1, vec![Command::Cancel { tag: 1 }], &m())
        .await;
    assert!(dones(&ev).is_empty());
    x.api()
        .statuses
        .lock()
        .unwrap()
        .insert(cloid.to_string(), RemoteStatus::Open);
    let ev = x.reconcile(T0 + 30_000, false).await;
    assert_eq!(dones(&ev), vec![(1, DoneReason::Canceled)]);
    assert_eq!(x.api().canceled.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn shutdown_cancels_and_keeps_position_by_default() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 1.0;
    let mut x = exec(fake);
    x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
    let ev = x.shutdown(T0 + 1, &m()).await;
    assert_eq!(dones(&ev), vec![(1, DoneReason::Canceled)]);
    assert_eq!(x.api().placed.lock().unwrap().len(), 1, "no flatten order");
}

#[tokio::test]
async fn funding_payments_are_reported_once() {
    let fake = Fake::default();
    fake.funding
        .lock()
        .unwrap()
        .extend([(T0 + 3_600_000, -0.02), (T0 + 7_200_000, 0.01)]);
    let mut x = exec(fake);
    x.startup(T0).await.unwrap();
    let ev = x.reconcile(T0 + 8_000_000, false).await;
    let f: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, ExecEvent::Funding { .. }))
        .collect();
    assert_eq!(f.len(), 2);
    let ev = x.reconcile(T0 + 8_100_000, false).await;
    assert!(!ev.iter().any(|e| matches!(e, ExecEvent::Funding { .. })));
}

#[test]
fn remote_status_mapping() {
    assert_eq!(RemoteStatus::from_status("open"), RemoteStatus::Open);
    assert_eq!(
        RemoteStatus::from_status("badAloPxRejected").done_reason(OrderKind::Alo),
        Some(DoneReason::PostOnlyRejected)
    );
    assert_eq!(
        RemoteStatus::from_status("canceled").done_reason(OrderKind::Ioc),
        Some(DoneReason::IocExpired)
    );
    assert_eq!(
        RemoteStatus::from_status("reduceOnlyCanceled").done_reason(OrderKind::Alo),
        Some(DoneReason::Canceled)
    );
    let v: Value = serde_json::from_str(
        r#"{"status":"order","order":{"order":{"coin":"xyz:ANT"},"status":"filled","statusTimestamp":1}}"#,
    )
    .unwrap();
    assert_eq!(
        RemoteStatus::from_order_status_json(&v),
        RemoteStatus::Filled
    );
    let v: Value = serde_json::from_str(r#"{"status":"unknownOid"}"#).unwrap();
    assert_eq!(
        RemoteStatus::from_order_status_json(&v),
        RemoteStatus::Unknown
    );
    let f: Vec<Value> = serde_json::from_str(
        r#"[{"time":5,"hash":"0x","delta":{"type":"funding","coin":"xyz:ANT","usdc":"-0.0123","szi":"2.0","fundingRate":"0.0000125"}},
            {"time":6,"hash":"0x","delta":{"type":"funding","coin":"BTC","usdc":"1.0","szi":"1.0","fundingRate":"0.0001"}}]"#,
    )
    .unwrap();
    assert_eq!(parse_user_funding(&f, "xyz:ANT"), vec![(5, -0.0123)]);
}

#[tokio::test]
async fn other_coin_position_on_the_dex_is_ignored() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 3.0;
    *fake.pos_coin.lock().unwrap() = Some("xyz:OTHER".into());
    let mut e = exec(fake);
    e.startup(T0).await.unwrap();
    assert_eq!(e.venue_position(), Some(0.0));
    // Flat on our coin: isolated leverage is set.
    assert_eq!(*e.api().leverage_calls.lock().unwrap(), vec![1]);
}

// ------------------------------------------- review: shutdown / unknown tags

mod shutdown_regression {
    use super::*;
    use crate::engine::{Engine, EngineParams};
    use crate::exec::state::Ledger;
    use crate::exec::OrderView;
    use crate::risk::{RiskManager, RiskParams, RiskState};
    use crate::strategy::premium::Side;
    use crate::strategy::signal::{
        MarketView, Phase, Strategy, StrategyParams, StrategyState, TickInput,
    };

    fn sparams() -> StrategyParams {
        StrategyParams::from_config(&Config::default(), 2, 10)
    }

    fn engine(state: Option<StrategyState>) -> Engine {
        let cfg = Config::default();
        let strat = match state {
            Some(s) => Strategy::from_state(sparams(), s),
            None => Strategy::new(sparams()),
        };
        Engine::new(
            EngineParams::from_config(&cfg),
            strat,
            RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
            Ledger::default(),
            0,
        )
    }

    fn view(now: u64, prem: f64) -> TickInput {
        TickInput {
            view: MarketView {
                now_ms: now,
                premium_bps: Some(prem),
                best_bid: Some(100.0),
                best_ask: Some(100.02),
                stale: false,
            },
            entries_allowed: true,
            available_margin_usd: None,
        }
    }

    /// Run commands through the executor until quiet (like runner::pump).
    async fn pump(e: &mut Engine, x: &mut LiveExecutor<Fake>, now: u64, mut cmds: Vec<Command>) {
        for _ in 0..10 {
            if cmds.is_empty() {
                return;
            }
            let evs = x.execute(now, std::mem::take(&mut cmds), &m()).await;
            for ev in evs {
                cmds.extend(e.on_exec(now, ev));
            }
        }
        panic!("did not settle");
    }

    /// Review finding 1: shutdown while exiting, restart, kill -> the
    /// flatten orders are sent (the strategy is not left waiting).
    #[tokio::test]
    async fn shutdown_while_exiting_then_restart_and_kill_flattens() {
        let mut e = engine(None);
        e.strategy
            .adopt_position(Side::Short, 24.0, 100.0, T0)
            .unwrap();
        let fake = Fake::default();
        *fake.szi.lock().unwrap() = -24.0;
        let mut x = exec(fake);
        // Converged: reduce-only maker exit resting.
        let st = e.strategy.on_tick(view(T0 + 60_000, 2.0));
        assert_eq!(st.commands.len(), 1, "{st:?}");
        pump(&mut e, &mut x, T0 + 60_000, st.commands).await;
        assert!(matches!(e.strategy.phase(), Phase::Exiting { .. }));
        let tag = e.strategy.working().unwrap().intent.tag;
        let cloid = x.state().orders[0].cloid.to_string();
        x.api
            .statuses
            .lock()
            .unwrap()
            .insert(cloid.clone(), RemoteStatus::Canceled("canceled".into()));
        x.api.filled.lock().unwrap().insert(cloid, 0.0);

        // Graceful shutdown: cancel, the strategy must not re-quote.
        let now = T0 + 120_000;
        e.suspend();
        let evs = x.shutdown(now, &m()).await;
        assert_eq!(dones(&evs), vec![(tag, DoneReason::Canceled)]);
        for ev in evs {
            assert!(
                e.on_exec(now, ev).is_empty(),
                "suspended strategy issued commands"
            );
        }
        assert!(e.strategy.working().is_none());
        let saved_strategy = e.strategy.state().clone();
        let saved_exec = x.save_state();

        // Restart.
        let fake = Fake::default();
        *fake.szi.lock().unwrap() = -24.0;
        let mut x = LiveExecutor::new(params(), fake, Some(saved_exec), now + 1_000).unwrap();
        let mut e = engine(Some(saved_strategy));
        let evs = x.startup(now + 1_000).await.unwrap();
        let mut cmds = Vec::new();
        for ev in evs {
            cmds.extend(e.on_exec(now + 1_000, ev));
        }
        cmds.extend(e.startup(now + 1_000));
        pump(&mut e, &mut x, now + 1_000, cmds).await;
        assert!(heal_working_cmds(&mut e, &x, now + 1_000).is_empty());

        // Kill with flatten: orders go out, IOC after the maker timeout.
        let placed_before = x.api.placed.lock().unwrap().len();
        let cmds = e.manual_kill_with(now + 2_000, "test", true);
        pump(&mut e, &mut x, now + 2_000, cmds).await;
        let mut t = now + 2_000;
        while t < now + 10 * 60_000 {
            t += 1_000;
            let st = e.strategy.on_tick(view(t, 60.0));
            let cmds = st.commands;
            pump(&mut e, &mut x, t, cmds).await;
        }
        let placed = x.api.placed.lock().unwrap();
        assert!(
            placed.len() > placed_before,
            "no flatten order after restart"
        );
        assert!(placed[placed_before..].iter().all(|r| r.reduce_only));
    }

    /// A state persisted by the old code: the working order was never sent
    /// and its cancel is already "requested". The runner's self-healing
    /// resolves it and the kill flatten proceeds.
    #[tokio::test]
    async fn unknown_working_order_is_healed() {
        let mut e = engine(None);
        e.strategy
            .adopt_position(Side::Short, 24.0, 100.0, T0)
            .unwrap();
        let st = e.strategy.on_tick(view(T0 + 60_000, 2.0));
        assert_eq!(st.commands.len(), 1); // dropped: never executed
        e.strategy.on_kill(T0 + 61_000, true, "test");
        // Taker time after the kill's maker window: cancel the maker exit
        // (dropped as well).
        let st = e.strategy.on_tick(view(T0 + 200_000, 60.0));
        assert_eq!(st.commands.len(), 1, "{st:?}");
        assert!(e.strategy.working().unwrap().cancel_requested);
        let tag = e.strategy.working().unwrap().intent.tag;

        let fake = Fake::default();
        *fake.szi.lock().unwrap() = -24.0;
        let mut x = exec(fake);
        assert_eq!(x.order_view(tag), OrderView::Unknown);
        // A cancel for an unknown tag is answered too.
        let evs = x
            .execute(T0 + 201_000, vec![Command::Cancel { tag }], &m())
            .await;
        assert!(matches!(&dones(&evs)[..], [(t, DoneReason::Rejected(_))] if *t == tag));
        assert!(x.api.canceled.lock().unwrap().is_empty());

        // The runner's healing settles the strategy order; the next tick
        // flattens with an IOC.
        let cmds = heal_working_cmds(&mut e, &x, T0 + 201_000);
        assert!(e.strategy.working().is_none() || !cmds.is_empty());
        pump(&mut e, &mut x, T0 + 201_000, cmds).await;
        let mut t = T0 + 201_000;
        while t < T0 + 10 * 60_000 {
            t += 1_000;
            let st = e.strategy.on_tick(view(t, 60.0));
            let cmds = st.commands;
            pump(&mut e, &mut x, t, cmds).await;
        }
        assert!(
            !x.api.placed.lock().unwrap().is_empty(),
            "kill flatten never ran"
        );
    }

    fn heal_working_cmds(e: &mut Engine, x: &LiveExecutor<Fake>, now: u64) -> Vec<Command> {
        crate::runner::heal_working(e, x, &mut crate::monitor::Monitor::disabled(), now)
    }

    /// Cancel of a finished order repeats its terminal state.
    #[tokio::test]
    async fn cancel_of_a_finished_order_repeats_done() {
        let mut x = exec(Fake::default());
        x.execute(T0, vec![Command::Place(alo(1))], &m()).await;
        let cloid = x.state().orders[0].cloid.to_string();
        x.api.filled.lock().unwrap().insert(cloid, 0.4);
        let evs = x
            .execute(T0 + 1, vec![Command::Cancel { tag: 1 }], &m())
            .await;
        assert!(evs.contains(&ExecEvent::Order(OrderUpdate::Done {
            tag: 1,
            reason: DoneReason::Canceled,
            filled_sz: Some(0.4),
        })));
        assert_eq!(
            x.order_view(1),
            OrderView::Done {
                reason: DoneReason::Canceled,
                filled_sz: Some(0.4)
            }
        );
        let evs = x
            .execute(T0 + 2, vec![Command::Cancel { tag: 1 }], &m())
            .await;
        assert_eq!(dones(&evs), vec![(1, DoneReason::Canceled)]);
        assert_eq!(
            x.api.canceled.lock().unwrap().len(),
            1,
            "no second cancel sent"
        );
    }
}

// ---- last-line order checks (same on testnet and mainnet) -------------

fn exec_with(cfg: &Config, asset_max_leverage: u32, fake: Fake) -> LiveExecutor<Fake> {
    LiveExecutor::new(
        LiveParams::from_config(cfg, 2, asset_max_leverage),
        fake,
        None,
        T0,
    )
    .unwrap()
}

fn small_cfg(max_notional: f64) -> Config {
    let mut c = Config::default();
    c.sizing.notional_usd = max_notional;
    c.sizing.max_notional_usd = max_notional;
    c
}

fn intent(tag: u64, kind: OrderKind, sz: f64, reduce_only: bool) -> OrderIntent {
    OrderIntent {
        tag,
        is_buy: true,
        px: 100.0,
        sz,
        kind,
        reduce_only,
        purpose: if reduce_only {
            Purpose::Exit
        } else {
            Purpose::Entry
        },
    }
}

fn refused(ev: &[ExecEvent], tag: u64) -> bool {
    matches!(dones(ev).as_slice(), [(t, DoneReason::Rejected(_))] if *t == tag)
        && ev
            .iter()
            .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("refused by the live executor")))
}

#[tokio::test]
async fn opening_order_must_be_post_only() {
    let mut x = exec(Fake::default());
    let ev = x
        .execute(
            T0,
            vec![Command::Place(intent(1, OrderKind::Ioc, 1.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(x.api().placed.lock().unwrap().is_empty(), "never sent");
    // The same order as ALO goes out, flagged post-only.
    x.execute(
        T0 + 1,
        vec![Command::Place(intent(2, OrderKind::Alo, 1.0, false))],
        &m(),
    )
    .await;
    let placed = x.api().placed.lock().unwrap().clone();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].tif, crate::hl::actions::Tif::Alo);
    assert!(!placed[0].reduce_only);
}

#[tokio::test]
async fn opening_order_capped_by_max_notional() {
    // max_notional 300 USD: 306 USD allowed (re-quote tolerance), 310 not.
    let mut x = exec_with(&small_cfg(300.0), 10, Fake::default());
    let ev = x
        .execute(
            T0,
            vec![Command::Place(intent(1, OrderKind::Alo, 3.1, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(x.api().placed.lock().unwrap().is_empty());
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(2, OrderKind::Alo, 3.06, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 2 })]);
}

#[tokio::test]
async fn opening_order_never_above_hard_cap() {
    // Default config: max_notional = hard cap = 10 000 USD, no tolerance.
    let mut x = exec(Fake::default());
    let ev = x
        .execute(
            T0,
            vec![Command::Place(intent(1, OrderKind::Alo, 100.01, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(2, OrderKind::Alo, 100.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 2 })]);
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn venue_position_counts_toward_the_cap() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 2.0; // 200 USD at entry 100
                                     // No startup / reconcile: the executor refreshes the venue position
                                     // itself before an opening order.
    let mut x = exec_with(&small_cfg(300.0), 10, fake);
    assert!(x.venue().is_none());
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(1, OrderKind::Alo, 1.5, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    let ev = x
        .execute(
            T0 + 2,
            vec![Command::Place(intent(2, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 2 })]);
}

#[tokio::test]
async fn reduce_only_exits_are_never_blocked() {
    // Maker exit and IOC taker fallback / flatten, far above the cap: they
    // can only shrink the position.
    let mut x = exec_with(&small_cfg(300.0), 10, Fake::default());
    x.execute(
        T0,
        vec![
            Command::Place(intent(1, OrderKind::Alo, 50.0, true)),
            Command::Place(intent(2, OrderKind::Ioc, 50.0, true)),
        ],
        &m(),
    )
    .await;
    let placed = x.api().placed.lock().unwrap().clone();
    assert_eq!(placed.len(), 2);
    assert!(placed.iter().all(|r| r.reduce_only));
    assert_eq!(placed[1].tif, crate::hl::actions::Tif::Ioc);
}

#[tokio::test]
async fn leverage_is_isolated_and_clamped() {
    // Config 2x on an asset whose max is 1x: 1x.
    let mut c = Config::default();
    c.sizing.leverage = 2;
    let mut x = exec_with(&c, 1, Fake::default());
    x.startup(T0).await.unwrap();
    assert_eq!(*x.api().leverage_calls.lock().unwrap(), vec![1]);
    // Config 2x, asset max 10x: 2x.
    let mut x = exec_with(&c, 10, Fake::default());
    x.startup(T0).await.unwrap();
    assert_eq!(*x.api().leverage_calls.lock().unwrap(), vec![2]);
    // An (unvalidated) 5x config on a 50x asset: never above MAX_ALLOWED_LEVERAGE.
    c.sizing.leverage = 5;
    let mut x = exec_with(&c, 50, Fake::default());
    x.startup(T0).await.unwrap();
    assert_eq!(
        *x.api().leverage_calls.lock().unwrap(),
        vec![MAX_ALLOWED_LEVERAGE]
    );
    // (`HlApi::update_leverage_isolated` sends isCross = false: checked on
    // the wire in tests/exchange_mock.rs.)
}

#[tokio::test]
async fn startup_position_on_cross_margin_is_reported_loudly() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 1.0;
    *fake.pos_leverage.lock().unwrap() = Some(("cross".into(), 5));
    let mut x = exec(fake);
    let ev = x.startup(T0).await.unwrap();
    assert!(x.api().leverage_calls.lock().unwrap().is_empty());
    assert!(
        ev.iter()
            .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("CROSS margin at 5x"))),
        "{ev:?}"
    );
    let v = x.venue().unwrap();
    assert_eq!((v.isolated, v.leverage), (Some(false), Some(5)));
}

// ------------------------------- kill rules -> flatten on the live path

/// The kill rules live in the engine, shared by paper and live: these drive
/// a real [`Engine`] with market snapshots / spec polls through the live
/// executor and check that each rule's kill flattens with reduce-only
/// orders (maker ALO first, IOC after `kill_flatten_maker_secs`).
mod kill_flatten_live {
    use super::*;
    use crate::engine::{Engine, EngineParams, Notice};
    use crate::exec::state::Ledger;
    use crate::hl::market::DataSource;
    use crate::hl::types::{L2Book, Level};
    use crate::risk::{KillReason, RiskManager, RiskParams, RiskState};
    use crate::strategy::premium::Side;
    use crate::strategy::signal::{Phase, Strategy, StrategyParams};

    const SZ: f64 = 24.0;

    fn engine(cfg: &Config) -> Engine {
        let mut e = Engine::new(
            EngineParams::from_config(cfg),
            Strategy::new(StrategyParams::from_config(cfg, 2, 10)),
            RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
            Ledger::default(),
            0,
        );
        e.strategy
            .adopt_position(Side::Short, SZ, 100.0, T0)
            .unwrap();
        e
    }

    /// Book mid `mid`, oracle `oracle` (premium = mid / oracle - 1).
    fn market(now: u64, mid: f64, oracle: f64) -> MarketState {
        let mut m = MarketState::new("xyz:ANT");
        let l = |px| Level {
            px,
            sz: 1000.0,
            n: 1,
        };
        m.apply_book(
            &L2Book {
                coin: "xyz:ANT".into(),
                time: now,
                levels: (vec![l(mid - 0.01)], vec![l(mid + 0.01)]),
            },
            now,
            DataSource::Rest,
        );
        m.oracle_px = Some(oracle);
        m.mark_px = Some(mid);
        m.impact_bid_px = Some(mid - 0.01);
        m.impact_ask_px = Some(mid + 0.01);
        m.ctx_recv_ms = Some(now);
        m.funding = Some(0.0);
        m
    }

    async fn pump(
        e: &mut Engine,
        x: &mut LiveExecutor<Fake>,
        now: u64,
        mk: &MarketState,
        mut cmds: Vec<Command>,
    ) {
        for _ in 0..20 {
            if cmds.is_empty() {
                return;
            }
            let evs = x.execute(now, std::mem::take(&mut cmds), mk).await;
            for ev in evs {
                cmds.extend(e.on_exec(now, ev));
            }
        }
        panic!("did not settle");
    }

    fn killed_by(e: &mut Engine) -> KillReason {
        e.drain_notices()
            .into_iter()
            .find_map(|n| match n {
                Notice::Kill(_, k) => {
                    assert!(k.flatten && k.cancel_orders && k.stop_entries, "{k:?}");
                    Some(k.reason)
                }
                _ => None,
            })
            .expect("no kill notice")
    }

    /// After the kill at `t_kill`: a reduce-only ALO buy (closing the
    /// short) at once, then a reduce-only IOC buy once the maker window ran
    /// out. Nothing that could open a position is ever sent.
    async fn assert_flattens(
        cfg: &Config,
        e: &mut Engine,
        x: &mut LiveExecutor<Fake>,
        t_kill: u64,
        mk: &MarketState,
        cmds: Vec<Command>,
    ) {
        let before = x.api().placed.lock().unwrap().len();
        pump(e, x, t_kill, mk, cmds).await;
        {
            let placed = x.api().placed.lock().unwrap();
            assert_eq!(placed.len(), before + 1, "maker flatten order");
            let o = &placed[before];
            assert!(o.reduce_only && o.is_buy, "{o:?}");
            assert_eq!(o.tif, crate::hl::actions::Tif::Alo);
            assert!((o.sz - SZ).abs() < 1e-9);
        }
        let maker_ms = cfg.risk.kill_flatten_maker_secs * 1000;
        let mut t = t_kill;
        while t < t_kill + maker_ms + 5_000 {
            t += 1_000;
            // Canceled maker orders report "nothing filled" (orderStatus).
            for o in &x.state().orders {
                x.api()
                    .filled
                    .lock()
                    .unwrap()
                    .entry(o.cloid.to_string())
                    .or_insert(0.0);
            }
            let mk = market(t, mk.mark_px.unwrap(), mk.oracle_px.unwrap());
            let c = e.on_market(t, &mk, None);
            pump(e, x, t, &mk, c).await;
        }
        let placed = x.api().placed.lock().unwrap();
        let ioc: Vec<_> = placed[before..]
            .iter()
            .filter(|o| o.tif == crate::hl::actions::Tif::Ioc)
            .collect();
        assert!(!ioc.is_empty(), "no IOC taker flatten: {placed:?}");
        assert!(ioc.iter().all(|o| o.is_buy && (o.sz - SZ).abs() < 1e-9));
        assert!(placed[before..].iter().all(|o| o.reduce_only));
        assert!(e.risk.is_killed());
        assert!(!e.risk.entries_allowed());
        assert!(matches!(
            e.strategy.phase(),
            Phase::Exiting { .. } | Phase::Halted { .. }
        ));
    }

    fn fake_short() -> Fake {
        let f = Fake::default();
        *f.szi.lock().unwrap() = -SZ;
        f
    }

    /// Live executor after its startup reconciliation (venue: short SZ).
    async fn live_short() -> LiveExecutor<Fake> {
        let mut x = exec(fake_short());
        x.startup(T0).await.unwrap();
        assert_eq!(x.venue_position(), Some(-SZ));
        x
    }

    #[tokio::test]
    async fn daily_loss_kill_flattens_on_live() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        let mut x = live_short().await;
        // Premium stays at +60 bps (no convergence exit): baseline equity.
        let m0 = market(T0 + 1_000, 100.6, 100.0);
        let c = e.on_market(T0 + 1_000, &m0, None);
        pump(&mut e, &mut x, T0 + 1_000, &m0, c).await;
        assert!(!e.risk.is_killed());
        // Price +10 % against the 24-lot short: -240 USD > 150 daily limit.
        let t = T0 + 2_000;
        let m1 = market(t, 110.66, 110.0);
        let c = e.on_market(t, &m1, None);
        assert!(matches!(killed_by(&mut e), KillReason::DailyLoss { .. }));
        assert_flattens(&cfg, &mut e, &mut x, t, &m1, c).await;
    }

    #[tokio::test]
    async fn max_drawdown_kill_flattens_on_live() {
        let mut cfg = Config::default();
        cfg.risk.daily_loss_limit_usd = 5_000.0;
        let mut e = engine(&cfg);
        let mut x = live_short().await;
        let m0 = market(T0 + 1_000, 100.6, 100.0);
        let c = e.on_market(T0 + 1_000, &m0, None);
        pump(&mut e, &mut x, T0 + 1_000, &m0, c).await;
        // -24 * 14 = -336 USD > 300 drawdown limit.
        let t = T0 + 2_000;
        let m1 = market(t, 114.68, 114.0);
        let c = e.on_market(t, &m1, None);
        assert!(matches!(killed_by(&mut e), KillReason::MaxDrawdown { .. }));
        assert_flattens(&cfg, &mut e, &mut x, t, &m1, c).await;
    }

    #[tokio::test]
    async fn persistent_premium_kill_flattens_on_live() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        let mut x = live_short().await;
        // +200 bps (> 150) for longer than persistent_premium_secs; the
        // loss stays under the equity limits (-48 USD).
        let m0 = market(T0 + 1_000, 102.0, 100.0);
        let c = e.on_market(T0 + 1_000, &m0, None);
        pump(&mut e, &mut x, T0 + 1_000, &m0, c).await;
        assert!(!e.risk.is_killed());
        let t = T0 + 1_000 + cfg.risk.persistent_premium_secs * 1000 + 1_000;
        let m1 = market(t, 102.0, 100.0);
        let c = e.on_market(t, &m1, None);
        assert!(matches!(
            killed_by(&mut e),
            KillReason::PersistentPremium { .. }
        ));
        assert_flattens(&cfg, &mut e, &mut x, t, &m1, c).await;
    }

    #[tokio::test]
    async fn spec_change_kill_flattens_on_live() {
        let cfg = Config::default();
        let mut e = engine(&cfg);
        let mut x = live_short().await;
        let m0 = market(T0 + 1_000, 100.6, 100.0);
        let c = e.on_market(T0 + 1_000, &m0, None);
        pump(&mut e, &mut x, T0 + 1_000, &m0, c).await;
        // The meta poll cannot find the market any more (delisted / halted).
        let t = T0 + 2_000;
        let c = e.on_spec(t, Err("xyz:ANT missing from meta".into()));
        assert!(matches!(killed_by(&mut e), KillReason::SpecChange { .. }));
        assert_flattens(&cfg, &mut e, &mut x, t, &m0, c).await;
    }

    /// Review M1 r1 finding 1, through the real engine: an entry partially
    /// fills on the venue, its cancel succeeds without a filled size, the
    /// fills never arrive and account reads fail. The strategy gives up
    /// waiting and retries the entry; no second opening order may reach
    /// the exchange until the exposure is known and fits the cap.
    #[tokio::test]
    async fn no_reentry_on_top_of_unconfirmed_fills() {
        let mut cfg = Config::default();
        cfg.sizing.notional_usd = 10_000.0;
        cfg.strategy.premium_avg_secs = 0; // instantaneous premium: no warm-up
        let mut e = Engine::new(
            EngineParams::from_config(&cfg),
            Strategy::new(StrategyParams::from_config(&cfg, 2, 10)),
            RiskManager::new(RiskParams::from(&cfg.risk), RiskState::default()),
            Ledger::default(),
            0,
        );
        // (The engine is given ample margin: Some(1e9) below.)
        let mut x = exec(Fake::default());
        x.startup(T0).await.unwrap();
        let entry_mk = |t| market(t, 100.0, 99.4); // +60 bps: short entry
        let flat_mk = |t| market(t, 100.0, 100.0); // converged
        let mk = entry_mk(T0);
        let c = e.on_market(T0, &mk, Some(1e9));
        pump(&mut e, &mut x, T0, &mk, c).await;
        let opening = |x: &LiveExecutor<Fake>| {
            x.api()
                .placed
                .lock()
                .unwrap()
                .iter()
                .filter(|o| !o.reduce_only)
                .count()
        };
        assert_eq!(opening(&x), 1, "first entry order");
        // 99 lots fill on the venue (no fill event), then account reads fail.
        *x.api().szi.lock().unwrap() = -99.0;
        *x.api().fail_reads.lock().unwrap() = true;
        let mut t = T0 + 1_000;
        let mk = flat_mk(t);
        let c = e.on_market(t, &mk, Some(1e9));
        pump(&mut e, &mut x, t, &mk, c).await;
        assert!(x.state().orders[0].done, "entry canceled");
        // Past the strategy's missing-fill wait and retry cooldown, with
        // forced reconciliations (failing reads) on the way.
        let mut alerted = false;
        while t < T0 + 200_000 {
            t += 5_000;
            if t % 30_000 < 5_000 {
                for ev in x.reconcile(t, true).await {
                    alerted |= matches!(&ev, ExecEvent::Alert(a) if a.contains("clearinghouseState failed"));
                    let c = e.on_exec(t, ev);
                    pump(&mut e, &mut x, t, &mk, c).await;
                }
                // A stale snapshot is not reported as the venue position.
                assert_eq!(x.venue_position(), None);
            }
            let mk = if t >= T0 + 60_000 {
                entry_mk(t)
            } else {
                flat_mk(t)
            };
            let c = e.on_market(t, &mk, Some(1e9));
            pump(&mut e, &mut x, t, &mk, c).await;
        }
        assert!(alerted, "persistent account-read failure alerted");
        assert_eq!(opening(&x), 1, "no second opening order while reads fail");
        // Reads recover: the venue shows the 9 900 USD short, so a new
        // ~10 000 USD entry still does not fit the cap.
        *x.api().fail_reads.lock().unwrap() = false;
        while t < T0 + 400_000 {
            t += 5_000;
            let mk = entry_mk(t);
            let c = e.on_market(t, &mk, Some(1e9));
            pump(&mut e, &mut x, t, &mk, c).await;
        }
        assert_eq!(opening(&x), 1, "cap holds once the venue is readable");
        assert_eq!(x.venue_position(), Some(-99.0));
        // The runner's mismatch check now sees the real gap.
        assert!((e.strategy_position() - (-99.0)).abs() > 1e-6);
    }
}

#[tokio::test]
async fn uncertain_opening_order_counts_toward_the_cap() {
    // A transport error leaves the first entry's outcome unknown: it may be
    // live, so a second opening order cannot stack on top of it.
    let fake = Fake::default();
    fake.place_replies
        .lock()
        .unwrap()
        .push_back(Err(ExchangeError::Http {
            status: 502,
            body: String::new(),
        }));
    let mut x = exec_with(&small_cfg(300.0), 10, fake);
    x.execute(
        T0,
        vec![Command::Place(intent(1, OrderKind::Alo, 2.5, false))],
        &m(),
    )
    .await;
    assert!(x.state().orders[0].uncertain);
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(2, OrderKind::Alo, 2.5, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 2), "{ev:?}");
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
}

// ---- review M1 r1: fresh account state and margin on opening orders -----

#[tokio::test]
async fn failed_account_read_refuses_opening_orders_but_not_exits() {
    let fake = Fake::default();
    let mut x = exec(fake);
    x.startup(T0).await.unwrap();
    *x.api().fail_reads.lock().unwrap() = true;
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(1, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(x.api().placed.lock().unwrap().is_empty());
    assert_eq!(x.venue(), None);
    assert_eq!(x.venue_position(), None);
    // Exits do not need the account read.
    x.execute(
        T0 + 2,
        vec![Command::Place(intent(2, OrderKind::Ioc, 1.0, true))],
        &m(),
    )
    .await;
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
    // Readable again: allowed.
    *x.api().fail_reads.lock().unwrap() = false;
    let ev = x
        .execute(
            T0 + 3,
            vec![Command::Place(intent(3, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);
}

#[tokio::test]
async fn canceled_entry_with_unknown_fill_blocks_reentry() {
    // Direct executor version of the engine regression: the cancel succeeds
    // but orderStatus gives no filled size; the fills never arrive.
    let mut x = exec(Fake::default());
    x.startup(T0).await.unwrap();
    x.execute(
        T0 + 1,
        vec![Command::Place(intent(1, OrderKind::Alo, 100.0, false))],
        &m(),
    )
    .await;
    *x.api().szi.lock().unwrap() = 99.0;
    x.execute(T0 + 2, vec![Command::Cancel { tag: 1 }], &m())
        .await;
    assert!(x.state().orders[0].done);
    // Reads fail: refused.
    *x.api().fail_reads.lock().unwrap() = true;
    let ev = x
        .execute(
            T0 + 80_000,
            vec![Command::Place(intent(2, OrderKind::Alo, 100.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 2), "{ev:?}");
    // Reads succeed: the venue position counts, still refused.
    *x.api().fail_reads.lock().unwrap() = false;
    let ev = x
        .execute(
            T0 + 81_000,
            vec![Command::Place(intent(3, OrderKind::Alo, 100.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 3), "{ev:?}");
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unconfirmed_fill_counts_until_fills_arrive_or_settle() {
    // The account read right after the cancel does not show the fill yet
    // (lagging info node): the unconfirmed size still counts.
    let mut x = exec_with(&small_cfg(300.0), 10, Fake::default());
    x.startup(T0).await.unwrap();
    x.execute(
        T0 + 1,
        vec![Command::Place(intent(1, OrderKind::Alo, 2.0, false))],
        &m(),
    )
    .await;
    let cloid = x.state().orders[0].cloid;
    x.api()
        .filled
        .lock()
        .unwrap()
        .insert(cloid.to_string(), 2.0);
    x.execute(T0 + 2, vec![Command::Cancel { tag: 1 }], &m())
        .await;
    assert_eq!(x.state().orders[0].done_filled_sz, Some(2.0));
    // 200 USD unconfirmed + 200 USD new > 306 USD: refused.
    let ev = x
        .execute(
            T0 + 3,
            vec![Command::Place(intent(2, OrderKind::Alo, 2.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 2), "{ev:?}");
    // The fills arrive: confirmed, and the venue now shows the position.
    x.on_user_event(
        T0 + 4,
        UserEvent::Fills {
            is_snapshot: false,
            fills: vec![fill(Some(cloid), 1, 900, T0 + 1, 2.0)],
        },
    )
    .await;
    *x.api().szi.lock().unwrap() = 2.0;
    let ev = x
        .execute(
            T0 + 5,
            vec![Command::Place(intent(3, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);

    // Fills that never arrive: released by an account read taken
    // FILL_SETTLE_MS after the order finished (which reflects them).
    let mut y = exec_with(&small_cfg(300.0), 10, Fake::default());
    y.startup(T0).await.unwrap();
    y.execute(
        T0 + 1,
        vec![Command::Place(intent(1, OrderKind::Alo, 2.0, false))],
        &m(),
    )
    .await;
    y.execute(T0 + 2, vec![Command::Cancel { tag: 1 }], &m())
        .await; // filled size unknown: the whole 2 lots count
    let ev = y
        .execute(
            T0 + 2 + FILL_SETTLE_MS - 1,
            vec![Command::Place(intent(2, OrderKind::Alo, 2.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 2), "{ev:?}");
    let ev = y
        .execute(
            T0 + 2 + FILL_SETTLE_MS,
            vec![Command::Place(intent(3, OrderKind::Alo, 2.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);
}

#[tokio::test]
async fn unsafe_margin_position_is_exit_only_then_isolated_before_reentry() {
    // Review M1 r1 finding 2: a position carried at startup on CROSS 5x.
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 1.0;
    *fake.pos_leverage.lock().unwrap() = Some(("cross".into(), 5));
    let mut x = exec(fake);
    let ev = x.startup(T0).await.unwrap();
    assert!(ev
        .iter()
        .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("exits only"))));
    // A resumed partial entry cannot grow it.
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(1, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(
        ev.iter()
            .any(|e| matches!(e, ExecEvent::Alert(a) if a.contains("CROSS margin at 5x"))),
        "{ev:?}"
    );
    // Exits go out.
    x.execute(
        T0 + 2,
        vec![Command::Place(intent(2, OrderKind::Alo, 1.0, true))],
        &m(),
    )
    .await;
    assert_eq!(x.api().placed.lock().unwrap().len(), 1);
    assert!(x.api().leverage_calls.lock().unwrap().is_empty());
    // Closed: the next entry first establishes isolated 1x, then goes out.
    *x.api().szi.lock().unwrap() = 0.0;
    let ev = x
        .execute(
            T0 + 3,
            vec![Command::Place(intent(3, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);
    assert_eq!(*x.api().leverage_calls.lock().unwrap(), vec![1]);
    // Its top-up on an isolated 1x position is fine, without a new call.
    *x.api().szi.lock().unwrap() = 1.0;
    *x.api().pos_leverage.lock().unwrap() = Some(("isolated".into(), 1));
    let ev = x
        .execute(
            T0 + 4,
            vec![Command::Place(intent(4, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 4 })]);
    assert_eq!(x.api().leverage_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn isolated_position_above_bot_leverage_cannot_grow() {
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 1.0;
    *fake.pos_leverage.lock().unwrap() = Some(("isolated".into(), 3));
    let mut x = exec(fake); // bot leverage 1x
    x.startup(T0).await.unwrap();
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(1, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(x.api().placed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_update_leverage_refuses_the_entry() {
    // Started with a (safe) position, now flat: the entry from flat needs
    // isolated leverage first; when that fails nothing is sent.
    let fake = Fake::default();
    *fake.szi.lock().unwrap() = 1.0;
    let mut x = exec(fake);
    x.startup(T0).await.unwrap();
    *x.api().szi.lock().unwrap() = 0.0;
    *x.api().fail_leverage.lock().unwrap() = true;
    let ev = x
        .execute(
            T0 + 1,
            vec![Command::Place(intent(1, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert!(refused(&ev, 1), "{ev:?}");
    assert!(x.api().placed.lock().unwrap().is_empty());
    assert_eq!(x.api().leverage_calls.lock().unwrap().len(), 1);
    *x.api().fail_leverage.lock().unwrap() = false;
    let ev = x
        .execute(
            T0 + 2,
            vec![Command::Place(intent(2, OrderKind::Alo, 1.0, false))],
            &m(),
        )
        .await;
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 2 })]);
}

#[tokio::test]
async fn isolated_leverage_is_resent_before_a_later_entry_from_flat() {
    let mut x = exec(Fake::default());
    x.startup(T0).await.unwrap();
    assert_eq!(x.api().leverage_calls.lock().unwrap().len(), 1);
    // Soon after startup: no new call.
    x.execute(T0 + 1, vec![Command::Place(alo(1))], &m()).await;
    assert_eq!(x.api().leverage_calls.lock().unwrap().len(), 1);
    // Much later (a manual change while flat would be invisible): re-sent.
    let t = T0 + LEVERAGE_RECHECK_MS + 1;
    x.execute(t, vec![Command::Cancel { tag: 1 }], &m()).await;
    x.execute(t, vec![Command::Place(alo(2))], &m()).await;
    assert_eq!(*x.api().leverage_calls.lock().unwrap(), vec![1, 1]);
}

#[tokio::test]
async fn cancel_after_prune_targets_the_right_order() {
    // More than DONE_ORDERS_KEEP finished orders: the prune inside cancel
    // shifts indexes; the cancel must still go to the requested order.
    let fake = Fake::default();
    let n = DONE_ORDERS_KEEP as u64 + 1;
    for _ in 0..n {
        fake.place_replies
            .lock()
            .unwrap()
            .push_back(Ok(OrderStatus::Error("rejected".into())));
    }
    let mut p = params();
    p.live.max_exchange_actions_per_min = 1_000;
    let mut x = LiveExecutor::new(p, fake, None, T0).unwrap();
    for tag in 1..=n {
        x.execute(
            T0,
            vec![Command::Place(intent(tag, OrderKind::Alo, 1.0, true))],
            &m(),
        )
        .await;
    }
    let (b, a) = (n + 1, n + 2);
    x.execute(
        T0,
        vec![
            Command::Place(intent(b, OrderKind::Alo, 1.0, true)),
            Command::Place(intent(a, OrderKind::Alo, 1.0, true)),
        ],
        &m(),
    )
    .await;
    let cloid_of = |x: &LiveExecutor<Fake>, tag: u64| {
        x.state()
            .orders
            .iter()
            .find(|o| o.tag == tag)
            .unwrap()
            .cloid
    };
    let (cb, ca) = (cloid_of(&x, b), cloid_of(&x, a));
    // B is canceled on the exchange (orderUpdates): one more finished order.
    let up = crate::hl::types::OrderUpdate {
        order: OpenOrder {
            coin: "xyz:ANT".into(),
            side: "B".into(),
            limit_px: 100.0,
            sz: 1.0,
            oid: 1,
            timestamp: T0,
            orig_sz: Some(1.0),
            cloid: Some(cb.to_string()),
        },
        status: "canceled".into(),
        status_timestamp: T0 + 1,
    };
    x.on_user_event(T0 + 1, UserEvent::OrderUpdates(vec![up]))
        .await;
    let ev = x
        .execute(T0 + 2, vec![Command::Cancel { tag: a }], &m())
        .await;
    assert_eq!(*x.api().canceled.lock().unwrap(), vec![ca]);
    assert_eq!(dones(&ev), vec![(a, DoneReason::Canceled)]);
}
