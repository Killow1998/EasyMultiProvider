"""Shadow-local migration codec and read-only export snapshots.

No import/configuration command is sent to production. Secrets stay in this
process until the selected encrypted download is produced; previews are scrubbed.
"""
import copy
import json
import secrets
import subprocess
import threading
import time

from migration_selection import PreviewError, scrub, select_payload, combine_payloads, import_notices

MAX_BYTES = 32 * 1024 * 1024


class MigrationPreview:
    def __init__(self, helper, config_path, native_auth_path):
        self.helper = helper
        self.config_path = config_path
        self.native_auth_path = native_auth_path
        self.gate = threading.BoundedSemaphore(1)
        self.packages = {}

    def codec(self, request):
        if not self.helper or not self.helper.is_file():
            raise PreviewError('请先构建 shadow_migration_codec；参见影子页 README。 / Build shadow_migration_codec first; see the shadow README.')
        result = subprocess.run([str(self.helper)], input=json.dumps(request).encode(),
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=90)
        if result.returncode or len(result.stdout) > MAX_BYTES * 2:
            raise PreviewError('迁移包处理失败 / Migration codec failed')
        response = json.loads(result.stdout)
        if 'error' in response:
            raise PreviewError(response['error']['message'])
        return response

    def decode(self, request):
        payload = self.codec({'operation':'decode', 'password':request.get('password', ''), 'bundle':request.get('bundle', '')})['payload']
        if len(self.packages) >= 4:
            del self.packages[next(iter(self.packages))]
        token = secrets.token_urlsafe(24)
        self.packages[token] = (time.monotonic()+1800, payload, None)
        return {'token':token, 'payload':scrub(payload),
                'credential_accounts':[a['metadata']['id'] for a in payload.get('accounts', []) if a.get('auth')],
                'credential_providers':list(payload.get('provider_keys', {}))}

    def preview(self, request):
        entry = self.packages.get(request.get('token'))
        if not entry:
            raise PreviewError('请重新打开迁移包 / Reopen the migration package')
        payload = select_payload(entry[1], request.get('selections', []),
            request.get('include_context', True), request.get('include_rename', True))
        current = scrub(request.get('current_config', {}))
        identities = {}
        for account in current.get('accounts', []):
            cached = self.packages.get(account.get('_shadow_migration_token'))
            if cached and cached[2]:
                for cached_account in cached[2].get('accounts', []):
                    if cached_account['metadata']['id'] == account['id']:
                        identities[account['id']] = cached_account['auth']
        provider_identities = {}
        for provider in current.get('providers', []):
            cached = self.packages.get(provider.get('_shadow_migration_token'))
            if cached and cached[2]:
                key = cached[2].get('provider_keys', {}).get(provider['id'])
                if key:
                    provider_identities[provider['id']] = key
        arguments = {'operation':'preview','config_path':str(self.config_path),
            'payload':payload,'preview_auth':identities,'preview_provider_keys':provider_identities}
        if current:
            arguments['current_config'] = current
        result = self.codec(arguments)
        config = result['config']
        imported = result.pop('imported_auth')
        provider_map = result.pop('provider_id_map')
        providers = set(provider_map.values())
        portable = copy.deepcopy(payload)
        portable['accounts'] = [{'metadata':copy.deepcopy(a),'auth':imported[a['id']]}
            for a in config.get('accounts', []) if a['id'] in imported]
        portable['config']['accounts'] = [a['metadata'] for a in portable['accounts']]
        portable['provider_keys'] = {provider_map[key]:value for key,value in payload.get('provider_keys', {}).items()}
        portable['config']['providers'] = [copy.deepcopy(p) for p in config.get('providers', []) if p['id'] in providers]
        portable['config']['models'] = [copy.deepcopy(m) for m in config.get('models', []) if m['provider'] in providers]
        renamed_previews = {}
        source_ids = {}
        for target_id, auth in imported.items():
            original = next((a for a in payload.get('accounts', []) if a.get('auth') == auth),None)
            if original:
                source_ids[target_id] = original['metadata']['id']
                renamed_previews[target_id] = payload.get('model_previews', {}).get(original['metadata']['id'], [])
        prefixes = dict(provider_map)
        for account in portable['accounts']:
            original = next((item for item in payload['accounts'] if item['auth'] == account['auth']),None)
            if original:
                prefixes[original['metadata'].get('prefix',original['metadata']['id'])] = account['metadata'].get('prefix',account['metadata']['id'])
        presentations = portable['config'].get('catalog_presentations', {})
        portable['config']['catalog_presentations'] = {
            (prefixes.get(route.partition('/')[0],route.partition('/')[0])+'/'+route.partition('/')[2] if '/' in route else route):value
            for route,value in presentations.items()}
        portable['model_previews'] = renamed_previews
        config['_shadow_model_previews'] = {**current.get('_shadow_model_previews', {}), **renamed_previews}
        for field in ['accounts','providers']:
            existing = {record['id']:record for record in current.get(field, [])}
            for record in config.get(field, []):
                record.pop('_shadow_source_id',None)
                for flag in ['_shadow_migration_token','credential_set','api_key_set']:
                    if flag in existing.get(record['id'], {}):
                        record[flag] = existing[record['id']][flag]
        for a in config.get('accounts', []):
            if a['id'] in imported:
                a['_shadow_migration_token'] = request['token']; a['credential_set'] = True
                a['_shadow_source_id'] = source_ids.get(a['id'],a['id'])
        for p in config.get('providers', []):
            if p['id'] in providers:
                p['_shadow_migration_token'] = request['token']
                p['api_key_set'] = bool(portable['provider_keys'].get(p['id']))
                p['_shadow_source_id'] = next(key for key,value in provider_map.items() if value == p['id'])
        prepared = combine_payloads([entry[2],portable]) if entry[2] else portable
        self.packages[request['token']] = (time.monotonic()+1800,entry[1],prepared)
        result['warnings'] = import_notices(current,config,imported,providers)
        return scrub(result)

    def demo(self, request):
        current = scrub(request.get('current_config', {}))
        existing = (current.get('accounts') or [{}])[0]
        provider = (current.get('providers') or [{}])[0]
        account = {'id':existing.get('id','sample-account'),'prefix':existing.get('prefix','sample-account'),
                   'name':existing.get('name','Example subscription'),'hidden_models':[]}
        service = {'id':provider.get('id','sample-api'),'name':existing.get('name','Example subscription'),
                   'base_url':'https://example.invalid/v1','protocol':'responses','auth_mode':'api_key'}
        payload = {'schema':'easy-multi-provider-migration','version':1,
            'config':{'accounts':[account],'providers':[service],'models':[{
                'id':service['id']+'/sample-model','provider':service['id'],'upstream_id':'sample-model',
                'display_name':'Example model','context_window':128000}]},
            'accounts':[{'metadata':account,'auth':{'tokens':{'account_id':'shadow-demo-only','access_token':'shadow-demo-only'}}}],
            'provider_keys':{service['id']:'shadow-demo-only'}}
        if len(self.packages) >= 4:
            del self.packages[next(iter(self.packages))]
        token = secrets.token_urlsafe(24)
        self.packages[token] = (time.monotonic()+1800,payload,None)
        return {'token':token,'payload':scrub(payload),'credential_accounts':[account['id']],
                'credential_providers':[service['id']]}

    def export(self, request):
        selections = request.get('selections', [])
        if not selections or len(selections) > 1000:
            raise PreviewError('请选择服务 / Select services')
        password = request.get('password', '')
        password_bytes = len(password.strip().encode('utf-8'))
        if password_bytes < 12: raise PreviewError('密码至少需要 12 个 UTF-8 字节 / Password requires 12 UTF-8 bytes')
        if password_bytes > 4096: raise PreviewError('密码不能超过 4096 个 UTF-8 字节 / Password cannot exceed 4096 UTF-8 bytes')
        sources = []
        groups = {}
        for selection in selections:
            groups.setdefault(selection.get('source_token'), []).append(selection)
        for token, selected in groups.items():
            if token:
                entry = self.packages.get(token)
                if not entry or entry[2] is None:
                    raise PreviewError('请重新打开原迁移包 / Reopen the original migration package')
                source = entry[2]
            else:
                source = self.codec({'operation':'export','config_path':str(self.config_path),
                    'native_auth_path':str(self.native_auth_path),'services':selected})['payload']
            sources.append(select_payload(source,selected,request.get('include_context',True),request.get('include_rename',True)))
        payload = combine_payloads(sources)
        return self.codec({'operation':'encode','password':password,'payload':payload})

    def handle(self, operation, request):
        if not self.gate.acquire(blocking=False):
            raise PreviewError('正在处理迁移包，请稍候。 / A migration package is being processed.')
        try:
            self.packages = {key:item for key,item in self.packages.items() if item[0]>time.monotonic()}
            match operation:
                case 'decode': return self.decode(request)
                case 'preview': return self.preview(request)
                case 'demo': return self.demo(request)
                case 'export': return self.export(request)
                case _: raise PreviewError('未知迁移操作 / Unknown migration operation')
        finally:
            self.gate.release()
