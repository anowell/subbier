//! The `sR` menu bar mark (`docs/ARCHITECTURE.md` §9), embedded because subbier runs as a
//! bare binary and `Icon::from_path` is `#[cfg(windows)]` only. The raster must stay
//! **square**: tray-icon hardcodes an 18pt height and derives the width from the aspect
//! ratio. Black + alpha, so it draws as a template image and cannot carry severity.

/// The @2x menu bar raster, generated from `assets/sr.svg` by `assets/build.sh`.
const MARK_PNG: &[u8] = include_bytes!("../../../assets/sr-36.png");

/// The tray icon that receives this must set `with_icon_as_template(true)`.
pub fn menu_bar_icon() -> tray_icon::Icon {
    let mark = image::load_from_memory_with_format(MARK_PNG, image::ImageFormat::Png)
        .expect("embedded assets/sr-36.png is not a decodable PNG")
        .into_rgba8();
    let (width, height) = mark.dimensions();

    tray_icon::Icon::from_rgba(mark.into_raw(), width, height)
        .expect("embedded assets/sr-36.png is not a valid RGBA icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_mark_is_a_square_raster() {
        let mark = image::load_from_memory_with_format(MARK_PNG, image::ImageFormat::Png)
            .expect("embedded PNG decodes");
        assert_eq!(mark.width(), mark.height());
        let _ = menu_bar_icon();
    }
}
