//! Unit tests: one or more per transition of the state machine.

use super::*;
use crate::config::Config;

const T0: u64 = 1_800_000_000_000;
const S: u64 = 1_000;
const M: u64 = 60 * S;
const H: u64 = 60 * M;
const BID: f64 = 100.00;
const ASK: f64 = 100.02;

fn params() -> StrategyParams {
    StrategyParams::from_config(&Config::default(), 2, 10)
}

fn strat() -> Strategy {
    Strategy::new(params())
}

fn view(now: u64, prem: Option<f64>, bid: f64, ask: f64) -> MarketView {
    MarketView {
        now_ms: now,
        premium_bps: prem,
        best_bid: Some(bid),
        best_ask: Some(ask),
        stale: false,
    }
}

fn tick_full(s: &mut Strategy, v: MarketView, allowed: bool) -> Step {
    s.on_tick(TickInput {
        view: v,
        entries_allowed: allowed,
        available_margin_usd: None,
    })
}

fn tick(s: &mut Strategy, now: u64, prem: f64) -> Step {
    tick_full(s, view(now, Some(prem), BID, ASK), true)
}

fn tick_px(s: &mut Strategy, now: u64, prem: f64, bid: f64, ask: f64) -> Step {
    tick_full(s, view(now, Some(prem), bid, ask), true)
}

fn placed(st: &Step) -> Vec<OrderIntent> {
    st.commands
        .iter()
        .filter_map(|c| match c {
            Command::Place(o) => Some(*o),
            _ => None,
        })
        .collect()
}

fn cancels(st: &Step) -> Vec<u64> {
    st.commands
        .iter()
        .filter_map(|c| match c {
            Command::Cancel { tag } => Some(*tag),
            _ => None,
        })
        .collect()
}

fn closed(st: &Step) -> Option<TradeRecord> {
    st.events.iter().find_map(|e| match e {
        Event::TradeClosed(r) => Some(r.clone()),
        _ => None,
    })
}

fn fill(s: &mut Strategy, now: u64, o: &OrderIntent, px: f64, sz: f64) -> Step {
    s.on_order_update(
        now,
        OrderUpdate::Filled {
            tag: Some(o.tag),
            is_buy: o.is_buy,
            px,
            sz,
            fee_usd: px
                * sz
                * if o.kind == OrderKind::Ioc {
                    0.00009
                } else {
                    0.00003
                },
            crossed: o.kind == OrderKind::Ioc,
            time_ms: now,
        },
    )
}

/// `Done` reporting exactly the fills the strategy has already seen.
fn done(s: &mut Strategy, now: u64, tag: u64, reason: DoneReason) -> Step {
    let filled_sz = s
        .working()
        .filter(|w| w.intent.tag == tag)
        .map(|w| w.filled);
    s.on_order_update(
        now,
        OrderUpdate::Done {
            tag,
            reason,
            filled_sz,
        },
    )
}

/// Enter a short at `T0` (premium +60 bps), fully filled at the ask.
fn short_in_position(s: &mut Strategy) -> OrderIntent {
    let st = tick(s, T0, 60.0);
    let o = placed(&st)[0];
    fill(s, T0 + S, &o, o.px, o.sz);
    done(s, T0 + S, o.tag, DoneReason::Filled);
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    o
}

// ------------------------------------------------------------- Flat

#[test]
fn flat_to_entering_short_on_positive_premium() {
    let mut s = strat();
    let st = tick(&mut s, T0, 60.0);
    assert_eq!(s.phase().kind(), PhaseKind::Entering);
    let o = placed(&st);
    assert_eq!(o.len(), 1);
    let o = o[0];
    assert!(!o.is_buy, "premium > 0 -> short -> sell");
    assert_eq!(o.kind, OrderKind::Alo);
    assert_eq!(o.purpose, Purpose::Entry);
    assert!(!o.reduce_only);
    assert_eq!(o.px, ASK, "maker sell joins the best ask");
    assert_eq!(o.sz, 24.99); // 2500 / 100.02 = 24.995 -> 24.99
    assert!(matches!(
        st.events[0],
        Event::EntryStarted {
            side: Side::Short,
            ..
        }
    ));
}

#[test]
fn flat_to_entering_long_on_negative_premium() {
    let mut s = strat();
    let st = tick(&mut s, T0, -50.0); // threshold is inclusive
    let o = placed(&st)[0];
    assert!(o.is_buy);
    assert_eq!(o.px, BID);
    assert_eq!(o.sz, 25.0);
}

#[test]
fn flat_stays_flat_without_valid_signal() {
    let mut s = strat();
    assert!(tick(&mut s, T0, 49.9).is_empty());
    assert!(tick(&mut s, T0, -49.9).is_empty());
    // missing premium
    assert!(tick_full(&mut s, view(T0, None, BID, ASK), true).is_empty());
    // stale data
    let mut v = view(T0, Some(80.0), BID, ASK);
    v.stale = true;
    assert!(tick_full(&mut s, v, true).is_empty());
    // entries blocked by risk
    assert!(tick_full(&mut s, view(T0, Some(80.0), BID, ASK), false).is_empty());
    // no book
    let v = MarketView {
        now_ms: T0,
        premium_bps: Some(80.0),
        best_bid: None,
        best_ask: None,
        stale: false,
    };
    assert!(tick_full(&mut s, v, true).is_empty());
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
}

