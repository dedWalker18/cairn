//! Content extraction. Runs only inside sandboxed worker processes
//! (`cairnd worker extract`): parsers handle untrusted bytes, so a panic,
//! hang or memory blow-up here kills a disposable worker, never the daemon.

use std::fs::File;
use std::io::{BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::timeutil;

/// Hard cap on extracted text per file.
pub const MAX_TEXT_BYTES: usize = 1 << 20;
/// Hard cap on bytes read from a plain text file.
pub const MAX_READ_BYTES: u64 = 2 << 20;
/// Cap on uncompressed bytes read from any single zip member.
const MAX_ZIP_MEMBER: u64 = 16 << 20;
const PDF_TIMEOUT: Duration = Duration::from_secs(30);
pub const CLIP_SIZE: u32 = 224;

#[derive(Debug, Serialize, Deserialize)]
pub enum ExtractRequest {
    Text { path: Vec<u8>, ext: String },
    ImageMeta { path: Vec<u8> },
    ImagePixels { path: Vec<u8>, ext: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ImageMeta {
    pub taken_civil: Option<String>,
    pub taken_offset: Option<i32>,
    pub gps: Option<(f64, f64)>,
    pub orientation: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ExtractResponse {
    Text { text: String },
    ImageMeta(ImageMeta),
    /// RGB, CLIP_SIZE x CLIP_SIZE, carried as the binary payload.
    ImagePixels,
    Error { message: String },
}

/// Open read-only without following symlinks, without blocking on FIFOs and
/// without updating atime. Refuses anything that is not a regular file.
pub fn open_readonly(path: &Path) -> Result<File> {
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags | libc::O_NOATIME)
        .open(path)
        .or_else(|e| {
            // O_NOATIME is only allowed on files we own.
            if e.raw_os_error() == Some(libc::EPERM) {
                std::fs::OpenOptions::new().read(true).custom_flags(flags).open(path)
            } else {
                Err(e)
            }
        })
        .with_context(|| format!("open {}", path.display()))?;
    let meta = f.metadata()?;
    if !meta.file_type().is_file() {
        bail!("not a regular file");
    }
    Ok(f)
}

fn read_capped(f: &mut File, cap: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    crate::vecstore::drop_page_cache(f);
    Ok(buf)
}

fn cap_text(mut s: String) -> String {
    if s.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

pub fn handle(req: &ExtractRequest) -> (ExtractResponse, Vec<u8>) {
    let r = match req {
        ExtractRequest::Text { path, ext } => extract_text(&crate::paths::from_bytes(path), ext).map(|t| (ExtractResponse::Text { text: cap_text(t) }, Vec::new())),
        ExtractRequest::ImageMeta { path } => image_meta(&crate::paths::from_bytes(path)).map(|m| (ExtractResponse::ImageMeta(m), Vec::new())),
        ExtractRequest::ImagePixels { path, ext } => image_pixels(&crate::paths::from_bytes(path), ext).map(|px| (ExtractResponse::ImagePixels, px)),
    };
    match r {
        Ok(v) => v,
        Err(e) => (ExtractResponse::Error { message: format!("{e:#}") }, Vec::new()),
    }
}

pub fn extract_text(path: &Path, ext: &str) -> Result<String> {
    match ext {
        "pdf" => pdf_text(path),
        "docx" => zip_xml_text(path, &["word/document.xml"], &["p", "tr", "br"], None),
        "pptx" => zip_xml_text(path, &[], &["p"], Some("ppt/slides/slide")),
        "xlsx" => zip_xml_text(path, &["xl/workbook.xml", "xl/sharedStrings.xml"], &["si", "sheet"], None),
        "odt" | "odp" | "ods" => zip_xml_text(path, &["content.xml"], &["p", "h", "table-row"], None),
        "svg" => {
            let mut f = open_readonly(path)?;
            let bytes = read_capped(&mut f, MAX_READ_BYTES)?;
            Ok(xml_text(&bytes, &["text", "title", "desc", "tspan"]))
        }
        "step" | "stp" => {
            let t = plain_text(path)?;
            Ok(t.lines().filter(|l| l.contains("FILE_") || l.contains("PRODUCT(") || l.contains("SHAPE_REPRESENTATION")).collect::<Vec<_>>().join("\n"))
        }
        "dxf" => {
            // Keep human text; drop the sea of coordinates and group codes.
            let t = plain_text(path)?;
            Ok(t.lines()
                .map(str::trim)
                .filter(|l| l.len() > 2 && l.chars().any(|c| c.is_alphabetic()) && !l.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        _ => plain_text(path),
    }
}

fn plain_text(path: &Path) -> Result<String> {
    let mut f = open_readonly(path)?;
    let bytes = read_capped(&mut f, MAX_READ_BYTES)?;
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        bail!("binary content");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn pdf_text(path: &Path) -> Result<String> {
    // Verify it is a readable regular file before handing the path out.
    drop(open_readonly(path)?);
    let mut child = Command::new("pdftotext")
        .args(["-q", "-l", "60", "-enc", "UTF-8", "-nopgbrk"])
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running pdftotext (install poppler-utils)")?;
    let mut out = child.stdout.take().context("pdftotext stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = (&mut out).take(MAX_TEXT_BYTES as u64 + 1).read_to_end(&mut buf);
        let _ = tx.send(r.map(|_| buf));
    });
    let started = Instant::now();
    let text = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(r) => break r?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if started.elapsed() < PDF_TIMEOUT => continue,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("pdftotext timed out");
            }
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    Ok(String::from_utf8_lossy(&text).into_owned())
}

fn zip_xml_text(path: &Path, members: &[&str], para: &[&str], numbered_prefix: Option<&str>) -> Result<String> {
    let f = open_readonly(path)?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f)).context("not a valid zip container")?;
    let mut names: Vec<String> = members.iter().map(|s| s.to_string()).collect();
    if let Some(prefix) = numbered_prefix {
        let mut slides: Vec<(u32, String)> = zip
            .file_names()
            .filter_map(|n| {
                let rest = n.strip_prefix(prefix)?.strip_suffix(".xml")?;
                Some((rest.parse().ok()?, n.to_string()))
            })
            .collect();
        slides.sort();
        names.extend(slides.into_iter().take(1000).map(|(_, n)| n));
    }
    let mut out = String::new();
    for name in names {
        let Ok(mut member) = zip.by_name(&name) else { continue };
        let mut bytes = Vec::new();
        (&mut member).take(MAX_ZIP_MEMBER).read_to_end(&mut bytes)?;
        out.push_str(&xml_text(&bytes, para));
        out.push_str("\n\n");
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    Ok(out)
}

/// Concatenate text nodes; newline after elements whose local name is in `para`.
pub fn xml_text(bytes: &[u8], para: &[&str]) -> String {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    out.push_str(&s);
                }
            }
            Ok(Event::CData(t)) => out.push_str(&String::from_utf8_lossy(&t)),
            Ok(Event::End(e)) => {
                let name = e.local_name();
                if para.iter().any(|p| p.as_bytes() == name.as_ref()) {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                if name.as_ref() == b"br" || name.as_ref() == b"tab" {
                    out.push(' ');
                } else if name.as_ref() == b"sheet" {
                    // xlsx workbook: <sheet name="Q4 Sales" .../>
                    if let Ok(Some(a)) = e.try_get_attribute("name") {
                        if let Ok(v) = a.unescape_value() {
                            out.push_str(&v);
                            out.push('\n');
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    out
}

fn ascii_field(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let f = exif.get_field(tag, exif::In::PRIMARY)?;
    match &f.value {
        exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

fn gps_coord(exif: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag) -> Option<f64> {
    let f = exif.get_field(tag, exif::In::PRIMARY)?;
    let v = match &f.value {
        exif::Value::Rational(r) if r.len() >= 3 => r[0].to_f64() + r[1].to_f64() / 60.0 + r[2].to_f64() / 3600.0,
        _ => return None,
    };
    let sign = match ascii_field(exif, ref_tag).as_deref() {
        Some("S") | Some("W") => -1.0,
        _ => 1.0,
    };
    if v.is_finite() {
        Some(sign * v)
    } else {
        None
    }
}

pub fn image_meta(path: &Path) -> Result<ImageMeta> {
    let f = open_readonly(path)?;
    let mut r = BufReader::new(f);
    let exif = match exif::Reader::new().read_from_container(&mut r) {
        Ok(e) => e,
        Err(_) => {
            crate::vecstore::drop_page_cache(r.get_ref());
            return Ok(ImageMeta::default());
        }
    };
    crate::vecstore::drop_page_cache(r.get_ref());
    let mut m = ImageMeta::default();
    let dt = ascii_field(&exif, exif::Tag::DateTimeOriginal)
        .and_then(|s| timeutil::parse_exif_datetime(&s))
        .or_else(|| ascii_field(&exif, exif::Tag::DateTimeDigitized).and_then(|s| timeutil::parse_exif_datetime(&s)));
    m.taken_civil = dt.map(timeutil::civil_to_string);
    m.taken_offset = ascii_field(&exif, exif::Tag::OffsetTimeOriginal).and_then(|s| timeutil::parse_exif_offset(&s));
    let lat = gps_coord(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef);
    let lon = gps_coord(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef);
    if let (Some(a), Some(b)) = (lat, lon) {
        if a.abs() <= 90.0 && b.abs() <= 180.0 && !(a == 0.0 && b == 0.0) {
            m.gps = Some((a, b));
        }
    }
    m.orientation = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0));
    Ok(m)
}

fn decode_image(path: &Path, ext: &str) -> Result<image::RgbImage> {
    const MAX_DIM: u32 = 16384;
    if ext == "jpg" || ext == "jpeg" {
        // Decode at reduced scale straight from the DCT: 4-8x less work for
        // camera photos, which are the bulk of most photo folders.
        let f = open_readonly(path)?;
        let mut dec = jpeg_decoder::Decoder::new(BufReader::new(f));
        dec.read_info().context("jpeg header")?;
        let info = dec.info().context("jpeg info")?;
        if info.width as u32 > MAX_DIM || info.height as u32 > MAX_DIM {
            bail!("image too large");
        }
        let (w, h) = dec.scale(CLIP_SIZE as u16, CLIP_SIZE as u16)?;
        let px = dec.decode().context("jpeg decode")?;
        let info = dec.info().context("jpeg info")?;
        let (w, h) = (w as u32, h as u32);
        let rgb = match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => px,
            jpeg_decoder::PixelFormat::L8 => px.iter().flat_map(|v| [*v, *v, *v]).collect(),
            jpeg_decoder::PixelFormat::L16 => px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0]]).collect(),
            jpeg_decoder::PixelFormat::CMYK32 => px
                .chunks_exact(4)
                .flat_map(|c| {
                    let k = 255 - c[3] as u32;
                    [((255 - c[0] as u32) * k / 255) as u8, ((255 - c[1] as u32) * k / 255) as u8, ((255 - c[2] as u32) * k / 255) as u8]
                })
                .collect(),
        };
        return image::RgbImage::from_raw(w, h, rgb).context("jpeg buffer size mismatch");
    }
    let f = open_readonly(path)?;
    let mut reader = image::ImageReader::new(BufReader::new(f)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    Ok(reader.decode().context("decode")?.to_rgb8())
}

fn apply_orientation(img: image::RgbImage, o: Option<u32>) -> image::RgbImage {
    use image::imageops;
    match o {
        Some(2) => imageops::flip_horizontal(&img),
        Some(3) => imageops::rotate180(&img),
        Some(4) => imageops::flip_vertical(&img),
        Some(5) => imageops::flip_horizontal(&imageops::rotate90(&img)),
        Some(6) => imageops::rotate90(&img),
        Some(7) => imageops::flip_horizontal(&imageops::rotate270(&img)),
        Some(8) => imageops::rotate270(&img),
        _ => img,
    }
}

/// Decode, orient, resize shortest side to CLIP_SIZE, centre-crop.
pub fn image_pixels(path: &Path, ext: &str) -> Result<Vec<u8>> {
    let orientation = image_meta(path).ok().and_then(|m| m.orientation);
    let img = apply_orientation(decode_image(path, ext)?, orientation);
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image");
    }
    let s = CLIP_SIZE as f32 / w.min(h) as f32;
    let (nw, nh) = (((w as f32 * s).round() as u32).max(CLIP_SIZE), ((h as f32 * s).round() as u32).max(CLIP_SIZE));
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::CatmullRom);
    let (x, y) = ((nw - CLIP_SIZE) / 2, (nh - CLIP_SIZE) / 2);
    let crop = image::imageops::crop_imm(&resized, x, y, CLIP_SIZE, CLIP_SIZE).to_image();
    Ok(crop.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_paragraphs() {
        let x = br#"<w:document><w:body><w:p><w:r><w:t>Standard Operating</w:t></w:r><w:r><w:t> Procedure</w:t></w:r></w:p><w:p><w:r><w:t>Step &amp; one</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(xml_text(x, &["p"]), "Standard Operating Procedure\nStep & one\n");
    }

    #[test]
    fn malformed_xml_does_not_panic() {
        let _ = xml_text(b"<a><b>unterminated", &["b"]);
        let _ = xml_text(&[0xff, 0xfe, 0x00, 0x3c], &["b"]);
    }

    #[test]
    fn refuses_non_regular_files() {
        assert!(open_readonly(Path::new("/dev/null")).is_err());
        assert!(open_readonly(Path::new("/proc/self")).is_err());
    }
}
