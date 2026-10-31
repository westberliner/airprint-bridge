//! Turns scanned pages into the JPEG and PDF documents eSCL clients ask for.

use jpeg_encoder::{ColorType, Encoder, PixelDensity, PixelDensityUnit};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;

use crate::scanner::{Page, ScanParams};

/// An 8-bit image, interleaved RGB when `channels == 3`.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub dpi: u32,
    pub data: Vec<u8>,
}

/// Decodes the scanner's JPEG strips (colour) into one image, cropped to `max_height` lines.
pub fn assemble(page: Page, dpi: u32, max_height: u32) -> Result<Image, String> {
    let (width, height, channels, data) = match page {
        Page::Raw { width, height, channels, data } => (width, height, channels, data),
        Page::Jpeg(strips) => {
            let mut data = Vec::new();
            let mut width = 0;
            let mut height = 0;
            for strip in strips {
                let mut decoder = JpegDecoder::new(ZCursor::new(&strip));
                let pixels = decoder.decode().map_err(|e| format!("decoding scan strip: {e:?}"))?;
                let (w, h) = decoder.dimensions().ok_or("scan strip without dimensions")?;
                if width != 0 && w as u32 != width {
                    return Err(format!("scan strips differ in width ({width} vs {w})"));
                }
                width = w as u32;
                height += h as u32;
                data.extend_from_slice(&pixels[..w * h * 3]);
            }
            (width, height, 3, data)
        }
    };
    let height = height.min(max_height).max(1);
    let mut data = data;
    data.truncate((width * height * channels as u32) as usize);
    Ok(Image { width, height, channels, dpi, data })
}

const JPEG_QUALITY: u8 = 85;

/// A finished scan document, ready to send.
pub struct Document {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
}

impl Document {
    /// Encodes a scanned page as JPEG, or as a PDF wrapping that JPEG.
    pub fn from_page(page: Page, params: &ScanParams, pdf: bool) -> Result<Self, String> {
        let max_height = (params.height as u64 * params.dpi as u64 / 1200) as u32;
        let img = assemble(page, params.dpi, max_height)?;
        let jpeg = to_jpeg(&img, JPEG_QUALITY)?;
        let (content_type, bytes) = if pdf { ("application/pdf", to_pdf(&jpeg, &img)) } else { ("image/jpeg", jpeg) };
        Ok(Document { content_type, bytes, width: img.width, height: img.height, channels: img.channels })
    }

    pub fn describe(&self) -> String {
        format!("{} {}x{}x{}", self.content_type, self.width, self.height, self.channels)
    }
}

pub fn to_jpeg(img: &Image, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_density(PixelDensity { density: (img.dpi as u16, img.dpi as u16), unit: PixelDensityUnit::Inches });
    let color = if img.channels == 3 { ColorType::Rgb } else { ColorType::Luma };
    encoder
        .encode(&img.data, img.width as u16, img.height as u16, color)
        .map_err(|e| format!("encoding JPEG: {e}"))?;
    Ok(out)
}

/// A one-page PDF showing `jpeg` at its scan resolution.
pub fn to_pdf(jpeg: &[u8], img: &Image) -> Vec<u8> {
    let points = |px: u32| px as f64 * 72.0 / img.dpi as f64;
    let (w, h) = (points(img.width), points(img.height));
    let color_space = if img.channels == 3 { "/DeviceRGB" } else { "/DeviceGray" };
    let content = format!("q {w:.2} 0 0 {h:.2} 0 0 cm /Im0 Do Q");

    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    let mut object = |out: &mut Vec<u8>, head: String, stream: Option<&[u8]>| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{head}\n", offsets.len()).as_bytes());
        if let Some(s) = stream {
            out.extend_from_slice(b"stream\n");
            out.extend_from_slice(s);
            out.extend_from_slice(b"\nendstream\n");
        }
        out.extend_from_slice(b"endobj\n");
    };
    object(&mut out, "<< /Type /Catalog /Pages 2 0 R >>".into(), None);
    object(&mut out, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), None);
    object(
        &mut out,
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w:.2} {h:.2}] \
             /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
        ),
        None,
    );
    object(
        &mut out,
        format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {color_space} \
             /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>",
            img.width,
            img.height,
            jpeg.len()
        ),
        Some(jpeg),
    );
    object(&mut out, format!("<< /Length {} >>", content.len()), Some(content.as_bytes()));

    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1).as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray() -> Image {
        Image { width: 16, height: 8, channels: 1, dpi: 300, data: (0..128).map(|v| v as u8).collect() }
    }

    #[test]
    fn jpeg_round_trip_through_strips() {
        let img = gray();
        let rgb = Image { channels: 3, data: img.data.iter().flat_map(|v| [*v, *v, *v]).collect(), ..img };
        let strip = to_jpeg(&rgb, 90).unwrap();
        let joined = assemble(Page::Jpeg(vec![strip.clone(), strip]), 300, 12).unwrap();
        assert_eq!((joined.width, joined.height, joined.channels), (16, 12, 3));
        assert_eq!(joined.data.len(), 16 * 12 * 3);
    }

    #[test]
    fn pdf_has_valid_xref() {
        let img = gray();
        let pdf = to_pdf(&to_jpeg(&img, 80).unwrap(), &img);
        let tail = String::from_utf8_lossy(&pdf);
        let startxref: usize = tail.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
        let xref = std::str::from_utf8(&pdf[startxref..]).unwrap();
        assert!(xref.starts_with("xref"));
        // Every xref offset points at "N 0 obj".
        for (i, line) in xref.lines().skip(3).take(5).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(pdf[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()));
        }
    }
}
