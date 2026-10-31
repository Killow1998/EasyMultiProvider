pub(super) fn decode(source: &[u8]) -> Result<(Vec<u8>, u32), Box<dyn std::error::Error>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(source));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    if info.width == 0 || info.width != info.height || info.width > 1024 {
        return Err("icon must be square and at most 1024 pixels".into());
    }
    let mut bytes = vec![0; reader.output_buffer_size().ok_or("icon dimensions")?];
    let frame = reader.next_frame(&mut bytes)?;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => bytes[..frame.buffer_size()].to_vec(),
        png::ColorType::Rgb => bytes[..frame.buffer_size()]
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        _ => return Err("icon must use RGB or RGBA pixels".into()),
    };
    Ok((rgba, frame.width))
}

pub(super) fn resize(rgba: &[u8], from: u32, to: u32) -> Vec<u8> {
    let mut resized = Vec::with_capacity((to * to * 4) as usize);
    for y in 0..to {
        for x in 0..to {
            let position = ((y * from / to * from + x * from / to) * 4) as usize;
            resized.extend_from_slice(&rgba[position..position + 4]);
        }
    }
    resized
}

#[cfg(any(windows, test))]
pub(super) fn ico(image: &super::Image) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 256, 256);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()?
            .write_image_data(&resize(&image.rgba, image.size, 256))?;
    }
    // Vista+ ICO permits a PNG payload. Zero width/height bytes mean 256 pixels.
    let mut ico = vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 32, 0];
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22_u32.to_le_bytes());
    ico.extend_from_slice(&png);
    Ok(ico)
}