#[test]
fn sizing_failure_warns_and_backs_off() {
    let mut s = strat();
    let st = s.on_tick(TickInput {
        view: view(T0, Some(80.0), BID, ASK),
        entries_allowed: true,
        available_margin_usd: Some(0.0),
    });
    assert!(st.commands.is_empty());
    assert!(matches!(st.events[0], Event::Warning(_)));
    assert_eq!(s.state().cooldown_until_ms, T0 + 60 * S);
    assert!(tick(&mut s, T0 + 30 * S, 80.0).is_empty(), "retry cooldown");
    assert_eq!(placed(&tick(&mut s, T0 + 60 * S, 80.0)).len(), 1);
}

// ------------------------------------------------------------- Entering

#[test]
fn entering_to_in_position_on_full_fill() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    s.on_order_update(T0 + S, OrderUpdate::Resting { tag: o.tag });
    assert!(s.working().unwrap().acked);
    let st = fill(&mut s, T0 + 2 * S, &o, o.px, o.sz);
    assert!(st.commands.is_empty(), "filled order is not canceled");
    assert_eq!(s.phase().kind(), PhaseKind::Entering, "waits for Done");
    let st = done(&mut s, T0 + 2 * S, o.tag, DoneReason::Filled);
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    assert!(matches!(st.events[0], Event::PositionOpened { size, .. } if size == o.sz));
    let p = s.position().unwrap();
    assert_eq!(p.side, Side::Short);
    assert_eq!(p.opened_ms, T0 + 2 * S, "hold clock starts at first fill");
    assert_eq!(p.entry_premium_bps, Some(60.0));
    assert!(s.working().is_none());
}

#[test]
fn entering_requote_on_drift_after_interval() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    // Too early: touch moved but interval not elapsed.
    assert!(tick_px(&mut s, T0 + 10 * S, 60.0, 99.88, 99.90).is_empty());
    // Interval elapsed but drift 2 bps < 5 bps.
    assert!(tick_px(&mut s, T0 + 20 * S, 60.0, 99.98, 100.00).is_empty());
    // Interval elapsed and drift 12 bps: cancel first...
    let st = tick_px(&mut s, T0 + 21 * S, 60.0, 99.88, 99.90);
    assert_eq!(cancels(&st), vec![o.tag]);
    assert!(placed(&st).is_empty(), "never two live orders");
    assert!(matches!(st.events[0], Event::Requote { new_px, .. } if new_px == 99.90));
    // ...a partial fill races the cancel...
    fill(&mut s, T0 + 21 * S, &o, o.px, 10.0);
    // ...then replace for the remainder once the cancel is confirmed.
    let st = done(&mut s, T0 + 22 * S, o.tag, DoneReason::Canceled);
    let n = placed(&st);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].px, 99.90);
    assert_eq!(n[0].sz, 14.99);
    assert_ne!(n[0].tag, o.tag);
    assert_eq!(s.phase().kind(), PhaseKind::Entering);
}

#[test]
fn entering_post_only_reject_is_requoted_after_min_interval() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    let st = done(&mut s, T0 + 100, o.tag, DoneReason::PostOnlyRejected);
    assert!(placed(&st).is_empty(), "min_order_interval_ms");
    assert!(matches!(st.events[0], Event::OrderRejected { .. }));
    let st = tick(&mut s, T0 + S, 60.0);
    assert_eq!(placed(&st).len(), 1);
    assert_eq!(s.phase().kind(), PhaseKind::Entering);
}

#[test]
fn entering_rejected_aborts_to_flat() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    let st = done(
        &mut s,
        T0 + S,
        o.tag,
        DoneReason::Rejected("Insufficient margin".into()),
    );
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    assert!(st.events.iter().any(|e| matches!(
        e,
        Event::EntryAborted {
            reason: AbortReason::Rejected
        }
    )));
    assert_eq!(s.state().cooldown_until_ms, T0 + S + 60 * S);
}

#[test]
fn entering_timeout_without_fill_goes_flat_with_retry_cooldown() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    assert!(cancels(&tick(&mut s, T0 + 899 * S, 60.0)).is_empty());
    let st = tick(&mut s, T0 + 900 * S, 60.0);
    assert_eq!(cancels(&st), vec![o.tag]);
    // repeated ticks do not repeat the cancel
    assert!(tick(&mut s, T0 + 901 * S, 60.0).commands.is_empty());
    let st = done(&mut s, T0 + 902 * S, o.tag, DoneReason::Canceled);
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    assert!(st.events.contains(&Event::EntryAborted {
        reason: AbortReason::Timeout
    }));
    assert!(s.position().is_none());
    assert!(tick(&mut s, T0 + 950 * S, 60.0).is_empty());
    assert_eq!(placed(&tick(&mut s, T0 + 962 * S, 60.0)).len(), 1);
}

#[test]
fn entering_timeout_with_partial_fill_keeps_partial_position() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    fill(&mut s, T0 + 100 * S, &o, o.px, 5.0);
    let st = tick(&mut s, T0 + 900 * S, 60.0);
    assert_eq!(cancels(&st), vec![o.tag]);
    // late fill between cancel request and Done is counted
    fill(&mut s, T0 + 900 * S + 1, &o, o.px, 1.0);
    let st = done(&mut s, T0 + 901 * S, o.tag, DoneReason::Canceled);
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    assert!(
        matches!(st.events[0], Event::PositionOpened { size, .. } if (size - 6.0).abs() < 1e-9)
    );
    assert_eq!(s.position().unwrap().opened_ms, T0 + 100 * S);
}

