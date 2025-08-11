#[cfg(test)]
mod tests;

mod auth_difference;
mod conflicted_subgraph;
mod iterative_auth_check;
mod mainline_sort;
mod power_sort;
mod split_conflicted;
mod topological_sort;

use std::collections::{BTreeMap, BTreeSet};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, future::OptionFuture};
use ruma::{OwnedEventId, events::StateEventType, room_version_rules::RoomVersionRules};

pub use self::topological_sort::topological_sort;
use self::{
	auth_difference::auth_difference, conflicted_subgraph::conflicted_subgraph_dfs,
	iterative_auth_check::iterative_auth_check, mainline_sort::mainline_sort,
	power_sort::power_sort, split_conflicted::split_conflicted_state,
};
#[cfg(test)]
use super::test_utils;
use crate::{
	Result, debug,
	matrix::{Event, TypeStateKey},
	utils::stream::{BroadbandExt, IterStream},
};

/// ConflictMap of OwnedEventId specifically.
pub type ConflictMap = StateMap<ConflictVec>;

/// A mapping of event type and state_key to some value `T`, usually an
/// `EventId`.
pub type StateMap<Id> = BTreeMap<TypeStateKey, Id>;

/// Full recursive set of `auth_events` for each event in a StateMap.
pub type AuthSet<Id> = BTreeSet<Id>;

/// List of conflicting event_ids
type ConflictVec = Vec<OwnedEventId>;

