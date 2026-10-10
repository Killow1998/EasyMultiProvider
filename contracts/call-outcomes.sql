(SELECT request_id,operation,started_at,finished_at,
    provider_id,account_id,model_id,session_id,facts,delivery,
    CASE
      WHEN json_extract(facts,'$.error_class')='client_cancelled' THEN 'interrupted'
      WHEN json_extract(facts,'$.error_class')='client_disconnect' THEN 'cancelled'
      WHEN json_extract(facts,'$.error_code')='previous_response_not_found'
        OR (json_extract(facts,'$.success') IS NOT 1 AND json_extract(delivery,'$.error_code')='previous_response_not_found') THEN 'recovery_required'
      WHEN json_extract(facts,'$.success')=1 THEN 'completed'
      WHEN json_extract(facts,'$.success')=0 OR json_extract(facts,'$.status') IS NOT NULL
        OR json_extract(facts,'$.response_status') IN ('failed','incomplete')
        OR json_extract(delivery,'$.downstream_terminal') IN ('error','failed') THEN 'failed'
      ELSE 'unknown'
    END AS state FROM call_records) AS call_records
