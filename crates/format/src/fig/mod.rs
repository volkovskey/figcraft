//! Import of `.fig` files (best effort), written from public format knowledge only.
//!
//! A `.fig` file is a zip archive holding `canvas.fig` (the document), `meta.json`, a thumbnail
//! and `images/<hash>` files. `canvas.fig` (also accepted on its own) is an 8-byte `fig-…` magic,
//! a little-endian `u32` version and length-prefixed chunks, each compressed with deflate or
//! zstd: a binary schema in the kiwi format ([`kiwi`]), then a message encoded with it. Every file
//! carries its own schema, so the message is decoded generically and read by field name: a list
//! of node changes (each with its id, parent id and position among its siblings) that make up the
//! layer tree, plus blobs (vector outlines and editable vector networks, [`network`]). [`map`]
//! turns that into the REST API file JSON shape and the REST importer builds the document, so both
//! imports share one mapping. Layer kinds the editor doesn't have (sticky notes, connectors,
//! tables, …) come in as the closest frames, text and vectors. Anything not understood is skipped
//! with a warning; malformed input never panics.

#[cfg(test)]
mod decoder_regression_tests;
mod export;
#[cfg(test)]
mod export_tests;
#[cfg(test)]
mod interop_tests;
pub mod kiwi;
mod map;
mod network;
#[cfg(test)]
mod property_resource_regression_tests;
#[cfg(test)]
mod property_resource_tests;
pub(crate) mod property_resources;
#[cfg(test)]
mod source_identity_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod text_case_tests;
#[cfg(test)]
mod text_decoration_tests;
#[cfg(test)]
mod text_paragraph_tests;
#[cfg(test)]
mod text_round_trip_tests;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Read;

use figcraft_doc::Document;

pub use export::export;

use crate::FormatError;
use kiwi::Schema;

/// Largest `canvas.fig` member read, and largest decompressed chunk.
const MAX_CANVAS: u64 = 256 * 1024 * 1024;
const MAX_CHUNK: u64 = 512 * 1024 * 1024;
/// Largest image member read.
const MAX_IMAGE: u64 = 256 * 1024 * 1024;
/// Image members share a total budget; many individually valid members must not fill memory. A
/// real 705 MB file holds 562 MB of images, and a `.fig` file is read up to 2 GiB.
const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_IMAGES: usize = 4096;
/// Chunks read from `canvas.fig`: the schema and the message.
const USED_CHUNKS: usize = 2;

/// Whether `bytes` look like a `.fig` file: a zip holding `canvas.fig`, or a bare canvas.
pub fn is_fig(bytes: &[u8]) -> bool {
    bytes.starts_with(b"fig-") || (bytes.starts_with(b"PK") && zip_has_canvas(bytes))
}

fn zip_has_canvas(bytes: &[u8]) -> bool {
    zip::ZipArchive::new(std::io::Cursor::new(bytes)).is_ok_and(|z| z.file_names().any(|n| n == "canvas.fig"))
}

/// Import `bytes` when they are a `.fig` file; `None` when they are something else.
pub fn try_import(bytes: &[u8], name: &str) -> Option<Result<(Document, Vec<String>), FormatError>> {
    is_fig(bytes).then(|| import(bytes, name))
}

