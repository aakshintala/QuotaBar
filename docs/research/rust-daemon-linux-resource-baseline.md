# Offline Rust daemon Linux resource baseline

## Scope

This is an offline Linux baseline of the daemon library's real scheduler coordinator, loopback HTTP listener and Reqwest loopback client. It uses `daemon/examples/offline-resource-check.rs`, not a complete daemon executable or a production-provider run.

The example creates a unique mode-0700 temporary HOME, opens runtime state only there and removes it on normal completion. Its five provider checks are synthetic and return observations after a 50 ms async delay. It checks that neither 60-second idle interval starts provider work, that the first loopback GET starts exactly five concurrent checks, and that 1,000 sequential cached GETs start no more checks. The HTTP client disables proxies and redirects and has five-second connect and request timeouts. It is configured with Rustls, but the loopback HTTP requests perform no TLS handshake.

The run used a temporary source copy at `/tmp/quotabar-final-check.OouRPU`, created from `daemon/Cargo.toml`, `daemon/Cargo.lock`, `daemon/src`, `daemon/tests`, `daemon/examples`, and `Tests/Fixtures/OpenCode`. It excluded `.git` and `target`. The macOS copy used `COPYFILE_DISABLE=1 tar --no-xattrs`; the Linux builds and tests used `CARGO_NET_OFFLINE=true`.

## Verification

Host on 2026-11-04: Ubuntu 26.10, Linux `7.3.0-9-generic` x86_64, Intel Core i7-13700T, Rust/Cargo 1.99.0. The separate commands below all passed on the copied snapshot:

```sh
CARGO_NET_OFFLINE=true cargo fmt --all -- --check
CARGO_NET_OFFLINE=true cargo clippy --locked --all-targets -- -Dwarnings
CARGO_NET_OFFLINE=true cargo test --locked --all-targets
CARGO_NET_OFFLINE=true cargo build --locked --release --lib
CARGO_NET_OFFLINE=true cargo build --locked --release --example offline-resource-check
```

Test results: 78 library tests and 9 TCP/listener integration tests passed (87 total); example test harnesses contained zero tests. Formatting, Clippy with warnings denied, release library build and release resource-example build passed. The final gates completed offline against the lockfile.

## Measured result

The one benchmark invocation was bounded externally to 160 seconds and measured with `/usr/bin/time -v`:

```sh
timeout --signal=TERM --kill-after=5s 160s \
  /usr/bin/time -v target/release/examples/offline-resource-check \
  --run-offline-resource-check
```

The process completed normally in 2:00.26. Both 60-second idle phases passed with no provider work. The first GET started five checks with maximum concurrency five. The 1,000 cached GETs caused no additional checks and took 139.290 ms total.

| Observation | Linux result |
|---|---:|
| `/usr/bin/time -v` user + system CPU, whole process | 0.10 s + 0.06 s over 120.26 s |
| `/usr/bin/time -v` peak RSS | 5,452 KiB |
| `/usr/bin/time -v` voluntary / involuntary context switches | 304 / 23 for the whole process |
| quiet idle before first request | 0 CPU ticks observed; 5-second samples through 55 s unchanged |
| idle after activity | 16 cumulative CPU ticks at the stage boundary and unchanged at 5-second samples through 115 s |
| observed threads | 1 at every `/proc` sample |
| sampled RSS | 5,452 KiB at start; between 3,736 and 4,696 KiB during the later idle samples |
| provider checks | 0 in each idle; 5 on first GET; 0 additional across 1,000 cached GETs |

The `/proc/<pid>/stat` CPU values use 100 ticks per second on this host, so one tick is about 10 ms. The quiet-idle snapshots therefore show no CPU increase at that resolution; they do not establish finer CPU bounds. The 16 cumulative ticks after activity include startup, the first refresh, the cached requests and other work before the idle-after-activity boundary; the unchanged count in subsequent five-second snapshots is the idle observation. Whole-process time includes all 1,000 HTTP requests and startup/shutdown, so it is not an idle-only CPU figure.

Context-switch counts came from `/proc/<pid>/status` and `/usr/bin/time -v`. They are scheduler context-switch counts, not exact wakeup counts. The final `/proc` sample also recorded 43 voluntary context switches while the process was finishing; no periodic activity was seen in the preceding idle samples. `Maximum resident set size` and sampled RSS are resident memory observations, not private-memory footprint. Linux RSS is not comparable to macOS memory footprint.

For context, the separate macOS baseline reported 9,256,960 bytes peak RSS, 2,785,616 bytes peak memory footprint, and 0.11 seconds whole-process CPU over 120.24 seconds. The Linux run reports no footprint-equivalent measurement; the figures are host-specific observations, not a cross-platform memory budget.

## Limits

- Synthetic provider callbacks and HTTP loopback traffic do not exercise real provider requests, real payloads, credentials, rate limits or TLS network handshakes. No live provider request was made in this run.
- This is not an integrated, long-lived Linux daemon acceptance test. It excludes app integration, launch/login behavior, prolonged cache expiry and sustained external traffic.
- RSS is not private footprint, and `/proc` tick granularity cannot prove exact zero CPU or exact wakeups during idle.
- The temporary-copy evidence covers the copied source and tests only; it does not establish behavior of a final packaged daemon or private-memory ceilings.
