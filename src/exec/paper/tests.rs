use super::*;
use crate::hl::market::DataSource;
use crate::hl::types::L2Book;
use crate::strategy::signal::Purpose;

const T0: u64 = 1_800_000_000_000;

fn params() -> PaperParams {
    PaperParams {
        maker_rate: 0.00003,
        taker_rate: 0.00009,
        fill_on_touch: true,
        sz_decimals: 2,
        initial_balance_usd: 10_000.0,
        leverage: 1,
        stale_ms: 15_000,
    }
}

fn lvl(px: f64, sz: f64) -> Level {
    Level { px, sz, n: 1 }
}

fn market(now: u64, bids: Vec<Level>, asks: Vec<Level>) -> MarketState {
    let mut m = MarketState::new("xyz:ANT");
    m.apply_book(
        &L2Book {
            coin: "xyz:ANT".into(),
            time: now,
            levels: (bids, asks),
        },
        now,
        DataSource::Rest,
    );
    m.oracle_px = Some(100.0);
    m.mark_px = Some(100.0);
    m.funding = Some(0.0001);
    m
}

fn book(now: u64) -> MarketState {
    market(
        now,
        vec![lvl(100.0, 5.0), lvl(99.99, 10.0)],
        vec![lvl(100.02, 3.0), lvl(100.03, 4.0), lvl(100.10, 100.0)],
    )
}

fn alo(tag: u64, is_buy: bool, px: f64, sz: f64) -> OrderIntent {
    OrderIntent {
        tag,
        is_buy,
        px,
        sz,
        kind: OrderKind::Alo,
        reduce_only: false,
        purpose: Purpose::Entry,
    }
}

fn ioc(tag: u64, is_buy: bool, px: f64, sz: f64, reduce_only: bool) -> OrderIntent {
    OrderIntent {
        tag,
        is_buy,
        px,
        sz,
        kind: OrderKind::Ioc,
        reduce_only,
        purpose: Purpose::Exit,
    }
}

fn trade(px: f64, sz: f64, buyer: bool, time: u64, tid: u64) -> WsTrade {
    WsTrade {
        coin: "xyz:ANT".into(),
        side: if buyer { "B" } else { "A" }.into(),
        px,
        sz,
        time,
        tid,
    }
}

fn fills(ev: &[ExecEvent]) -> Vec<(f64, f64, f64, bool)> {
    ev.iter()
        .filter_map(|e| match e {
            ExecEvent::Order(OrderUpdate::Filled {
                px,
                sz,
                fee_usd,
                crossed,
                ..
            }) => Some((*px, *sz, *fee_usd, *crossed)),
            _ => None,
        })
        .collect()
}

