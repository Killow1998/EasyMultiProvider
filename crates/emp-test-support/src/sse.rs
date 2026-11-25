pub fn event(frame: &str) -> Option<&str> {
    field_values(frame, "event").last()
}

pub fn data(frame: &str) -> Option<String> {
    let mut lines = field_values(frame, "data");
    let mut data = lines.next()?.to_owned();
    for line in lines {
        data.push('\n');
        data.push_str(line);
    }
    Some(data)
}

fn field_values<'a>(frame: &'a str, name: &str) -> impl Iterator<Item = &'a str> {
    frame.lines().filter_map(move |line| {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        (field == name).then(|| value.strip_prefix(' ').unwrap_or(value))
    })
}

#[cfg(test)]
mod tests {
    use super::{data, event};

    #[test]
    fn data_joins_fields_with_newlines() {
        assert_eq!(
            data("data: first\ndata: second\n").as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn data_ignores_unprefixed_continuation() {
        let frame = "event: response.failed\ndata: {\n\"type\":\"response.failed\"}\n\n";
        assert_eq!(event(frame), Some("response.failed"));
        assert_eq!(data(frame).as_deref(), Some("{"));
    }

    #[test]
    fn fields_accept_values_with_or_without_one_space() {
        assert_eq!(event("event:created"), Some("created"));
        assert_eq!(event("event: created"), Some("created"));
        assert_eq!(
            data("data:first\ndata: second\ndata:  third").as_deref(),
            Some("first\nsecond\n third")
        );
    }

    #[test]
    fn fields_ignore_comments() {
        let frame = ":event: ignored\nevent: updated\n:data: ignored\ndata: kept\n";
        assert_eq!(event(frame), Some("updated"));
        assert_eq!(data(frame).as_deref(), Some("kept"));
        assert_eq!(event(":event: ignored"), None);
        assert_eq!(data(":data: ignored"), None);
    }
}
