pub fn data(frame: &str) -> Option<String> {
    let mut lines = frame.lines().filter_map(|line| {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        (field == "data").then(|| value.strip_prefix(' ').unwrap_or(value))
    });
    let mut data = lines.next()?.to_owned();
    for line in lines {
        data.push('\n');
        data.push_str(line);
    }
    Some(data)
}
