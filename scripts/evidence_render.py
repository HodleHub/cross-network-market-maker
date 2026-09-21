#!/usr/bin/env python3
"""Render a captured CLI transcript verbatim as PNG, with source hash and status."""
import argparse
import hashlib
import json
from pathlib import Path
import textwrap
from PIL import Image, ImageDraw, ImageFont


FONT_CANDIDATES = [Path('/System/Library/Fonts/Menlo.ttc'),
                   Path('/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf')]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('metadata', type=Path)
    parser.add_argument('--tail', type=int, default=38)
    args = parser.parse_args()
    record = json.loads(args.metadata.read_text())
    source = args.metadata.parent / record['transcript']
    data = source.read_bytes()
    if hashlib.sha256(data).hexdigest() != record['transcript_sha256']:
        raise SystemExit('Transcript hash mismatch: refusing to render altered evidence')
    font_path = next((path for path in FONT_CANDIDATES if path.exists()), None)
    if font_path is None:
        raise SystemExit('Install a monospace font (Menlo or DejaVu Sans Mono)')
    font = ImageFont.truetype(str(font_path), 20)
    small = ImageFont.truetype(str(font_path), 16)
    title_font = ImageFont.truetype(str(font_path), 25)
    raw_lines = data.decode().splitlines()
    start = max(0, len(raw_lines) - args.tail)
    selected = raw_lines[start:]
    lines = []
    for line in selected:
        lines.extend(textwrap.wrap(line, width=106, replace_whitespace=False,
                                   drop_whitespace=False, break_on_hyphens=False) or [''])
    width = 1400
    height = 200 + len(lines) * 29
    picture = Image.new('RGB', (width, height), '#10151d')
    draw = ImageDraw.Draw(picture)
    draw.text((36, 25), record['title'], font=title_font, fill='#edf2f7')
    note = f"Actual CLI output | {record['started_at_utc']} | exit {record['exit_code']}"
    draw.text((36, 63), note, font=small, fill='#aebdcc')
    draw.text((36, 88), f'Rendered transcript: lines {start + 1}-{len(raw_lines)} of {len(raw_lines)}', font=small, fill='#aebdcc')
    draw.line((36, 121, width - 36, 121), fill='#334155')
    for index, line in enumerate(lines):
        color = '#d9e3ed'
        if line.startswith('$ '):
            color = '#6fcbef'
        if 'test result: ok.' in line or line == '[exit code: 0]':
            color = '#87d6ab'
        draw.text((36, 141 + index * 29), line, font=font, fill=color)
    draw.text((36, height - 37), f"SHA256 {record['transcript_sha256']}", font=small, fill='#8fa2b6')
    output = args.metadata.with_suffix('.png')
    picture.save(output)
    print(output)


if __name__ == '__main__':
    main()
