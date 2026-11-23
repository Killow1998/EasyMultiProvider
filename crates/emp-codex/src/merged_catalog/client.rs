use serde_json::Value;

const FIRST_AUDIO_CLIENT_MINOR: u64 = 168;

pub fn for_client_version(mut catalog: Value, version: Option<&str>) -> Value {
    let supports_audio = version
        .and_then(|version| {
            let mut parts = version.split('.');
            Some((
                parts.next()?.parse::<u64>().ok()?,
                parts.next()?.parse::<u64>().ok()?,
            ))
        })
        .is_some_and(|(major, minor)| major > 0 || minor >= FIRST_AUDIO_CLIENT_MINOR);
    if !supports_audio {
        for model in catalog["models"].as_array_mut().expect("model catalog") {
            if let Some(modes) = model
                .get_mut("input_modalities")
                .and_then(Value::as_array_mut)
            {
                modes.retain(|mode| mode != "audio");
            }
        }
    }
    catalog
}
