#!/usr/bin/env python3
"""Record a real command and its output without collecting the environment."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--name', required=True)
    parser.add_argument('--title', required=True)
    parser.add_argument('--directory', default='evidence')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or not re.fullmatch(r'[a-z0-9][a-z0-9-]*', args.name):
        parser.error('Use a safe lowercase evidence name and a command after --')
    revision = subprocess.run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True, check=False)
    source_commit = revision.stdout.strip() if revision.returncode == 0 else None
    status = subprocess.run(['git', 'status', '--porcelain'], capture_output=True, text=True, check=False)
    source_worktree_dirty = bool(status.stdout.strip()) if status.returncode == 0 else None
    start = datetime.datetime.now(datetime.timezone.utc).isoformat()
    timer = time.monotonic()
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=False)
    output = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', result.stdout)
    sensitive = r'(?i)(?:private[_ -]?key|preimage|mnemonic|macaroon|wallet[_ -]?password)[\"\s]*[:=][\"\s]*([0-9a-f]{32,}|[a-z]+(?: [a-z]+){11,})'
    display_command = shlex.join(command)
    if re.search(sensitive, output) or re.search(sensitive, display_command):
        raise SystemExit('Refusing to publish output containing potentially secret material')
    transcript = f'$ {display_command}\n{output}\n[exit code: {result.returncode}]\n'
    directory = Path(args.directory)
    directory.mkdir(parents=True, exist_ok=True)
    source = directory / f'{args.name}.txt'
    source.write_text(transcript)
    os.chmod(source, 0o644)
    digest = hashlib.sha256(transcript.encode()).hexdigest()
    record = {'schema_version': 1, 'title': args.title, 'command': command,
              'source_commit': source_commit, 'source_worktree_dirty': source_worktree_dirty,
              'started_at_utc': start, 'duration_seconds': round(time.monotonic() - timer, 3),
              'exit_code': result.returncode, 'transcript': source.name,
              'transcript_sha256': digest, 'image_kind': 'Rendered actual CLI transcript; not a desktop screenshot'}
    (directory / f'{args.name}.json').write_text(json.dumps(record, indent=2) + '\n')
    print(transcript, end='')
    print(f'Evidence: {source} (SHA256 {digest})')
    raise SystemExit(result.returncode)


if __name__ == '__main__':
    main()
