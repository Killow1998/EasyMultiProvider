"""Verify a legacy usage snapshot with frozen, local-only Codex history copies."""
import argparse
import contextlib
import hashlib
import json
import sqlite3
import tempfile
import time
from pathlib import Path
from urllib.parse import urlencode

from e2e_environment import request, start_backend

FIELDS=['input_tokens','output_tokens','cached_input_tokens','cache_write_tokens',
        'cache_write_1h_tokens','reasoning_tokens','cost_nanos']


def connect(path):
    return sqlite3.connect(path.resolve().as_uri()+'?mode=ro',uri=True,timeout=10)


def fingerprint(path):
    digest=hashlib.sha256()
    with connect(path) as database:
        for row in database.execute('SELECT * FROM usage_events ORDER BY id'):
            digest.update(json.dumps(row,separators=(',',':')).encode())
            digest.update(b'\n')
        totals=dict(zip(['requests',*FIELDS],database.execute(
            'SELECT COUNT(*),'+','.join('COALESCE(SUM('+key+'),0)' for key in FIELDS)+
            ' FROM accounted_usage').fetchone()))
        events=database.execute('SELECT COUNT(*) FROM usage_events').fetchone()[0]
    return {'events':events,'sha256':digest.hexdigest(),'totals':totals}


def freeze_history(database, source_home, target_home):
    """Copy only checkpointed bytes; never use hardlinks to active rollouts."""
    stats={'copied':0,'bytes':0,'missing':0,'outside_home':0,'changed':0}
    checkpoints=database.execute('SELECT path,checkpoint FROM usage_files').fetchall()
    saved=[]
    for name,encoded in checkpoints:
        source=Path(name)
        if not source.is_relative_to(source_home) or source.is_symlink():
            stats['outside_home']+=1
            continue
        if not source.is_file():
            stats['missing']+=1
            continue
        checkpoint=json.loads(encoded)
        offset=checkpoint['offset']
        if source.stat().st_size<offset:
            stats['changed']+=1
            continue
        target=target_home/source.relative_to(source_home)
        target.parent.mkdir(parents=True,exist_ok=True)
        with source.open('rb') as original:
            original.seek(max(0,offset-256))
            tail=original.read(min(256,offset))
            if hashlib.sha256(tail).hexdigest()!=checkpoint['tail']:
                stats['changed']+=1
                continue
            original.seek(0)
            with target.open('wb') as copy:
                remaining=offset
                while remaining:
                    block=original.read(min(1024*1024,remaining))
                    assert block,'Source shrank while its frozen copy was read'
                    copy.write(block)
                    remaining-=len(block)
        with target.open('rb') as copy:
            copy.seek(max(0,offset-256))
            assert hashlib.sha256(copy.read()).hexdigest()==checkpoint['tail']
        checkpoint.pop('session_usage_version',None)
        saved.append((str(target),json.dumps(checkpoint)))
        stats['copied']+=1
        stats['bytes']+=offset
    assert saved,'No unchanged checkpointed history was available'
    database.execute('DELETE FROM usage_files')
    database.executemany('INSERT INTO usage_files VALUES (?,?)',saved)
    return stats


def wait_scan(base,headers,*,command=None):
    started=time.monotonic()
    health=[]
    while time.monotonic()-started<600:
        status,report=request(base+'/api/usage?series=true&start=0&end=1',headers=headers,timeout=20)
        assert status==200,'History status is unavailable'
        history=report['history']
        before=time.monotonic()
        status,_=request(base+'/healthz',timeout=10)
        health.append(time.monotonic()-before)
        assert status==200,'History indexing blocked service health'
        if history.get('last_scan_at') and not history['running'] and not history['queued']:
            if command is None or history['completed']>=command:
                assert history['errors']==0,history
                return {'seconds':time.monotonic()-started,'health_max_seconds':max(health),
                        'health_samples':len(health),'history':history}
        time.sleep(.25)
    raise TimeoutError('Frozen history scan did not finish')


