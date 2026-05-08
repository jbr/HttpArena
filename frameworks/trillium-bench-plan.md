# Trillium HttpArena benchmark plan

Pre-AWS notes covering the preliminary results from PR #696, the fixes applied
since, what we expect to see in the next run, and follow-on optimization
directions worth investigating once we have apples-to-apples numbers against
actix / hyper / nginx.

---

## Preliminary results (PR #696, pre-fix)

Both variants were submitted with no manual tuning of trillium-compression
defaults and the original `StaticPreload` that cloned a `Vec<u8>` per request.

### Wins (trillium-tuned, top of leaderboard)

| Test | Conn | RPS | Rank | Leader / peer |
|---|---|---|---|---|
| api-4 | 256 | 62,784 | **#1/42** | next: ngx-php 62k, simplew 53k |
| api-16 | 1024 | 173,700 | **#1/42** | next: ngx-php 142k, actix 111k |
| async-db | 1024 | 291,648 | **#1/43** | next: Swoole 243k, actix 153k |
| crud | 4096 | 576,036 | **#1/9** | next: genhttp 484k |
| json | 4096 | 836,703 | #7/46 | actix 1.18M |
| json-tls | 4096 | 705,152 (prod) | #4/27 | ngx-php 804k |

The composite-workload tests (routing + state + DB + JSON) are where Trillium
shines.

### Pretty good

