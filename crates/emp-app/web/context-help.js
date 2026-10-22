// Describe the same catalog budget Codex reads; never treat the trigger as a compression ratio.
function createContextHelp({getState, tr}) {
  const number = value => Math.floor(value).toLocaleString('en-US');
  function budget(model, tokens, {custom = false, external = false} = {}) {
    const settings = getState().codex_context_settings || {};
    const configured = Number(settings.model_context_window) > 0 ? Number(settings.model_context_window) : Number(tokens);
    if (!Number.isFinite(configured) || configured <= 0) return null;
    const ratio = Math.min(100, Math.max(0, Number(model.effective_context_window_percent ?? 95)));
    const usable = Math.floor(configured * ratio / 100);
    const modelLimit = external ? Math.floor(Number(tokens) * .8) : custom ? null : model.auto_compact_token_limit;
    const explicit = Number(settings.model_auto_compact_token_limit) > 0 ? Number(settings.model_auto_compact_token_limit) : Number(modelLimit) > 0 ? Number(modelLimit) : Math.floor(configured * .9);
    const threshold = Math.min(Math.floor(configured * .9), explicit, usable);
    return {tokens:configured, usable, ratio, threshold, percent:Number((threshold / configured * 100).toFixed(2))};
  }
  function describe(model, tokens, options = {}) {
    const settings = getState().codex_context_settings || {}, limits = budget(model,tokens,options);
    if (!limits) return tr('models.context_limits_unknown');
    const {external = false} = options;
    const values = {...limits,tokens:number(limits.tokens),usable:number(limits.usable),threshold:number(limits.threshold)};
    const lines = [tr('models.context_budget_window', values), tr('models.context_budget_usable', values), tr('models.context_budget_compact', values)];
    if (Number(settings.model_context_window) > 0) lines.push(tr('models.context_codex_override'));
    if (settings.model_auto_compact_token_limit_scope === 'body_after_prefix') lines.push(tr('models.context_compact_growth_scope'));
    lines.push(tr(external ? 'models.context_external_adjustment' : 'models.context_subscription_adjustment'));
    if (external) lines.push(tr('models.context_external_guard'));
    if (settings.background_enabled) lines.push(tr('models.context_background_trigger', {threshold:number(limits.tokens * Number(settings.background_threshold_percent || 65) / 100), percent:settings.background_threshold_percent || 65}));
    if (settings.unavailable) lines.push(tr('models.context_config_unavailable'));
    lines.push(tr('models.context_runtime_overrides'));
    return lines.join('\n');
  }
  function background() {
    const settings = getState().codex_context_settings || {};
    return [tr('models.background_compaction_beta'), tr('models.background_compaction_help', {percent:settings.background_threshold_percent || 65}), tr('models.background_compaction_adjustment')].join('\n\n');
  }
  return {budget, describe, background};
}
