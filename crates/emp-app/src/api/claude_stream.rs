//! Public Claude text is live; validated tool proposals still complete the response.
use emp_router::ProjectionIds;
use serde_json::{Value, json};
use std::io;

pub(super) struct LiveText<F> {
    ids: ProjectionIds,
    send: F,
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

impl<F: FnMut(&Value) -> io::Result<()>> LiveText<F> {
    pub(super) fn new(ids: &ProjectionIds, started: bool, send: F) -> Self {
        Self {
            ids: ids.clone(),
            send,
            started,
            items: Vec::new(),
        }
    }

    pub(super) fn started(&self) -> bool {
        self.started
    }

    pub(super) fn event(&mut self, event: &Value) -> io::Result<()> {
        (self.send)(event)
    }

    pub(super) fn push(&mut self, source: &str, delta: &str) -> io::Result<()> {
        if !self.started {
            self.started = true;
            self.event(&json!({"type":"response.created","response":{"id":self.ids.response_id(),"object":"response","status":"in_progress","output":[]}}))?;
        }
        if self.items.last().is_none_or(|item| source != item.source) {
            self.close_item()?;
            let index = self.items.len();
            let id = format!("{}_live_{index}", self.ids.message_id());
            let item = json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[]});
            self.event(
                &json!({"type":"response.output_item.added","output_index":index,"item":item}),
            )?;
            let part = json!({"type":"output_text","text":"","annotations":[]});
            self.event(&json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":part}))?;
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
        self.event(&event)
    }

    fn close_item(&mut self) -> io::Result<()> {
        let Some(item) = self.items.last() else {
            return Ok(());
        };
        let item = item.completed();
        let index = self.items.len() - 1;
        self.event(&json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":0,"text":item["content"][0]["text"]}))?;
        self.event(&json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":0,"part":item["content"][0]}))?;
        self.event(&json!({"type":"response.output_item.done","output_index":index,"item":item}))
    }

    pub(super) fn finish(&mut self, mut response: Value) -> io::Result<()> {
        self.close_item()?;
        response["id"] = json!(self.ids.response_id());
        let output = response["output"]
            .as_array_mut()
            .expect("validated CLI response");
        // CLI repeats its last public message in the final proposal.
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
            self.event(&event)?;
        }
        Ok(())
    }
}