#[test]
fn entering_aborts_when_signal_gone() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    assert!(tick(&mut s, T0 + S, 30.0).is_empty(), "still a dislocation");
    let st = tick(&mut s, T0 + 2 * S, 9.0);
    assert_eq!(cancels(&st), vec![o.tag]);
    let st = done(&mut s, T0 + 3 * S, o.tag, DoneReason::Canceled);
    assert!(st.events.contains(&Event::EntryAborted {
        reason: AbortReason::SignalGone
    }));
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
}

#[test]
fn entering_aborts_when_entries_blocked() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    let st = tick_full(&mut s, view(T0 + S, Some(60.0), BID, ASK), false);
    assert_eq!(cancels(&st), vec![o.tag]);
    let st = done(&mut s, T0 + 2 * S, o.tag, DoneReason::Canceled);
    assert!(st.events.contains(&Event::EntryAborted {
        reason: AbortReason::EntriesBlocked
    }));
}

// ------------------------------------------------------------- InPosition / Exiting

#[test]
fn converge_exit_maker_then_flat_with_cooldown_and_record() {
    let mut s = strat();
    let e = short_in_position(&mut s);
    assert!(tick(&mut s, T0 + H, 20.0).is_empty(), "not yet converged");
    let st = tick_px(&mut s, T0 + 2 * H, 8.0, 99.50, 99.52);
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
    assert!(matches!(
        st.events[0],
        Event::ExitStarted {
            reason: ExitReason::Converged,
            ..
        }
    ));
    let x = placed(&st)[0];
    assert!(x.is_buy && x.reduce_only);
    assert_eq!(x.kind, OrderKind::Alo);
    assert_eq!(x.px, 99.50, "maker buy joins the bid");
    assert_eq!(x.sz, e.sz);
    fill(&mut s, T0 + 2 * H + S, &x, 99.50, x.sz);
    let st = done(&mut s, T0 + 2 * H + S, x.tag, DoneReason::Filled);
    let _ = st;
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    assert!(s.working().is_none());
    // the record was emitted on the fill
    let mut s2 = strat();
    let e2 = short_in_position(&mut s2);
    let x2 = placed(&tick_px(&mut s2, T0 + 2 * H, 8.0, 99.50, 99.52))[0];
    let st = fill(&mut s2, T0 + 2 * H + S, &x2, 99.50, x2.sz);
    let r = closed(&st).expect("trade closed");
    assert_eq!(r.side, Side::Short);
    assert_eq!(r.exit_reason, ExitReason::Converged);
    assert_eq!(r.entry_px, e2.px);
    assert!((r.exit_px - 99.50).abs() < 1e-9);
    let gross = (e2.px - 99.50) * e2.sz;
    assert!((r.gross_pnl_usd - gross).abs() < 1e-9);
    let fees = e2.px * e2.sz * 0.00003 + 99.50 * e2.sz * 0.00003;
    assert!((r.fees_usd - fees).abs() < 1e-9);
    assert!((r.pnl_usd - (gross - fees)).abs() < 1e-9);
    assert!((r.pnl_bps - r.pnl_usd / (e2.px * e2.sz) * 1e4).abs() < 1e-9);
    assert_eq!(r.exit_taker_sz, 0.0);
    assert_eq!(r.entry_premium_bps, Some(60.0));
    assert_eq!(r.exit_premium_bps, Some(8.0));
    assert!(r.moved_toward_zero());
    assert!((r.hold_secs() - (2.0 * 3600.0)).abs() < 1e-9);
    assert_eq!(s2.state().cooldown_until_ms, T0 + 2 * H + S + H);
    // a filled order is never canceled; its Done is awaited
    assert!(tick(&mut s2, T0 + 2 * H + 2 * S, 90.0).commands.is_empty());
    done(&mut s2, T0 + 2 * H + 2 * S, x2.tag, DoneReason::Filled);
    // cooldown blocks a new entry, then lets it through
    assert!(tick(&mut s2, T0 + 2 * H + 30 * M, 90.0).commands.is_empty());
    assert_eq!(placed(&tick(&mut s2, T0 + 3 * H + S, 90.0)).len(), 1);
}

#[test]
fn converge_exit_overshoot_counts() {
    let mut s = strat();
    short_in_position(&mut s);
    tick(&mut s, T0 + H, -35.0);
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
}

#[test]
fn early_exit_disabled_holds_until_schedule() {
    let mut p = params();
    p.early_exit_on_converge = false;
    let mut s = Strategy::new(p);
    short_in_position(&mut s);
    assert!(tick(&mut s, T0 + H, 0.0).is_empty());
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
}

#[test]
fn stale_data_does_not_trigger_converge_exit() {
    let mut s = strat();
    short_in_position(&mut s);
    let mut v = view(T0 + H, Some(0.0), BID, ASK);
    v.stale = true;
    assert!(tick_full(&mut s, v, true).is_empty());
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
}

