"""Run isolated, real-Codex feature contracts against synthetic loopback providers."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import subprocess

from . import adaptive, catalog, effort, forks, imports, memory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--native-catalog', type=Path)
    parser.add_argument('--suite', choices=['all', 'effort', 'adaptive', 'memory', 'imports', 'forks', 'catalog'], default='all')
    args = parser.parse_args()
    if args.suite in ('all', 'catalog') and args.native_catalog is None:
        parser.error('--native-catalog must be the pinned official models-manager/models.json')
    args.root.mkdir(parents=True, exist_ok=True)
    args.output.mkdir(parents=True, exist_ok=True)
    version_home = args.root/'version-home'
    version_home.mkdir()
    version = subprocess.check_output([str(args.codex), '--version'], text=True,
              env={'PATH':os.environ['PATH'], 'HOME':str(version_home), 'CODEX_HOME':str(version_home)}).strip()
    report = {'started_at':datetime.now(timezone.utc).isoformat(), 'codex_version':version,
              'source_ref':'rust-v0.165.0', 'real_provider_calls':0, 'cases':[], 'inference_requests':0,
              'binary_sha256':{str(path):hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()
                               for path in (args.codex, args.emp)}}
    cases = [('effort-sse', effort.run, {'transport':'sse'}),
             ('effort-websocket', effort.run, {'transport':'websocket'}),
             *[(f'adaptive-{transport}-{source}', adaptive.run, {'transport':transport,'source':source})
               for transport in ('sse','websocket') for source in ('native','imported')],
             *[('memory-'+source, memory.run, {'source':source}) for source in ('native', 'external', 'imported')],
             ('imports', imports.run, {}), ('forks', forks.run, {}),
             ('catalog', catalog.run, {'catalog':args.native_catalog})]
    for name, run, options in cases:
        if args.suite != 'all' and name.split('-')[0] != args.suite:
            continue
        result = run(args.codex, args.emp, args.root/name, args.output/name, **options)
        details = result if isinstance(result, list) else [result]
        count = sum(item.get('requests', item.get('model_requests', 0)) for item in details)
        report['cases'].append({'case':name, 'passed':True, 'checks':len(details), 'inference_requests':count})
        report['inference_requests'] += count
        report['updated_at'] = datetime.now(timezone.utc).isoformat()
        (args.output/'acceptance.json').write_text(json.dumps(report, indent=2))
    print(json.dumps(report), flush=True)


if __name__ == '__main__':
    main()
