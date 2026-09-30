use super::media::png_dimensions;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

const MAX_IMAGE_BYTES: usize = super::projection::MAX_TRANSCRIPT_BYTES;
const MAX_BASE64_CHARS: usize = 4 * MAX_IMAGE_BYTES.div_ceil(3);
const MAX_NOTE_BYTES: usize = 192;

#[derive(Clone, Copy)]
struct Dimensions {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DominantScale {
    original: u32,
    displayed: u32,
}

/// Match a CLI-resized image and its immediate geometry-note block.
///
/// The caller must pass the actual text block immediately after `actual` and
/// keep the original expected-block order. This checks image format, shape,
/// dimensions, and note geometry; it does not prove pixel provenance.
pub(super) fn matches_resize(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    note: &serde_json::Value,
) -> bool {
    if expected == actual {
        return false;
    }

    let Some((expected_mime, expected_data, original)) = image_payload(expected) else {
        return false;
    };
    let Some((actual_mime, actual_data, displayed)) = image_payload(actual) else {
        return false;
    };
    if expected_mime != actual_mime || expected_data == actual_data {
        return false;
    }
    let Some(scale) = proportional_downscale(original, displayed) else {
        return false;
    };

    geometry_note_matches(note, original, displayed, scale)
}

fn proportional_downscale(original: Dimensions, displayed: Dimensions) -> Option<DominantScale> {
    if displayed.width > original.width
        || displayed.height > original.height
        || (displayed.width == original.width && displayed.height == original.height)
    {
        return None;
    }

    let (original_long, original_short, displayed_long, displayed_short) =
        if original.width >= original.height {
            (
                original.width,
                original.height,
                displayed.width,
                displayed.height,
            )
        } else {
            (
                original.height,
                original.width,
                displayed.height,
                displayed.width,
            )
        };

    // The CLI scales from the dominant axis, then rounds the short axis to an
    // integer pixel. Accept only the nearest-pixel result (at most half a
    // pixel from that uniform scale), not a crop or independent stretch.
    let expected_short_numerator = u64::from(original_short) * u64::from(displayed_long);
    let actual_short_numerator = u64::from(displayed_short) * u64::from(original_long);
    let rounding_error = expected_short_numerator.abs_diff(actual_short_numerator);
    if rounding_error.checked_mul(2)? > u64::from(original_long) {
        return None;
    }

    Some(DominantScale {
        original: original_long,
        displayed: displayed_long,
    })
}

fn image_payload(block: &Value) -> Option<(&str, Vec<u8>, Dimensions)> {
    let object = block.as_object()?;
    if object.len() != 2 || object.get("type")?.as_str()? != "image" {
        return None;
    }
    let source = object.get("source")?.as_object()?;
    if source.len() != 3 || source.get("type")?.as_str()? != "base64" {
        return None;
    }
    let mime = source.get("media_type")?.as_str()?;
    if !matches!(mime, "image/png" | "image/jpeg" | "image/webp") {
        return None;
    }
    let encoded = source.get("data")?.as_str()?;
    if encoded.len() > MAX_BASE64_CHARS {
        return None;
    }
    let data = STANDARD.decode(encoded).ok()?;
    if data.is_empty() || data.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let dimensions = image_dimensions(mime, &data)?;
    Some((mime, data, dimensions))
}

fn image_dimensions(mime: &str, data: &[u8]) -> Option<Dimensions> {
    let (width, height) = match mime {
        "image/png" => png_dimensions(data)?,
        "image/jpeg" => jpeg_dimensions(data)?,
        "image/webp" => webp_dimensions(data)?,
        _ => return None,
    };
    (width > 0 && height > 0).then_some(Dimensions { width, height })
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }

    let mut cursor = 2usize;
    while cursor < data.len() {
        if data.get(cursor) != Some(&0xff) {
            return None;
        }
        while data.get(cursor) == Some(&0xff) {
            cursor = cursor.checked_add(1)?;
        }
        let marker = *data.get(cursor)?;
        cursor = cursor.checked_add(1)?;
        if marker == 0x00 || marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }

        let length_end = cursor.checked_add(2)?;
        let segment_length =
            u16::from_be_bytes(data.get(cursor..length_end)?.try_into().ok()?) as usize;
        if segment_length < 2 {
            return None;
        }
        let segment_end = cursor.checked_add(segment_length)?;
        if segment_end > data.len() {
            return None;
        }