#[test]
fn max_hold_schedule_maker_then_taker_before_deadline() {
    let mut s = strat();
    short_in_position(&mut s);
    let opened = T0 + S;
    let deadline = opened + 8 * H;
    // maker exit starts at deadline - 15 min - 10 min
    assert!(tick(&mut s, deadline - 25 * M - 1, 40.0).is_empty());
    let st = tick(&mut s, deadline - 25 * M, 40.0);
    assert!(matches!(
        st.events[0],
        Event::ExitStarted {
            reason: ExitReason::MaxHold,
            taker_at_ms,
            ..
        } if taker_at_ms == deadline - 15 * M
    ));
    let x = placed(&st)[0];
    assert_eq!(x.kind, OrderKind::Alo);
    // partial maker fill
    fill(&mut s, deadline - 20 * M, &x, x.px, 4.0);
    assert!(tick(&mut s, deadline - 15 * M - 1, 40.0)
        .commands
        .is_empty());
    // taker window: cancel the maker order first
    let st = tick(&mut s, deadline - 15 * M, 40.0);
    assert_eq!(cancels(&st), vec![x.tag]);
    assert!(placed(&st).is_empty());
    // then IOC for the remainder at best ask + 50 bps (rounded down)
    let st = done(&mut s, deadline - 15 * M + S, x.tag, DoneReason::Canceled);
    let ioc = placed(&st)[0];
    assert_eq!(ioc.kind, OrderKind::Ioc);
    assert!(ioc.is_buy && ioc.reduce_only);
    assert!((ioc.sz - 20.99).abs() < 1e-9);
    assert_eq!(ioc.px, 100.52); // 100.02 * 1.005 = 100.5201 -> 100.52
    assert!(st
        .events
        .iter()
        .any(|e| matches!(e, Event::TakerFallback { .. })));
    // IOC partially fills, remainder expires -> retry
    fill(&mut s, deadline - 15 * M + S, &ioc, 100.03, 15.0);
    let st = done(
        &mut s,
        deadline - 15 * M + S,
        ioc.tag,
        DoneReason::IocExpired,
    );
    assert!(placed(&st).is_empty(), "min interval");
    let st = tick(&mut s, deadline - 15 * M + 3 * S, 40.0);
    let ioc2 = placed(&st)[0];
    assert_eq!(ioc2.kind, OrderKind::Ioc);
    assert!((ioc2.sz - 5.99).abs() < 1e-9);
    assert!(!st
        .events
        .iter()
        .any(|e| matches!(e, Event::TakerFallback { .. })));
    let st = fill(&mut s, deadline - 15 * M + 3 * S, &ioc2, 100.04, ioc2.sz);
    let r = closed(&st).unwrap();
    assert_eq!(r.exit_reason, ExitReason::MaxHold);
    assert!((r.exit_taker_sz - 20.99).abs() < 1e-9);
    assert!(r.closed_ms < deadline);
    done(
        &mut s,
        deadline - 15 * M + 3 * S,
        ioc2.tag,
        DoneReason::Filled,
    );
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    assert!(s.working().is_none());
}

#[test]
fn converge_exit_goes_taker_after_maker_timeout() {
    let mut s = strat();
    short_in_position(&mut s);
    let st = tick(&mut s, T0 + H, 5.0);
    let x = placed(&st)[0];
    assert!(tick(&mut s, T0 + H + 10 * M - 1, 5.0).commands.is_empty());
    let st = tick(&mut s, T0 + H + 10 * M, 5.0);
    assert_eq!(cancels(&st), vec![x.tag]);
    let st = done(&mut s, T0 + H + 10 * M, x.tag, DoneReason::Canceled);
    assert_eq!(placed(&st)[0].kind, OrderKind::Ioc);
}

#[test]
fn exit_maker_requotes() {
    let mut s = strat();
    short_in_position(&mut s);
    let x = placed(&tick(&mut s, T0 + H, 5.0))[0];
    let st = tick_px(&mut s, T0 + H + 30 * S, 5.0, 100.20, 100.22);
    assert_eq!(cancels(&st), vec![x.tag]);
    let st = done(&mut s, T0 + H + 31 * S, x.tag, DoneReason::Canceled);
    assert_eq!(placed(&st)[0].px, 100.20);
}

#[test]
fn stale_book_delays_taker_until_fresh() {
    let mut s = strat();
    short_in_position(&mut s);
    let deadline = T0 + S + 8 * H;
    let mut v = view(deadline - 15 * M, Some(40.0), BID, ASK);
    v.stale = true;
    let st = tick_full(&mut s, v, true);
    assert!(placed(&st).is_empty());
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
    let st = tick(&mut s, deadline - 14 * M, 40.0);
    assert_eq!(placed(&st)[0].kind, OrderKind::Ioc);
}

// ------------------------------------------------------------- kill / resume

#[test]
fn kill_while_flat_halts_and_resume_restores() {
    let mut s = strat();
    let st = s.on_kill(T0, true, "daily loss");
    assert!(matches!(st.events[0], Event::Halted { .. }));
    assert_eq!(s.phase().kind(), PhaseKind::Halted);
    assert!(
        tick(&mut s, T0 + S, 90.0).is_empty(),
        "no entries when halted"
    );
    assert!(s.on_kill(T0 + S, true, "again").is_empty(), "idempotent");
    let st = s.resume(T0 + 2 * S);
    assert!(st.events.contains(&Event::Resumed));
    assert!(st.commands.is_empty(), "waits for a fresh tick");
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    assert_eq!(placed(&tick(&mut s, T0 + 3 * S, 90.0)).len(), 1);
}

