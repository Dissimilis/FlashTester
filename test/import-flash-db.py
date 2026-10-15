"""Regenerate the embedded table from a usbflashinfo research pack (no device I/O)."""
import hashlib
import json
from pathlib import Path
import sys

source = Path(sys.argv[1])
destination = Path(__file__).resolve().parents[1] / 'src' / 'flash-data'
destination.mkdir(exist_ok=True)
rows = json.loads((source / 'data/flash-database.json').read_text())
assert len(rows) == 1457
for i, row in enumerate(rows):
    assert row['index'] == i
    assert int(row['id_word_hex'], 16).to_bytes(4, 'little').hex().upper() == row['id_bytes_hex']
    assert int(row['extended_id_word_hex'], 16).to_bytes(2, 'little').hex().upper() == row['extended_id_bytes_hex']
(destination / 'nand.csv').write_text('\n'.join(r['raw_record'] for r in rows) + '\n', encoding='utf-8')
prefixes = json.loads((source / 'data/nand-prefix-whitelist.json').read_text())
(destination / 'prefixes.txt').write_text('\n'.join(r['bytes_hex'] for r in prefixes) + '\n', encoding='utf-8')
manifest = json.loads((source / 'evidence/artifact-manifest.json').read_text())
provenance = dict(analysis_date=manifest['analysis_date'], source_sha256=next(r['sha256'] for r in manifest['sources'] if r['path'].endswith('gfienc.dll')),
                 records=len(rows), source_table_sha256=hashlib.sha256((source / 'data/flash-database.json').read_bytes()).hexdigest())
(destination / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n', encoding='utf-8')
profiles = []
for p in json.loads((source / 'data/controller-profiles.json').read_text()):
    offsets = {a['offset']: a['captured_value'] for a in p['assignments']}
    if p['function'] == '0x4641b0':
        # SanDisk's consumer 0x45fc28 reads product/serial only, bypassing the
        # shared validator. These zeros are unused, unlike SSS's PID at zero.
        for field in ['0x30', '0x32', '0x3a', '0x3e']:
            offsets[field] = None
    profiles.append(dict(key=p['function'], family=p['family'], vid=offsets.get('0x30'), pid=offsets.get('0x32'),
                         big_endian=bool(offsets.get('0x34')), vendor=offsets.get('0x3a'), product=offsets.get('0x3c'), revision=offsets.get('0x3e')))
(destination / 'profiles.json').write_text(json.dumps(profiles, indent=2) + '\n', encoding='utf-8')