fn dones(ev: &[ExecEvent]) -> Vec<(u64, DoneReason)> {
    ev.iter()
        .filter_map(|e| match e {
            ExecEvent::Order(OrderUpdate::Done { tag, reason }) => Some((*tag, reason.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn alo_that_would_cross_is_rejected() {
    let mut s = PaperSim::new(params());
    let m = book(T0);
    let ev = s.place(T0, alo(1, true, 100.02, 1.0), &m);
    assert_eq!(dones(&ev), vec![(1, DoneReason::PostOnlyRejected)]);
    let ev = s.place(T0, alo(2, false, 100.0, 1.0), &m);
    assert_eq!(dones(&ev), vec![(2, DoneReason::PostOnlyRejected)]);
    assert!(s.open_orders().is_empty());
    // At the touch on our own side: rests.
    let ev = s.place(T0, alo(3, true, 100.0, 1.0), &m);
    assert_eq!(ev, vec![ExecEvent::Order(OrderUpdate::Resting { tag: 3 })]);
    // No book / stale book: rejected.
    let ev = s.place(T0, alo(4, true, 99.0, 1.0), &MarketState::new("xyz:ANT"));
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(m) if m == "no book"));
    let ev = s.place(T0 + 60_000, alo(5, true, 99.0, 1.0), &m);
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(m) if m == "book stale"));
}

#[test]
fn resting_bid_fills_on_trade_through_and_touch() {
    let mut s = PaperSim::new(params());
    s.place(T0, alo(1, true, 100.0, 2.0), &book(T0));
    // Trade older than the order: ignored (ws snapshot on resubscribe).
    assert!(s
        .on_trades(&[trade(99.0, 5.0, false, T0 - 1, 1)])
        .is_empty());
    // Seller hits at our price (touch): partial fill up to the trade size.
    let ev = s.on_trades(&[trade(100.0, 0.5, false, T0 + 1, 2)]);
    assert_eq!(fills(&ev), vec![(100.0, 0.5, 100.0 * 0.5 * 0.00003, false)]);
    // Buyer aggressor at our price cannot hit our bid.
    assert!(s
        .on_trades(&[trade(100.0, 1.0, true, T0 + 2, 3)])
        .is_empty());
    // Same trade id again (resubscription snapshot): ignored.
    assert!(s
        .on_trades(&[trade(100.0, 0.5, false, T0 + 1, 2)])
        .is_empty());
    // Trade through our price: fills the remainder, order done.
    let ev = s.on_trades(&[trade(99.98, 10.0, false, T0 + 3, 4)]);
    assert_eq!(fills(&ev), vec![(100.0, 1.5, 100.0 * 1.5 * 0.00003, false)]);
    assert_eq!(dones(&ev), vec![(1, DoneReason::Filled)]);
    assert!(s.open_orders().is_empty());
    assert!((s.account().position_sz - 2.0).abs() < 1e-12);
    assert!((s.account().entry_px - 100.0).abs() < 1e-12);
}

#[test]
fn touch_does_not_fill_when_disabled() {
    let mut p = params();
    p.fill_on_touch = false;
    let mut s = PaperSim::new(p);
    s.place(T0, alo(1, false, 100.02, 1.0), &book(T0));
    assert!(s
        .on_trades(&[trade(100.02, 5.0, true, T0 + 1, 1)])
        .is_empty());
    let ev = s.on_trades(&[trade(100.03, 5.0, true, T0 + 2, 2)]);
    assert_eq!(fills(&ev).len(), 1);
    assert!((s.account().position_sz + 1.0).abs() < 1e-12);
}

#[test]
fn resting_order_fills_when_book_crosses_it() {
    let mut s = PaperSim::new(params());
    s.place(T0, alo(1, true, 100.0, 5.0), &book(T0));
    // Same book again (ctx-only update): nothing.
    assert!(fills(&s.on_book(T0 + 1, &book(T0))).is_empty());
    // Ask drops to our bid with 1.5 lots at 100.0 and 1.0 below.
    let m = market(
        T0 + 2,
        vec![lvl(99.9, 1.0)],
        vec![lvl(99.95, 1.0), lvl(100.0, 1.5), lvl(100.01, 9.0)],
    );
    let ev = s.on_book(T0 + 2, &m);
    // Maker fill at our own price, up to the crossing depth (2.5).
    assert_eq!(fills(&ev), vec![(100.0, 2.5, 100.0 * 2.5 * 0.00003, false)]);
    assert!(dones(&ev).is_empty());
    // Re-using that book does not fill again.
    assert!(fills(&s.on_book(T0 + 3, &m)).is_empty());
    assert_eq!(s.open_orders()[0].filled, 2.5);
}

#[test]
fn ioc_walks_the_book_with_taker_fee() {
    let mut s = PaperSim::new(params());
    let m = book(T0);
    // Buy 5 with limit 100.05: 3 @ 100.02 + 2 @ 100.03.
    let ev = s.place(T0, ioc(1, true, 100.05, 5.0, false), &m);
    let f = fills(&ev);
    assert_eq!(f.len(), 2);
    assert_eq!((f[0].0, f[0].1, f[0].3), (100.02, 3.0, true));
    assert_eq!((f[1].0, f[1].1), (100.03, 2.0));
    assert!((f[0].2 - 100.02 * 3.0 * 0.00009).abs() < 1e-12);
    assert_eq!(dones(&ev), vec![(1, DoneReason::Filled)]);
    // Limit allows only the first two levels (7 lots): remainder expires.
    let ev = s.place(T0, ioc(2, true, 100.03, 10.0, false), &m);
    assert_eq!(fills(&ev).iter().map(|f| f.1).sum::<f64>(), 7.0);
    assert_eq!(dones(&ev), vec![(2, DoneReason::IocExpired)]);
}

#[test]
fn reduce_only_is_capped_and_canceled_without_position() {
    let mut s = PaperSim::new(params());
    let m = book(T0);
    let ev = s.place(T0, ioc(1, false, 99.0, 1.0, true), &m);
    assert!(matches!(&dones(&ev)[0].1, DoneReason::Rejected(_)));
    // Long 2, reduce-only sell 5 -> only 2 trade.
    s.place(T0, ioc(2, true, 101.0, 2.0, false), &m);
    let ev = s.place(T0, ioc(3, false, 99.0, 5.0, true), &m);
    assert!((fills(&ev).iter().map(|f| f.1).sum::<f64>() - 2.0).abs() < 1e-12);
    assert_eq!(dones(&ev), vec![(3, DoneReason::Filled)]);
    assert_eq!(s.account().position_sz, 0.0);
}

#[test]
fn account_pnl_fees_and_margin() {
    let mut s = PaperSim::new(params());
    let m = book(T0);
    // Short 3 @ 100.0 (IOC hits bid), cover 3 @ 100.02 (IOC lifts ask).
    s.place(T0, ioc(1, false, 99.0, 3.0, false), &m);
    assert!((s.account().position_sz + 3.0).abs() < 1e-12);
    assert!((s.available_margin(Some(100.0)) - (10_000.0 - 300.0 - 300.0 * 0.00009)).abs() < 1e-9);
    s.place(T0, ioc(2, true, 101.0, 3.0, true), &m);
    let a = s.account();
    assert_eq!(a.position_sz, 0.0);
    assert!((a.realized_pnl_usd - (-0.06)).abs() < 1e-9);
    let fees = (300.0 + 300.06) * 0.00009;
    assert!((a.fees_usd - fees).abs() < 1e-12);
    assert!((s.equity(Some(100.0)) - (10_000.0 - 0.06 - fees)).abs() < 1e-9);
    assert_eq!(a.fills, 2);
}

#[test]
fn funding_settles_on_hour_boundaries() {
    let mut s = PaperSim::new(params());
    let h = 3_600_000u64;
    let t = (T0 / h) * h + 1_000;
    let mut m = book(t);
    assert!(s.funding_clock(t, &m).is_empty()); // first sample arms the clock
    s.place(t, ioc(1, true, 101.0, 2.0, false), &m); // long 2
    assert!(s.funding_clock(t + 1_000, &m).is_empty()); // same hour
    m.funding = Some(0.0001);
    let ev = s.funding_clock(t + h, &m);
    // Long pays positive funding: -2 * 100 * 0.0001 = -0.02.
    assert_eq!(
        ev,
        vec![ExecEvent::Funding {
            usd: -0.02,
            time_ms: (t / h + 1) * h
        }]
    );
    assert!((s.account().funding_usd + 0.02).abs() < 1e-12);
}

#[test]
fn cancel_and_state_roundtrip() {
    let mut s = PaperSim::new(params());
    s.place(T0, alo(1, true, 99.0, 1.0), &book(T0));
    s.place(T0, ioc(2, true, 101.0, 1.0, false), &book(T0));
    let json = serde_json::to_value(s.state()).unwrap();
    let back: PaperState = serde_json::from_value(json).unwrap();
    assert_eq!(&back, s.state());
    let mut r = PaperSim::from_state(params(), back);
    assert_eq!(r.cancel(1), vec![done(1, DoneReason::Canceled)]);
    assert!(r.cancel(1).is_empty(), "second cancel is a no-op");
    assert!((r.account().position_sz - 1.0).abs() < 1e-12);
}
