WITH page AS MATERIALIZED (
    SELECT * FROM temp.report_calls ORDER BY started_at DESC,request_id DESC,operation LIMIT ? OFFSET ?
)
SELECT json(page.facts),page.delivery,
    (SELECT json_object('cost_nanos',cost_nanos,'price_issue',price_issue,'price_key',price_key)
     FROM accounted_usage WHERE id=json_extract(page.facts,'$.usage_event_id') LIMIT 1),
    page.state,
    CASE WHEN page.state='recovery_required' THEN (
        SELECT retry.request_id FROM main.call_records AS retry
        WHERE retry.operation=page.operation
          AND CAST(CASE WHEN json_valid(retry.facts)
              THEN json_extract(retry.facts,'$.continuation_recovery_from') END AS TEXT)=page.request_id
          AND retry.state='completed'
        ORDER BY retry.finished_at DESC,retry.request_id DESC LIMIT 1
    ) END
FROM page
ORDER BY page.started_at DESC,page.request_id DESC,page.operation
