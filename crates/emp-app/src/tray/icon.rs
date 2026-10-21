//! Reuse the packaged EMP mark, keeping the native tray payload small.
pub(super) const SIZE: u32 = 48;

pub(super) fn rgba() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let source = include_bytes!("../../../../assets/branding/easy-multi-provider-icon-1024.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(source));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut bytes = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("invalid EMP icon dimensions")?
    ];
    let frame = reader.next_frame(&mut bytes)?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => return Err("EMP icon must use RGB or RGBA pixels".into()),
    };
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let position = ((y * frame.height / SIZE * frame.width + x * frame.width / SIZE)
                as usize)
                * channels;
            rgba.extend_from_slice(&bytes[position..position + 3]);
            rgba.push(if channels == 4 {
                bytes[position + 3]
            } else {
                255
            });
        }
    }
    Ok(rgba)
}
