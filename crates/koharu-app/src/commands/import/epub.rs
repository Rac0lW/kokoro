use std::{fs::File, io::Read, path::Path};

use anyhow::{Context as _, Result, bail};
use roxmltree::{Document, Node};

use super::EncodedPage;

const CONTAINER_PATH: &str = "META-INF/container.xml";
const PACKAGE_MEDIA_TYPE: &str = "application/oebps-package+xml";
const XLINK_NAMESPACE: &str = "http://www.w3.org/1999/xlink";

/// `roxmltree` rejects a DTD by default. XHTML documents routinely declare the
/// XHTML DTD without relying on it, so the declaration has to be accepted.
fn parse_xml(text: &str) -> Result<Document<'_>> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    Ok(Document::parse_with_options(text, options)?)
}

/// An EPUB carries no order in its member names: the package document owns the
/// reading order through the spine, and each spine document names the one page
/// image it displays. Sorting archive members therefore cannot recover a
/// fixed-layout comic's page sequence, so the package is always read.
struct Container {
    archive: zip::ZipArchive<File>,
}

impl Container {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open EPUB container {}", path.display()))?;
        let archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read EPUB container {}", path.display()))?;
        Ok(Self { archive })
    }

    fn member(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let mut member = match self.archive.by_name(name) {
            Ok(member) => member,
            Err(::zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read EPUB member {name:?}"));
            }
        };
        let mut bytes = Vec::new();
        member
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read EPUB member {name:?}"))?;
        Ok(Some(bytes))
    }

    fn text(&mut self, name: &str) -> Result<Option<String>> {
        let Some(bytes) = self.member(name)? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes)
            .with_context(|| format!("EPUB member {name:?} is not UTF-8 encoded"))?;
        Ok(Some(text))
    }

    /// Resolves the package document through `META-INF/container.xml`. A handful
    /// of packers omit the container, in which case the only `.opf` member is the
    /// package document by construction.
    fn package_name(&mut self, path: &Path) -> Result<String> {
        if let Some(text) = self.text(CONTAINER_PATH)? {
            let document = parse_xml(&text).with_context(|| {
                format!(
                    "failed to parse the container document of {}",
                    path.display()
                )
            })?;
            let mut rootfiles = document
                .descendants()
                .filter(|node| node.is_element() && node.tag_name().name() == "rootfile")
                .filter_map(|node| {
                    Some((
                        node.attribute("media-type") == Some(PACKAGE_MEDIA_TYPE),
                        resolve("", node.attribute("full-path")?),
                    ))
                })
                .collect::<Vec<_>>();
            rootfiles.sort_by_key(|(preferred, _)| !*preferred);
            if let Some((_, rootfile)) = rootfiles.into_iter().next() {
                return Ok(rootfile);
            }
        }
        let fallback = self
            .archive
            .file_names()
            .filter_map(|name| name.ok())
            .find(|name| name.ends_with(".opf"))
            .map(|name| name.into_owned());
        fallback.with_context(|| format!("EPUB {} declares no package document", path.display()))
    }
}

/// Spine document names in reading order.
fn spine(container: &mut Container, name: &str, path: &Path) -> Result<Vec<String>> {
    let text = container.text(name)?.with_context(|| {
        format!(
            "EPUB package document {name:?} of {} is missing",
            path.display()
        )
    })?;
    let document = parse_xml(&text)
        .with_context(|| format!("failed to parse EPUB package document {name:?}"))?;
    let base = directory(name);

    // Manifest hrefs are relative to the package document, spine references are
    // manifest ids. `linear="no"` marks auxiliary content such as footnote
    // pop-ups, which are not part of the page sequence.
    let mut manifest = Vec::new();
    let mut spine = Vec::new();
    for node in document.descendants().filter(Node::is_element) {
        match node.tag_name().name() {
            "item" => {
                if let (Some(id), Some(href)) = (node.attribute("id"), node.attribute("href")) {
                    manifest.push((id, resolve(base, href)));
                }
            }
            "itemref" => {
                if node.attribute("linear") == Some("no") {
                    continue;
                }
                if let Some(idref) = node.attribute("idref") {
                    spine.push(idref);
                }
            }
            _ => {}
        }
    }
    let spine = spine
        .into_iter()
        .filter_map(|idref| {
            manifest
                .iter()
                .find(|(id, _)| *id == idref)
                .map(|(_, href)| href.clone())
        })
        .collect();
    Ok(spine)
}

