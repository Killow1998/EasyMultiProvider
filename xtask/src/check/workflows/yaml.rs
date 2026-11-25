pub(super) use saphyr::MarkedYamlOwned as Node;
use saphyr::{ScalarOwned, YamlDataOwned, YamlLoader};
use saphyr_parser::{Event, Parser, Span, SpannedEventReceiver};
use std::{borrow::Cow, collections::BTreeMap};

pub(super) fn parse(source: &str) -> crate::Result<Option<Node>> {
    let mut workflow = WorkflowYaml::default();
    workflow.loader.allow_duplicate_keys(true);
    Parser::new_from_str(source)
        .load(&mut workflow, true)
        .map_err(|error| error.to_string())?;
    if let Some(error) = workflow.loader.error() {
        return Err(error.to_string());
    }
    let mut documents = workflow.loader.into_documents();
    if documents.len() > 1 {
        return Err(
            "deserializing from YAML containing more than one document is not supported".into(),
        );
    }
    Ok(documents.pop())
}

#[derive(Default)]
struct WorkflowYaml<'source> {
    loader: YamlLoader<'source, Node>,
    scalar_anchors: BTreeMap<usize, Span>,
}

impl<'source> SpannedEventReceiver<'source> for WorkflowYaml<'source> {
    fn on_event(&mut self, event: Event<'source>, span: Span) {
        let span = match event {
            Event::DocumentStart(_) => {
                self.scalar_anchors.clear();
                span
            }
            Event::Scalar(_, _, anchor, _) if anchor != 0 => {
                self.scalar_anchors.insert(anchor, span);
                span
            }
            Event::Alias(anchor) => self.scalar_anchors.get(&anchor).copied().unwrap_or(span),
            _ => span,
        };
        self.loader.on_event(event, span);
    }
}

pub(super) trait NodeExt: Sized {
    fn get(&self, key: &str) -> Option<&Self>;
    fn entries(&self) -> Option<impl Iterator<Item = (Cow<'_, str>, &Self)>>;
    fn as_sequence(&self) -> Option<&[Self]>;
    fn as_str(&self) -> Option<&str>;
    fn scalar_text(&self) -> Option<Cow<'_, str>>;
    fn line(&self) -> usize;
}

impl NodeExt for Node {
    fn get(&self, key: &str) -> Option<&Self> {
        self.entries()?
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    fn entries(&self) -> Option<impl Iterator<Item = (Cow<'_, str>, &Self)>> {
        Some(self.data.as_mapping()?.iter().filter_map(|(key, value)| {
            let name = match key.data {
                YamlDataOwned::Value(ScalarOwned::Null) => Cow::Borrowed("null"),
                _ => key.scalar_text()?,
            };
            Some((name, value))
        }))
    }

    fn as_sequence(&self) -> Option<&[Self]> {
        self.data.as_sequence().map(Vec::as_slice)
    }

    fn as_str(&self) -> Option<&str> {
        self.data.as_str()
    }

    fn scalar_text(&self) -> Option<Cow<'_, str>> {
        match &self.data {
            YamlDataOwned::Value(ScalarOwned::String(value)) => Some(Cow::Borrowed(value)),
            YamlDataOwned::Value(ScalarOwned::Boolean(value)) => {
                Some(Cow::Borrowed(if *value { "true" } else { "false" }))
            }
            YamlDataOwned::Value(ScalarOwned::Integer(value)) => {
                Some(Cow::Owned(value.to_string()))
            }
            YamlDataOwned::Value(ScalarOwned::FloatingPoint(value)) => Some(match value.0 {
                f64::INFINITY => Cow::Borrowed(".inf"),
                f64::NEG_INFINITY => Cow::Borrowed("-.inf"),
                value if value.is_nan() => Cow::Borrowed(".nan"),
                value => Cow::Owned(format!("{value:?}")),
            }),
            _ => None,
        }
    }

    fn line(&self) -> usize {
        self.span.start.line()
    }
}
