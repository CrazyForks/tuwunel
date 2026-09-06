use super::{Direction, PduCount, Service};

#[test]
fn forward_backfilled_zero_stays_at_lower_bound() {
	let pdu_id = Service::pdu_count_to_id(1, PduCount::Backfilled(0), Direction::Forward);

	assert_eq!(pdu_id.pdu_count(), PduCount::Backfilled(0));
}
