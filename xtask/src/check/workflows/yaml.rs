pub(super) use saphyr::MarkedYamlOwned as Node;
use saphyr::YamlLoader;
use saphyr_parser::{Event, Parser, ScalarStyle, Span, SpannedEventReceiver};
use std::collections::BTreeMap;

pub(super) struct Workflow {
    pub(super) root: Node,
    script_lines: BTreeMap<usize, ScriptLines>,
}

impl Workflow {
    pub(super) fn script_lines(&self, node: &Node) -> ScriptLines {
        let start = node.span.start;
        self.script_lines
            .get(&start.index())
            .copied()
            .unwrap_or(ScriptLines::Scalar(start.line()))
    }
}

#[derive(Clone, Copy)]
pub(super) enum ScriptLines {
    Literal(usize),
    /// Folded and plain scalars can fold source line breaks into spaces.
    Scalar(usize),
}

impl ScriptLines {
    pub(super) fn command(self, offset: usize) -> usize {
        match self {
            Self::Literal(header) => header + 1 + offset,
            Self::Scalar(start) => start,
        }
    }
}

pub(super) fn parse(source: &str) -> crate::Result<Option<Workflow>> {
    let mut workflow = WorkflowYaml::default();
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
    Ok(documents.pop().map(|root| Workflow {
        root,
        script_lines: workflow.script_lines,
    }))
}

#[derive(Default)]
struct WorkflowYaml<'source> {
    loader: YamlLoader<'source, Node>,
    scalar_anchors: BTreeMap<usize, ScriptLines>,
    script_lines: BTreeMap<usize, ScriptLines>,
}

impl<'source> SpannedEventReceiver<'source> for WorkflowYaml<'source> {
    fn on_event(&mut self, event: Event<'source>, span: Span) {
        let lines = match event {
            Event::DocumentStart(_) => {
                self.scalar_anchors.clear();
                None
            }
            Event::Scalar(_, style, anchor, _) => {
                let start = span.start.line();
                let lines = if style == ScalarStyle::Literal {
                    ScriptLines::Literal(start)
                } else {
                    ScriptLines::Scalar(start)
                };
                if anchor != 0 {
                    self.scalar_anchors.insert(anchor, lines);
                }
                matches!(lines, ScriptLines::Literal(_)).then_some(lines)
            }
            Event::Alias(anchor) => self.scalar_anchors.get(&anchor).copied(),
            _ => None,
        };
        if let Some(lines) = lines {
            self.script_lines.insert(span.start.index(), lines);
        }
        self.loader.on_event(event, span);
    }
}
