# Tuwunel 1.6.1

May 1, 2026

### New Features & Enhancements

- **Next-gen OIDC account management**, courtesy of @shaba in (#407), implements MSC2965 and provides the in-browser session list, session-end flows, and profile page for users authenticated via OIDC. The same PR fixes URL-encoding of `idp_id` in the SSO redirect path and adds the SSO/OIDC bypass path through User-Interactive Authentication so that users without a password can complete UIAA-protected actions. This closes (#433) opened by @jonathanmajh. Thank you!

- **Appservices with `receive_ephemeral` now receive EDUs scoped to their namespaces** in (#406), shipped by @chbgdn and closing (#382). `m.typing` and `m.receipt` now route to subscribed bridges and bots. Confirmation testing was provided by @gymnae, thank you both!

- **systemd watchdog keep-alive pings** were graciously added by @VlaDexa in (#415). Unit files declare `WatchdogSec=30` and the runtime pings systemd, so an unresponsive process is restarted automatically; previously-tolerated long stalls (e.g., pathological state-resolution) may now trigger restarts.

- **Spoofing-resistant client-IP resolution** with a configurable `ip_source` was contributed by @theredspoon as a security finding (#427), implemented and landed across (#428) and (#429). The new `ConfiguredIpSource` extension and `ClientIp` extractor replace `axum_client_ip::InsecureClientIp` across the API, restoring trust in client IPs for rate-limiting and audit logging. Default behavior is unchanged for existing deployments; operators behind a trusted proxy should set `ip_source` to opt in.

- **MSC3030 (`/timestamp_to_event`) is implemented** (experimental), contributed by @donjuanplatinum in (#413). Clients can now jump to a specific point in time within a room. This is the third Matrix Spec Change @donjuanplatinum has shipped to Tuwunel and we are very grateful for the consistent contributions.

- **MSC3824 (delegated authentication / refresh-token capability)** is advertised on `/versions` and `LoginType::Sso` includes `delegated_oidc_compatibility`. The config key `sso_aware_preferred` is renamed to `oidc_aware_preferred`, with the old name accepted as alias.

- Thanks to @rexbron, who contributed extensive operational documentation in (#354) and (#438): a `testmatrix` example in the troubleshooting section, podman-quadlet examples, an OIDC Keycloak provider example, refactored troubleshooting links, and clarification of how to obtain `provider_id` for the user admin commands. Thorough work!

- @valentimarco wrote a complete Authelia authentication page in (#278), closing their own (#274) on the OIDC token endpoint. Thank you!

- Thanks to @winyadepla for reorganizing the calling chapter in (#431), clarifying TURN vs MatrixRTC and the rationale for Docker-only deployment. This addresses (#348) opened by @MadMan247. Thank you both!

- Thank you @alametti for adding an Authentik provider section in (#437).

- Configuration values that name byte sizes now accept SI/IEC unit strings (`64MiB`, `2GB`, etc.) in addition to raw integers.

- A persistent LRU cache was added for `userdevicesessionid_uiaainfo` to keep ongoing UIA sessions alive across restarts.

- Performance: appservice EDU conditions reworked for concurrent lazy serialization; lazy-loading witness write-back gained a mode argument; the legacy spacehierarchy runtime cache was replaced by a database-backed path (config key `roomid_spacehierarchy_cache_capacity` → `spacehierarchy_cache_ttl_min`/`spacehierarchy_cache_ttl_max`).

- Admin: new commands to dump PDUs to the filesystem, query the RocksDB sequence number, and force/override or bypass database migrations.

- Bootstrap stamps a `server_name` marker into the global column family (backfilled on first boot for pre-existing databases) so a misconfigured `server_name` pointed at the wrong database is caught on every start.

- The `media_storage_providers` config option now validates that named providers exist; an explicit empty provider list defaults to all configured providers.

- New documentation chapters: Authentication Systems overview, JWT auth, LDAP auth, multimedia and storage, storage-provider environment variables. Identity-linking semantics for trusted vs. untrusted IdPs are now documented. The development chapter links hosted rustdocs (newly deployed via CI) and a Testing section was added. (#324) opened by @TheButlah on the NixOS Module documentation is closed.

- OCI image labels now include accurate `org.opencontainers.image.version` and related metadata derived from the package, closing (#356) opened by @rexbron. Thank you for the detailed write-up!

### Bug Fixes

- **OIDC server-contract hardening**: `/_tuwunel/oidc/userinfo` rejects plain Matrix access tokens (with `WWW-Authenticate: Bearer` on `401`); the token endpoint returns `400`/`invalid_grant` instead of `500` on client errors and emits `Cache-Control: no-store`; PKCE `plain` is no longer accepted (only `S256`); the `m.oauth` UIA flow routes through `/login/sso/redirect` when no specific IdP is selected.

- **Storage-provider variant naming is now consistent**, with appreciation to @yonzilch for (#414). Both sub-tables use lowercase identifiers (`[global.storage_provider.<ID>.s3]`), unblocking environment-variable configuration. Existing `S3` configurations are still accepted.

- **OpenBSD startup is fixed** in (#422), tip of the hat to @Hukadan. `core_affinity_rs` misreports CPU counts on OpenBSD; Tuwunel now uses `num_cpus` there. Thank you for picking this up!

- @alaviss reported a 1.6.0 regression in (#432) where inline `[global.appservice.<ID>]` config no longer worked. Fixed in (9d10230ba); the appservice ID from the toml section is honored again. Sincere apologies for the inconvenience.

- Multiple users reported the room-spaces hierarchy endpoint returning incomplete or invalid results: @vrisalab in (#344) and @foxing-quietly in (#399). The hierarchy unit was refactored, optimized, and corrected (including discarding `m.space.child` events with empty content per MSC1772/MSC2946). Special thanks to @TheBrigandier for testing and confirming the fixes on both threads.

- Thanks to @utop-top, who reported in (#411) that S3 uploads to Cloudflare R2 timed out for large media (~200 MiB+). Multipart uploads now kick in above a configurable `multipart_threshold` (default `100 MiB`). We appreciate the patient testing!

- Thank you @utop-top for also reporting in (#401) that appservice E2EE was broken because `/whoami` wasn't returning a `device_id` per MSC3202, crashing matrix-hookshot on startup. Tuwunel now accepts and asserts the appservice-supplied `device_id` per MSC4326. Confirmation testing was provided by @Domoel, thanks to you both!

- @BVollmerhaus reported in (#327) that mautrix bridges (e.g., mautrix-signal) couldn't upload device keys via MSC4190, blocking Element's upcoming mandatory device verification rollout. The MSC4190 path no longer stores `as_token` as the access token, and honors the appservice-asserted `device_id` on create. Special thanks to @1matin, @Domoel, and @gymnae for active testing across the thread.

- Sliding-sync long-polls now release on client disconnect, credit to @chocycat for (#386). Refreshing a client no longer leaves the previous poll holding the connection mutex for the full timeout. Supplemented by task-detach and shutdown-timeout abstractions on main.

- Thanks to @kodazavr for the immediate report in (#444): Tuwunel failed to start with Sentry integration enabled because the Sentry transport was missing a TLS backend. The reqwest transport is now built with merged webpki roots.

- Thank you @dennisoderwald for catching in (#443) that OIDC discovery advertised `response_mode=fragment` while the authorize endpoint only accepted `query`. Both modes are now implemented through the authorize/complete path.

- @dennisoderwald also reported in (#434) that S3 storage worked over HTTP but not HTTPS. The missing `tls-webpki-roots` feature was added to the `object_store` dependency. Confirmation testing was provided by @ZoftTy and @kodazavr, thank you all!

- Thanks to @dlford for the report in (#403) that clearing the presence status message had no effect. Now implemented with correct state transitions.

- Thank you @oly-nittka for the careful diagnosis in (#385) that federation with `matrix.org` was failing: a stale SRV cache entry overrode the .well-known delegation, producing port `8443` instead of `443`. The `actual_dest_2`/`actual_dest_3_2` resolver paths now parse explicit ports from delegated hosts.

- @native4don reported in (#377) that `device_lists.changed` was missing from `/sync` after cross-signing key uploads or device-key changes. The per-room device-key-change row was restored and the sync path updated. Confirmation testing was provided by @rexbron and @x86pup, thank you all!

- Thanks to @Giwayume for spotting in (#376) that `GET /_matrix/client/v3/devices` returned `null` for `display_name` and was not spec-compliant. The Ruma `Device` type now skips serializing optional fields when absent.

- Thank you @foxing-quietly for the report in (#372) that `GET /room_keys/version` returned `500` instead of `404` for stored backups predating the `algorithm` field. A backup-algorithm serializer now migrates legacy records on the fly.

- @kuhnchris reported in (#435) that AppService regex matching was case-inconsistent. User and alias namespaces now use case-insensitive comparisons, matching how MXIDs are normalized. Thank you!

- @Himura2la updated the LiveKit configuration documentation to the modern form in (#420), addressing (#400) reported by @Morgan-SL-PUP. Thank you both!

- Thanks to @grinapo for noting in (#317) that the Caddy `.well-known` example used incorrect CORS syntax. The example was corrected.

- Thank you @jameskimmel for correcting the Docker port mapping in the example to match the listener (#393).

- A regression in `state_cache` where the per-user transit step was wiping the room-wide invite-via cache was fixed in (efd36ddf2).

- The `base_path` option for S3 bucket paths was fixed in (85688e5a2) (regression from 73d110727).

- The UIAA flow for `m.oauth` and other non-SSO flows was corrected in (de8e2a1f3) so password and other flows still advertise correctly alongside SSO/OIDC.

- Device `last_seen_ip` is now updated from the relevant client handlers in (e90272795).

- The `device_list` update is now included in `/sync` for plaintext rooms (0adec1e3a), matching the spec. Operators can revert to 1.6.0 behavior with `device_key_update_encrypted_rooms_only=true`.

- @dasha-uwu landed several spec-compliance and cleanup fixes: legacy media endpoints removed (a1bb05e5f), correct error code returned when OIDC is not configured to silence Element Web's warning (dfbab637c), `M_UNRECOGNIZED` status code changed from 405 to 404 to stop breaking CORS preflight (b926cd939), proper 405 returned for bad methods (e3b2ce6e1), the spurious "skipping presence update" log line removed (287140748), and HTML template fixes (a1742ac3). Thank you, dasha!

### Honorable Mentions

- @theredspoon's client-IP work warrants a second mention: a self-reported issue, a clean two-PR refactor for the fix, and willingness to coordinate the change across every handler in the API crate. This is exactly the kind of contribution every project hopes to receive.

- @rexbron is now a serial documentation contributor and operations-focused thinker. Between the testmatrix example, the podman-quadlet content, the Keycloak guide, and the OCI image-label report, this release was meaningfully better for it.

- @donjuanplatinum has now shipped MSC3030, MSC3706, and MSC2246 to Tuwunel across recent releases. Thank you for the steady stream of spec implementations.
