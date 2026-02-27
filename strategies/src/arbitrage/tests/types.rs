#[cfg(test)]
mod tests {
    use crate::arbitrage::types::{
        DEFAULT_UNWIND_CHUNKS, DeciderAction, OrderTracker, PairStatus, PerpLeg, TrackedOrder,
        TrackedOrderStatus, UnwindSchedule, next_unwind_action,
    };
    use boldtrax_core::types::{Exchange, InstrumentKey, InstrumentType, Order, OrderEvent, Pairs};
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn test_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        }
    }

    fn test_key_spot() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Spot,
        }
    }

    fn make_tracked_order(id: &str, key: InstrumentKey, qty: Decimal) -> TrackedOrder {
        TrackedOrder {
            order_id: id.to_string(),
            key,
            status: TrackedOrderStatus::Pending,
            requested_qty: qty,
            filled_qty: Decimal::ZERO,
            submitted_at: Utc::now(),
            order_price: None,
        }
    }

    fn make_stale_tracked_order(id: &str, key: InstrumentKey, qty: Decimal) -> TrackedOrder {
        TrackedOrder {
            order_id: id.to_string(),
            key,
            status: TrackedOrderStatus::Pending,
            requested_qty: qty,
            filled_qty: Decimal::ZERO,
            submitted_at: Utc::now() - Duration::seconds(120),
            order_price: None,
        }
    }

    fn make_order(client_id: &str) -> Order {
        Order {
            client_order_id: client_id.to_string(),
            ..Default::default()
        }
    }

    // ===================================================================
    // PairStatus
    // ===================================================================

    #[test]
    fn test_pair_status_transitional() {
        assert!(!PairStatus::Inactive.is_transitional());
        assert!(!PairStatus::Active.is_transitional());
        assert!(PairStatus::Entering.is_transitional());
        assert!(PairStatus::Exiting.is_transitional());
        assert!(PairStatus::Recovering.is_transitional());
        assert!(PairStatus::Unwinding.is_transitional());
    }

    // ===================================================================
    // TrackedOrderStatus
    // ===================================================================

    #[test]
    fn test_tracked_order_status_terminal() {
        assert!(!TrackedOrderStatus::Pending.is_terminal());
        assert!(!TrackedOrderStatus::Open.is_terminal());
        assert!(!TrackedOrderStatus::PartiallyFilled.is_terminal());
        assert!(TrackedOrderStatus::Filled.is_terminal());
        assert!(TrackedOrderStatus::Cancelled.is_terminal());
        assert!(TrackedOrderStatus::Rejected.is_terminal());
    }

    #[test]
    fn test_tracked_order_status_from_order_event() {
        let order = Order::default();
        assert_eq!(
            TrackedOrderStatus::from_order_event(&OrderEvent::New(order.clone())),
            TrackedOrderStatus::Open
        );
        assert_eq!(
            TrackedOrderStatus::from_order_event(&OrderEvent::PartiallyFilled(order.clone())),
            TrackedOrderStatus::PartiallyFilled
        );
        assert_eq!(
            TrackedOrderStatus::from_order_event(&OrderEvent::Filled(order.clone())),
            TrackedOrderStatus::Filled
        );
        assert_eq!(
            TrackedOrderStatus::from_order_event(&OrderEvent::Canceled(order.clone())),
            TrackedOrderStatus::Cancelled
        );
        assert_eq!(
            TrackedOrderStatus::from_order_event(&OrderEvent::Rejected(order)),
            TrackedOrderStatus::Rejected
        );
    }

    // ===================================================================
    // OrderTracker — basic tracking
    // ===================================================================

    #[test]
    fn test_tracker_new_is_empty() {
        let tracker = OrderTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
        assert!(!tracker.has_pending());
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_tracker_track_and_get() {
        let mut tracker = OrderTracker::new();
        let order = make_tracked_order("ord-1", test_key(), dec("10"));
        tracker.track(order);

        assert_eq!(tracker.len(), 1);
        assert!(!tracker.is_empty());
        assert!(tracker.has_pending());
        assert_eq!(tracker.pending_count(), 1);

        let fetched = tracker.get("ord-1").unwrap();
        assert_eq!(fetched.order_id, "ord-1");
        assert_eq!(fetched.status, TrackedOrderStatus::Pending);
        assert_eq!(fetched.requested_qty, dec("10"));
    }

    #[test]
    fn test_tracker_get_missing_returns_none() {
        let tracker = OrderTracker::new();
        assert!(tracker.get("nonexistent").is_none());
    }

    // ===================================================================
    // OrderTracker — update_status
    // ===================================================================

    #[test]
    fn test_tracker_update_status() {
        let mut tracker = OrderTracker::new();
        tracker.track(make_tracked_order("ord-1", test_key(), dec("5")));

        assert!(tracker.update_status("ord-1", TrackedOrderStatus::Open));
        assert_eq!(
            tracker.get("ord-1").unwrap().status,
            TrackedOrderStatus::Open
        );

        assert!(tracker.update_status("ord-1", TrackedOrderStatus::Filled));
        assert_eq!(
            tracker.get("ord-1").unwrap().status,
            TrackedOrderStatus::Filled
        );
    }

    #[test]
    fn test_tracker_update_status_unknown_order_returns_false() {
        let mut tracker = OrderTracker::new();
        assert!(!tracker.update_status("ghost", TrackedOrderStatus::Filled));
    }

    // ===================================================================
    // OrderTracker — update_fill
    // ===================================================================

    #[test]
    fn test_tracker_update_fill() {
        let mut tracker = OrderTracker::new();
        tracker.track(make_tracked_order("ord-1", test_key(), dec("10")));

        assert!(tracker.update_fill("ord-1", dec("5"), TrackedOrderStatus::PartiallyFilled));
        let o = tracker.get("ord-1").unwrap();
        assert_eq!(o.filled_qty, dec("5"));
        assert_eq!(o.status, TrackedOrderStatus::PartiallyFilled);

        assert!(tracker.update_fill("ord-1", dec("10"), TrackedOrderStatus::Filled));
        let o = tracker.get("ord-1").unwrap();
        assert_eq!(o.filled_qty, dec("10"));
        assert_eq!(o.status, TrackedOrderStatus::Filled);
    }

    #[test]
    fn test_tracker_update_fill_unknown_returns_false() {
        let mut tracker = OrderTracker::new();
        assert!(!tracker.update_fill("nope", dec("1"), TrackedOrderStatus::Filled));
    }

    // ===================================================================
    // OrderTracker — pending_for_leg
    // ===================================================================

    #[test]
    fn test_tracker_pending_for_leg() {
        let mut tracker = OrderTracker::new();
        let key_a = test_key();
        let key_b = test_key_spot();

        tracker.track(make_tracked_order("a-1", key_a, dec("1")));
        tracker.track(make_tracked_order("b-1", key_b, dec("2")));
        tracker.track(make_tracked_order("a-2", key_a, dec("3")));

        // Fill a-1 so it becomes terminal
        tracker.update_status("a-1", TrackedOrderStatus::Filled);

        let pending_a = tracker.pending_for_leg(&key_a);
        assert_eq!(pending_a.len(), 1);
        assert_eq!(pending_a[0].order_id, "a-2");

        let pending_b = tracker.pending_for_leg(&key_b);
        assert_eq!(pending_b.len(), 1);
        assert_eq!(pending_b[0].order_id, "b-1");
    }

    // ===================================================================
    // OrderTracker — has_pending / pending_count
    // ===================================================================

    #[test]
    fn test_tracker_pending_count_excludes_terminal() {
        let mut tracker = OrderTracker::new();
        tracker.track(make_tracked_order("a", test_key(), dec("1")));
        tracker.track(make_tracked_order("b", test_key(), dec("1")));
        tracker.track(make_tracked_order("c", test_key(), dec("1")));

        assert_eq!(tracker.pending_count(), 3);

        tracker.update_status("a", TrackedOrderStatus::Filled);
        tracker.update_status("b", TrackedOrderStatus::Cancelled);

        assert_eq!(tracker.pending_count(), 1);
        assert!(tracker.has_pending());

        tracker.update_status("c", TrackedOrderStatus::Rejected);
        assert_eq!(tracker.pending_count(), 0);
        assert!(!tracker.has_pending());
    }

    // ===================================================================
    // OrderTracker — stale_orders
    // ===================================================================

    #[test]
    fn test_tracker_stale_orders() {
        let mut tracker = OrderTracker::new();
        // Fresh order — should NOT be stale
        tracker.track(make_tracked_order("fresh", test_key(), dec("1")));
        // Old order — should be stale
        tracker.track(make_stale_tracked_order("stale", test_key(), dec("2")));
        // Old but terminal — should NOT appear
        let mut terminal = make_stale_tracked_order("done", test_key(), dec("3"));
        terminal.status = TrackedOrderStatus::Filled;
        tracker.track(terminal);

        let stale = tracker.stale_orders(std::time::Duration::from_secs(60));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].order_id, "stale");
    }

    #[test]
    fn test_tracker_stale_orders_empty_when_all_fresh() {
        let mut tracker = OrderTracker::new();
        tracker.track(make_tracked_order("a", test_key(), dec("1")));
        tracker.track(make_tracked_order("b", test_key(), dec("2")));

        let stale = tracker.stale_orders(std::time::Duration::from_secs(60));
        assert!(stale.is_empty());
    }

    // ===================================================================
    // OrderTracker — remove_terminal
    // ===================================================================

    #[test]
    fn test_tracker_remove_terminal() {
        let mut tracker = OrderTracker::new();
        tracker.track(make_tracked_order("live", test_key(), dec("1")));
        tracker.track(make_tracked_order("dead-1", test_key(), dec("2")));
        tracker.track(make_tracked_order("dead-2", test_key(), dec("3")));

        tracker.update_status("dead-1", TrackedOrderStatus::Filled);
        tracker.update_status("dead-2", TrackedOrderStatus::Cancelled);

        assert_eq!(tracker.len(), 3);
        tracker.remove_terminal();
        assert_eq!(tracker.len(), 1);
        assert!(tracker.get("live").is_some());
        assert!(tracker.get("dead-1").is_none());
        assert!(tracker.get("dead-2").is_none());
    }

    // ===================================================================
    // OrderTracker — track_from_order / track_if_some
    // ===================================================================

    #[test]
    fn test_tracker_track_from_order() {
        let mut tracker = OrderTracker::new();
        let order = make_order("cid-42");
        tracker.track_from_order(&order, test_key(), dec("7.5"));

        let tracked = tracker.get("cid-42").unwrap();
        assert_eq!(tracked.status, TrackedOrderStatus::Pending);
        assert_eq!(tracked.requested_qty, dec("7.5"));
        assert_eq!(tracked.filled_qty, Decimal::ZERO);
    }

    #[test]
    fn test_tracker_track_if_some_with_order() {
        let mut tracker = OrderTracker::new();
        tracker.track_if_some(Some(make_order("cid-a")), test_key(), dec("1"));
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_tracker_track_if_some_with_none() {
        let mut tracker = OrderTracker::new();
        tracker.track_if_some(None, test_key(), dec("1"));
        assert!(tracker.is_empty());
    }

    // ===================================================================
    // UnwindSchedule — construction
    // ===================================================================

    #[test]
    fn test_unwind_schedule_new() {
        let sched = UnwindSchedule::new(4);
        assert_eq!(sched.num_chunks, 4);
        assert_eq!(sched.chunks_dispatched, 0);
        assert!(!sched.is_complete());
    }

    #[test]
    fn test_unwind_schedule_new_clamps_zero_to_one() {
        let sched = UnwindSchedule::new(0);
        assert_eq!(sched.num_chunks, 1);
    }

    // ===================================================================
    // UnwindSchedule — next_chunk
    // ===================================================================

    #[test]
    fn test_unwind_schedule_even_division() {
        let sched = UnwindSchedule::new(4);
        // 100 remaining, 4 chunks → 25 per chunk
        let (long, short) = sched.next_chunk(dec("-100"), dec("100")).unwrap();
        assert_eq!(long, dec("-25"));
        assert_eq!(short, dec("25"));
    }

    #[test]
    fn test_unwind_schedule_remaining_shrinks() {
        let mut sched = UnwindSchedule::new(4);

        // First chunk: 100/4 = 25
        let (l, s) = sched.next_chunk(dec("-100"), dec("100")).unwrap();
        assert_eq!(l, dec("-25"));
        assert_eq!(s, dec("25"));
        sched.advance();

        // Second chunk: 75/3 = 25
        let (l2, s2) = sched.next_chunk(dec("-75"), dec("75")).unwrap();
        assert_eq!(l2, dec("-25"));
        assert_eq!(s2, dec("25"));
        sched.advance();

        // Third chunk: 50/2 = 25
        let (l3, s3) = sched.next_chunk(dec("-50"), dec("50")).unwrap();
        assert_eq!(l3, dec("-25"));
        assert_eq!(s3, dec("25"));
        sched.advance();

        // Fourth chunk: 25/1 = 25
        let (l4, s4) = sched.next_chunk(dec("-25"), dec("25")).unwrap();
        assert_eq!(l4, dec("-25"));
        assert_eq!(s4, dec("25"));
        sched.advance();

        // After 4 advances, schedule is complete
        assert!(sched.is_complete());
        assert!(sched.next_chunk(dec("-0"), dec("0")).is_none());
    }

    #[test]
    fn test_unwind_schedule_uneven_division() {
        // 3 chunks for 100 → 33.33.., 33.33.., 33.33..
        let mut sched = UnwindSchedule::new(3);

        let (l1, _) = sched.next_chunk(dec("-100"), dec("100")).unwrap();
        // 100/3 ≈ 33.333...
        let expected_first = dec("-100") / Decimal::from(3);
        assert_eq!(l1, expected_first);
        sched.advance();

        // Remaining: 66.666... / 2 = 33.333...
        let remaining = dec("-100") - l1;
        let (l2, _) = sched.next_chunk(remaining, -remaining).unwrap();
        let expected_second = remaining / Decimal::from(2);
        assert_eq!(l2, expected_second);
    }

    // ===================================================================
    // UnwindSchedule — advance and is_complete
    // ===================================================================

    #[test]
    fn test_unwind_schedule_advance_completes() {
        let mut sched = UnwindSchedule::new(2);
        assert!(!sched.is_complete());
        sched.advance();
        assert!(!sched.is_complete());
        sched.advance();
        assert!(sched.is_complete());
    }

    #[test]
    fn test_unwind_schedule_complete_returns_none() {
        let mut sched = UnwindSchedule::new(1);
        sched.advance();
        assert!(sched.is_complete());
        assert!(sched.next_chunk(dec("-50"), dec("50")).is_none());
    }

    // ===================================================================
    // next_unwind_action helper
    // ===================================================================

    #[test]
    fn test_next_unwind_action_returns_unwind() {
        let mut sched = UnwindSchedule::new(2);
        let action = next_unwind_action(&mut sched, dec("-100"), dec("100"));

        assert!(action.is_some());
        match action.unwrap() {
            DeciderAction::Unwind {
                size_long,
                size_short,
            } => {
                assert_eq!(size_long, dec("-50"));
                assert_eq!(size_short, dec("50"));
            }
            other => panic!("Expected Unwind, got {:?}", other),
        }
        assert_eq!(sched.chunks_dispatched, 1);
    }

    #[test]
    fn test_next_unwind_action_advances_schedule() {
        let mut sched = UnwindSchedule::new(3);

        next_unwind_action(&mut sched, dec("-90"), dec("90"));
        assert_eq!(sched.chunks_dispatched, 1);

        next_unwind_action(&mut sched, dec("-60"), dec("60"));
        assert_eq!(sched.chunks_dispatched, 2);

        next_unwind_action(&mut sched, dec("-30"), dec("30"));
        assert_eq!(sched.chunks_dispatched, 3);

        // Now exhausted
        let action = next_unwind_action(&mut sched, dec("-0"), dec("0"));
        assert!(action.is_none());
    }

    #[test]
    fn test_default_unwind_chunks_constant() {
        assert_eq!(DEFAULT_UNWIND_CHUNKS, 4);
    }

    // ===================================================================
    // PerpLeg — depth_sufficient
    // ===================================================================

    #[test]
    fn test_perp_leg_depth_sufficient_unknown_allows() {
        let leg = PerpLeg::new(test_key());
        // No depth data → returns true (graceful degradation)
        assert!(leg.depth_sufficient(dec("100")));
        assert!(leg.depth_sufficient(dec("-100")));
    }

    #[test]
    fn test_perp_leg_depth_sufficient_zero_size() {
        let leg = PerpLeg::new(test_key());
        assert!(leg.depth_sufficient(Decimal::ZERO));
    }

    #[test]
    fn test_perp_leg_depth_sufficient_buy_checks_ask() {
        let mut leg = PerpLeg::new(test_key());
        leg.ask_depth = Some(dec("50"));
        leg.bid_depth = Some(dec("200"));

        // Buying 30 → ask_depth 50 >= 30 → sufficient
        assert!(leg.depth_sufficient(dec("30")));
        // Buying 80 → ask_depth 50 < 80 → insufficient
        assert!(!leg.depth_sufficient(dec("80")));
    }

    #[test]
    fn test_perp_leg_depth_sufficient_sell_checks_bid() {
        let mut leg = PerpLeg::new(test_key());
        leg.ask_depth = Some(dec("200"));
        leg.bid_depth = Some(dec("50"));

        // Selling -30 → bid_depth 50 >= 30 → sufficient
        assert!(leg.depth_sufficient(dec("-30")));
        // Selling -80 → bid_depth 50 < 80 → insufficient
        assert!(!leg.depth_sufficient(dec("-80")));
    }

    // ===================================================================
    // OrderTracker on pair — unwind_schedule field
    // ===================================================================

    #[test]
    fn test_tracker_unwind_schedule_initially_none() {
        let tracker = OrderTracker::new();
        assert!(tracker.unwind_schedule.is_none());
    }

    #[test]
    fn test_tracker_unwind_schedule_attach_and_use() {
        let mut tracker = OrderTracker::new();
        tracker.unwind_schedule = Some(UnwindSchedule::new(4));

        let sched = tracker.unwind_schedule.as_ref().unwrap();
        assert_eq!(sched.num_chunks, 4);
        assert!(!sched.is_complete());
    }
}
