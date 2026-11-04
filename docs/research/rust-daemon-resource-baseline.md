# Offline Rust daemon resource baseline

## Scope

Measured the real `quotabard::scheduler` coordinator, `LoopbackListener`, loopback HTTP/1 requests, and a configured Reqwest client using the offline example at `daemon/examples/offline-resource-check.rs`. This is a baseline for the daemon shape, not a production-daemon acceptance result.

The example is opt-in: without `--run-offline-resource-check`, it performs no work. It creates a unique mode-0700 temporary HOME below the system temporary directory, opens `RuntimeState` only there, and removes that directory on normal completion or ordinary errors. It never reads the user's HOME, credentials, Keychain, or services, and it makes no provider requests. All five provider IDs return synthetic observations after a 50 ms async delay. The first real GET starts the checks concurrently; the example verifies five active checks, five feed providers, and no additional checks across 1,000 sequential cached GETs. It also verifies no checks during either idle interval. The Reqwest client disables proxies and redirects and has 5-second connect/request timeouts. Its TLS configuration is built, but the HTTP-only loopback requests do not perform a TLS handshake.

## Measured result

Run on 2026-11-03, macOS 26.6.2 (Darwin 25.6.0), Apple M3 Pro / arm64, Rust 1.98.1, optimized release example. Command:

```sh
cargo build --offline --release --manifest-path daemon/Cargo.toml --example offline-resource-check
/usr/bin/time -l daemon/target/release/examples/offline-resource-check --run-offline-resource-check
```

Two runs are reported separately below. Both use the same two 60-second idle stages, one first GET, and 1,000 sequential cached GETs; both passed the example's assertions (zero checks in idle, five concurrent synthetic checks on first GET, and no further checks after caching).

### Run A: coarse `time` and `ps` capture

This 120.24-second run used `/usr/bin/time -l` around the example and an external `ps` sampler targeting the example PID every five seconds. The tools reported different cumulative CPU totals for the same process: `/usr/bin/time -l` gave 0.05 s user + 0.06 s system, while `ps` showed 0.13 s during the post-activity idle. These are not separate-run values, and they do not reconcile; do not combine or average them. The `ps` values were 0.00 s throughout the first idle, 0.12 s at elapsed 61 s, and 0.13 s from elapsed 66 through 116 s. The 1,000 GETs took 147.357 ms total.

Other `time`/`ps` observations from Run A: peak memory footprint 2,785,616 bytes; maximum RSS 9,256,960 bytes; `ps` RSS 7,280 KiB before the first request and 8,928 KiB after activity; `time -l` counted 14 voluntary and 5,468 involuntary context switches over the process. The RSS and context-switch numbers are not private footprint or exact wakeup counts.

### Run B: native macOS process-resource counters

A separate 120.512-second run used a small temporary C supervisor, compiled against the installed macOS 26.5 SDK headers. It called `proc_pid_rusage` with `RUSAGE_INFO_V4` and the SDK-defined `struct rusage_info_v4`, from the same user and without elevated privileges. The supervisor sampled every five seconds and immediately at the example's stage markers. It lived under the system temporary directory and was removed after the child exited; the example itself also removed its generated temporary HOME.

| Run B observation | Result |
|---|---:|
| Quiet idle interval | elapsed 0.314–60.317 s; raw CPU delta 578 user ticks + 2,038 system ticks; converted 24,083 ns + 84,917 ns (109,000 ns total); `ri_interrupt_wkups` delta 3; `ri_pkg_idle_wkups` delta 0 |
| Idle after first GET and cache burst | elapsed 60.509–120.512 s; raw CPU delta 1,058 user ticks + 3,411 system ticks; converted 44,083 ns + 142,125 ns (186,208 ns total); `ri_interrupt_wkups` delta 4; `ri_pkg_idle_wkups` delta 0 |
| Process CPU at final sample | raw 1,064,812 user ticks + 1,275,935 system ticks; converted 44,367,167 ns + 53,163,958 ns (97,531,125 ns total) |
| `ri_lifetime_max_phys_footprint` | 2,785,616 bytes (about 2.66 MiB) |
| `ri_resident_size` at final sample | 9,224,192 bytes (about 8.80 MiB) |
| Cached GETs | 1,000 in 120.132 ms total |

During both idle intervals, the five-second samples stayed unchanged until the example's timer reached the end of that interval. The three and four `ri_interrupt_wkups` increments above include the interval-ending wake and stage work; they are not evidence of recurring periodic work. The field names are reported as the OS provides them: this is not a claim that these counters capture every possible wakeup. No `powermetrics`, `sudo`, or other privileged instrumentation was used.

The SDK header defines these CPU fields as `uint64_t` but does not document their time unit. On this host, `mach_timebase_info` returned a numerator of 125 and denominator of 3, so one Mach absolute-time tick converts to 125/3 nanoseconds. Run B's raw values are ticks, not nanoseconds.

A separate temporary C calibration spun on CPU for 250 ms, then compared `proc_pid_rusage` deltas converted with the measured timebase against `CLOCK_PROCESS_CPUTIME_ID`. It measured 6,000,081 raw ticks, which convert to 250,003,375 ns; the process CPU clock measured 250,004,000 ns. `/usr/bin/time -p` reported 0.07 s user + 0.17 s system for that short calibration. This supports the tick interpretation and conversion on this host. The converted Run B total of 97.531 ms is also in the same range as Run A's coarse 0.11 s `/usr/bin/time -l` total, while Run A's `ps` result still disagrees and remains explicitly unresolved.

Run A remains a coarse capture with a same-run discrepancy; do not treat its `ps` CPU value as a second run or combine it with Run B. These observations do not set a resource budget. Footprint and resident size are distinct measures; neither gives virtual-stack reservation. The SDK's unshared-stack field was not used to estimate stack reservation. No Linux runtime measurement was made.

## Limits and follow-up

- The HTTP round trips are real loopback traffic, but the 50 ms provider tasks are synthetic. Real provider HTTP, credentials, rate limits, payload variation, and TLS-handshake/network memory are excluded.
- This measures one idle process and a short burst, not long-lived cache expiry, sustained request load, launch/login overhead, app integration, or all five real provider implementations.
- The HTTP client's Rustls configuration is included in the binary and client construction; there is no TLS network activity in this benchmark.
- `time -l` measures the entire process. `ps` samples every five seconds and has coarse CPU-time resolution. Neither gives an exact idle wakeup count.
- Do not infer a RAM ceiling from this single macOS run. Repeat on Linux and with the final integrated daemon, then measure actual provider acceptance paths before setting a resource budget.
