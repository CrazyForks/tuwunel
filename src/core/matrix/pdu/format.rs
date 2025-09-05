use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomVersionId,
	room_version_rules::{EventsReferenceFormatVersion, RoomVersionRules},
};

use crate::{
	Result, err, extract_variant, is_equal_to,
	matrix::{PduEvent, room_version},
};

pub fn into_outgoing_federation(
	mut pdu_json: CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> CanonicalJsonObject {
	if let Some(unsigned) = pdu_json
		.get_mut("unsigned")
		.and_then(|val| val.as_object_mut())
	{
		unsigned.remove("transaction_id");
	}

	let Ok(room_rules) = room_version::rules(room_version) else {
		pdu_json.remove("event_id");
		return pdu_json;
	};

	if !room_rules.event_format.require_event_id {
		pdu_json.remove("event_id");
	}

	if !room_rules
		.event_format
		.require_room_create_room_id
	{
		if pdu_json
			.get("type")
			.and_then(CanonicalJsonValue::as_str)
			.is_some_and(is_equal_to!("m.room.create"))
		{
			pdu_json.remove("room_id");
		}
	}

	if matches!(room_rules.events_reference_format, EventsReferenceFormatVersion::V1) {
		if let Some(value) = pdu_json.get_mut("auth_events") {
			mutate_outgoing_reference_format(value);
		}
		if let Some(value) = pdu_json.get_mut("prev_events") {
			mutate_outgoing_reference_format(value);
		}
	}

	pdu_json
}

fn mutate_outgoing_reference_format(value: &mut CanonicalJsonValue) {
	value
		.as_array_mut()
		.into_iter()
		.flatten()
		.for_each(|value| {
			if let Some(event_id) = value.as_str().map(ToOwned::to_owned) {
				*value = CanonicalJsonValue::Array(vec![
					CanonicalJsonValue::String(event_id),
					CanonicalJsonValue::Object([(String::new(), "".into())].into()),
				]);
			}
		});
}

pub fn from_incoming_federation(
	event_id: &EventId,
	pdu_json: &mut CanonicalJsonObject,
	room_rules: &RoomVersionRules,
) -> Result<PduEvent> {
	if matches!(room_rules.events_reference_format, EventsReferenceFormatVersion::V1) {
		if let Some(value) = pdu_json.get_mut("auth_events") {
			mutate_incoming_reference_format(value);
		}
		if let Some(value) = pdu_json.get_mut("prev_events") {
			mutate_incoming_reference_format(value);
		}
	}

	pdu_json.insert("event_id".to_owned(), CanonicalJsonValue::String(event_id.into()));

	serde_json::from_value::<PduEvent>(serde_json::to_value(&pdu_json)?)
		.map_err(|e| err!(Request(BadJson(debug_warn!("Event is not a valid PDU: {e}")))))
}

fn mutate_incoming_reference_format(value: &mut CanonicalJsonValue) {
	value
		.as_array_mut()
		.into_iter()
		.flat_map(|vec| vec.iter_mut())
		.for_each(|value| {
			let event_id = value
				.as_array()
				.into_iter()
				.find_map(|vec| vec.first())
				.and_then(|val| extract_variant!(val, CanonicalJsonValue::String))
				.cloned()
				.unwrap_or_default();

			*value = CanonicalJsonValue::String(event_id);
		});
}
