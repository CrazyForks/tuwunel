use std::{
	borrow::Borrow,
	collections::HashMap,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{OwnedEventId, RoomId, RoomVersionId};
use tuwunel_core::{
	Result, err, error, implement,
	matrix::room_version,
	trace,
	utils::stream::{IterStream, ReadyExt, TryWidebandExt, WidebandExt},
};

use crate::rooms::{
	state_compressor::CompressedState,
	state_res::{self, AuthSet, StateMap},
};

#[implement(super::Service)]
#[tracing::instrument(
	name = "state",
	level = "debug",
	skip_all,
	fields(
		incoming = ?incoming_state.len()
	),
)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	incoming_state: HashMap<u64, OwnedEventId>,
) -> Result<Arc<CompressedState>> {
	trace!("Loading current room state ids");
	let current_sstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.map_err(|e| err!(Database(error!("No state for {room_id:?}: {e:?}"))))
		.await?;

	let current_state_ids: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(current_sstatehash)
		.collect()
		.await;

	trace!("Loading fork states");
	let fork_states = [current_state_ids, incoming_state];
	let auth_chains = fork_states
		.iter()
		.try_stream()
		.wide_and_then(|state| {
			// The chain walk dedups short ids and maps them injectively, so
			// the collected ids are distinct as `from_distinct` requires.
			self.services
				.auth_chain
				.event_ids_iter(room_id, room_version, state.values().map(Borrow::borrow))
				.try_collect()
				.map_ok(AuthSet::from_distinct)
		})
		.ready_filter_map(Result::ok);

	let fork_states = fork_states
		.iter()
		.stream()
		.wide_then(|fork_state| {
			let shortstatekeys = fork_state.keys().copied().stream();
			let event_ids = fork_state.values().cloned().stream();
			self.services
				.short
				.multi_get_statekey_from_short(shortstatekeys)
				.zip(event_ids)
				.ready_filter_map(|(ty_sk, id)| Some((ty_sk.ok()?, id)))
				.collect::<StateMap<OwnedEventId>>()
		});

	trace!("Resolving state");
	let state = self
		.state_resolution(room_id, room_version, fork_states, auth_chains, None)
		.await?;

	trace!("State resolution done.");
	let state_events: Vec<_> = state
		.iter()
		.stream()
		.wide_then(|((event_type, state_key), event_id)| {
			self.services
				.short
				.get_or_create_shortstatekey(event_type, state_key)
				.map(move |shortstatekey| (shortstatekey, event_id))
		})
		.collect()
		.await;

	trace!("Compressing state...");
	let new_room_state: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(
			state_events
				.iter()
				.map(|(ssk, eid)| (ssk, (*eid).borrow())),
		)
		.collect()
		.await;

	Ok(Arc::new(new_room_state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "resolve", level = "debug", skip_all)]
pub(super) async fn state_resolution<StateSets, AuthSets>(
	&self,
	_room_id: &RoomId,
	room_version: &RoomVersionId,
	state_sets: StateSets,
	auth_chains: AuthSets,
	complete: Option<&AtomicBool>,
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Stream<Item = StateMap<OwnedEventId>> + Send,
	AuthSets: Stream<Item = AuthSet<OwnedEventId>> + Send,
{
	let fetch = async |event_id: OwnedEventId| match self.event_fetch(&event_id).await {
		| Err(error) if complete.is_some() && error.is_not_found() => {
			if let Some(complete) = complete {
				complete.store(false, Ordering::Relaxed);
			}

			Err(err!(Database("State resolution references missing event {event_id}.")))
		},
		| result => result,
	};

	let exists = async |event_id: OwnedEventId| match self.event_exists(&event_id).await {
		| Ok(false) if complete.is_some() => {
			if let Some(complete) = complete {
				complete.store(false, Ordering::Relaxed);
			}

			Err(err!(Database("State resolution references missing event {event_id}.")))
		},
		| result => result,
	};

	state_res::resolve(
		&room_version::rules(room_version)?,
		state_sets,
		auth_chains,
		&fetch,
		&exists,
		self.services.server.config.hydra_backports,
	)
	.inspect_err(|error| {
		if let Some(complete) = complete {
			complete.store(false, Ordering::Relaxed);
		}

		error!(?error, "State resolution failed.");
	})
	.await
}
