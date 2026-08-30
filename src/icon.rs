use std::io::Cursor;

use winit::window::Icon;

const ICON_32_PNG: &[u8] = include_bytes!("../assets/icon-32.png");
const ICON_48_PNG: &[u8] = include_bytes!("../assets/icon-48.png");
const STRIP_PNG: &[u8] = include_bytes!("../assets/icon-strip.png");

pub(crate) fn window_icon() -> Option<Icon> {
    let (rgba, width, height) = decode_frame(ICON_32_PNG)?;
    Icon::from_rgba(rgba, width, height).ok()
}

pub(crate) fn taskbar_icon() -> Option<Icon> {
    let (rgba, width, height) = decode_frame(ICON_48_PNG)?;
    Icon::from_rgba(rgba, width, height).ok()
}

pub(crate) fn strip_frame() -> Option<(Vec<u8>, u32, u32)> {
    decode_frame(STRIP_PNG)
}

fn decode_frame(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;

    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.color_type != png::ColorType::Rgba {
        return None;
    }

    Some((
        buffer[..info.buffer_size()].to_vec(),
        info.width,
        info.height,
    ))
}