#[test]
fn kill_in_position_with_flatten_exits_then_halts() {
    let mut s = strat();
    let e = short_in_position(&mut s);
    let st = s.on_kill(T0 + H, true, "hit rate");
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
    let x = placed(&st)[0];
    assert_eq!(x.kind, OrderKind::Alo);
    assert!(x.reduce_only);
    // taker after kill_flatten_maker_secs (120 s)
    let st = tick(&mut s, T0 + H + 120 * S, 60.0);
    assert_eq!(cancels(&st), vec![x.tag]);
    let st = done(&mut s, T0 + H + 121 * S, x.tag, DoneReason::Canceled);
    let ioc = placed(&st)[0];
    assert_eq!(ioc.kind, OrderKind::Ioc);
    let st = fill(&mut s, T0 + H + 121 * S, &ioc, 100.05, e.sz);
    assert_eq!(closed(&st).unwrap().exit_reason, ExitReason::Kill);
    assert_eq!(s.phase().kind(), PhaseKind::Halted);
    done(&mut s, T0 + H + 121 * S, ioc.tag, DoneReason::Filled);
    assert!(tick(&mut s, T0 + 3 * H, 90.0).is_empty(), "no re-entry");
}

#[test]
fn kill_without_flatten_cancels_and_keeps_position() {
    let mut s = strat();
    short_in_position(&mut s);
    let x = placed(&tick(&mut s, T0 + H, 5.0))[0]; // converge exit resting
    let st = s.on_kill(T0 + H + S, false, "spec change");
    assert_eq!(cancels(&st), vec![x.tag]);
    assert_eq!(s.phase().kind(), PhaseKind::Halted);
    done(&mut s, T0 + H + 2 * S, x.tag, DoneReason::Canceled);
    assert!(s.position().unwrap().size > 0.0);
    // nothing happens while halted, even past the deadline
    assert!(tick(&mut s, T0 + 9 * H, 0.0).is_empty());
    // resume: position managed again (deadline passed -> exit)
    let st = s.resume(T0 + 9 * H);
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
    assert!(st.commands.is_empty(), "no order before a fresh tick");
    let st = tick(&mut s, T0 + 9 * H + S, 0.0);
    assert_eq!(placed(&st)[0].kind, OrderKind::Ioc);
}

#[test]
fn kill_during_entry_with_partial_fill_flattens() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    fill(&mut s, T0 + S, &o, o.px, 3.0);
    let st = s.on_kill(T0 + 2 * S, true, "persistent premium");
    assert_eq!(cancels(&st), vec![o.tag]);
    let st = done(&mut s, T0 + 3 * S, o.tag, DoneReason::Canceled);
    assert_eq!(s.phase().kind(), PhaseKind::Exiting);
    let x = placed(&st)[0];
    assert!(x.reduce_only && x.is_buy);
    assert_eq!(x.sz, 3.0);
}

#[test]
fn kill_during_entry_without_fill_halts() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    s.on_kill(T0 + S, true, "max drawdown");
    done(&mut s, T0 + 2 * S, o.tag, DoneReason::Canceled);
    assert_eq!(s.phase().kind(), PhaseKind::Halted);
    assert!(s.position().is_none());
}

#[test]
fn kill_during_exit_accelerates_taker() {
    let mut s = strat();
    short_in_position(&mut s);
    let x = placed(&tick(&mut s, T0 + H, 5.0))[0];
    s.on_kill(T0 + H + S, true, "daily loss");
    match s.phase() {
        Phase::Exiting {
            reason,
            taker_at_ms,
            ..
        } => {
            assert_eq!(*reason, ExitReason::Kill);
            assert_eq!(*taker_at_ms, T0 + H + S + 120 * S);
        }
        p => panic!("{p:?}"),
    }
    assert_eq!(cancels(&tick(&mut s, T0 + H + 121 * S, 5.0)), vec![x.tag]);
}

// ------------------------------------------------------------- robustness

#[test]
fn unknown_fill_while_flat_is_adopted_and_managed() {
    let mut s = strat();
    let st = s.on_order_update(
        T0,
        OrderUpdate::Filled {
            tag: None,
            is_buy: true,
            px: 100.0,
            sz: 5.0,
            fee_usd: 0.0,
            crossed: false,
            time_ms: T0,
        },
    );
    assert!(st.events.iter().any(|e| matches!(e, Event::Warning(_))));
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    assert_eq!(s.position().unwrap().side, Side::Long);
}

#[test]
fn late_fill_after_done_is_attributed() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    let st = tick(&mut s, T0 + 900 * S, 60.0);
    assert_eq!(cancels(&st), vec![o.tag]);
    done(&mut s, T0 + 901 * S, o.tag, DoneReason::Canceled);
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
    // fill reported after Done (ws ordering): known tag, no warning, adopted
    let st = fill(&mut s, T0 + 902 * S, &o, o.px, 2.0);
    assert!(!st
        .events
        .iter()
        .any(|e| matches!(e, Event::Warning(w) if w.contains("unknown order"))));
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
}

#[test]
fn duplicate_done_and_invalid_fill_are_ignored() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    let st = s.on_order_update(
        T0,
        OrderUpdate::Filled {
            tag: Some(o.tag),
            is_buy: false,
            px: f64::NAN,
            sz: 1.0,
            fee_usd: 0.0,
            crossed: false,
            time_ms: T0,
        },
    );
    assert!(matches!(st.events[0], Event::Warning(_)));
    assert!(s.position().is_none());
    assert!(done(&mut s, T0, 999, DoneReason::Canceled).is_empty());
}

