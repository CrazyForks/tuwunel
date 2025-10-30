use std::{collections::HashSet, sync::Arc};

use futures::StreamExt;
use ruma::{
	OwnedUserId, RoomId, UserId,
	api::client::push::ProfileTag,
	events::{GlobalAccountDataEventType, TimelineEventType, push_rules::PushRulesEvent},
	push::{Action, Actions, Ruleset, Tweak},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Result, implement,
	matrix::{
		event::Event,
		pdu::{Count, Pdu, PduId, RawPduId},
	},
	utils::{self, ReadyExt, time::now_millis},
};
use tuwunel_database::{Json, Map};

use crate::rooms::short::ShortRoomId;

/// Succinct version of Ruma's Notification. Appended to the database when the
/// user is notified. The PduCount is part of the database key so only the
/// shortroomid is included. Together they  make the PduId.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Notified {
	/// Milliseconds time at which the event notification was sent.
	pub ts: u64,

	/// ShortRoomId
	pub sroomid: ShortRoomId,

	/// The profile tag of the rule that matched this event.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tag: Option<ProfileTag>,

	/// Actions vector
	pub actions: Actions,
}

/// Called by timeline append_pdu.
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all)]
pub(crate) async fn append_pdu(&self, pdu_id: RawPduId, pdu: &Pdu) -> Result {
	// Don't notify the sender of their own events, and dont send from ignored users
	let mut push_target: HashSet<_> = self
		.services
		.state_cache
		.active_local_users_in_room(pdu.room_id())
		.map(ToOwned::to_owned)
		.ready_filter(|user| *user != pdu.sender())
		.filter_map(async |recipient_user| {
			self.services
				.users
				.user_is_ignored(pdu.sender(), &recipient_user)
				.await
				.eq(&false)
				.then_some(recipient_user)
		})
		.collect()
		.await;

	let mut notifies = Vec::with_capacity(push_target.len().saturating_add(1));
	let mut highlights = Vec::with_capacity(push_target.len().saturating_add(1));

	if *pdu.kind() == TimelineEventType::RoomMember {
		if let Some(state_key) = pdu.state_key() {
			let target_user_id = UserId::parse(state_key)?;

			if self
				.services
				.users
				.is_active_local(target_user_id)
				.await
			{
				push_target.insert(target_user_id.to_owned());
			}
		}
	}

	let serialized = pdu.to_format();
	for user in &push_target {
		let rules_for_user = self
			.services
			.account_data
			.get_global(user, GlobalAccountDataEventType::PushRules)
			.await
			.map_or_else(
				|_| Ruleset::server_default(user),
				|ev: PushRulesEvent| ev.content.global,
			);

		let mut highlight = false;
		let mut notify = false;

		let power_levels = self
			.services
			.state_accessor
			.get_power_levels(pdu.room_id())
			.await?;

		let actions = self
			.services
			.pusher
			.get_actions(user, &rules_for_user, &power_levels, &serialized, pdu.room_id())
			.await;

		for action in actions {
			match action {
				| Action::Notify => notify = true,
				| Action::SetTweak(Tweak::Highlight(true)) => {
					highlight = true;
				},
				| _ => {},
			}

			// Break early if both conditions are true
			if notify && highlight {
				break;
			}
		}

		if notify {
			notifies.push(user.clone());
		}

		if highlight {
			highlights.push(user.clone());
		}

		if !notify && !highlight {
			continue;
		}

		let id: PduId = pdu_id.into();
		let notified = Notified {
			ts: now_millis(),
			sroomid: id.shortroomid,
			tag: None,
			actions: actions.into(),
		};

		if matches!(id.count, Count::Normal(_)) {
			self.db
				.useridcount_notification
				.put((user, id.count.into_unsigned()), Json(notified));
		}

		self.services
			.pusher
			.get_pushkeys(user)
			.ready_for_each(|push_key| {
				self.services
					.sending
					.send_pdu_push(&pdu_id, user, push_key.to_owned())
					.expect("TODO: replace with future");
			})
			.await;
	}

	self.increment_notification_counts(pdu.room_id(), notifies, highlights);

	Ok(())
}

#[implement(super::Service)]
fn increment_notification_counts(
	&self,
	room_id: &RoomId,
	notifies: Vec<OwnedUserId>,
	highlights: Vec<OwnedUserId>,
) {
	let _cork = self.db.db.cork();

	for user in notifies {
		let mut userroom_id = user.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());
		increment(&self.db.userroomid_notificationcount, &userroom_id);
	}

	for user in highlights {
		let mut userroom_id = user.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());
		increment(&self.db.userroomid_highlightcount, &userroom_id);
	}
}

//TODO: this is an ABA
fn increment(db: &Arc<Map>, key: &[u8]) {
	let old = db.get_blocking(key);
	let new = utils::increment(old.ok().as_deref());
	db.insert(key, new);
}
