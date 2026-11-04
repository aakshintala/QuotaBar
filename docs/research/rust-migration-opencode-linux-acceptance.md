# OpenCode Linux HTTP acceptance

## Result

The Rust Reqwest/Rustls client successfully fetched OpenCode Go usage on native Linux. The separately approved, bounded check reported:

```text
provider=opencode-go elapsed_ms=415 result=success http_status=200
LIVE_CHECK_EXIT=0
```

The owner approved provider calls before execution. The example was invoked once, after independent review cleared the response-time Retry-After defect. No quota response, credential, account identifier or raw error was printed or saved as a fixture.

This closes the Phase 1 OpenCode Linux HTTP compatibility check. It does not establish acceptance of the unfinished listener, all five providers, resource consumption, macOS credential access or service installation.

## Scope and safeguards

The check used `daemon/examples/opencode-live-check.rs` with the exact opt-in argument `--run-live-check`, built in release mode on Linux x86_64 with Rust/Cargo 1.99.0. The source package uses Reqwest 0.13.5 with Rustls.

The example holds the instance lock and checks the actual daemon configuration and any saved OpenCode cooldown before probing. It reads the host's existing credential file in memory; credentials were not copied between machines. The client has a fixed provider HTTPS URL, refuses redirects and environment proxies, and bounds request time and response size. The example preserves a future Retry-After in runtime state before returning. An external 25-second timeout bounded execution.

No Swift engine ran on that Linux host. No service was installed, no lingering setting was changed, and no cutover occurred. No owner Keychain item or OAuth renewal endpoint was accessed. The successful observation was not persisted.

## Verification and reproducibility

A new temporary source directory was based on the previously verified 65-test core snapshot, with the reviewed OpenCode probe, provider declaration, example and pinned manifest/lock overlaid. This kept concurrent listener edits out of the check. The older core's subsequently discovered owner-drop shutdown issue is not exercised by this example: the example owns RuntimeState directly and does not start the scheduler.

Before execution, that isolated Linux snapshot passed:

- formatting
- all-target clippy with warnings denied
- all 76 tests, including the 11 OpenCode HTTP tests
- release build of the live-check example

The extra test over the earlier OpenCode implementation covers delayed response headers, relative Retry-After, a truncated response body and persistence across RuntimeState reopen. Independent scoped review returned FIX-OK before execution.

The verification log is `/var/folders/dq/_c83wftd34g281_1002c_gl00000gn/T/pi-jobs-qf1QZe/cd3aa289.log` on the development Mac. The temporary Linux source snapshot is `/tmp/quotabar-opencode-live.u4awrU`. Temporary build artifacts are not an installation.

## Remaining gates

- Finish and review the listener, enable/disable control and scheduler owner-drop repair.
- Measure idle CPU time, wakeups or explicitly labelled proxies, resident memory and post-activity behavior.
- Verify the final integrated source snapshot on both platforms.
- Complete the other provider adapters and preserve both installed Claude hooks.
- Resolve the documented Claude macOS no-dialog blocker and perform owner-present credential acceptance before Mac cutover.
