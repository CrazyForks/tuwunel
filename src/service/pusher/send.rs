use ipaddress::IPAddress;
use ruma::{
	UInt, UserId,
	api::{
		client::push::{Pusher, PusherKind},
		push_gateway::send_event_notification::{
			self,
			v1::{Device, Notification, NotificationCounts, NotificationPriority},
		},
	},
	events::{TimelineEventType, room::power_levels::RoomPowerLevels},
	push::{Action, PushFormat, Ruleset, Tweak},
	uint,
};
use tuwunel_core::{Err, Result, err, implement, matrix::Event};

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_push_notice<E>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	ruleset: &Ruleset,
	event: &E,
) -> Result
where
	E: Event,
{
	let mut notify = None;
	let mut tweaks = Vec::new();

	let unread: UInt = self
		.services
		.pusher
		.notification_count(user_id, event.room_id())
		.await
		.try_into()?;

	let power_levels: RoomPowerLevels = self
		.services
		.state_accessor
		.get_power_levels(event.room_id())
		.await?;

	let serialized = event.to_format();
	for action in self
		.get_actions(user_id, ruleset, &power_levels, &serialized, event.room_id())
		.await
	{
		let n = match action {
			| Action::Notify => true,
			| Action::SetTweak(tweak) => {
				tweaks.push(tweak.clone());
				continue;
			},
			| _ => false,
		};

		if notify.is_some() {
			return Err!(Database(
				r#"Malformed pushrule contains more than one of these actions: ["dont_notify", "notify", "coalesce"]"#
			));
		}

		notify = Some(n);
	}

	if notify == Some(true) {
		self.send_notice(unread, pusher, tweaks, event)
			.await?;
	}
	// Else the event triggered no actions

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_notice<Pdu: Event>(
	&self,
	unread: UInt,
	pusher: &Pusher,
	tweaks: Vec<Tweak>,
	event: &Pdu,
) -> Result {
	// TODO: email
	match &pusher.kind {
		| PusherKind::Http(http) => {
			let url = &http.url;
			let url = url::Url::parse(&http.url).map_err(|e| {
				err!(Request(InvalidParam(
					warn!(%url, "HTTP pusher URL is not a valid URL: {e}")
				)))
			})?;

			if ["http", "https"]
				.iter()
				.all(|&scheme| scheme != url.scheme().to_lowercase())
			{
				return Err!(Request(InvalidParam(
					warn!(%url, "HTTP pusher URL is not a valid HTTP/HTTPS URL")
				)));
			}

			if let Ok(ip) = IPAddress::parse(url.host_str().expect("URL previously validated")) {
				if !self.services.client.valid_cidr_range(&ip) {
					return Err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is a forbidden remote address")
					)));
				}
			}

			// TODO (timo): can pusher/devices have conflicting formats
			let event_id_only = http.format == Some(PushFormat::EventIdOnly);

			let mut device = Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
			device.data.data.clone_from(&http.data);
			device.data.format.clone_from(&http.format);

			// Tweaks are only added if the format is NOT event_id_only
			if !event_id_only {
				device.tweaks.clone_from(&tweaks);
			}

			let d = vec![device];
			let mut notifi = Notification::new(d);

			notifi.event_id = Some(event.event_id().to_owned());
			notifi.room_id = Some(event.room_id().to_owned());
			if http
				.data
				.get("org.matrix.msc4076.disable_badge_count")
				.is_none() && http.data.get("disable_badge_count").is_none()
			{
				notifi.counts = NotificationCounts::new(unread, uint!(0));
			} else {
				// counts will not be serialised if it's the default (0, 0)
				// skip_serializing_if = "NotificationCounts::is_default"
				notifi.counts = NotificationCounts::default();
			}

			if !event_id_only {
				if *event.kind() == TimelineEventType::RoomEncrypted
					|| tweaks
						.iter()
						.any(|t| matches!(t, Tweak::Highlight(true) | Tweak::Sound(_)))
				{
					notifi.prio = NotificationPriority::High;
				} else {
					notifi.prio = NotificationPriority::Low;
				}
				notifi.sender = Some(event.sender().to_owned());
				notifi.event_type = Some(event.kind().to_owned());
				notifi.content = serde_json::value::to_raw_value(event.content()).ok();

				if *event.kind() == TimelineEventType::RoomMember {
					notifi.user_is_target = event.state_key() == Some(event.sender().as_str());
				}

				notifi.sender_display_name = self
					.services
					.users
					.displayname(event.sender())
					.await
					.ok();

				notifi.room_name = self
					.services
					.state_accessor
					.get_name(event.room_id())
					.await
					.ok();

				notifi.room_alias = self
					.services
					.state_accessor
					.get_canonical_alias(event.room_id())
					.await
					.ok();
			}

			self.send_request(&http.url, send_event_notification::v1::Request::new(notifi))
				.await?;

			Ok(())
		},
		// TODO: Handle email
		//PusherKind::Email(_) => Ok(()),
		| _ => Ok(()),
	}
}