/// Build a document from a `.fig` file. Returns it with the warnings about what was skipped or
/// approximated. `name`: the document name when the file doesn't carry one.
pub fn import(bytes: &[u8], name: &str) -> Result<(Document, Vec<String>), FormatError> {
    let mut archive = Archive::open(bytes)?;
    // Each stage's input is dropped as soon as the next one is built: in a large file every one
    // of them takes gigabytes, and holding them all at once runs out of memory.
    let (schema, message) = decode_canvas(&std::mem::take(&mut archive.canvas))?;
    let root = schema.find("Message").ok_or_else(|| corrupt("the schema has no Message definition"))?;
    let msg = schema.decode_import_message(root, &message).map_err(|e| corrupt(e.to_string()))?;
    drop(message);
    let file_name = archive.file_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(name);
    let (json, mut warnings, networks) = map::to_rest(&msg, file_name)?;
    // Copied out so the message can go: only older files keep image bytes in it.
    let embedded: BTreeMap<String, Vec<u8>> = embedded_images(&msg, &archive.image_limits_hit).into_iter().map(|(k, v)| (k, v.to_vec())).collect();
    drop(msg);
    let images = |r: &str| archive.resolve_image(r, embedded.get(r).map(Vec::as_slice));
    let (doc, more) = crate::rest::import_fig(&json, &images, networks)?;
    warnings.extend(more.into_iter().filter_map(map::adapt_warning));
    if archive.image_limits_hit.get() {
        warnings
            .push(format!("image import is limited to {MAX_IMAGES} image refs and {MAX_IMAGE_BYTES} total bytes; remaining images are placeholders"));
    }
    Ok((doc, warnings))
}

/// Older canvases may store image bytes in `Message.blobs`, addressed by `Image.dataBlob`. Only
/// a paint's `image` is drawn (thumbnails and other image lists aren't), so only those count.
/// Keep borrowed slices here; the resolver applies its budgets before copying any image.
fn embedded_images<'m>(msg: &'m kiwi::Kv<'_>, limits_hit: &Cell<bool>) -> BTreeMap<String, &'m [u8]> {
    fn visit<'m>(v: &'m kiwi::Kv<'_>, blobs: &'m [kiwi::Kv<'_>], images: &mut BTreeMap<String, &'m [u8]>, limits_hit: &Cell<bool>) {
        match v {
            kiwi::Kv::Object(fields) => {
                for (name, field) in fields {
                    if *name == "image"
                        && let Some(hash) = field.get("hash").and_then(kiwi::Kv::bytes).filter(|h| !h.is_empty() && h.len() <= 64)
                        && let Some(kiwi::Kv::Uint(index)) = field.get("dataBlob")
                        && let Some(bytes) =
                            usize::try_from(*index).ok().and_then(|i| blobs.get(i)).and_then(|b| b.get("bytes")).and_then(kiwi::Kv::bytes)
                    {
                        let key: String = hash.iter().map(|b| format!("{b:02x}")).collect();
                        if images.len() < MAX_IMAGES || images.contains_key(&key) {
                            images.entry(key).or_insert(bytes);
                        } else {
                            limits_hit.set(true);
                        }
                    }
                    visit(field, blobs, images, limits_hit);
                }
            }
            kiwi::Kv::Array(values) => {
                for value in values {
                    visit(value, blobs, images, limits_hit);
                }
            }
            _ => {}
        }
    }
    let blobs = msg.get("blobs").map(kiwi::Kv::list).unwrap_or_default();
    let mut images = BTreeMap::new();
    for node in msg.get("nodeChanges").map(kiwi::Kv::list).unwrap_or_default() {
        visit(node, blobs, &mut images, limits_hit);
    }
    images
}

/// A font style under the name the format uses: the standard weight names as words ("SemiBold"
/// → "Semi Bold", "ExtraBoldItalic" → "Extra Bold Italic", "Regular Italic" → "Italic"). Other
/// names stay as they are.
fn style_name(style: &str) -> String {
    const WEIGHTS: [&str; 12] =
        ["Thin", "Extra Light", "Ultra Light", "Light", "Regular", "Medium", "Semi Bold", "Demi Bold", "Bold", "Extra Bold", "Ultra Bold", "Black"];
    let compact = |s: &str| s.chars().filter(|c| !matches!(c, ' ' | '-' | '_')).flat_map(char::to_lowercase).collect::<String>();
    let key = compact(style);
    let (base, italic) = match key.strip_suffix("italic") {
        Some(b) => (b, true),
        None => (key.as_str(), false),
    };
    let weight = if base.is_empty() && italic { Some("Regular") } else { WEIGHTS.into_iter().find(|w| compact(w) == base) };
    match (weight, italic) {
        (None, _) => style.to_string(),
        (Some("Regular"), true) => "Italic".into(),
        (Some(w), true) => format!("{w} Italic"),
        (Some(w), false) => w.into(),
    }
}