#[test]
fn order_actions_are_rate_limited() {
    let mut p = params();
    p.max_order_actions_per_min = 3;
    p.min_order_interval_ms = 0;
    let mut s = Strategy::new(p);
    let mut now = T0;
    let mut placed_n = 0;
    // Post-only reject loop: every place is immediately rejected.
    for _ in 0..10 {
        let mut pending = placed(&tick(&mut s, now, 60.0));
        while let Some(o) = pending.pop() {
            placed_n += 1;
            pending.extend(placed(&done(
                &mut s,
                now,
                o.tag,
                DoneReason::PostOnlyRejected,
            )));
        }
        now += S;
    }
    assert_eq!(placed_n, 3);
    // after a minute the budget is back
    let st = tick(&mut s, T0 + 61 * S, 60.0);
    assert_eq!(placed(&st).len(), 1);
}

#[test]
fn state_roundtrip_and_determinism() {
    let run = || {
        let mut s = strat();
        let mut out = Vec::new();
        let o = placed(&tick(&mut s, T0, 60.0))[0];
        out.push(fill(&mut s, T0 + S, &o, o.px, o.sz));
        out.push(done(&mut s, T0 + S, o.tag, DoneReason::Filled));
        out.push(tick(&mut s, T0 + H, 5.0));
        (s, out)
    };
    let (a, out_a) = run();
    let (b, out_b) = run();
    assert_eq!(out_a, out_b);
    assert_eq!(a.state(), b.state());
    let json = serde_json::to_string(a.state()).unwrap();
    let back: StrategyState = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, a.state());
    // a resumed machine continues identically
    let mut r = Strategy::from_state(params(), back);
    let mut a = a;
    let x = a.working().unwrap().intent;
    assert_eq!(
        fill(&mut r, T0 + H + S, &x, x.px, x.sz),
        fill(&mut a, T0 + H + S, &x, x.px, x.sz)
    );
}

#[test]
fn adopt_position_and_unrealized_pnl() {
    let mut s = strat();
    s.adopt_position(Side::Long, 10.0, 100.0, T0).unwrap();
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    assert!(s.adopt_position(Side::Long, 1.0, 100.0, T0).is_err());
    let p = s.position().unwrap();
    assert!((p.unrealized_pnl(101.0) - 10.0).abs() < 1e-9);
    let mut s = strat();
    assert!(s.adopt_position(Side::Short, 0.0, 100.0, T0).is_err());
}

#[test]
fn funding_is_added_to_trade() {
    let mut s = strat();
    let e = short_in_position(&mut s);
    s.on_funding(1.25);
    s.on_funding(f64::NAN);
    let x = placed(&tick(&mut s, T0 + H, 5.0))[0];
    let r = closed(&fill(&mut s, T0 + H + S, &x, x.px, e.sz)).unwrap();
    assert!((r.funding_usd - 1.25).abs() < 1e-12);
    assert!((r.pnl_usd - (r.gross_pnl_usd - r.fees_usd + 1.25)).abs() < 1e-9);
}

#[test]
fn market_view_from_market_state() {
    use crate::hl::market::{DataSource, MarketState};
    use crate::hl::types::{L2Book, Level, PerpAssetCtx};
    let mut m = MarketState::new("xyz:ANT");
    m.ws_connected = true;
    m.apply_ctx(
        &PerpAssetCtx {
            funding: 0.0,
            open_interest: 1.0,
            prev_day_px: None,
            day_ntl_vlm: None,
            premium: None,
            oracle_px: 100.0,
            mark_px: 100.0,
            mid_px: None,
            impact_pxs: Some((100.4, 100.6)),
            day_base_vlm: None,
        },
        T0,
        DataSource::Ws,
    );
    let l = |px| Level { px, sz: 1.0, n: 1 };
    m.apply_book(
        &L2Book {
            coin: "xyz:ANT".into(),
            time: 1,
            levels: (vec![l(100.49)], vec![l(100.51)]),
        },
        T0,
        DataSource::Ws,
    );
    let v = MarketView::from_market(&m, T0 + S, PremiumSource::Impact, 15_000);
    assert!((v.premium_bps.unwrap() - 50.0).abs() < 1e-9);
    assert_eq!(v.best_bid, Some(100.49));
    assert!(!v.stale);
    let v = MarketView::from_market(&m, T0 + 20 * S, PremiumSource::Impact, 15_000);
    assert!(v.stale);
}

// ------------------------------------------------- late fills / shutdown

fn big_params() -> StrategyParams {
    let mut cfg = Config::default();
    cfg.sizing.notional_usd = 10_000.0;
    StrategyParams::from_config(&cfg, 2, 10)
}

fn long_view(now: u64, bid: f64) -> MarketView {
    view(now, Some(-100.0), bid, bid + 0.02)
}

/// Review finding: the cancel answer arrives before the fill that happened
/// just before the cancel. The requote must not be sized without it.
#[test]
fn fill_after_cancel_ack_does_not_oversize_the_entry() {
    let mut s = Strategy::new(big_params());
    let st = tick_full(&mut s, long_view(T0, 100.0), true);
    let o1 = placed(&st)[0];
    assert_eq!(o1.sz, 100.0);
    s.on_order_update(T0, OrderUpdate::Resting { tag: o1.tag });
    let st = tick_full(&mut s, long_view(T0 + 30 * S, 100.1), true);
    assert_eq!(cancels(&st), vec![o1.tag]);
    // Cancel acknowledged: the venue says 99 filled, we have seen none.
    let st = s.on_order_update(
        T0 + 30 * S + 100,
        OrderUpdate::Done {
            tag: o1.tag,
            reason: DoneReason::Canceled,
            filled_sz: Some(99.0),
        },
    );
    assert!(placed(&st).is_empty(), "{st:?}");
    assert_eq!(s.awaiting_fill_sz(Purpose::Entry), 99.0);
    let st = tick_full(&mut s, long_view(T0 + 32 * S, 100.1), true);
    assert!(placed(&st).is_empty(), "must wait for the in-flight fills");
    // The late fill arrives: only the remainder is quoted, and at the new
    // (higher) bid the notional cap trims it to 0.99 (9900 + 0.99 * 100.1).
    let st = fill(&mut s, T0 + 33 * S, &o1, 100.0, 99.0);
    assert_eq!(s.awaiting_fill_sz(Purpose::Entry), 0.0);
    let o2 = placed(&st)[0];
    assert_eq!(o2.px, 100.1);
    assert!((o2.sz - 0.99).abs() < 1e-9, "{o2:?}");
    fill(&mut s, T0 + 35 * S, &o2, o2.px, o2.sz);
    let p = s.position().unwrap();
    assert!((p.size - 99.99).abs() < 1e-9, "{p:?}");
    assert!(p.entry_notional <= HARD_CAP_NOTIONAL_USD + 1e-6);
}

