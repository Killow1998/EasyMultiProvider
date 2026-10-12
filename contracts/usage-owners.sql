(SELECT u.id,u.observed_at,u.category,
 CASE WHEN u.owner LIKE 'history:%' AND u.usage_turn!='' AND u.route_model!=''
 THEN COALESCE((SELECT MIN(r.owner) FROM usage_events r
   WHERE r.origin='realtime' AND r.usage_turn=u.usage_turn AND r.route_model=u.route_model
     AND r.owner LIKE 'account:%'
   HAVING COUNT(DISTINCT r.owner)=1),u.owner)
 ELSE u.owner END AS owner,
 u.model,u.service_tier,u.operation,u.input_tokens,u.output_tokens,u.cached_input_tokens,
 u.cache_write_tokens,u.cache_write_1h_tokens,u.reasoning_tokens,u.cost_nanos,u.price_issue,
 u.price_key,u.price_fetched_at,u.price_revision,u.rates,u.origin,u.usage_turn,u.route_model,u.match_key
 FROM accounted_usage u)
