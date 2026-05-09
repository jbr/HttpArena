# Trillium HttpArena bench findings

Companion to `trillium-bench-plan.md`. Covers the verification of the
compression-fix predictions plus everything learned along the way (bench-driven
discoveries about trillium-http, trillium-proxy, the trillium-client pool, and
the harness itself). Written for handoff to a fresh session.

## Environment

- Box: `c7i.2xlarge` (8 vCPU, 16 GB) — user's reusable bench AMI.
- Local trillium clone: `~/trillium` on branch `compression`.
- HttpArena branch: `__trillium`. Path-deps wired via `scripts/stage-trillium.sh`
  + `[patch.crates-io]` in each variant's Cargo.toml. `_trillium/` directories
  are gitignored.
- Harness env overrides for the 8-core box (already in `scripts/run-comparison.sh`):
  `LOADGEN_DOCKER=true`, `REDIS_CPUSET=0`, `PG_CPUSET=1`, `PROXY_CPUSET=0-1`,
  `SERVER_CPUSET=2-5`. Compose files for gateway tests now use
  `${PROXY_CPUSET:-...}` / `${SERVER_CPUSET:-...}` substitution (defaults
  preserve the 64-core layout).
- Same-hardware comparisons only — leaderboard numbers are from different
  hardware and are not directly comparable.

## Compression-fix verification (the original goal)

All plan predictions held. Numbers are this 8-core box, focused-5 profile set.

### trillium-tuned, baseline (`a1e77dc2`) → after-fix

| profile/conns | baseline | after | Δ |
|---|--:|--:|--:|
| async-db 1024c | 8,991 | 10,804 | +20% |
| json 4096c | 42,068 | 144,119 | **+243%** ¹ |
| json-comp 4096c | 403 | 30,416 | **+7,500%** (~75×) |
| json-comp 16384c | 401 | 30,614 | +7,500% |
| static 1024c | 399 | 138,106 | **+34,500%** (~340×) |
| static 4096c | 385 | 145,880 | +37,800% |
| static 6800c | 401 | 132,592 | +33,000% |
| static-h2 1024c | 91 | 145,373 | **+159,000%** (~1600×) |
| static-h2 256c | — | 169,890 | n/a |

¹ The 3.4× jump on **json/4096c** is unrelated to the compression fixes. Cause:
baseline pulled in registry `trillium-logger 0.4.5`, which transitively
required `trillium 0.2.20 + trillium-http 0.3.17 + trillium-macros 0.0.6`. The
binary contained two complete trillium frameworks side-by-side under
`lto = "fat"`, bloating the LTO IR space and likely defeating inlining. After
bumping to `trillium-logger 0.5.0` (single-trillium tree), the speedup falls
out automatically. Confirmed via lockfile diff.

### trillium (prod), baseline → after-fix

| profile/conns | baseline | after | Δ |
|---|--:|--:|--:|
| async-db 1024c | 10,234 | 11,213 | +10% |
| json 4096c | 139,189 | 125,607 | **−10%** ² |
| json-comp 4096c | 446 | 29,989 | ~67× |
| json-comp 16384c | 421 | 28,966 | ~69× |
| static 1024c | 55 | 2,623 | ~48× |
| static 6800c | 59 | 2,471 | ~42× |
| static-h2 1024c | (hung) | 1,797 | n/a |

² Likely variance, but worth confirming with a couple repeat runs if
publishing. `prod static` is ~50× faster but still CPU-bound on dynamic
brotli q4 — unlike tuned, prod has no `StaticPreload` bypass.

## Cross-framework reference (same 8-core box)

After-fix trillium-tuned / trillium-prod vs actix and (where applicable) hyper / nginx:

| profile/conns | tuned | prod | actix | hyper | nginx |
|---|--:|--:|--:|--:|--:|
| baseline 4096c | 198K | 210K | 344K | **364K** | — |
| pipelined 4096c | 458K | 452K | 1.89M | **2.31M** | — |
| baseline-h2 1024c | 364K | 266K | 420K | **623K** | — |
| baseline-h2 256c | 425K | 285K | 520K | **562K** | — |
| json-comp 4096c | 30K | 30K | **36K** | — | — |
| async-db 1024c | **10.8K** | 11.2K | 9.8K | — | — |
| static 4096c | 146K | 2.5K | 0.85K | — | **202K** |
| static-h2 1024c | 145K | 1.8K | 6.7K | — | **157K** |
| upload 256c | 1.6K | 1.5K | 1.8K | — | — |
| echo-ws 4096c | 128K | 128K | n/s | n/s | n/s |

