# trillium-client h2 pool: unbounded fresh-connect when all pooled connections are at MAX_CONCURRENT_STREAMS

## Summary

Under sustained h2 client load that exceeds `peer_settings.MAX_CONCURRENT_STREAMS` × pooled-connection-count, `trillium_client::Client` opens fresh upstream TCP connections without bound. There is no per-origin cap and no wait-for-available semantics: when every existing pooled connection classifies as `Busy`, the call site falls through to a fresh-connect path unconditionally.

In practice this surfaces as the proxy opening **>100 upstream TCP connections to a single backend** under saturated h3/h2 traffic, and as soft "open new conn → arrive too late → it's Busy too → open another" feedback loops that compound when the application has multiple isolated `Client` instances (e.g. one per `current_thread` worker in a SO_REUSEPORT layout).

## Reproduction

Observed on `trillium-proxy` running the HttpArena `gateway-h3` profile against an h2 upstream:

- gcannon load: `-c 256 -m 32` (256 concurrent QUIC connections, 32 streams each = 8,192 in-flight)
- Upstream advertises `SETTINGS_MAX_CONCURRENT_STREAMS = 100` (Kestrel/aspnet default)
- Proxy: `trillium_proxy::Client::new_with_quic(...)`, default config (no explicit pool cap)
- Result: **159 upstream TCP connections** to a single backend (counted via `ss -tn | grep <upstream-port>`)

Pool churn scales linearly with worker count when the proxy uses isolated per-worker `Client` instances: `N workers × ~20 conns/pool ≈ 159` at `N=8`. On larger boxes (32+ phys cores), multiplying by `N` gives many hundreds.

## Root cause

The h2 pool peek in `client/src/conn/h2.rs:77` correctly classifies entries:

```rust
let Some(pooled) = h2_pool.peek_candidate_classify(&origin, |p| {
    let conn = p.connection();
    if !conn.swansong().state().is_running() {
        crate::pool::PoolEntryStatus::Dead
    } else if !conn.can_open_stream() {
        crate::pool::PoolEntryStatus::Busy
    } else {
        crate::pool::PoolEntryStatus::Available
    }
}) else {
    return Ok(false);
};
```

The `Pool::peek_candidate_classify` semantics (`client/src/pool.rs:215`) keep `Busy` entries in the pool but skip them for this request — the right policy *if* the caller will then wait for one of them to become Available. But there is no such wait.

The call site in `client/src/conn/shared.rs:42`:

```rust
if self.try_exec_h2_pooled().await? {
    return Ok(());
}
// ↓ unconditionally falls through to fresh-connect
if self.http_version == Version::Http2 {
    return self.exec_h2_prior_knowledge().await;
}
self.exec_h1_or_promote_h2().await
```

`exec_h2_prior_knowledge` and `exec_h1_or_promote_h2` both call `self.config.connect(&self.url).await?` to open a fresh transport, then `try_exec_h2_with_transport` (`client/src/conn/h2.rs:142`) inserts the new connection into the pool. With no cap, this loop can run indefinitely.

The pool itself has no concept of capacity; `Pool::insert` always pushes.

## Proposed fix

Two coupled changes:

1. **Per-origin pool cap.** `Pool` (or the h2-specific pool in `Client`) gains a `max_connections_per_origin` setting. Sensible default: something like `2 × num_cpus` or just `num_cpus`, configurable via `Client::with_max_h2_connections_per_origin`. Once we hit the cap, `Pool::insert` declines new entries (or replaces a Dead one).
2. **Wait-for-available on Busy.** When `peek_candidate_classify` returns `None` because every entry was Busy *and* the origin is at cap, the caller waits for one of the existing connections to signal Available. Mechanism: each `H2Pooled` entry exposes a `notify_available()` future that resolves when its underlying connection's stream count drops below `MAX_CONCURRENT_STREAMS`. The peek call returns either `Available(value)` or `WaitFor(Vec<NotifyHandle>)`, and the call site `select!`s across the wait handles plus an optional timeout.

A simpler v0 (no signaling) is "spin with delay" — reattempt the pool peek after a short sleep — but that's wasteful and racy.

The connection-level signal exists in shape already: the `H2Connection` driver knows when a stream finishes. Wiring an `event_listener::Event` (or tokio `Notify`) per-connection that fires on stream completion is small.

## Open design questions

1. **Default cap value.** Per-runtime we expect `2 × n_workers` to be plenty for typical proxy workloads. But for a single `Client` shared across a multi-thread runtime, the cap should reflect the runtime's parallelism, not the user's worker count. Probably we want `num_cpus::get()` as the default.

2. **Cross-runtime sharing.** Today `Client` is per-runtime by convention (cross-runtime sharing of pooled connections risks bouncing tasks between reactors). With proper cap-and-wait semantics, do we want to support a true cross-runtime client (single shared pool, runtime-aware connection placement)? Probably out of scope for this issue, but worth noting.

3. **GoAway interaction.** When an upstream sends `GOAWAY`, the `H2Connection` is marked Dead. The pool currently drops Dead entries on the next peek but doesn't proactively make room — under saturation, a Dead drop happens at the same time we're declining new entries due to the cap. Fix: GoAway → notify a "slot freed" channel in addition to marking Dead.

4. **Backpressure to caller.** When all pool slots are at cap and no stream becomes available within a timeout, what error do we return? `Error::PoolExhausted`? Should this be retryable from the caller's perspective?

5. **h1 pool symmetry.** The h1 pool has the same shape but the failure mode is benign (a "Busy" h1 connection just means one in flight, and h1 has no multiplexing). h1 could keep open-on-no-pooled semantics. Worth confirming the docstring distinction.

## Impact

This is a behavior bug, not a correctness bug — clients still complete requests. But:

- **Resource exhaustion at scale.** A proxy fronting many backends with sustained streaming workloads will hold thousands of upstream sockets open. Each one consumes kernel buffer space, file descriptor slots, and TLS session state.
- **Unfair to upstream.** The upstream sees connection storms instead of stream-multiplexed traffic, defeating h2's design goal.
- **Benchmark numbers depend on the bug.** The HttpArena `gateway-h3` profile shows trillium-proxy hitting ~3,200 RPS at 64 conns under the per-worker design, *because* 8 isolated client pools each open ~20 conns to the upstream → effectively 160-way outbound parallelism. The same proxy with a single-pool design (one `Client` on a multi-thread runtime) hits ~700 RPS for the same workload — same CPU headroom, just less outbound parallelism. A correct cap-and-wait pool would land somewhere in between, but predictably.

## Files to touch

- `client/src/pool.rs` — add per-origin cap; surface a "wait for available" handle from `peek_candidate_classify`.
- `client/src/conn/h2.rs` — add `H2Connection::notify_stream_completed()` (firing when `can_open_stream()` becomes true) and propagate to `H2Pooled`.
- `client/src/conn/shared.rs:42` — extend the call site to wait when all entries are Busy and we're at cap.
- `client/src/client.rs` — `with_max_h2_connections_per_origin(n)` chainable setter and a `Default` value.

## Discovered while

Investigating the `gateway-h3 256c` stall (989 RPS at 60-76% CPU) in HttpArena's PR #696 trillium results. Bench-side mitigation is the new mixed-runtime design (per-worker current_thread for TCP h1/h2 + dedicated multi-thread runtime for QUIC), but the pool bug is orthogonal: fixing it will improve every `trillium-proxy` workload that uses h2 upstream, and is a prerequisite for the proxy's gateway-h3 64c result to reflect honest single-pool capacity rather than per-worker pool churn.