fn corrupt(msg: impl Into<String>) -> FormatError {
    FormatError::Corrupt(format!(".fig file: {}", msg.into()))
}

/// The parts of a `.fig` archive the import reads.
struct Archive<'b> {
    canvas: Vec<u8>,
    file_name: Option<String>,
    /// `images/<hash>` members (or nothing for a bare canvas).
    zip: Option<zip::ZipArchive<std::io::Cursor<&'b [u8]>>>,
    image_bytes: Cell<u64>,
    image_count: Cell<usize>,
    /// Image hashes looked up so far: whether each resolved.
    image_refs: RefCell<BTreeMap<String, bool>>,
    image_limits_hit: Cell<bool>,
}

impl<'b> Archive<'b> {
    fn open(bytes: &'b [u8]) -> Result<Self, FormatError> {
        // Only the canvas is capped as a whole; image members have their own budgets, so a large
        // archive with a small canvas still opens.
        if bytes.starts_with(b"fig-") {
            if bytes.len() as u64 > MAX_CANVAS {
                return Err(corrupt("canvas.fig is too large"));
            }
            return Ok(Archive {
                canvas: bytes.to_vec(),
                file_name: None,
                zip: None,
                image_bytes: Cell::new(0),
                image_count: Cell::new(0),
                image_refs: RefCell::new(BTreeMap::new()),
                image_limits_hit: Cell::new(false),
            });
        }
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| corrupt(e.to_string()))?;
        let canvas = crate::read_member(&mut z, "canvas.fig", MAX_CANVAS)?;
        let file_name = crate::read_member(&mut z, "meta.json", 1 << 20)
            .ok()
            .and_then(|m| serde_json::from_slice::<serde_json::Value>(&m).ok())
            .and_then(|m| m.get("file_name").and_then(serde_json::Value::as_str).map(str::to_string));
        Ok(Archive {
            canvas,
            file_name,
            zip: Some(z),
            image_bytes: Cell::new(0),
            image_count: Cell::new(0),
            image_refs: RefCell::new(BTreeMap::new()),
            image_limits_hit: Cell::new(false),
        })
    }

    /// Bytes of image `hash` (lowercase hex) from `images/`.
    #[cfg(test)]
    fn image(&self, hash: &str) -> Option<Vec<u8>> {
        self.resolve_image(hash, None)
    }

    /// Archive members take precedence; a missing or damaged member can use an embedded blob.
    fn resolve_image(&self, hash: &str, embedded: Option<&[u8]>) -> Option<Vec<u8>> {
        if hash.is_empty() || hash.len() > 128 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let known = self.image_refs.try_borrow().ok()?.get(hash).copied();
        // An image that failed once fails again: don't read it twice.
        if known == Some(false) {
            return None;
        }
        let seen = known == Some(true);
        let name = format!("images/{hash}");
        // A clone shares the parsed directory and the bytes; the resolver only has shared access.
        let mut z = self.zip.clone();
        let member_size = z.as_mut().and_then(|z| z.by_name(&name).ok().map(|member| member.size()));
        if member_size.is_none() && embedded.is_none() {
            return None;
        }
        if !seen {
            if self.image_count.get() >= MAX_IMAGES {
                self.image_limits_hit.set(true);
                return None;
            }
            // Charge the attempt, so reads that fail use up the count too.
            self.image_count.set(self.image_count.get() + 1);
        }
        let limit = if seen { MAX_IMAGE } else { MAX_IMAGE_BYTES.saturating_sub(self.image_bytes.get()).min(MAX_IMAGE) };
        let mut over = false;
        let from_member = match member_size {
            Some(size) if size > limit => {
                over = true;
                None
            }
            Some(_) => z.as_mut().and_then(|z| crate::read_member(z, &name, limit).ok()),
            None => None,
        };
        let bytes = from_member.or_else(|| match embedded {
            Some(b) if b.len() as u64 > limit => {
                over = true;
                None
            }
            b => b.map(<[u8]>::to_vec),
        });
        self.image_refs.try_borrow_mut().ok()?.insert(hash.to_owned(), bytes.is_some());
        match &bytes {
            // Multiple paints can share an image; charge its bytes only once.
            Some(b) if !seen => self.image_bytes.set(self.image_bytes.get() + b.len() as u64),
            None if over => self.image_limits_hit.set(true),
            _ => {}
        }
        bytes
    }
}

