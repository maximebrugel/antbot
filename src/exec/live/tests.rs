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
        Ok(())
    }
    async fn clearinghouse(&self) -> anyhow::Result<ClearinghouseState> {
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
                        leverage: Leverage {
                            kind: "isolated".into(),
                            value: 1,
                            raw_usd: None,
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
    LiveParams::from_config(&Config::default(), 2)
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
        crate::runner::heal_working(e, x, now)
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