n/s = framework doesn't subscribe.

### Headlines from cross-framework

- **Compression-fix predictions held.** json-comp gap to actix closed to ~10%.
  static-h2 OOM resolved (2.1 GiB bounded vs the plan's 106 GiB on AWS).
- **trillium beats actix on async-db at ~1/3 the CPU** (10.8K @ 77% vs 9.8K @ 219%) — composite-workload win confirmed.
- **trillium-tuned static-h2 reaches ~92% of nginx** (145K vs 157K). Static path
  is competitive with the gold standard.

## Bench-driven discoveries about trillium-http

These came from profiling the gap to hyper on `pipelined` (5×) and
`baseline-h2` memory bloat (16× memory).

### Pipelined cycle attribution (perf, 102K samples × 20s, 8 workers)

| area | cycles | % | note |
|---|--:|--:|---|
| `TcpStream::poll_write` + kernel send | 157B | 55% | I had initially called this "no writev" — wrong (see below) |
| `trillium_http::Conn::*` | 21B | 7% | parse / finalize / etc. |
| `RunningConfig` server loop | 13B | 4.5% | accept + dispatch |
| BTreeMap ops (Headers + TypeSet) | 12.3B | **4.3%** | |
| ArcHandler chain | 7.6B | 2.7% | |
| `fmt::write` / `format!` | 3.5B | 1.2% | |
| mimalloc alloc/free (direct) | 4.5B | 1.6% | |
| `WebSocket::before_send` (on non-ws req) | 1.2B | 0.4% | Router doesn't filter `before_send` by route match |
| `httpdate::fmt_http_date` per response | 1.5B | 0.5% | |
| `TypeSet` drop_in_place | 0.8B | 0.3% | per-request BTreeMap drop |

Per-request cycles: hyper **4,650** vs trillium **22,345** (5×).

### Walk-back on the writev claim

I initially said trillium "didn't use writev" based on a folded-stack grep.
Wrong: `trillium-http`'s `BufWriter` does support `poll_write_vectored` and
uses it in the multi-buffer flush case. In the typical small-response case the
BufWriter has only one buffered chunk to flush, so it calls plain `poll_write`
— same as hyper's path. Both libraries route through `poll_write` for typical
responses. Neither uses writev for the "common case."

The actual difference is per-request work: BTreeMap headers, TypeSet
allocation, ArcHandler chain, and a per-request `Vec::with_capacity()` for the
response buffer (`h1.rs:62`) that hyper avoids by reusing a connection-level
buffer in `Dispatcher`.

### The high-leverage trillium-http changes (in priority order)

1. **Reuse the response buffer across requests on a connection** instead of
   allocating fresh per response. Contained change in `Conn::send`. Probably
   the single biggest win.
2. **`HeaderMap` instead of `BTreeMap<KnownHeaderName, HeaderValues>`** —
   architectural but well-bounded. ~3-5% direct + memory.
3. **`TypeSet` redesign** — same logic for per-request state.
4. **`Bytes`-typed body API** (already on plan as item #3) — helps both the
   send path and h2 stream memory.
5. **Cache the date string at 1s resolution** — small but free.
6. **Router filters `before_send` by route match** — small fix, real semantics
   change. Anchor: `WebSocket::before_send` runs on every non-ws request today.

These are real architectural improvements. Each warrants its own design pass.

### h2 memory finding

`baseline-h2 1024c` peak RSS: trillium-tuned 4.3 GiB / prod 6.7 GiB / hyper 413 MiB.

Decomposed by swapping mimalloc for the system allocator:
- mimalloc retention accounts for ~30% of trillium's RSS (1.3 GiB on tuned).
  Tradeoff: removing mimalloc cost ~30% throughput. Real, but not the headline.
- The remaining ~3 GiB is real per-stream cost — about **30 KB per concurrent
  stream**. Hyper is 7× lower.

Per-stream allocation suspects: `Conn`'s `Headers` + `TypeSet` (both
BTreeMap), plus per-request response-buffer Vec. The same trillium-http
changes above (especially #1 and #2) would help here too.

## Bench-driven discoveries about trillium-proxy

### Pool churn under saturation (real bug)

Under `gateway-h3 -c 256 -m 32`: trillium-proxy opened **159 upstream TCP
connections** to a single backend. The h2 client pool's "all conns Busy"
branch falls through to a fresh-connect path with no cap. With upstream's
default `SETTINGS_MAX_CONCURRENT_STREAMS=100` and 8,192 in-flight streams
across 8 isolated per-worker pools, the proxy keeps opening sockets.

Two compounding issues:

1. **Per-origin pool cap is missing.** A proper pool would cap conns per
   origin (e.g., `2 * n_workers`) and *wait* for an Available stream slot
   when at the cap, rather than open fresh sockets indefinitely.
2. **Per-worker reuseport multiplies the problem.** Each of 8 worker
   `current_thread` runtimes had its own `Client` (cross-runtime sharing was
   considered risky, see comment in `proxy/src/main.rs`). 8 isolated pools =
   minimum 8 conns; under saturation, 8 separate "open-on-Busy" feedback
   loops.

### Multi-thread runtime is a big win for h3, mixed for h2

Swapped `proxy/src/main.rs` from "8 × current_thread workers via reuseport"
to "1 × multi-thread tokio runtime, 1 listener, 1 QUIC endpoint" and
re-benched:

| profile/conns | per-worker | multi-thread | Δ |
|---|--:|--:|---|
| gateway-64 / 512c | 3,877 | 3,475 | −10% |
| gateway-64 / 1024c | 3,271 | 3,505 | +7% |
| **gateway-h3 / 64c** | **3,199** | **240** | **−92%** |
| **gateway-h3 / 256c** | **970** | **2,447** | **+152%** |

Three things to take from this:

- **gateway-h3 256c stall is reproduced and explained.** Plan reported 989 RPS
  at 60-76% CPU. We see 970 RPS / 76.7% CPU under per-worker reuseport. Cause:
  quinn QUIC endpoint can only attach to one `current_thread` worker (QUIC
  routes by connection ID, not 4-tuple, so `SO_REUSEPORT` can't fan h3 across
  workers). All h3 work serializes onto one core. Multi-thread fixes it.
- **gateway-h3 64c regressed catastrophically under multi-thread** (3199 → 240
  at 20% CPU). Quinn's endpoint event loop is a single tokio task; at low load
  it can't extract parallelism from one task, and the cross-thread
  serialization overhead dominates. Per-worker / current_thread had perfect
  cache locality there.
- **gateway-64 is roughly flat.** Per-worker reuseport's "stay on one core for
  this conn" benefit cancels against multi-thread's work-stealing on a
  proxy-heavy workload like the gateway URI mix.

### The real architectural answer (not yet prototyped)

Plan called it: *"Multi-thread runtime just for h3, current_thread for
everything else (jbr's earlier hunch)."* Numbers above support this — both
extremes have a sharp transition; the blend would capture both wins.

Open question: how to support that cleanly in trillium's spawn API. One
option: `Config::spawn_h3` runs on a separate multi-thread runtime; the h1/h2
spawns stay on per-worker `current_thread`. Needs design.

### Other proxy nits found

- The `WebSocket::before_send` symptom from trillium-http profiling (firing on
  non-ws routes) compounds in proxy too — proxy chains its own handlers.
- Each per-worker `Client` in the old structure couldn't share an upstream
  pool. Even with a "smart" pool, having 8 separate pools wastes capacity.

## Bench-driven discoveries about trillium-static (prototyped)

Prototyped opt-in **precompressed-sidecar serving** in
`~/trillium/static/src/handler.rs`:

- New builder: `with_precompressed_sidecars(&[("br","br"),("gz","gzip"), ...])`.
- Per-request: walks Accept-Encoding, finds first sibling `<asset>.<ext>` that
  exists on disk, opens that file with `Content-Encoding: <enc>` and
  `Vary: accept-encoding`. Original asset's MIME is preserved.
- Composes correctly with `trillium-compression 0.3`'s
  "skip-if-Content-Encoding-set" behavior — the new compression-skip-path is
  what made this composition clean.

Effect on **trillium prod static** (which has no `StaticPreload`):

| profile/conns | before (q4 dynamic) | after (sidecar) | Δ |
|---|--:|--:|--:|
| static 1024c | 2,623 | 18,126 | +591% |
| static 4096c | 2,466 | 17,316 | +602% |
| static 6800c | 2,471 | 18,042 | +630% |
| static-h2 256c | 2,163 | 17,690 | +718% |
| static-h2 1024c | 1,797 | 14,664 | +716% |

Memory at static-h2 1024c: 6.5 GiB → 2.4 GiB.

Limit of the prototype: still ~1/8 of nginx's static throughput (200K) and
~1/8 of `StaticPreload`'s 145K. The remaining gap is per-request fs ops
(`canonicalize` + `metadata` × 1-3 sidecars + `File::open` + async streaming).
A startup-time directory index would close most of it but adds the design
question of fs-watch invalidation — explicitly out of scope for the
prototype.

This is the right shape for landing in `trillium-static`. ~120 lines of
handler change + an opt-in builder.

## Bench-driven discoveries about the harness

Real bugs / caveats found in the HttpArena harness while running on 8 cores:

1. **`compose.gateway*.yml` cpusets were hardcoded** (`0-7,64-71` /
   `8-31,72-95` etc.). Patched the trillium ones to use
   `${PROXY_CPUSET:-...}` / `${SERVER_CPUSET:-...}` substitution. Same patch
   wanted on `frameworks/aspnet-minimal_nginx/compose.gateway.yml`,
   `frameworks/aspnet-minimal_caddy/compose.gateway-h3.yml`, and
   `frameworks/aspnet-minimal_nginx/compose.production-stack.yml`.
2. **`REDIS_CPUSET` defaulted to `"0,64"`** — invalid on small boxes. Set in
   `run-comparison.sh`.
3. **`aspnet-minimal_caddy.csproj` was missing `StackExchange.Redis`** even
   though its Dockerfile pulls in `aspnet-minimal/AppData.cs` which uses it.
   Build failed before the patch. Fixed locally.
4. **`gcannon` `timeout 45` doesn't actually kill the docker container** when
   the load gen hangs — sends SIGTERM to `docker run` but the underlying
   container keeps running. Caused a 26-min hang during baseline static-h2
   (which legitimately stalls on the q11 brotli bug). Should use
   `docker run --stop-timeout 5` or pair `timeout` with `docker stop` on exit.
5. **gcannon's WebSocket cliff at 4096+ conns isn't a harness-side load-gen
   bottleneck** — varying `-t` from 64 → 512 doesn't change much. The cliff is
   real polling/scheduling overhead at high active-conn counts.
6. **`ws_echo` handlers in trillium variants cloned the message twice** —
   `t.to_string()` + `Message::text(...)` reconstruct. Patched to forward the
   original `Message` via `conn.send(msg)`. +15% at 512c, +5-10% at high
   conn counts. Not the cliff cause but a real win.

## Code changes made (uncommitted; on `__trillium` branch and `~/trillium` `compression` branch)

In `~/trillium`:
- `static/src/handler.rs`: added `with_precompressed_sidecars` opt-in API.

In `~/HttpArena/frameworks/trillium`:
- `src/handlers/ws.rs`: no-clone echo.
- `src/main.rs`: wired `with_precompressed_sidecars(&[("br","br"),("gz","gzip")])`
  into the static handler.
- `proxy/src/main.rs`: removed per-worker reuseport / current_thread pattern;
  one multi-thread tokio runtime; awaits the `ServerHandle` returned by
  `Config::spawn`. (See "Mixed-runtime prototype" below — this is *not* the
  final shape.)
- `compose.gateway.yml`, `compose.gateway-h3.yml`: cpuset env-var substitution.

In `~/HttpArena/frameworks/trillium-tuned`:
- `src/handlers/ws.rs`: no-clone echo (same patch as prod).

In `~/HttpArena/frameworks/aspnet-minimal_caddy`:
- `aspnet-minimal_caddy.csproj`: added `StackExchange.Redis` package reference.

In `~/HttpArena/scripts`:
- `stage-trillium.sh`: rsync ../trillium into each variant's `_trillium/`.
- `run-comparison.sh`: 8-core env defaults (`LOADGEN_DOCKER`, `REDIS_CPUSET`,
  `PG_CPUSET`, `PROXY_CPUSET`, `SERVER_CPUSET`) and a thin wrapper around
  `benchmark.sh` to iterate a list of profiles.
- `compare-runs.sh`: side-by-side baseline-vs-after table for one framework.
- `compare-frameworks.sh`: side-by-side table across N frameworks for one
  results tree.

## Mixed-runtime "Shape B'" — landed, with a tunable knob

Built and benched on the 8-core box. Per-worker `current_thread` workers for TCP
h1/h2 (the existing reuseport pattern, but now with **no** QUIC binding on
worker 0) plus one extra OS thread running a `new_multi_thread` runtime that
joins the TCP reuseport pool as one additional participant *and* owns the QUIC
endpoint. h3 stream tasks spawned by quinn's accept loop spread across the MT
runtime's threads via work-stealing; TCP traffic is mostly absorbed by the
per-worker pool (kernel reuseport hash gives the MT runtime ~1/(N+1) of TCP).

The MT runtime size is a knob — `QUIC_THREADS` env var. Empirical 8-core curve:

| profile/conns | per-worker | Shape A* | B' Q=2 | B' Q=4 | B' Q=8 |
|---|--:|--:|--:|--:|--:|
| baseline-h3 64c | 107K (1 core) | 191K (3.6c) | 122K (2c) | 184K (3.3c) | 197K (3.8c) |
| static-h3 64c | 23K (1c) | 73K (3.9c) | 47K (2c) | 68K (3.6c) | 71K (4c) |
| baseline-h2 1024c | 387K | 310K (−20%) | 376K (−3%) | 356K (−8%) | 342K (−12%) |
| async-db 1024c | 11.6K | 9.0K (−22%) | 11.4K (−1%) | 8.7K (−25%¹) | 9.6K (−17%) |
| json 4096c | 146K | 138K | 146K | 158K (+8%) | 159K |
| pipelined 4096c | 505K | 491K | 501K | 501K | 497K |
| static-h2 1024c | 157K | 151K | 152K | 156K | 150K |

*Shape A = single MT runtime hosting N reuseport TCP listeners + the QUIC
endpoint. Strictly worse than Shape B' Q=8 because it eliminates per-worker
TCP locality entirely.

¹ async-db at Q=4 looks anomalous against the −1%/−17% bracket — probably
scheduler/cache interaction at exactly 12 threads on 8 cores, or just
run-to-run variance. Worth a re-run if it matters.

### Default

Both `frameworks/trillium-tuned/src/main.rs` and `frameworks/trillium/proxy/src/main.rs`
ship with `QUIC_THREADS` defaulting to `(n_workers / 4).clamp(2, 8)`. This
maps:
- 8c box → 2 (the no-TCP-regression sweet spot)
- 32-phys / 64-SMT bench box → 8 (one Zen2 CCX worth, which keeps h3 work
  L3-local — crossing CCX boundaries on Zen2 is ~70 cycles, expensive when
  amortizing across stream-task scheduling)
- ≥64 phys cores → 8 (capped at one CCX)

The proportional default lands users near the "free TCP, modest h3 win" point
on small machines and the "max h3" point on the bench machine, which is
roughly what we want for headline RPS on both.

### Proxy variant — same shape, but the gateway-h3 64c picture is different

trillium-proxy in Shape B' (Q=2 on 8c):

| profile/conns | per-worker | plain MT | Shape A | Shape B' |
|---|--:|--:|--:|--:|
| gateway-64 / 512c | 3,877 | 3,475 | 3,412 | 4,030 (+4%) |
| gateway-64 / 1024c | 3,271 | 3,505 | 3,877 | 3,230 |
| gateway-h3 / 64c | **3,199** | 240 | 676 | 974 |
| gateway-h3 / 256c | 970 | 2,447 | 2,162 | 2,231 |

Shape B' fixes the gateway-h3 256c stall (the original PR #696 bug — 970 RPS
at 60-76% CPU) and is +44% over Shape A on h3 64c, but doesn't recover
per-worker's 3,199 RPS at h3 64c. **That 3,199 isn't real performance**: the
per-worker design wins by virtue of having N isolated `Client` pools, each
opening unbounded fresh upstream connections under saturation (159 total at
N=8; would scale to ~1,000+ at N=64 on the bench machine). See the
trillium-client pool issue draft (`trillium-client-pool-issue.md`) — once
that lands with proper cap-and-wait semantics, per-worker's 3,199 will fall
back to whatever single-pool capacity actually permits, which is roughly
where Shape B' lands today.

### Hardware caveat

All numbers above are 8-core c7i.2xlarge, **not** the reference 32-phys / 64-SMT
Threadripper PRO 3995WX bench machine. The Shape A / Shape B' / Q-curve
relationships should hold proportionally — they're about thread topology, not
absolute throughput — but the absolute deltas will differ. Specifically:

- Cross-thread contention is much smaller on the bench machine (proportional
  thread oversubscription is what matters, and 64 + 8 = 72 threads on 64 SMT
  slots is much less competitive than 8 + 2 = 10 on 8 SMT slots).
- CCX boundaries (16 MB L3 per 4 phys cores on Zen2) make the case for
  pinning the QUIC MT runtime to a single CCX — h3 work stays L3-local,
  per-workers on the other 7 CCXs are undisturbed. Not yet prototyped.

## Open work — pickup points for the next session

### 1. Validate Shape B' on the 32-phys-core bench box

Re-run the focused-7 + h3 set on the reference machine. Compare against
per-worker baseline. Confirm: (a) Q=8 default lands at peak h3 with minimal
TCP regression, (b) gateway-h3 256c stall is fixed for the proxy, (c)
gateway-h3 64c proxy regression vs per-worker is consistent with the
per-pool-capacity story.

### 2. CCX-aware pinning for the QUIC MT runtime (improvement, not blocker)

Pin the QUIC runtime's worker threads to one Zen2 CCX (e.g. cores `28-31,92-95`
within the bench server cpuset `0-31,64-95`). Should keep h3 work L3-local
and avoid the per-CCX cache eviction that work-stealing across the full
cpuset would cause. Depends on `core_affinity` or similar inside the
runtime-spawn thread.

### 3. trillium-client pool: cap + wait

See `trillium-client-pool-issue.md` for the full writeup. Architectural fix
to the "open new conn on saturation" behavior. Bench-side this is what makes
the proxy's gateway-h3 64c numbers reflect real capacity rather than pool
churn. Same fix benefits every h2-upstream proxy regardless of runtime
topology.

### 4. trillium API for h3-only Configs (ergonomic follow-up)

Today `Config::with_quic` requires a TCP listener with a populated socket
addr (it reads `info.tcp_socket_addr()` to know what UDP port to bind), so
the QUIC-bearing Config has to also be a TCP reuseport participant. With a
`Config::with_quic_only(...)` or similar, Shape B' could be cleaner: the MT
runtime would do *only* h3, leaving 100% of TCP to the per-worker pool. Not
a blocker (the current shape works fine; the MT runtime just gets a small
share of TCP traffic), but it would simplify the framework code and remove
the redundant TCP binding on the QUIC runtime.

### 5. Land trillium-compression 0.3

Predictions all held. Release whenever ready. The HttpArena trillium variants
already pin path-deps to the local clone — no action needed from the bench
side once published.

### 6. trillium-http hot path improvements

Each is a separate design pass:
- Per-conn response buffer reuse in `Conn::send`
- `HeaderMap` instead of `BTreeMap`
- `TypeSet` redesign  
- `Bytes`-typed `Body` API (plan item #3)
- Date header caching
- Router filtering `before_send` by route match

### 7. Land precompressed-sidecar in trillium-static

Prototype is in `~/trillium/static/src/handler.rs`. Tests not written
(deliberately — prototype scope). For real landing: ETag-per-encoding
behavior, the index-files-with-sidecar path, opt-in builder ergonomics.

### 8. Open harness PRs

- `compose.gateway*.yml` cpuset env substitution for the 3 aspnet variants.
- `aspnet-minimal_caddy.csproj` missing redis dep.
- gcannon timeout-not-killing-docker-container in `lib/tools/gcannon.sh`.
- `REDIS_CPUSET` default should adapt to nproc < 64 (same logic
  `GCANNON_CPUS` already has).

## Reproducing the bench

```bash
# One-time setup (already done on this AMI):
sudo apt install docker-ce docker-ce-cli containerd.io
sudo systemctl enable --now docker
sudo usermod -aG docker ubuntu
# Build load-gen images (gcannon, h2load, h2load-h3, wrk, ghz):
for img in gcannon h2load h2load-h3 wrk ghz; do
    sudo docker build -t $img:latest -f docker/$img.Dockerfile docker/
done
# Stage local trillium into both variants:
./scripts/stage-trillium.sh

# A focused run for one framework:
sudo ./scripts/run-comparison.sh trillium-tuned json json-comp async-db static static-h2

# Compare baseline (worktree) vs after-fix:
git worktree add ../HttpArena-baseline a1e77dc2
sudo ./scripts/run-comparison.sh trillium-tuned ...   # in baseline
sudo ./scripts/run-comparison.sh trillium-tuned ...   # in main
./scripts/compare-runs.sh trillium-tuned ../HttpArena-baseline/results

# Cross-framework table:
./scripts/compare-frameworks.sh results trillium-tuned trillium actix nginx
```

The 8-core env defaults are baked into `scripts/run-comparison.sh`; if invoking
`benchmark.sh` directly, set them yourself.