        if is_jpeg_start_of_frame(marker) {
            if segment_length < 8 {
                return None;
            }
            let component_count = usize::from(*data.get(cursor + 7)?);
            if component_count == 0 {
                return None;
            }
            let required_length = component_count.checked_mul(3)?.checked_add(8)?;
            if segment_length != required_length {
                return None;
            }
            let height = u16::from_be_bytes(data.get(cursor + 3..cursor + 5)?.try_into().ok()?);
            let width = u16::from_be_bytes(data.get(cursor + 5..cursor + 7)?.try_into().ok()?);
            return (width > 0 && height > 0).then_some((u32::from(width), u32::from(height)));
        }
        cursor = segment_end;
    }
    None
}

fn is_jpeg_start_of_frame(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.get(..4)? != &b"RIFF"[..] || data.get(8..12)? != &b"WEBP"[..] {
        return None;
    }
    let riff_size = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    let riff_end = riff_size.checked_add(8)?;
    if riff_end != data.len() || riff_end < 12 {
        return None;
    }

    let mut cursor = 12usize;
    while cursor < riff_end {
        let header_end = cursor.checked_add(8)?;
        if header_end > riff_end {
            return None;
        }
        let tag = data.get(cursor..cursor + 4)?;
        let chunk_size =
            u32::from_le_bytes(data.get(cursor + 4..header_end)?.try_into().ok()?) as usize;
        let payload_end = header_end.checked_add(chunk_size)?;
        let next_chunk = payload_end.checked_add(chunk_size & 1)?;
        if next_chunk > riff_end {
            return None;
        }
        let payload = data.get(header_end..payload_end)?;

        let dimensions = match tag {
            b"VP8X" => webp_vp8x_dimensions(payload),
            b"VP8 " => webp_vp8_dimensions(payload),
            b"VP8L" => webp_vp8l_dimensions(payload),
            _ => None,
        };
        if dimensions.is_some() {
            return dimensions;
        }
        cursor = next_chunk;
    }
    None
}

fn webp_vp8x_dimensions(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 10 {
        return None;
    }
    let width = read_u24_le(payload.get(4..7)?)?.checked_add(1)?;
    let height = read_u24_le(payload.get(7..10)?)?.checked_add(1)?;
    Some((width, height))
}

fn webp_vp8_dimensions(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 10 || payload[0] & 1 != 0 || payload.get(3..6)? != &[0x9d, 0x01, 0x2a][..] {
        return None;
    }
    let width = u16::from_le_bytes(payload.get(6..8)?.try_into().ok()?) & 0x3fff;
    let height = u16::from_le_bytes(payload.get(8..10)?.try_into().ok()?) & 0x3fff;
    (width > 0 && height > 0).then_some((u32::from(width), u32::from(height)))
}

fn webp_vp8l_dimensions(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 5 || payload[0] != 0x2f {
        return None;
    }
    let b1 = u32::from(payload[1]);
    let b2 = u32::from(payload[2]);
    let b3 = u32::from(payload[3]);
    let b4 = u32::from(payload[4]);
    let width = b1 | ((b2 & 0x3f) << 8);
    let height = (b2 >> 6) | (b3 << 2) | ((b4 & 0x0f) << 10);
    Some((width.checked_add(1)?, height.checked_add(1)?))
}

fn read_u24_le(bytes: &[u8]) -> Option<u32> {
    let [a, b, c]: [u8; 3] = bytes.try_into().ok()?;
    Some(u32::from(a) | (u32::from(b) << 8) | (u32::from(c) << 16))
}

fn geometry_note_matches(
    note: &Value,
    original: Dimensions,
    displayed: Dimensions,
    actual_scale: DominantScale,
) -> bool {
    let Some(object) = note.as_object() else {
        return false;
    };
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("text") {
        return false;
    }
    let Some(text) = object.get("text").and_then(Value::as_str) else {
        return false;
    };
    if text.len() > MAX_NOTE_BYTES || !text.is_ascii() {
        return false;
    }
    let Some(text) = text.strip_prefix("[Image: original ") else {
        return false;
    };
    let Some((original_dimensions, text)) = text.split_once(", displayed at ") else {
        return false;
    };
    let Some((displayed_dimensions, text)) = text.split_once(". Multiply coordinates by ") else {
        return false;
    };
    let Some(multiplier) = text.strip_suffix(" to map to original image.]") else {
        return false;
    };
    let Some((note_original_width, note_original_height)) = parse_dimensions(original_dimensions)
    else {
        return false;
    };
    let Some((note_displayed_width, note_displayed_height)) =
        parse_dimensions(displayed_dimensions)
    else {
        return false;
    };
    let note_displayed = Dimensions {
        width: note_displayed_width,
        height: note_displayed_height,
    };
    let Some(note_scale) = parse_scale_hundredths(multiplier) else {
        return false;
    };
    let Some(note_axes) = proportional_downscale(original, note_displayed) else {
        return false;
    };

    note_original_width == original.width
        && note_original_height == original.height
        // Claude Code 2.1.283 reported the nominal 2000px target while its
        // encoded raster was 1999px wide. Keep that observed discrepancy
        // bounded to one pixel per axis and verify both geometries separately.
        && note_displayed_width.abs_diff(displayed.width) <= 1
        && note_displayed_height.abs_diff(displayed.height) <= 1
        && rounded_scale_hundredths(actual_scale.original, actual_scale.displayed)
            == Some(note_scale)
        && rounded_scale_hundredths(note_axes.original, note_axes.displayed) == Some(note_scale)
}

