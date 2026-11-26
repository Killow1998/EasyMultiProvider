//! Public Claude text is live; validated tool proposals still complete the response.
use emp_router::ProjectionIds;
use serde_json::{Value, json};
use std::io;

pub(super) struct LiveText {
    ids: ProjectionIds,
    started: bool,
    items: Vec<PublicText>,
}

struct PublicText {
    source: String,
    id: String,
    text: String,
}

impl PublicText {
    fn completed(&self) -> Value {
        json!({"id":self.id,"type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":self.text,"annotations":[]}]})
    }
}

impl LiveText {
    pub(super) fn new(ids: &ProjectionIds, started: bool) -> Self {
        Self {
            ids: ids.clone(),
            started,
            items: Vec::new(),
        }
    }

    pub(super) fn started(&self) -> bool {
        self.started
    }

    pub(super) fn push(
        &mut self,
        source: &str,
        delta: &str,
        mut send: impl FnMut(&Value) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.started {
            self.started = true;
            send(
                &json!({"type":"response.created","response":{"id":self.ids.response_id(),"object":"response","status":"in_progress","output":[]}}),
            )?;
        }
        if self.items.last().is_none_or(|item| source != item.source) {
            self.close_item(&mut send)?;
            let index = self.items.len();
            let id = format!("{}_live_{index}", self.ids.message_id());
            let item = json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[]});
            send(&json!({"type":"response.output_item.added","output_index":index,"item":item}))?;
            let part = json!({"type":"output_text","text":"","annotations":[]});
            send(
                &json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":part}),
            )?;
            self.items.push(PublicText {
                source: source.to_owned(),
                id,
                text: String::new(),
            });
        }
        let index = self.items.len() - 1;
        let item = &mut self.items[index];
        item.text.push_str(delta);
        let event = json!({"type":"response.output_text.delta","item_id":item.id,"output_index":index,"content_index":0,"delta":delta});
        send(&event)
    }

    fn close_item(&self, mut send: impl FnMut(&Value) -> io::Result<()>) -> io::Result<()> {
        let Some(item) = self.items.last() else {
            return Ok(());
        };
        let item = item.completed();
        let index = self.items.len() - 1;
        send(
            &json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":0,"text":item["content"][0]["text"]}),
        )?;
        send(
            &json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":0,"part":item["content"][0]}),
        )?;
        send(&json!({"type":"response.output_item.done","output_index":index,"item":item}))
    }

    pub(super) fn finish(
        &mut self,
        mut response: Value,
        mut send: impl FnMut(&Value) -> io::Result<()>,
    ) -> io::Result<()> {
        self.close_item(&mut send)?;
        response["id"] = json!(self.ids.response_id());
        let output = response["output"]
            .as_array_mut()
            .expect("validated CLI response");
        if let Some(last) = self.items.last() {
            let final_text = last.completed();
            output.retain(|item| {
                item["type"] != "message" || item["content"] != final_text["content"]
            });
        }
        let delivered = self.items.len();
        let mut items = self
            .items
            .drain(..)
            .map(|item| item.completed())
            .collect::<Vec<_>>();
        items.append(output);
        *output = items;
        let events = emp_router::response_json_stream_events(response, &self.ids, false)
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
        for event in events {
            if (self.started && event["type"] == "response.created")
                || event["output_index"]
                    .as_u64()
                    .is_some_and(|index| index < delivered as u64)
            {
                continue;
            }
            send(&event)?;
        }
        Ok(())
    }
}
