# Prepared consumer artifacts

These files are stored for review. They are not installed or active. Consumer cutover is not approved.

The Pi patch and shell wrapper remain inactive. The staged public shell command is `aiquota`; Pi's existing `/quota` command and `get_quotas` tool are unchanged. Apply the Pi patch only after the owner accepts the cutover. Nothing here applies patches, installs files, starts services, or changes an installed consumer.

## Pi extension patch

`pi-extensions.patch` contains only the eight approved Pi source diffs. It excludes all three shell bundle additions from the private diff. Its headers use repository-relative `a/` and `b/` paths. A scratch-tree `git apply --check` and apply round trip matched all eight approved staged source hashes. The actual Pi repository was not modified.

The source baseline was `/tmp/quotabar-consumers-stage.1zwKv7/source`, described by `original-hashes.sha256` (SHA-256 `7c4e93b6cc12a82b0f015fffefef3ba1ecfd8a10e7ae84e6b6010794f7ae4861`). Before packaging, each selected original file in `~/work/pi-extensions` matched that manifest. Each applied result matched the approved private stage:

| Pi path | Original SHA-256 | Approved staged SHA-256 |
|---|---|---|
| `README.md` | `eaa5079e27501860a2de699ab1429122c1a6b12f6316d0c11a7f61ac5be924f8` | `3a200d896708787aceade301df9976d4ab3152c84f37563f988b634c16922f01` |
| `extensions/status/README.md` | `85bc1dfed7a72cc10d97b9b852785af2dd675a8cbfff5a581a585ded78d26df5` | `102d20767c97ee39cc70eb4974506bad2747e3c2c8dc9f4743477e5d4b90af51` |
| `extensions/status/footer.ts` | `38986fbe4aa2790510fe4e64c6baada66c9475f94df202f852b7b02153502d28` | `271cf1cd004657d4764e9285de342d74f2f09a767acb9180372671e0131aa540` |
| `extensions/status/footer.test.mjs` | `d6293fc260e1b7292af4b536313da84e0e63aba8690e3a5a50951b8f5203883c` | `70813a66ed86f2096c7864a4a355666f20005c742c09616d2245613bbeb0d499` |
| `extensions/status/quota.ts` | `9a8805ebbc35c58eed036eb1ee27c6b4eb2efecb6bbe1bd802882e48fc502be6` | `6e69cfb8f684d39cad18cf2646bfa618e1f505450850659ceb4b53585682ed9d` |
| `extensions/status/quota.test.mjs` | `0ad591da78f340144334ed9c56fe5be8d0dbaac7e2ee0b5eac3cfbd8ec6918a8` | `821b075797bfcd5224bb8c96a28f8167e1147561bf709433ced0186a4e0cec65` |
| `extensions/status/session.test.mjs` | `c8042ba77cbeccfd785b4fccb37a1bc17082de2a31fecf021963ab909c08506b` | `c6f9b366d4b0d6e31e80465da6dbdaa82f21d917fcb499eee62de5b3a4ad4d4a` |
| `tests/fixtures/status/index.ts` | `607d2620194f100308b3d8630e7aa6ef2ea5b640d2293dc5f8c1b65ecc2d2462` | `e5f21bcd626c07ba701638584fc51664792852b397b92896dc6c49926fbe9b0b` |

The reviewed private diff SHA-256 is `2909f48821781579c3e6adf990d11bbcce1ec8e4094f1927f55f3068fae584a2`. The selected patch SHA-256 is recorded in `MANIFEST.sha256`.

## Shell wrapper

`shell/` holds the staged wrapper, its tests and README. The `aiquota` wrapper retains its executable mode. Its public name does not change the underlying `quotabard quota` dispatch. The wrapper runs only when a person invokes it; this package does not install it or run it automatically.

The private-stage review at `/tmp/quotabar-consumer-stage-fix-review.md` approves the F1–F5 repair. The earlier review is `/tmp/quotabar-consumer-stage-review.md`. Supplied review evidence records 574 package tests, 37 focused tests, five wrapper tests and a passing `bash -n` check. Packaging reran the wrapper's five Node tests and `bash -n` only.

## QuotaBar baseline

The frozen Rust source baseline is `/tmp/quotabar-hooks-base.nd4syJ`, with a 37-file source manifest SHA-256 of `113ddb5dd4046647685889fd36b8eacffd7eebf7679f5893fe287865f0313c97`. The native macOS and Linux 243-test gates passed before packaging. This packaging task made no Rust source changes and did not rerun those gates.

`MANIFEST.sha256` lists the hashes of this README and all four packaged artifacts. It does not include itself.