/// Image references of one spine document, in document order and deduplicated.
/// A fixed-layout page is either an `<img>` or an SVG `<image>` wrapper around one.
fn document_images(container: &mut Container, name: &str) -> Result<Vec<String>> {
    let Some(text) = container.text(name)? else {
        return Ok(Vec::new());
    };
    let document = parse_xml(&text)
        .with_context(|| format!("failed to parse EPUB spine document {name:?}"))?;
    let base = directory(name);
    let mut references: Vec<String> = Vec::new();
    for node in document.descendants().filter(Node::is_element) {
        let href = match node.tag_name().name() {
            "img" => node.attribute("src"),
            "image" => node
                .attribute("href")
                .or_else(|| node.attribute((XLINK_NAMESPACE, "href"))),
            _ => None,
        };
        let Some(href) = href else {
            continue;
        };
        let reference = resolve(base, href);
        if !references.contains(&reference) {
            references.push(reference);
        }
    }
    Ok(references)
}

pub(super) fn extract(path: &Path) -> Result<Vec<EncodedPage>> {
    let mut container = Container::open(path)?;
    let package_name = container.package_name(path)?;
    let spine = spine(&mut container, &package_name, path)?;

    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "page".to_owned());
    let page_number_width = spine.len().to_string().len().max(3);
    let mut pages = Vec::new();
    for document in &spine {
        for reference in document_images(&mut container, document)? {
            let bytes = container.member(&reference)?.with_context(|| {
                format!(
                    "EPUB spine document {document:?} of {} references missing member {reference:?}",
                    path.display()
                )
            })?;
            // Spine documents also reference stylesheets, fonts, and navigation
            // material. Only members that decode as a raster belong to the page
            // sequence, which keeps this independent of the media-type list.
            if image::guess_format(&bytes).is_err() {
                continue;
            }
            let page_number = pages.len() + 1;
            pages.push(EncodedPage {
                name: format!(
                    "{stem}-{page_number:0page_number_width$}.{}",
                    extension(&reference),
                    page_number_width = page_number_width
                ),
                bytes,
            });
        }
    }
    if pages.is_empty() {
        bail!(
            "EPUB container {} contains no spine page images",
            path.display()
        );
    }
    Ok(pages)
}

fn directory(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn extension(name: &str) -> &str {
    name.rsplit_once('.')
        .map_or("png", |(_, extension)| extension)
}

/// Resolves an href against the directory of the document that referenced it.
/// Hrefs are URI references: percent-encoded, root-relative or traversing, and
/// possibly carrying a fragment, while archive members are plain names.
fn resolve(base: &str, href: &str) -> String {
    let href = href.split(['#', '?']).next().unwrap_or_default();
    let href = percent_decode(href);
    let joined = match href.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None if base.is_empty() => href,
        None => format!("{base}/{href}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in joined.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            component => parts.push(component),
        }
    }
    parts.join("/")
}