/// Unknown filled size on a cancel: assume the worst, stop topping up after
/// the wait if the fills never come.
#[test]
fn unknown_fill_size_after_cancel_ends_the_entry() {
    let mut s = Strategy::new(big_params());
    let o1 = placed(&tick_full(&mut s, long_view(T0, 100.0), true))[0];
    tick_full(&mut s, long_view(T0 + 30 * S, 100.1), true);
    s.on_order_update(
        T0 + 30 * S,
        OrderUpdate::Done {
            tag: o1.tag,
            reason: DoneReason::Canceled,
            filled_sz: None,
        },
    );
    assert!(placed(&tick_full(&mut s, long_view(T0 + 40 * S, 100.1), true)).is_empty());
    let st = tick_full(&mut s, long_view(T0 + 30 * S + AWAIT_FILLS_MS, 100.1), true);
    assert!(placed(&st).is_empty());
    assert!(st.events.iter().any(|e| matches!(
        e,
        Event::EntryAborted {
            reason: AbortReason::FillsUnconfirmed
        }
    )));
    assert_eq!(s.phase().kind(), PhaseKind::Flat);
}

/// An IOC `Filled` answer before its fills: no second IOC until they come.
#[test]
fn exit_waits_for_ioc_fills_reported_by_done() {
    let mut s = strat();
    short_in_position(&mut s);
    let deadline = T0 + S + params().max_hold_ms;
    let t = deadline - params().taker_fallback_window_ms;
    tick(&mut s, t - 1, 60.0); // maker exit
    let st = tick(&mut s, t, 60.0); // taker time: cancel the ALO
    let alo = cancels(&st)[0];
    done(&mut s, t, alo, DoneReason::Canceled);
    let ioc = placed(&tick(&mut s, t + 2 * S, 60.0))[0];
    assert_eq!(ioc.kind, OrderKind::Ioc);
    s.on_order_update(
        t + 2 * S,
        OrderUpdate::Done {
            tag: ioc.tag,
            reason: DoneReason::Filled,
            filled_sz: Some(ioc.sz),
        },
    );
    assert!(placed(&tick(&mut s, t + 4 * S, 60.0)).is_empty());
    let st = fill(&mut s, t + 5 * S, &ioc, ioc.px, ioc.sz);
    assert!(closed(&st).is_some());
}

#[test]
fn suspended_strategy_issues_no_commands() {
    let mut s = strat();
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    s.suspend();
    // Shutdown cancels the entry: no re-quote, nothing left working.
    let st = done(&mut s, T0 + S, o.tag, DoneReason::Canceled);
    assert!(st.commands.is_empty(), "{st:?}");
    assert!(s.working().is_none());
    assert!(tick(&mut s, T0 + 2 * S, 60.0).commands.is_empty());
    assert!(s.on_kill(T0 + 3 * S, true, "k").commands.is_empty());
    // Resumed from the persisted state: quotes again.
    let mut r = Strategy::from_state(params(), s.state().clone());
    r.resume(T0 + 4 * S);
}

#[test]
fn stale_data_past_the_deadline_still_exits_with_ioc() {
    let mut s = strat();
    short_in_position(&mut s);
    let deadline = T0 + S + params().max_hold_ms;
    let mut v = view(deadline - 2 * H, Some(60.0), BID, ASK);
    v.stale = true;
    // Stale before the deadline: wait.
    assert!(tick_full(&mut s, v, true).commands.is_empty());
    v.now_ms = deadline - 60 * S;
    assert!(tick_full(&mut s, v, true).commands.is_empty());
    // Last fresh data hours old: hold, but warn.
    v.now_ms = deadline + S;
    let st = tick_full(&mut s, v, true);
    assert!(st.commands.is_empty(), "{st:?}");
    assert!(st
        .events
        .iter()
        .any(|e| matches!(e, Event::Warning(w) if w.contains("no recent book"))));
    // Fresh data returns briefly: IOC, which expires unfilled.
    let st = tick(&mut s, deadline + 2 * S, 60.0);
    let o = placed(&st)[0];
    assert_eq!(o.kind, OrderKind::Ioc);
    done(&mut s, deadline + 2 * S, o.tag, DoneReason::IocExpired);
    // Stale again shortly after: IOC on the last book (the warning is
    // throttled to one per 5 min).
    v.now_ms = deadline + 60 * S;
    let st = tick_full(&mut s, v, true);
    let o = placed(&st);
    assert_eq!(o.len(), 1, "{st:?}");
    assert_eq!(o[0].kind, OrderKind::Ioc);
    assert!(o[0].reduce_only && o[0].is_buy);
    assert!(st.events.is_empty(), "{st:?}");
    done(&mut s, deadline + 60 * S, o[0].tag, DoneReason::IocExpired);
    v.now_ms = deadline + 5 * M + 2 * S;
    let st = tick_full(&mut s, v, true);
    assert_eq!(placed(&st).len(), 1, "{st:?}");
    assert!(st.events.iter().any(
        |e| matches!(e, Event::Warning(w) if w.contains("closing with IOC on the last known book"))
    ));
}