def read_bills(base,headers,before):
    query={'sessions':'true','start':0,'end':time.time()+3600,'limit':100}
    totals={'requests':0,**dict.fromkeys(FIELDS,0)}
    durations=[]
    offset=0
    first=None
    while True:
        started=time.monotonic()
        status,page=request(base+'/api/usage?'+urlencode({**query,'offset':offset}),headers=headers,timeout=60)
        durations.append(time.monotonic()-started)
        assert status==200,'Session bills are unavailable'
        if first is None:
            first=page
            for key,value in before['totals'].items():
                assert page['totals'][key]==value,('bill total changed',key)
            if page['unassigned']:
                for key in totals:
                    totals[key]+=page['unassigned'][key]
        for row in page['records']:
            for key in totals:
                totals[key]+=row[key]
        offset+=len(page['records'])
        if offset>=page['total']:
            break
        assert page['records'],'Paging stopped before all sessions were read'
    assert totals==before['totals'],'Session pages and unassigned usage do not reconcile'
    assert first['records'],'No session attribution was recovered'
    row=first['records'][0]
    started=time.monotonic()
    status,detail=request(base+'/api/usage?'+urlencode({'series':'true',
        'session':row['session_id'],'start':0,'end':query['end']}),headers=headers,timeout=60)
    detail_seconds=time.monotonic()-started
    assert status==200
    for key in totals:
        assert detail['totals'][key]==row[key],('model detail mismatch',key)
    return {'sessions':first['total'],'unassigned_requests':(first['unassigned'] or {}).get('requests',0),
            'pages':len(durations),'page_seconds':durations,'detail_seconds':detail_seconds}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--ledger',type=Path,required=True)
    parser.add_argument('--codex-home',type=Path,required=True)
    parser.add_argument('--scratch-dir',type=Path,required=True,help='Disk-backed scratch, not a RAM-backed /tmp')
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    started=time.monotonic()
    with tempfile.TemporaryDirectory(prefix='.emp-bill-upgrade-',dir=args.scratch_dir) as temporary:
        root=Path(temporary)
        state=root/'state';state.mkdir()
        copied_ledger=state/'usage.sqlite3'
        with connect(args.ledger) as source,sqlite3.connect(copied_ledger) as target:
            source.backup(target)
        before=fingerprint(copied_ledger)
        frozen_home=root/'frozen_codex';frozen_home.mkdir()
        with sqlite3.connect(copied_ledger) as database:
            assert not database.execute("SELECT 1 FROM sqlite_master WHERE name='usage_sessions'").fetchone(), 'Expected a pre-session-bill ledger'
            stats=freeze_history(database,args.codex_home.resolve(),frozen_home)
        assert fingerprint(copied_ledger)==before,'Fixture preparation changed accounting'
        # No credentials or live configuration are copied. Empty future-dated
        # rates keep price refresh separate from the session-index migration.
        (state/'api_prices.json').write_text(json.dumps({'fetched_at':time.time()+86400,'prices':{}}))
        snapshot_seconds=time.monotonic()-started
        print(json.dumps({'stage':'snapshot','events':before['events'],'history':stats,
                          'seconds':snapshot_seconds}),flush=True)
        with contextlib.ExitStack() as stack:
            started=time.monotonic()
            base,_,_,headers,upstream=start_backend(stack,args.emp.resolve(),root,
                {'codex_base_url':'http://127.0.0.1:9/fixture-only'},
                {'CODEX_HOME':str(frozen_home),'PATH':'/usr/bin:/bin',
                 'HTTP_PROXY':'http://127.0.0.1:9','HTTPS_PROXY':'http://127.0.0.1:9',
                 'ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'127.0.0.1,localhost'})
            headers['Origin']=base
            initial=wait_scan(base,headers)
            initial['startup_and_index_seconds']=time.monotonic()-started
            after=fingerprint(copied_ledger)
            assert after==before,'Upgrade changed an existing usage event or frozen price'
            bills=read_bills(base,headers,before)
            status,command=request(base+'/api/usage/scan',{},headers)
            assert status==202
            incremental=wait_scan(base,headers,command=command['command_id'])
            assert incremental['history']['updated']==0,'Unchanged history was replayed again'
            assert fingerprint(copied_ledger)==before,'Incremental scan changed historical accounting'
            with connect(copied_ledger) as database:
                links=database.execute('SELECT COUNT(*) FROM usage_sessions').fetchone()[0]
                indexed=database.execute("SELECT COUNT(*) FROM usage_files WHERE json_extract(checkpoint,'$.session_usage_version')=1").fetchone()[0]
            assert indexed==stats['copied'] and links>0
            assert upstream.calls==0
    receipt={'passed':True,'source':'read-only local legacy ledger and checkpointed rollout copies',
        'history':stats,'snapshot_seconds':snapshot_seconds,'accounting':before,
        'all_existing_events_and_prices_unchanged':True,'initial':initial,'bills':bills,
        'incremental':incremental,'session_links':links,'scratch_removed':not root.exists(),
        'credentials_copied':False,'real_model_calls':0}
    args.output.write_text(json.dumps(receipt,indent=2)+'\n')
    print(json.dumps(receipt),flush=True)


if __name__=='__main__':
    main()
