# gap-analysis.md

**Scope for this run:** README.md's own "Backlog (deliberately out of scope
for this MVP)" section is this repo's hand-curated roadmap (repo-config-style
scope doc) — parity here means closing those two named items, not a fresh
diff against Python `requests`/`reqwest`. Both are audited below against the
current `main` (rusty_tls rev `415d8e0d1b58461990377b9e2cdc8799492f8efc`,
pinned for this run).

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Configurable `TrustPolicy` | fn (new builder methods) | roadmap | both | `rusty_tls::TrustPolicy` (`PinnedAnchors`, `DangerNoVerification` already implemented upstream) | no | S | `src/client.rs:1064` hardcodes `&rusty_tls::TrustPolicy::System` in `establish()`. Upstream `rusty_tls` (checked at the pinned rev) already fully implements `System`/`PinnedAnchors`/`DangerNoVerification` — this is purely a matter of exposing an existing capability through `ClientBuilder`/`RequestBuilder`, no upstream work needed. Purely additive: new `.trust_policy(...)` methods, default unchanged. |
| HTTP/2 / ALPN | fn | roadmap | both | N/A — no reference exists yet | no (but not implementable here) | — | Checked pinned `rusty_tls` source directly: no ALPN support anywhere in the crate (`src/{client,async_client,trust}.rs`), and its own lib.rs doc only claims client-only HTTP/1-shaped TLS. HTTP/2 needs ALPN negotiated during the TLS handshake, which is `rusty_tls`'s responsibility, not this crate's — there is nothing to implement in `rusty_request` alone. **Blocked on upstream `rusty_tls`**, not a workable issue for this loop. |

## Housekeeping note (not a gap, flagging for reconciliation)

Open issue **#1 "HTTPS/TLS support"** predates PR #19 (which shipped full
`https://` support via `rusty_tls`) and PR #20 (the `rusty_http` migration).
Its body describes an obsolete decision point ("candidate directions: a
`rustils` Security-surface addition, or FFI into an OS-native TLS library")
that was superseded once `rusty_tls` landed. HTTPS itself is done and
documented in the README; the only real remaining gap under that old issue's
umbrella is the "configurable trust policy" row above, which gets its own,
precise, issue. Recommend closing #1 as superseded/completed by #19, to
avoid two issues nominally tracking the same thing.

## Result

- 1 workable, purely-additive gap → issue to be filed and implemented this run.
- 1 gap that's real but not actionable from this repo (upstream ALPN
  dependency) → will be filed as `needs-human`/`blocked` so it's tracked but
  not looped on, rather than silently dropped.
- 1 stale issue recommended for closure (asking before closing it, since
  closing existing issues isn't this loop's normal write path).
