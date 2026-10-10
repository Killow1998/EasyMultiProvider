"""Read-only chart projections of EMP's existing reconciled accounting view."""
import math
import sqlite3
import time
from pathlib import Path

FIELDS = ('input_tokens', 'output_tokens', 'cached_input_tokens', 'cache_write_tokens', 'reasoning_tokens')


def parameters(query):
    start, end = float(query.get('start', ['0'])[0]), float(query.get('end', ['0'])[0])
    category = query.get('category', ['all'])[0] or 'all'
    if not math.isfinite(start) or not math.isfinite(end) or start < 0 or end <= start:
        raise ValueError('Invalid statistics period')
    if category not in {'all', 'native', 'subscription', 'external', 'unknown'}:
        raise ValueError('Invalid statistics category')
    result = {'start': start, 'end': end, 'category': category}
    for key in ['account', 'provider', 'model', 'session', 'state']:
        value = query.get(key, [''])[0]
        if len(value) > 256 or any(ord(c) < 32 for c in value):
            raise ValueError('Invalid statistics filter')
        if value:
            result[key] = value
    return result


def aggregate(path: Path, filters, metadata):
    start, end = filters['start'], filters['end']
    bucket = max(60, (end-start)/48)
    predicate, args = ['u.observed_at >= ?', 'u.observed_at < ?'], [start, end]
    if filters['category'] != 'all':
        predicate.append('u.category = ?')
        args.append(filters['category'])
    if filters.get('provider'):
        predicate.extend(["u.category = 'external'", 'u.owner = ?'])
        args.append(filters['provider'])
    if filters.get('account'):
        # The existing account-scoped API resolves the identity, without reading credentials here.
        owners = sorted({row['owner'] for row in metadata.get('groups', [])})
        predicate.append('u.owner IN ('+','.join('?' for _ in owners)+')' if owners else '0')
        args.extend(owners)
    if filters.get('model'):
        predicate.append('(u.model = ? OR u.route_model = ?)')
        args.extend([filters['model']]*2)
    if filters.get('session') or filters.get('state'):
        call_predicate = ["json_extract(c.facts,'$.usage_event_id') = u.id"]
        if filters.get('session'):
            call_predicate.append("(c.session_id = ? OR json_extract(c.facts,'$.thread_id') = ?)")
            args.extend([filters['session']]*2)
        if filters.get('state'):
            call_predicate.append('c.state = ?')
            args.append(filters['state'])
        projection = (Path(__file__).resolve().parents[2]/'contracts/call-outcomes.sql').read_text().strip()
        call_predicate = [clause.replace('c.', 'call_records.') for clause in call_predicate]
        predicate.append('EXISTS(SELECT 1 FROM '+projection+' WHERE '+' AND '.join(call_predicate)+')')
    sums = ','.join(f'SUM(COALESCE(u.{field},0)) AS {field}' for field in FIELDS)
    sql = f"""SELECT CAST((u.observed_at-?)/? AS INTEGER) AS bucket,
        u.category,u.owner,u.model,{sums},COUNT(*) AS requests,
        SUM(u.cost_nanos IS NOT NULL) AS priced_requests,
        SUM(COALESCE(u.cost_nanos,0)) AS cost_nanos
        FROM accounted_usage u WHERE {' AND '.join(predicate)}
        GROUP BY bucket,u.category,u.owner,u.model ORDER BY bucket,u.category,u.owner,u.model"""
    # No schema migration, writes, history scan or repricing. Read the live WAL as well.
    connection = sqlite3.connect(path.as_uri()+'?mode=ro', uri=True, timeout=3)
    connection.row_factory = sqlite3.Row
    deadline = time.monotonic()+8
    connection.set_progress_handler(lambda: int(time.monotonic() > deadline), 10000)
    try:
        connection.execute('PRAGMA query_only=ON')
        rows = [dict(row) for row in connection.execute(sql, [start, bucket, *args])]
    finally:
        connection.close()
    names = {(row['category'], row['owner']): row.get('owner_name', '') for row in metadata.get('groups', [])}
    groups, periods = {}, {}
    numeric = (*FIELDS, 'requests', 'priced_requests', 'cost_nanos')
    totals = {key: 0 for key in numeric}
    for row in rows:
        row['start'] = start+row.pop('bucket')*bucket
        row['owner_name'] = names.get((row['category'], row['owner']), '')
        key = (row['category'], row['owner'], row['model'])
        group = groups.setdefault(key, {k: row[k] for k in ['category', 'owner', 'owner_name', 'model']} | {k: 0 for k in numeric})
        period = periods.setdefault(row['start'], {'start': row['start']} | {k: 0 for k in numeric})
        for field in numeric:
            row[field] = row[field] or 0
            group[field] += row[field]
            period[field] += row[field]
            totals[field] += row[field]
    return {'start': start, 'end': end, 'bucket': bucket, 'totals': totals,
            'groups': list(groups.values()), 'periods': list(periods.values()), 'series': rows,
            'pricing': metadata.get('pricing', {}), 'history': metadata.get('history', {})}
