# CLI evidence

The PNG files in this directory render actual Rust command transcripts. They
are not desktop screenshots, generated illustrations, or simulated results.
Each image has a matching UTF-8 `.txt` transcript and `.json` metadata with the
command, execution time, exit status, source commit (when available), and
SHA-256 of the complete transcript. A cropped image identifies its displayed
line range; the text file always retains the full sanitized output.

## Recorded qualification

| Evidence | Result | Image and full output |
| --- | --- | --- |
| Protocol and recovery | 34 passed; 8 real-node tests intentionally ignored here | [PNG](protocol-tests.png), [transcript](protocol-tests.txt), [metadata](protocol-tests.json) |
| Real-node suite | 7 passed: Bitcoin/Elements primitives, two-maker CLI, LND, forward/reverse/replay and funded failure/refund | [PNG](regtest-swaps.png), [transcript](regtest-swaps.txt), [metadata](regtest-swaps.json) |
| Operator demo | Maker B selected; settled and replayed with identical transaction IDs | [PNG](market-maker-cli.png), [transcript](market-maker-cli.txt), [metadata](market-maker-cli.json) |
| Native Lightning exit | 1 passed: offline peer, exact HTLC timeout, ordinary and HTLC CSV sweeps | [PNG](native-exit.png), [transcript](native-exit.txt), [metadata](native-exit.json) |

The eight ignored entries in the normal test run are seven regular live tests
and one native-exit test. They are not counted among the thirty-four passing checks.
The real-node suite runs those seven regular tests explicitly, with none ignored.
The separate native-exit run passes the eighth real-node test, also with none
ignored. Its 23,456-satoshi HTLC produces a final owned output of 23,321 satoshis
after the 144-block CSV delay. Conservative net recovery is 22,222 satoshis:
the accounting charges the full 1,099-satoshi timeout transaction fee and the
135-satoshi final sweep fee to the principal, including externally sponsored
timeout fees. The separately proven ordinary channel output is not counted as
HTLC recovery.

All four captures refer to source commit `7598d0e415d185c6f2bb81b17de30a5257658006`.
The following publication commit adds documentation and these captured artifacts;
it does not change the qualified Rust implementation.

Metadata also records whether the worktree was dirty at capture time. A commit
identifier with `source_worktree_dirty: true` is a reference point, not a claim
that the executed source exactly matched that commit. Evidence files themselves
can make a subsequent capture dirty; inspect the corresponding Git diff.

Capture and render a command after building the Rust binary:

```sh
python3 scripts/evidence_capture.py --name routes --title 'Supported routes' -- target/debug/xmm routes
python3 scripts/evidence_render.py evidence/routes.json
```

The renderer requires Python 3, Pillow, and Menlo (macOS) or DejaVu Sans Mono
(Linux). These dependencies are only needed to regenerate images, not to run
the Rust market maker. The checked-in images can be viewed directly.

The capture script records stdout and stderr without collecting environment
variables. It rejects output that appears to expose cryptographic secrets.
The renderer verifies the transcript hash before producing the PNG. Before
publication, review every transcript for secrets, private configuration, and
unsupported claims; the automated check is not a substitute for that review.
