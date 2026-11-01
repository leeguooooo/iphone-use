#!/usr/bin/env python3
"""Check the external pixel verifier with known PNG bytes, a wrong nonce and wrong dimensions."""
import argparse
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import zlib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verifier', type=Path, required=True)
    args = parser.parse_args()
    verifier = args.verifier.resolve(strict=True)
    width, height, scale, cell = 1179, 2556, 3, 20
    marker = '01234567-89AB-4CDE-8F01-23456789ABCD'
    bits = ''.join(f'{int(c, 16):04b}' for c in marker.replace('-', ''))
    pixels = bytearray(b'\xff' * width * height * 3)
    for index, bit in enumerate(bits):
        x = (20 + index % 16 * cell + 1) * scale
        y = (200 + index // 16 * cell + 1) * scale
        row = (b'\x00\x00\xff' if bit == '1' else b'\xff\x00\x00') * ((cell - 2) * scale)
        for offset in range((cell - 2) * scale):
            start = ((y + offset) * width + x) * 3
            pixels[start:start + len(row)] = row
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    def png(flipped=False):
        order = reversed(range(height)) if flipped else range(height)
        rows = b''.join(b'\x00' + pixels[y * width * 3:(y + 1) * width * 3] for y in order)
        return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>II5B', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b'')
    ready = {'run_id': marker, 'screen_width': width, 'screen_height': height,
             'screen_points_width': 393, 'screen_points_height': 852, 'cell_points': cell, 'grid_x': 20, 'grid_y': 200}
    cases = [('matching_marker', ready, 'reference.png', True),
             ('wrong_marker', dict(ready, run_id='F' + marker[1:]), 'reference.png', False),
             ('wrong_dimensions', dict(ready, screen_width=width - 1), 'reference.png', False),
             ('upside_down_image', ready, 'upside-down.png', False)]
    results = {}
    with tempfile.TemporaryDirectory(prefix='ipu-capture-verifier-') as directory:
        root = Path(directory)
        (root / 'reference.png').write_bytes(png())
        (root / 'upside-down.png').write_bytes(png(flipped=True))
        for name, value, image_name, expected in cases:
            path = root / (name + '.json')
            path.write_text(json.dumps(value))
            result = json.loads(subprocess.check_output([str(verifier), str(root / image_name), str(path)], text=True))
            assert result['verified'] is expected, f'{name}: incorrect verdict'
            results[name] = {'passed': True, 'verified': result['verified'], 'matched_cells': result['matched_cells']}
    print(json.dumps(results))


if __name__ == '__main__':
    main()
