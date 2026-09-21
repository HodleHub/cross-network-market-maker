# CLI evidence

The PNG files in this directory render actual Rust command transcripts. They
are not desktop screenshots, generated illustrations, or simulated results.
Each image has a matching UTF-8 `.txt` transcript and `.json` metadata with the
command, execution time, exit status, source commit (when available), and
SHA-256 of the complete transcript. A cropped image identifies its displayed
line range; the text file always retains the full sanitized output.

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