fn percent_decode(value: &str) -> String {
    fn digit(byte: u8) -> u8 {
        (byte as char).to_digit(16).unwrap_or_default() as u8
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            decoded.push(digit(bytes[index + 1]) << 4 | digit(bytes[index + 2]));
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;
    use std::{
        io::{Cursor, Write},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn sample_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode PNG");
        bytes.into_inner()
    }

    fn write_epub(entries: &[(&str, &[u8])]) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "koharu-import-{}-{timestamp}.epub",
            std::process::id()
        ));
        let file = File::create(&path).expect("create archive");
        let mut archive = ::zip::ZipWriter::new(file);
        let options = ::zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            archive.start_file(*name, options).expect("start member");
            archive.write_all(bytes).expect("write member");
        }
        archive.finish().expect("finish archive");
        path
    }

    // Member names are deliberately shuffled and traversal-shaped so the test
    // only passes when the spine, not the archive layout, drives the order.
    #[test]
    fn reads_pages_in_spine_order() {
        let png = sample_png();
        let page = |image: &str| {
            format!(
                "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><div><img src=\"../{image}\" /></div></body></html>"
            )
        };
        let first = page("image/moe-900013.png");
        let second = page("image/moe-000002.png");
        let end = page("image/moe-055555.png");
        let path = write_epub(&[
            (
                "META-INF/container.xml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="vol.opf" media-type="application/oebps-package+xml" /></rootfiles></container>"#,
            ),
            (
                "vol.opf",
                br#"<?xml version="1.0" encoding="UTF-8" ?><package version="3.0" xmlns="http://www.idpf.org/2007/opf"><metadata/><manifest><item id="nav" href="xml/nav.xhtml" media-type="application/xhtml+xml" /><item id="last"    href="html/z.html" media-type="application/xhtml+xml" /><item id="first" href="html/a.html" media-type="application/xhtml+xml" /><item id="second" href="html/m.html" media-type="application/xhtml+xml" /><item id="notes" href="html/notes.html" media-type="application/xhtml+xml" /></manifest><spine page-progression-direction="rtl"><itemref idref="first" /><itemref idref="second" /><itemref idref="last" /><itemref idref="notes" linear="no" /></spine></package>"#,
            ),
            ("html/a.html", first.as_bytes()),
            ("html/m.html", second.as_bytes()),
            ("html/z.html", end.as_bytes()),
            ("html/notes.html", first.as_bytes()),
            ("image/moe-900013.png", &png),
            ("image/moe-000002.png", &png),
            ("image/moe-055555.png", &png),
        ]);
        let pages = extract(&path).expect("read epub");
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .expect("fixture stem");
        std::fs::remove_file(&path).expect("remove archive");
        assert_eq!(
            pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            [
                format!("{stem}-001.png"),
                format!("{stem}-002.png"),
                format!("{stem}-003.png")
            ]
        );
        assert!(pages.iter().all(|page| page.bytes == png));
    }

    #[test]
    fn resolves_percent_encoded_and_svg_wrapped_references() {
        let png = sample_png();
        let path = write_epub(&[
            (
                "META-INF/container.xml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/package.opf" media-type="application/oebps-package+xml" /></rootfiles></container>"#,
            ),
            (
                "OEBPS/package.opf",
                br#"<?xml version="1.0" encoding="UTF-8" ?><package version="3.0" xmlns="http://www.idpf.org/2007/opf"><metadata/><manifest><item id="svg" href="text/page%201.xhtml" media-type="application/xhtml+xml" /><item id="cover" href="text/cover.xhtml" media-type="application/xhtml+xml" /></manifest><spine><itemref idref="cover" /><itemref idref="svg" /></spine></package>"#,
            ),
            (
                "OEBPS/text/page 1.xhtml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><html xmlns="http://www.w3.org/1999/xhtml"><body><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 2 3"><image width="2" height="3" xlink:href="../images/%E6%BC%AB%E7%94%BB.png#frag" /></svg></body></html>"#,
            ),
            (
                "OEBPS/text/cover.xhtml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><html xmlns="http://www.w3.org/1999/xhtml"><body><div><img src="/OEBPS/images/cover.png" /></div></body></html>"#,
            ),
            ("OEBPS/images/漫画.png", &png),
            ("OEBPS/images/cover.png", &png),
        ]);
        let pages = extract(&path).expect("read epub");
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .expect("fixture stem");
        std::fs::remove_file(&path).expect("remove archive");
        assert_eq!(
            pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            [format!("{stem}-001.png"), format!("{stem}-002.png")]
        );
    }

    #[test]
    fn rejects_epub_without_spine_images() {
        let path = write_epub(&[
            (
                "META-INF/container.xml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="vol.opf" media-type="application/oebps-package+xml" /></rootfiles></container>"#,
            ),
            (
                "vol.opf",
                br#"<?xml version="1.0" encoding="UTF-8" ?><package version="3.0" xmlns="http://www.idpf.org/2007/opf"><metadata/><manifest><item id="text" href="page.xhtml" media-type="application/xhtml+xml" /></manifest><spine><itemref idref="text" /></spine></package>"#,
            ),
            (
                "page.xhtml",
                br#"<?xml version="1.0" encoding="UTF-8" ?><html xmlns="http://www.w3.org/1999/xhtml"><body><p>text</p></body></html>"#,
            ),
        ]);
        let error = extract(&path).expect_err("epub should be rejected");
        std::fs::remove_file(&path).expect("remove archive");
        assert!(error.to_string().contains("no spine page images"));
    }
}