/// The schema and the raw message of a `canvas.fig`.
fn decode_canvas(canvas: &[u8]) -> Result<(Schema, Vec<u8>), FormatError> {
    let chunks = chunks(canvas)?;
    let mut it = chunks.into_iter();
    let (Some(schema), Some(message)) = (it.next(), it.next()) else {
        return Err(corrupt("canvas.fig needs a schema and a message chunk"));
    };
    let schema = Schema::decode(&inflate(schema)?).map_err(|e| corrupt(e.to_string()))?;
    Ok((schema, inflate(message)?))
}

/// The length-prefixed chunks after the 12-byte header that the import uses; later chunks and
/// trailing padding aren't read.
fn chunks(canvas: &[u8]) -> Result<Vec<&[u8]>, FormatError> {
    if canvas.len() < 12 || !canvas.starts_with(b"fig-") {
        return Err(corrupt("canvas.fig has no header"));
    }
    let mut out = Vec::new();
    let mut pos = 12usize;
    while pos < canvas.len() && out.len() < USED_CHUNKS {
        let len = canvas.get(pos..pos + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).ok_or_else(|| corrupt("truncated chunk"))?;
        let start = pos + 4;
        let end = start.checked_add(len as usize).filter(|e| *e <= canvas.len()).ok_or_else(|| corrupt("truncated chunk"))?;
        out.push(canvas.get(start..end).unwrap_or_default());
        pos = end;
    }
    Ok(out)
}

/// Decompress a chunk: zstd (by its magic number), else raw deflate, else zlib.
fn inflate(data: &[u8]) -> Result<Vec<u8>, FormatError> {
    fn capped(r: impl Read) -> Option<Vec<u8>> {
        crate::read_capped(r, MAX_CHUNK, 0).ok().flatten()
    }
    if data.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let mut src = data;
        let dec = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| corrupt(format!("zstd: {e}")))?;
        return capped(dec).ok_or_else(|| corrupt("damaged or oversized zstd chunk"));
    }
    capped(flate2::read::DeflateDecoder::new(data))
        .or_else(|| capped(flate2::read::ZlibDecoder::new(data)))
        .ok_or_else(|| corrupt("damaged or oversized deflate chunk"))
}

/// Decoded message `msg` as JSON (byte arrays as their length), for looking at files.
#[cfg(test)]
pub(crate) fn debug_json(v: &kiwi::Kv) -> serde_json::Value {
    use kiwi::Kv;
    use serde_json::{Value, json};
    match v {
        Kv::Bool(b) => Value::Bool(*b),
        Kv::Int(i) => json!(i),
        Kv::Uint(u) => json!(u),
        Kv::Float(f) => json!(f),
        Kv::Str(s) => json!(s),
        Kv::Bytes(b) => json!(format!("<{} bytes>", b.len())),
        Kv::Enum(e) => json!(e),
        Kv::Array(a) => Value::Array(a.iter().map(debug_json).collect()),
        Kv::Object(f) => Value::Object(f.iter().map(|(k, v)| (k.to_string(), debug_json(v))).collect()),
    }
}
