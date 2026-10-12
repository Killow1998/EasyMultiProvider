"""Pure service/settings projection and redaction; no files, processes or cache."""
import copy

class PreviewError(ValueError):
    pass


SECRET_KEYS = {'imported_auth', 'auth', 'provider_keys', 'api_key', 'api_key_file', 'auth_file', 'auth_json', 'access_token',
               'refresh_token', 'id_token', 'password', 'authorization', 'cookie'}


def scrub(value):
    if isinstance(value, list):
        return [scrub(item) for item in value]
    if isinstance(value, dict):
        return {key: scrub(item) for key, item in value.items() if key.lower() not in SECRET_KEYS}
    return value


def select_payload(source, selections, context=True, rename=True):
    """Project selected services; existing encrypted bundle/schema stays unchanged."""
    payload = copy.deepcopy(source)
    config = payload.get('config', {})
    account_choices = {s['id']: s for s in selections if s['kind'] == 'account'}
    provider_choices = {s['id']: s for s in selections if s['kind'] == 'provider'}
    native = account_choices.pop('@native', None)
    native_id = None
    if native:
        # Production export appends the native-login account after subscriptions.
        for entry in reversed(payload.get('accounts', [])):
            if entry.get('metadata', {}).get('name') == 'Native login':
                account_choices[entry['metadata']['id']] = native
                native_id = entry['metadata']['id']
                break
    accounts = []
    for entry in payload.get('accounts', []):
        meta = entry.get('metadata', {})
        selected = account_choices.get(meta.get('id'))
        if not selected:
            continue
        meta['name'] = selected.get('name') or meta.get('name')
        choices = selected.get('models', [])
        if choices:
            meta['hidden_models'] = [m['id'] for m in choices if not m.get('selected', True)]
        if context:
            windows = meta.setdefault('model_context_windows', {})
            for model in choices:
                value = model.get('context_window')
                if value:
                    number = int(value)
                    if number <= 0 or number > 1000000000: raise PreviewError('Invalid context window')
                    windows[model['id']] = number
                elif 'context_window' in model:
                    windows.pop(model['id'],None)
        else:
            meta.pop('model_context_windows', None)
        accounts.append(entry)
    payload['accounts'] = accounts
    previews = {}
    for entry in accounts:
        selected = account_choices[entry['metadata']['id']]
        models = copy.deepcopy(selected.get('models', []))
        for model in models:
            if not context: model.pop('context_window',None)
            if not rename: model['name'] = model.get('original_name') or model['id']
        previews[entry['metadata']['id']] = models
    payload['model_previews'] = previews
    for entry in accounts: entry['metadata'].pop('auth_file',None)
    config['accounts'] = [entry['metadata'] for entry in accounts]
    providers = []
    for provider in config.get('providers', []):
        selected = provider_choices.get(provider.get('id'))
        if selected:
            provider.pop('api_key_file',None);provider.pop('api_key',None)
            provider['name'] = selected.get('name') or provider.get('name') or provider['id']
            providers.append(provider)
    config['providers'] = providers
    allowed = {p['id'] for p in providers}
    model_choices = {m['id']: m for choice in provider_choices.values() for m in choice.get('models', [])}
    config['models'] = [m for m in config.get('models', []) if m.get('provider') in allowed and model_choices.get(m['id'], {}).get('selected', True)]
    for model in config['models']:
        selected = model_choices.get(model['id'], {})
        if selected.get('name'):
            model['display_name'] = selected['name']
        if selected.get('context_window'):
            model['context_window'] = int(selected['context_window'])
    payload['provider_keys'] = {key: value for key, value in payload.get('provider_keys', {}).items() if key in allowed}
    if not context:
        config.pop('native_model_context_windows', None)
    prefixes = [a.get('prefix', '')+'/' for a in config['accounts']]
    native_routes = {m['id'] for m in (native or {}).get('models', [])}
    model_routes = {m['id'] for m in config['models']}
    families = {m.get('family_id') or m.get('upstream_id') or m['id'] for m in config['models']}
    families.update(m['id'] for choice in account_choices.values() for m in choice.get('models', []))
    config['catalog_presentations'] = {route:value for route,value in config.get('catalog_presentations', {}).items()
        if route in native_routes or route in model_routes or any(route.startswith(prefix) for prefix in prefixes)}
    config['catalog_family_presentations'] = {family:value for family,value in config.get('catalog_family_presentations', {}).items() if family in families}
    for field in ['catalog_presentations', 'catalog_family_presentations']:
        presentations = config.get(field, {})
        if not rename:
            for route, value in list(presentations.items()):
                if route.startswith('gpt-') or any(route.startswith(prefix) for prefix in prefixes):
                    if isinstance(value, dict):
                        value.pop('catalog_alias', None)
    if rename:
        presentations = config.setdefault('catalog_presentations', {})
        for entry in accounts:
            meta = entry['metadata']
            selected = account_choices[meta['id']]
            for model in selected.get('models', []):
                if model.get('name'):
                    route = meta.get('prefix', meta['id'])+'/'+model['id']
                    presentations.setdefault(route, {})['catalog_alias'] = model['name']
    if native_id and native.get('export_id'):
        target = native['export_id']
        reserved = {record.get(field) for record in source.get('config', {}).get('accounts', [])
                    + source.get('config', {}).get('providers', []) for field in ['id','prefix']}
        if not isinstance(target,str) or len(target) != 4 or not target.isascii() or not target.isalnum() or target in reserved:
            raise PreviewError('Native 导出 ID 必须是未占用的四位字母或数字 / Native export ID must be four unused letters or digits')
        meta = next(entry['metadata'] for entry in accounts if entry['metadata']['id'] == native_id)
        original_prefix = meta['prefix']
        meta.update(id=target,prefix=target)
        payload['model_previews'][target] = payload['model_previews'].pop(native_id, [])
        config['catalog_presentations'] = {
            (target+'/'+route.split('/',1)[1] if route.startswith(original_prefix+'/') else
             target+'/'+route if route in native_routes else route):value
            for route,value in config.get('catalog_presentations', {}).items()}
    return payload