fn parse_dimensions(text: &str) -> Option<(u32, u32)> {
    let (width_text, height_text) = text.split_once('x')?;
    if width_text.is_empty()
        || height_text.is_empty()
        || (width_text.len() > 1 && width_text.starts_with('0'))
        || (height_text.len() > 1 && height_text.starts_with('0'))
    {
        return None;
    }
    let width = width_text.parse::<u32>().ok()?;
    let height = height_text.parse::<u32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

fn parse_scale_hundredths(text: &str) -> Option<u64> {
    let (whole_text, fraction_text) = match text.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (text, ""),
    };
    if whole_text.is_empty()
        || whole_text.len() > 1 && whole_text.starts_with('0')
        || !whole_text.bytes().all(|byte| byte.is_ascii_digit())
        || fraction_text.len() > 2
        || !fraction_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole_text.parse::<u64>().ok()?;
    let fraction = match fraction_text.len() {
        0 => 0,
        1 => u64::from(fraction_text.parse::<u8>().ok()?) * 10,
        2 => u64::from(fraction_text.parse::<u8>().ok()?),
        _ => return None,
    };
    whole.checked_mul(100)?.checked_add(fraction)
}

fn rounded_scale_hundredths(original: u32, displayed: u32) -> Option<u64> {
    if displayed == 0 {
        return None;
    }
    (u64::from(original)
        .checked_mul(100)?
        .checked_add(u64::from(displayed) / 2)?)
    .checked_div(u64::from(displayed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    fn jpeg_sof(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xc0];
        bytes.extend_from_slice(&17u16.to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.push(3);
        bytes.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        bytes
    }

    fn webp_vp8x(width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0];
        payload.extend_from_slice(&((width - 1).to_le_bytes())[..3]);
        payload.extend_from_slice(&((height - 1).to_le_bytes())[..3]);
        let riff_size = 4u32 + 8 + payload.len() as u32;
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WEBPVP8X");
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn image_block(media_type: &str, bytes: &[u8]) -> Value {
        json!({
            "type":"image",
            "source":{"type":"base64","media_type":media_type,"data":STANDARD.encode(bytes)}
        })
    }

    fn geometry_note(
        original_width: u32,
        original_height: u32,
        displayed_width: u32,
        displayed_height: u32,
        scale: &str,
    ) -> Value {
        json!({
            "type":"text",
            "text":format!(
                "[Image: original {original_width}x{original_height}, displayed at {displayed_width}x{displayed_height}. Multiply coordinates by {scale} to map to original image.]"
            )
        })
    }

    #[test]
    fn accepts_the_probed_proportional_resize_for_png_jpeg_and_webp() {
        let cases = [
            ("image/png", png_header(2560, 1440), png_header(2000, 1125)),
            ("image/jpeg", jpeg_sof(2560, 1440), jpeg_sof(2000, 1125)),
            ("image/webp", webp_vp8x(2560, 1440), webp_vp8x(2000, 1125)),
        ];
        let note = geometry_note(2560, 1440, 2000, 1125, "1.28");
        for (mime, original, displayed) in cases {
            assert!(matches_resize(
                &image_block(mime, &original),
                &image_block(mime, &displayed),
                &note
            ));
        }
    }

    #[test]
    fn accepts_observed_odd_png_rounding_and_nominal_note_dimension() {
        let expected = image_block("image/png", &png_header(2561, 1441));
        let actual = image_block("image/png", &png_header(1999, 1125));
        let note = geometry_note(2561, 1441, 2000, 1125, "1.28");
        assert!(matches_resize(&expected, &actual, &note));
    }

    #[test]
    fn accepts_short_axis_pixel_rounding_for_wide_and_tall_images() {
        let wide_expected = image_block("image/png", &png_header(4000, 101));
        let wide_actual = image_block("image/png", &png_header(2000, 51));
        let wide_note = geometry_note(4000, 101, 2000, 51, "2.00");
        assert!(matches_resize(&wide_expected, &wide_actual, &wide_note));

        let tall_expected = image_block("image/png", &png_header(101, 4000));
        let tall_actual = image_block("image/png", &png_header(51, 2000));
        let tall_note = geometry_note(101, 4000, 51, 2000, "2.00");
        assert!(matches_resize(&tall_expected, &tall_actual, &tall_note));
    }

    #[test]
    fn rejects_unchanged_images_and_keeps_them_on_callers_exact_path() {
        let image = image_block("image/png", &png_header(2560, 1440));
        let note = geometry_note(2560, 1440, 2560, 1440, "1.00");
        assert!(!matches_resize(&image, &image, &note));
    }

    #[test]
    fn rejects_wrong_adjacent_note_or_unrelated_text() {
        let expected = image_block("image/jpeg", &jpeg_sof(2560, 1440));
        let actual = image_block("image/jpeg", &jpeg_sof(2000, 1125));
        let wrong_geometry = geometry_note(1280, 720, 1000, 562, "1.28");
        assert!(!matches_resize(&expected, &actual, &wrong_geometry));
        assert!(!matches_resize(
            &expected,
            &actual,
            &json!({"type":"text","text":"unrelated prompt text"})
        ));
        assert!(!matches_resize(
            &expected,
            &actual,
            &json!({"type":"text","text":"[Image: original 2560x1440, displayed at 2000x1125. Multiply coordinates by 1.28 to map to original image.] extra"})
        ));
    }

    #[test]
    fn rejects_bad_note_shape_and_extra_image_fields() {
        let expected = image_block("image/webp", &webp_vp8x(2560, 1440));
        let actual = image_block("image/webp", &webp_vp8x(2000, 1125));
        let valid_note = geometry_note(2560, 1440, 2000, 1125, "1.28");
        let mut note_extra = valid_note.clone();
        note_extra["source"] = json!("unexpected");
        assert!(!matches_resize(&expected, &actual, &note_extra));

        let mut actual_extra = actual.clone();
        actual_extra["detail"] = json!("high");
        assert!(!matches_resize(&expected, &actual_extra, &valid_note));
        let mut source_extra = actual.clone();
        source_extra["source"]["quality"] = json!(95);
        assert!(!matches_resize(&expected, &source_extra, &valid_note));
    }

    #[test]
    fn rejects_mime_changes_bad_geometry_and_malformed_payloads() {
        let expected_png = image_block("image/png", &png_header(2560, 1440));
        let actual_webp = image_block("image/webp", &webp_vp8x(2000, 1125));
        let note = geometry_note(2560, 1440, 2000, 1125, "1.28");
        assert!(!matches_resize(&expected_png, &actual_webp, &note));

        let actual_wrong_scale = image_block("image/png", &png_header(2000, 1125));
        let wrong_scale = geometry_note(2560, 1440, 2000, 1125, "1.27");
        assert!(!matches_resize(
            &expected_png,
            &actual_wrong_scale,
            &wrong_scale
        ));
        let actual_wrong_aspect = image_block("image/png", &png_header(2000, 1000));
        let wrong_aspect_note = geometry_note(2560, 1440, 2000, 1000, "1.28");
        assert!(!matches_resize(
            &expected_png,
            &actual_wrong_aspect,
            &wrong_aspect_note
        ));

        let odd_expected = image_block("image/png", &png_header(2561, 1441));
        let odd_distorted = image_block("image/png", &png_header(2000, 1127));
        let odd_note = geometry_note(2561, 1441, 2000, 1127, "1.28");
        assert!(!matches_resize(&odd_expected, &odd_distorted, &odd_note));

        let odd_actual = image_block("image/png", &png_header(1999, 1125));
        let note_too_far = geometry_note(2561, 1441, 2001, 1125, "1.28");
        assert!(!matches_resize(&odd_expected, &odd_actual, &note_too_far));
        let actual_upscaled = image_block("image/png", &png_header(3000, 1688));
        let upscale_note = geometry_note(2560, 1440, 3000, 1688, "0.85");
        assert!(!matches_resize(
            &expected_png,
            &actual_upscaled,
            &upscale_note
        ));

        let malformed = json!({
            "type":"image",
            "source":{"type":"base64","media_type":"image/jpeg","data":"not base64"}
        });
        let jpeg = image_block("image/jpeg", &jpeg_sof(2000, 1125));
        assert!(!matches_resize(
            &image_block("image/jpeg", &jpeg_sof(2560, 1440)),
            &malformed,
            &note
        ));
        assert!(!matches_resize(&malformed, &jpeg, &note));
    }
}
