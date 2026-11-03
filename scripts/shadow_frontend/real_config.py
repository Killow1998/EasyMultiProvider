"""Project real source metadata with the candidate's supported capabilities."""


def project_config(public, source_read, candidate_read=None):
    models = public.get('catalog_models', [])
    if any('ultra_reasoning_levels' not in model or 'ultra_default_effort' not in model for model in models):
        # Versionless catalogs hide Ultra from old clients. Read actual versioned metadata.
        status, catalog = source_read('/v1/models?client_version=0.163.0')
        if status != 200:
            return 503, {'error': {'message': 'Could not read the live model catalog'}}
        entries = {model['slug']: model for model in catalog.get('models', [])}
        concrete = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']
        for model in models:
            entry = entries.get(model['id'], {})
            ordered_levels = [level.get('effort') for level in entry.get('supported_reasoning_levels', [])]
            levels = set(ordered_levels)
            supports_ultra = model.get('source_type') != 'provider' and 'ultra' in levels
            model['ultra_reasoning_levels'] = [level for level in concrete if level in levels] if supports_ultra else []
            if 'ultra_default_effort' in model:
                continue
            default = entry.get('multi_agent_reasoning_effort')
            # An active EMP override is not the official default.
            override = public.get('ultra_reasoning_efforts', {}).get(model.get('family_id'))
            if not model['ultra_reasoning_levels'] or override:
                default = None
            elif default not in model['ultra_reasoning_levels']:
                default = 'max' if 'max' in levels else next(
                    (level for level in reversed(ordered_levels) if level in concrete), 'medium')
            model['ultra_default_effort'] = default
    if candidate_read:
        status, candidate = candidate_read('/api/config')
        if status != 200:
            return 503, {'error': {'message': 'Candidate configuration is unavailable'}}
        for key in ['emp_version', 'codex_context_settings']:
            if key in candidate:
                public[key] = candidate[key]
        public['provider_balances'] = {
            identifier: entry for identifier, entry in public.get('provider_balances', {}).items()
            if identifier in candidate['provider_balances']}
    return 200, public