// ------------------------------------------------- size noise (2026-11-17)

/// Short entry filled 20.19 + 0.35 + 0.02 (f64 sum 20.560000000000002),
/// ended by the entry timeout.
fn short_noisy_entry(s: &mut Strategy) -> OrderIntent {
    let o = placed(&tick(s, T0, 60.0))[0];
    assert!(o.sz > 20.56);
    for sz in [20.19, 0.35, 0.02] {
        fill(s, T0 + 10 * S, &o, o.px, sz);
    }
    assert_eq!(s.working().unwrap().filled, 20.56, "no noise in w.filled");
    let st = tick(s, T0 + 900 * S, 60.0);
    assert_eq!(cancels(&st), vec![o.tag]);
    let st = done(s, T0 + 901 * S, o.tag, DoneReason::Canceled);
    assert!(matches!(st.events[0], Event::PositionOpened { size, .. } if size == 20.56));
    assert_eq!(s.phase().kind(), PhaseKind::InPosition);
    o
}

#[test]
fn accumulated_fill_sizes_carry_no_float_noise() {
    assert_ne!(20.19 + 0.35 + 0.02, 20.56);
    let mut s = strat();
    short_noisy_entry(&mut s);
    let p = s.position().unwrap();
    assert_eq!(p.size, 20.56);
    assert_eq!(p.entry_sz, 20.56);
    // The persisted state serialises the clean value.
    let json = serde_json::to_string(s.state()).unwrap();
    assert!(json.contains("\"size\":20.56"), "{json}");
    assert!(!json.contains("20.560000000000002"), "{json}");
}

#[test]
fn exit_in_noisy_partials_closes_exactly_without_dust() {
    let mut s = strat();
    short_noisy_entry(&mut s);
    // Converged: the reduce-only close is exactly the position.
    let st = tick_px(&mut s, T0 + H, 8.0, 99.50, 99.52);
    let x = placed(&st)[0];
    assert!(x.reduce_only && x.is_buy);
    assert_eq!(x.sz, 20.56);
    // Partial exit fills in a different split.
    fill(&mut s, T0 + H + S, &x, 99.50, 0.35);
    assert_eq!(s.position().unwrap().size, 20.21);
    fill(&mut s, T0 + H + S, &x, 99.50, 20.19);
    assert_eq!(s.position().unwrap().size, 0.02);
    assert_eq!(s.position().unwrap().exit_sz, 20.54);
    let st = fill(&mut s, T0 + H + S, &x, 99.50, 0.02);
    let r = closed(&st).expect("closed on the last lot, no dust left");
    assert_eq!(r.size, 20.56);
    assert!(s.position().is_none());
    assert!(!st
        .events
        .iter()
        .any(|e| matches!(e, Event::Warning(w) if w.contains("exceeds"))));
}

#[test]
fn requoted_exit_after_partial_fill_is_the_exact_remainder() {
    let mut s = strat();
    short_noisy_entry(&mut s);
    let x = placed(&tick_px(&mut s, T0 + H, 5.0, 99.50, 99.52))[0];
    assert_eq!(x.sz, 20.56);
    fill(&mut s, T0 + H + S, &x, 99.50, 20.19);
    // The book moves: the maker exit is requoted for what is left.
    let st = tick_px(&mut s, T0 + H + 30 * S, 5.0, 99.70, 99.72);
    assert_eq!(cancels(&st), vec![x.tag]);
    let st = done(&mut s, T0 + H + 31 * S, x.tag, DoneReason::Canceled);
    let y = placed(&st)[0];
    // Raw f64 accounting would hold 20.560000000000002 - 20.19 =
    // 0.370000000000001 here: the open size is exactly the remaining lot.
    assert_eq!(s.position().unwrap().size, 0.37);
    assert_eq!(y.sz, 0.37);
    assert!(y.reduce_only);
    let st = fill(&mut s, T0 + H + 32 * S, &y, 99.70, 0.37);
    assert_eq!(closed(&st).unwrap().size, 20.56);
}

#[test]
fn trade_record_size_is_on_the_lot_grid_for_sz_decimals_0() {
    let mut p = params();
    p.sz_decimals = 0;
    let mut s = Strategy::new(p);
    let o = placed(&tick(&mut s, T0, 60.0))[0];
    assert_eq!(o.sz, o.sz.round());
    fill(&mut s, T0 + S, &o, o.px, o.sz);
    done(&mut s, T0 + S, o.tag, DoneReason::Filled);
    let x = placed(&tick_px(&mut s, T0 + H, 5.0, 99.50, 99.52))[0];
    assert_eq!(x.sz, o.sz);
    let st = fill(&mut s, T0 + H + S, &x, 99.50, x.sz);
    let r = closed(&st).unwrap();
    assert_eq!(r.size, o.sz);
    assert_eq!(
        crate::hl::rounding::fmt_size(r.size, 0),
        format!("{}", o.sz)
    );
}
