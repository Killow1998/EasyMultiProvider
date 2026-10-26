//! Bounded replay of diagnostic receipts; never retain request content.
use super::super::{PROBE_CAPACITY, is_probe};
use super::{Journal, MAX_RECORD, sanitize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};

impl Journal {
    pub(in crate::diagnostics) fn read_observations(
        &self,
        limit: usize,
    ) -> (Vec<Value>, Vec<Value>) {
        let mut groups = [Vec::new(), Vec::new()];
        let mut order = 0;
        for (_, _, path, _) in self.parts() {
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let Ok(file) = options.open(path) else {
                continue;
            };
            let mut reader = BufReader::new(file);
            let mut line = Vec::new();
            let mut oversized = false;
            loop {
                let Ok(buffer) = reader.fill_buf() else {
                    break;
                };
                if buffer.is_empty() {
                    break;
                }
                let length = buffer
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(buffer.len(), |i| i + 1);
                let ended = buffer[length - 1] == b'\n';
                if line.len() + length > MAX_RECORD + 1 {
                    oversized = true;
                }
                if !oversized {
                    line.extend_from_slice(&buffer[..length]);
                }
                reader.consume(length);
                if !ended {
                    continue;
                }
                if !oversized
                    && let Ok(record) = serde_json::from_slice::<Value>(&line)
                    && record["fields"].is_object()
                {
                    let event = record["event"].as_str().unwrap_or("");
                    let fields = &record["fields"];
                    let group = match event {
                        "route_observation"
                            if !is_probe(fields["route"].as_str().unwrap_or("")) =>
                        {
                            Some(0)
                        }
                        "operation_finished" | "request_finished"
                            if is_probe(fields["operation"].as_str().unwrap_or("")) =>
                        {
                            Some(1)
                        }
                        _ => None,
                    };
                    if let Some(group) = group {
                        let recent = &mut groups[group];
                        let mut fields = sanitize(fields, 0);
                        if group == 1 {
                            fields.as_object_mut().unwrap().retain(|key, _| {
                                matches!(
                                    key.as_str(),
                                    "operation"
                                        | "request_id"
                                        | "operation_id"
                                        | "outcome"
                                        | "last_stage"
                                        | "last_phase"
                                        | "duration_ms"
                                        | "response_status"
                                        | "downstream_terminal"
                                        | "delivery"
                                        | "error_code"
                                        | "model_id"
                                )
                            });
                            fields["observed_at"] = record["timestamp"].clone();
                        }
                        recent.push((
                            record["timestamp"].as_str().unwrap_or("").to_owned(),
                            order,
                            fields,
                        ));
                        order += 1;
                        recent.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
                        let capacity = if group == 1 { PROBE_CAPACITY } else { limit };
                        if recent.len() > capacity {
                            recent.remove(0);
                        }
                    }
                }
                line.clear();
                oversized = false;
            }
        }
        let [routes, probes] =
            groups.map(|group| group.into_iter().map(|(_, _, fields)| fields).collect());
        (routes, probes)
    }
}