| Test | Best RPS | Leader / Rust peer |
|---|---|---|
| baseline-h2 256c | tuned 2.19M (#11/22) | h2o 13.6M, hyper 6.97M |
| echo-ws-pipeline 4096c | 4.66M (#6/9) | dogrider 50M, actix 43M |
| baseline 512c | 651k (#38/60) | next to hyper (2.94M); rust-epoll 4M |
| pipelined 4096c | 2.87M (#28/58) | rust-epoll 54M (~19× off) |

### Catastrophic cliffs (root-caused → all upstream-fixable)

| Test | RPS | Note |
|---|---|---|
| static 1024c | 454 / 1404 | actix 6404 (also broken), pingora 239k, leaders 800k–1.3M |
| json-comp 4096c | 1,475 | actix 152k. ~100× off |
| static-h2 1024c | 0 | prod hits 106 GiB memory; tuned reports 0 RPS |
| static-h3 64c | 423 / 52 | nginx 306k |
| baseline-h3 64c | 401k (prod) / 121k (tuned) | nginx 4.93M |
| gateway-h3 256c | 989 | aspnet+caddy 56k. CPU only 60-76% — stalled |

---

## Tuned vs production (PR results)

Excluding h3 (where tuned's single QUIC endpoint is expected to lose):

- **Tuned wins:** 14 tests (including +30%–300% on h2, async-db, crud,
  echo-ws-512c, static)
- **Tuned ties:** 11
- **Tuned regressions:** 5 — all at high connection counts where
  per-core-sharded current_thread runtimes lose to multi-threaded work
  stealing on cheap requests. Notable:
  - `baseline 4096c`: −40% (562k vs 931k)
  - `json-tls 4096c`: −20%
  - `baseline-h3` and `static-h3`: expected, single-QUIC-endpoint constraint

The reuseport-per-core pattern is a real win for connection-affinity workloads
(h2, ws, mid-conn-count tests) and a real loss when the kernel can't even out
load between workers. Worth keeping both variants.

---

## Changes since PR #696

### `trillium-compression` 0.3 (breaking)

`compression/src/lib.rs` — three real bugs and one breaking default change:

1. **Skip if response `Content-Encoding` is already set** — was unconditionally
   re-compressing, blowing up precompressed-sidecar workflows (root cause of
   `static-h2` reaching 100 GiB memory).
2. **Skip if response `Content-Type` is in a known already-compressed set**
   (image/jpeg/png/webp/avif/heic/gif/x-icon, video/*, audio/*, font/woff*,
   application/zip/gzip/zstd/x-bzip2/x-xz/x-7z-compressed). `image/svg+xml`
   and `application/wasm` are intentionally still compressed.
3. **Use `with_quality(...)` instead of `new()`** for all three encoders —
   per-request encoder allocation now respects the configured level instead
   of pulling `Level::Default` (which for brotli is q11).
4. **Default brotli level changed from `Level::Default` (q11) to
   `Level::Precise(4)`** — matches nginx/caddy/Cloudflare. q11 is roughly 100×
   slower than q4 for marginal size gains. This is the smoking gun for
   json-comp at 1.5k RPS.

New API: `with_brotli_level`/`with_gzip_level`/`with_zstd_level` taking
`async_compression::Level` (re-exported as `trillium_compression::Level`).

### `trillium-askama`

Docstring fix on `AskamaConnExt::render`: previously claimed it set
`Content-Type` from the template extension; askama 0.15 dropped exposure of
the template path/extension, so the feature was dropped without updating the
doc. Callers now correctly set `Content-Type` themselves.

### HttpArena `frameworks/trillium*/`

- **`StaticPreload` zero-alloc per request** (tuned only): bodies stored as
  `&'static [u8]` (leaked at startup; resident for process lifetime anyway),
  passed straight to `Conn::with_body` so per-request work is just a header
  write and a pointer copy.
- **`/fortunes` endpoint added** (both variants). Uses askama compile-time
  templates with a custom `NamedHtmlEscaper` because askama 0.15's built-in
  `Html` escaper writes numeric entities (`&#60;`) and the harness validator
  greps for the literal `&lt;script&gt;`.
- **`meta.json`** updated to subscribe to `fortunes` in both variants.

---

## Expected results on the next run

Predictions to verify against AWS numbers, ordered by expected impact.

### Should resolve catastrophic cliffs

| Test | Pre | Expected | Why |
|---|---|---|---|
| `json-comp 4096c` | 1,475 | 100k–250k | brotli q11 → q4 ≈ 10–25× faster, no other expensive work in this path |
| `static-h2 1024c` | 0 RPS / 106 GiB | 100k+ / <1 GiB | sidecar-bypass + q4 = no per-stream encoder explosion under h2 multiplexing |
| `static-h3 64c` | 423 / 52 | tens of k | same root cause as static-h2 |
| `static 1024c` | 454 / 1404 | 100k+ (tuned) | sidecar-bypass on tuned; prod still does dynamic compression at q4 |

### Should improve modestly

| Test | Pre | Expected | Why |
|---|---|---|---|
| `baseline-h3 64c` (prod) | 401k | maybe a touch | not compression-bound; mostly QUIC stack |
| `gateway-h3 256c` | 989 | unclear | CPU was 60–76% (under-utilized). May still be stalling on something orthogonal — needs investigation |

### Probably unchanged

The tuned regressions at high connection count (`baseline-4096c`,
`json-tls-4096c`) are architectural — current_thread runtimes shard
load via SO_REUSEPORT, which underperforms multi-thread work stealing for
cheap requests when kernel-side connection distribution gets bursty.

The h3 regression in tuned is structural — single QUIC endpoint pinned to
worker 0 means h3 has 1 core where prod has all of them.

### New on the leaderboard

- **`/fortunes`**: untested. askama compile-time templates + 200-row PG fetch.
  Reasonable expectation: somewhere in the middle of the template-engine field.

---

## Follow-on directions worth investigating on AWS

Listed roughly by ratio of (expected RPS gain) to (engineering effort).

### High value, low effort

1. **Re-measure `gateway-h3`.** CPU at 60-76% with 286-989 RPS is a stall, not
   a compute bound. Likely candidates: QUIC client connection-establishment
   loop in `trillium-client`/`trillium-quinn`, certificate verification doing
   sync work, or the `AcceptAnyServerCert` verifier we wrote being called more
   than expected. Strace + flamegraph from one worker for 5s tells the story.

2. **Investigate `echo-ws` cliff in tuned at 4096+ conn.** Tuned drops from
   1.76M (512c) to 419k (4096c) while production stays flat at ~350-430k
   across all conn counts. Suspicions: DashMap contention in shared state, or
   per-worker `current_thread` runtime saturation under uneven kernel
   connection distribution.

### Medium value, medium effort

3. **WriteEnd / Bytes-typed body API.** `Body::new_static` accepts
   `Cow<'static, [u8]>`. Even with leaked `&'static [u8]` for static files,
   dynamic JSON paths still allocate a `Vec<u8>` per request. A `Bytes`-backed
   variant of `Body` could let `crud_read` / `crud_list` avoid redundant
   copies between sonic-rs's output and the wire. Probably a 5–15% win on
   JSON-heavy paths.

4. **Quinn endpoint sharing across workers** for h3-tuned. Today only worker
   0 has the QUIC endpoint. Options:
   - Single endpoint + `SO_REUSEPORT` on the UDP socket (kernel splits flows
     by 4-tuple, but QUIC routes by connection ID, not 4-tuple — won't work
     correctly for migrated connections). Not a real solution.
   - One endpoint, work-stealing between workers via channel (loses per-core
     L1 cache benefit but evens load). Probably the right answer if h3 perf
     matters.
   - Multi-thread runtime *just* for h3, current_thread for everything else
     (jbr's earlier hunch). Cleanest if we can get trillium to support
     mixed-runtime spawn.

### Lower value, higher effort

5. **`trillium-runtime-adapter` for the reuseport pattern.** If the per-core
   pattern proves consistently faster on the workloads where it currently
   wins, expose it as a first-class adapter so end users don't have to write
   the bind/swansong/per-worker-pool plumbing themselves. Mentioned earlier
   as a possible follow-up.

6. **Body emission specialization for very small responses.** baseline /
   json-1 / pipelined responses are <100 bytes; the cost is dominated by
   syscalls and Body trait dispatch rather than payload work. Could explore
   a fast-path that batches multiple small responses into a single writev,
   or pre-encodes a status-line + headers buffer for the common "200
   text/plain short body" case. High effort, hard to predict ratio.

7. **`io_uring` runtime variant** (long-shot). leaders on baseline / pipelined
   (rust-epoll, ringzero, libreactorng) are all hand-rolled io_uring. Trillium
   can't realistically catch them on those tests with tokio-mio underneath,
   but a `trillium-tokio-uring` adapter could close some of the gap. Not a
   pre-1.0 priority.

### Things explicitly NOT worth investigating yet

- **Pipelined 19× behind rust-epoll.** Real Rust-vs-Rust gap, but Hyper is in
  the same neighborhood (2.94M baseline). Unless someone produces a profile
  pointing at a specific hot spot, "we lose to a hand-rolled epoll daemon" is
  expected and not actionable.
- **The high-conn-count regressions in tuned (-40% baseline, -20% json-tls).**
  Genuine architectural property of per-core sharding under bursty load.
  Documented; don't chase.

---

## Workflow notes for the AWS box

- The benchmark harness expects `cargo` to build both crates with their
  release profiles (LTO fat, opt-level 3). Cold builds take ~3-4 min on the
  c7i.2xlarge.
- The benchmark harness uses cgroups to isolate framework + load gen on
  separate CPU sets (`cpuset` lines in `compose.gateway.yml`).
- `validate.sh` runs against a live server and gates the benchmark; if a
  validation fails, the bench skips that profile and you'll see an empty
  cell in the result table.
- When iterating on trillium-compression, `cargo update -p trillium-compression`
  in HttpArena's Cargo.lock isn't enough if we're using a path dep — Docker's
  build context needs the full trillium workspace too. Easiest is to bind
  mount or copy the trillium dir into the framework dir before `docker build`.
- For ad-hoc smoke tests, the local trillium server can be exercised via the
  same env vars used in compose: `DATABASE_URL`, `DATASET_PATH`, `STATIC_DIR`,
  `TLS_CERT`, `TLS_KEY`, `TLS_PORT`, `WORKERS`.

---

## Open questions to resolve from AWS data

1. Did the brotli q4 default close the json-comp gap to actix? If we're still
   ≥3× off, dig into compression hot-path beyond level config.
2. Did `static-h2` come back from 0? If yes — by how much? If still 0 → there's
   a different bug (probably in `trillium-rustls` or the h2 stream lifecycle
   under high concurrent stream count).
3. Is `gateway-h3` still in the hundreds of RPS at <80% CPU? If so, it's a
   blocking call somewhere — find it.
4. Where does `/fortunes` land vs other Rust frameworks with compile-time
   templates (askama, maud, templ-equivalent)?
5. Does the `static` test go from 454 to roughly the actix range (6k) or to
   a real number (50k+) on prod? This tells us whether there's still a hidden
   per-request cost in `trillium-static` itself beyond the compression bug.