def combine_payloads(sources):
    """Keep one selected record per identity; never mutate source packages."""
    result = {'schema':'easy-multi-provider-migration','version':1,
              'config':{},'accounts':[],'provider_keys':{},'model_previews':{}}
    for field in ['accounts','providers','models']:
        by_id = {record['id']:copy.deepcopy(record)
                 for source in sources for record in source.get('config', {}).get(field, [])}
        result['config'][field] = list(by_id.values())
    accounts = {record['metadata']['id']:copy.deepcopy(record)
                for source in sources for record in source.get('accounts', [])}
    result['accounts'] = list(accounts.values())
    for source in sources:
        for field in ['catalog_presentations','catalog_family_presentations']:
            result['config'].setdefault(field, {}).update(copy.deepcopy(source.get('config', {}).get(field, {})))
        for field in ['model_previews','provider_keys']:
            result[field].update(copy.deepcopy(source.get(field, {})))
    return result


def import_notices(current, config, accounts, providers):
    """Explain per-service actions without credentials or persistence."""
    current_records = current.get('accounts', []) + current.get('providers', [])
    names = {}
    for kind, field in [('account','accounts'),('provider','providers')]:
        for record in config.get(field, []):
            names.setdefault(record.get('name') or record['id'], []).append((kind,record['id']))
    warnings = []
    for kind, field in [('account','accounts'),('provider','providers')]:
        for record in config.get(field, []):
            selected = record['id'] in accounts if kind == 'account' else record['id'] in providers
            if not selected:
                continue
            source_id = record.get('_shadow_source_id',record['id'])
            if source_id != record['id'] or any(old['id'] == source_id or old.get('prefix') == source_id for old in current_records):
                target_exists = any(old['id'] == record['id'] for old in current_records)
                if kind == 'provider' and not target_exists:
                    zh = '连接地址或 API key 不同／无法确认相同；作为独立服务导入，原服务保留。'
                    en = 'Connection URL or API key differs or cannot be verified. Import as a separate service; the original remains.'
                elif kind == 'provider':
                    zh = '连接方式、base URL 和 API key 已确认一致；合并更新已有服务。相同上游模型更新，其他模型保留。'
                    en = 'Verified matching connection method, base URL and API key; update the existing service. Matching upstream models update; other models remain.'
                elif source_id != record['id']:
                    zh = ('已确认同一账号，合并更新已有服务。') if target_exists else '账号身份不同／无法确认相同；作为独立服务导入，原账号保留。'
                    en = ('Verified matching account; update the existing service.') if target_exists else 'Different or unverifiable account; import separately and retain the original.'
                elif kind == 'account' and record.get('prefix',record['id']) != source_id:
                    zh = '内部引用发生冲突，已自动调整；服务可正常导入。'
                    en = 'Conflicting internal reference adjusted automatically; import can continue.'
                else:
                    zh = '已确认同一账号，将更新已有服务；可正常导入。'
                    en = 'Verified matching account; update the existing service. Import can continue.'
                warnings.append({'kind':kind,'id':record['id'],'message':zh,'message_en':en})
            name = record.get('name') or record['id']
            peers = [peer_id for peer_kind, peer_id in names.get(name, [])
                     if (peer_kind,peer_id) != (kind,record['id'])]
            if peers:
                warnings.append({'kind':kind,'id':record['id'],'code':'duplicate_name',
                    'message':'另有服务使用相同名称。名称相同不影响导入，也不会因此合并；可在右侧预览中直接修改名称。',
                    'message_en':'Another service uses the same name. Matching names neither block import nor cause a merge; edit the name directly in the preview on the right.'})
    return warnings