/// Apply the [state resolution] algorithm introduced in room version 2 to
/// resolve the state of a room.
///
/// ## Arguments
///
/// * `rules` - The rules to apply for the version of the current room.
///
/// * `state_maps` - The incoming states to resolve. Each `StateMap` represents
///   a possible fork in the state of a room.
///
/// * `auth_chains` - The list of full recursive sets of `auth_events` for each
///   event in the `state_maps`.
///
/// * `fetch_event` - Function to fetch an event in the room given its event ID.
///
/// ## Invariants
///
/// The caller of `resolve` must ensure that all the events are from the same
/// room.
///
/// ## Returns
///
/// The resolved room state.
///
/// [state resolution]: https://spec.matrix.org/latest/rooms/v2/#state-resolution
#[tracing::instrument(level = "debug", skip_all)]
pub async fn resolve<'a, States, AuthSets, FetchExists, ExistsFut, FetchEvent, EventFut, Pdu>(
	rules: &RoomVersionRules,
	state_maps: States,
	auth_sets: AuthSets,
	fetch: &FetchEvent,
	exists: &FetchExists,
	backport_css: bool,
) -> Result<StateMap<OwnedEventId>>
where
	States: Stream<Item = StateMap<OwnedEventId>> + Send,
	AuthSets: Stream<Item = AuthSet<OwnedEventId>> + Send,
	FetchExists: Fn(OwnedEventId) -> ExistsFut + Sync,
	ExistsFut: Future<Output = bool> + Send,
	FetchEvent: Fn(OwnedEventId) -> EventFut + Sync,
	EventFut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event + Clone,
{
	// Split the unconflicted state map and the conflicted state set.
	let (unconflicted_state, conflicted_states) = split_conflicted_state(state_maps).await;

	debug!(?unconflicted_state, unconflicted = unconflicted_state.len(), "unresolved state");
	debug!(?conflicted_states, conflicted = conflicted_states.len(), "unresolved states");

	if conflicted_states.is_empty() {
		return Ok(unconflicted_state.into_iter().collect());
	}

	let consider_conflicted_subgraph = rules
		.state_res
		.v2_rules()
		.is_some_and(|rules| rules.consider_conflicted_state_subgraph)
		|| backport_css;

	// Since `org.matrix.hydra.11`, fetch the conflicted state subgraph.
	let conflicted_subgraph: OptionFuture<_> = consider_conflicted_subgraph
		.then(|| conflicted_states.clone().into_values().flatten())
		.map(async |ids| conflicted_subgraph_dfs(ids.stream(), fetch))
		.into();

	let conflicted_subgraph = conflicted_subgraph
		.await
		.into_iter()
		.stream()
		.flatten();

	// 0. The full conflicted set is the union of the conflicted state set and the
	//    auth difference. Don't honor events that don't exist.
	let full_conflicted_set: AuthSet<_> = auth_difference(auth_sets)
		.chain(conflicted_states.into_values().flatten().stream())
		.broad_filter_map(async |id| exists(id.clone()).await.then_some(id))
		.chain(conflicted_subgraph)
		.collect::<AuthSet<_>>()
		.inspect(|set| debug!(count = set.len(), "full conflicted set"))
		.inspect(|set| debug!(?set, "full conflicted set"))
		.await;

	// 1. Select the set X of all power events that appear in the full conflicted
	//    set. For each such power event P, enlarge X by adding the events in the
	//    auth chain of P which also belong to the full conflicted set. Sort X into
	//    a list using the reverse topological power ordering.
	let sorted_power_events: Vec<_> = power_sort(rules, &full_conflicted_set, fetch)
		.inspect_ok(|list| debug!(count = list.len(), "sorted power events"))
		.inspect_ok(|list| debug!(?list, "sorted power events"))
		.await?;

	let sorted_power_events_set: AuthSet<_> = sorted_power_events.iter().collect();

	let sorted_power_events = sorted_power_events
		.iter()
		.stream()
		.map(AsRef::as_ref);

	let start_with_incoming_state = rules
		.state_res
		.v2_rules()
		.is_none_or(|r| !r.begin_iterative_auth_checks_with_empty_state_map);

	let initial_state = start_with_incoming_state
		.then(|| unconflicted_state.clone())
		.unwrap_or_default();

	// 2. Apply the iterative auth checks algorithm, starting from the unconflicted
	//    state map, to the list of events from the previous step to get a partially
	//    resolved state.
	let partially_resolved_state =
		iterative_auth_check(rules, sorted_power_events, initial_state, fetch)
			.inspect_ok(|map| debug!(count = map.len(), "partially resolved power state"))
			.inspect_ok(|map| debug!(?map, "partially resolved power state"))
			.await?;

	// This "epochs" power level event
	let power_ty_sk = (StateEventType::RoomPowerLevels, "".into());
	let power_event = partially_resolved_state.get(&power_ty_sk);
	debug!(event_id = ?power_event, "epoch power event");

	let remaining_events: Vec<_> = full_conflicted_set
		.into_iter()
		.filter(|id| !sorted_power_events_set.contains(id))
		.collect();

	debug!(count = remaining_events.len(), "remaining events");
	debug!(list = ?remaining_events, "remaining events");

	let have_remaining_events = !remaining_events.is_empty();
	let remaining_events = remaining_events
		.iter()
		.stream()
		.map(AsRef::as_ref);

	// 3. Take all remaining events that weren’t picked in step 1 and order them by
	//    the mainline ordering based on the power level in the partially resolved
	//    state obtained in step 2.
	let sorted_remaining_events: OptionFuture<_> = have_remaining_events
		.then(move || mainline_sort(power_event.cloned(), remaining_events, fetch))
		.into();

	let sorted_remaining_events = sorted_remaining_events
		.await
		.unwrap_or(Ok(Vec::new()))?;

	debug!(count = sorted_remaining_events.len(), "sorted remaining events");
	debug!(list = ?sorted_remaining_events, "sorted remaining events");

	let sorted_remaining_events = sorted_remaining_events
		.iter()
		.stream()
		.map(AsRef::as_ref);

	// 4. Apply the iterative auth checks algorithm on the partial resolved state
	//    and the list of events from the previous step.
	let mut resolved_state =
		iterative_auth_check(rules, sorted_remaining_events, partially_resolved_state, fetch)
			.await?;

	// 5. Update the result by replacing any event with the event with the same key
	//    from the unconflicted state map, if such an event exists, to get the final
	//    resolved state.
	resolved_state.extend(unconflicted_state);

	debug!(resolved_state = resolved_state.len(), "resolved state");
	debug!(?resolved_state, "resolved state");

	Ok(resolved_state)
}
