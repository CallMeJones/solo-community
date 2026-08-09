// SPDX-License-Identifier: Apache-2.0

//! Parse a document file by path: detect format, return normalized UTF-8 text.
//!
//! Format detection is registry-backed: extension chooses the extractor, then
//! strong magic bytes validate formats where the extension alone is unsafe
//! (`.pdf`, Office ZIP containers, and obvious PDF/ZIP masquerades).
//!
//! ## Backends
//!
//! - Plaintext / markdown / source code -> `std::fs::read_to_string` (must be
//!   valid UTF-8; latin-1 / shift-jis / etc. are rejected, matching the
//!   storage layer's UTF-8-only invariant).
//! - PDF -> [`pdf_extract::extract_text`] (pure-Rust, no C deps; quality is
//!   acceptable for text-bearing PDFs but degrades on scanned / image-only
//!   PDFs).
//! - HTML -> [`html2text::from_read`] with a deliberately huge wrap width
//!   (80 000 cols) so the chunker is not fed artificial line-breaks.
//! - CSV/TSV -> [`csv`] rows rendered as stable line-oriented text.
//! - XLSX -> [`calamine`] workbook sheets rendered as stable row text.
//! - DOCX -> [`quick_xml`] over `word/document.xml`, preserving paragraphs.
//! - PPTX -> [`quick_xml`] over slide XML parts, preserving slide/paragraph text.
//! - Images -> metadata text from safe format headers; OCR/captioning is a later
//!   optional extractor layer.
//! - BLEND -> metadata text from the stable 12-byte Blender file header
//!   (including Blender's zstd/gzip compressed save formats); full scene/object
//!   extraction is a later optional Blender-runtime layer.
//! - ZIP -> safe-mode archive manifest from central-directory metadata only; file
//!   contents are not extracted.
//! - 3D models -> safe metadata/count extraction for glTF/GLB, OBJ, and STL;
//!   geometry payloads are not expanded into chunks.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use calamine::Reader;
use quick_xml::{
    XmlVersion,
    escape::unescape,
    events::{BytesRef, BytesText, Event},
};

/// What [`parse_file`] returns on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDocument {
    pub text: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub extractor_name: String,
    pub extractor_version: String,
}

/// Errors surfaced from [`parse_file`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unsupported extension: {0}")]
    UnsupportedExtension(String),

    #[error("file is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PDF parse error: {0}")]
    Pdf(String),

    #[error("HTML parse error: {0}")]
    Html(String),

    #[error("CSV parse error: {0}")]
    Csv(String),

    #[error("spreadsheet parse error: {0}")]
    Spreadsheet(String),

    #[error("Office document parse error: {0}")]
    Office(String),

    #[error("presentation parse error: {0}")]
    Presentation(String),

    #[error("image parse error: {0}")]
    Image(String),

    #[error("Blender file parse error: {0}")]
    Blend(String),

    #[error("archive parse error: {0}")]
    Archive(String),

    #[error("3D model parse error: {0}")]
    Model(String),

    #[error("no extractable text for {mime_type}: {reason}")]
    NoExtractableText {
        mime_type: &'static str,
        reason: &'static str,
    },

    #[error(
        "file type mismatch for .{extension}: expected {expected_mime}, detected {detected_mime}"
    )]
    MimeMismatch {
        extension: String,
        expected_mime: String,
        detected_mime: String,
    },

    #[error("file is empty")]
    Empty,
}

/// Registered document extractors and their MIME types.
///
/// Anything outside this list returns [`ParseError::UnsupportedExtension`].
/// Extension matching is case-insensitive (lower-cased before lookup) so
/// `README.MD` and `Doc.PDF` work.
///
/// Keep this in sync with `default_allowed_extensions()` in
/// `crate::config::DocumentConfig`.
pub(crate) const REGISTRY: &[ExtractorSpec] = &[
    ExtractorSpec::new(
        &["md", "markdown"],
        "text/markdown",
        "markdown_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["txt"],
        "text/plain",
        "plaintext",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["rs"],
        "text/x-rust",
        "code_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["py"],
        "text/x-python",
        "code_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["toml"],
        "application/toml",
        "structured_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["yaml", "yml"],
        "application/yaml",
        "structured_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["json"],
        "application/json",
        "json_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["jsonl", "ndjson"],
        "application/x-ndjson",
        "json_text",
        ExtractorKind::Plaintext,
        None,
    ),
    ExtractorSpec::new(
        &["pdf"],
        "application/pdf",
        "pdf_text",
        ExtractorKind::Pdf,
        Some(MagicSignature::Pdf),
    ),
    ExtractorSpec::new(
        &["html", "htm"],
        "text/html",
        "html_text",
        ExtractorKind::Html,
        None,
    ),
    ExtractorSpec::new(
        &["csv"],
        "text/csv",
        "csv_table",
        ExtractorKind::Delimited { delimiter: b',' },
        None,
    ),
    ExtractorSpec::new(
        &["tsv"],
        "text/tab-separated-values",
        "tsv_table",
        ExtractorKind::Delimited { delimiter: b'\t' },
        None,
    ),
    ExtractorSpec::new(
        &["xlsx"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx_workbook",
        ExtractorKind::Xlsx,
        Some(MagicSignature::ZipPackage),
    ),
    ExtractorSpec::new(
        &["docx"],
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx_document",
        ExtractorKind::Docx,
        Some(MagicSignature::ZipPackage),
    ),
    ExtractorSpec::new(
        &["pptx"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx_deck",
        ExtractorKind::Pptx,
        Some(MagicSignature::ZipPackage),
    ),
    ExtractorSpec::new(
        &["png"],
        "image/png",
        "image_metadata",
        ExtractorKind::Image {
            format: ImageFormat::Png,
        },
        Some(MagicSignature::Png),
    ),
    ExtractorSpec::new(
        &["jpg", "jpeg"],
        "image/jpeg",
        "image_metadata",
        ExtractorKind::Image {
            format: ImageFormat::Jpeg,
        },
        Some(MagicSignature::Jpeg),
    ),
    ExtractorSpec::new(
        &["webp"],
        "image/webp",
        "image_metadata",
        ExtractorKind::Image {
            format: ImageFormat::Webp,
        },
        Some(MagicSignature::Webp),
    ),
    ExtractorSpec::new(
        &["tif", "tiff"],
        "image/tiff",
        "image_metadata",
        ExtractorKind::Image {
            format: ImageFormat::Tiff,
        },
        Some(MagicSignature::Tiff),
    ),
    ExtractorSpec::new(
        &["blend"],
        "application/x-blender",
        "blend_metadata",
        ExtractorKind::Blend,
        Some(MagicSignature::Blend),
    ),
    ExtractorSpec::new(
        &["zip"],
        "application/zip",
        "zip_manifest",
        ExtractorKind::ZipArchive,
        Some(MagicSignature::ZipPackage),
    ),
    ExtractorSpec::new(
        &["gltf"],
        "model/gltf+json",
        "model_metadata",
        ExtractorKind::Model {
            format: ModelFormat::GltfJson,
        },
        None,
    ),
    ExtractorSpec::new(
        &["glb"],
        "model/gltf-binary",
        "model_metadata",
        ExtractorKind::Model {
            format: ModelFormat::GlbBinary,
        },
        Some(MagicSignature::Glb),
    ),
    ExtractorSpec::new(
        &["obj"],
        "model/obj",
        "model_metadata",
        ExtractorKind::Model {
            format: ModelFormat::Obj,
        },
        None,
    ),
    ExtractorSpec::new(
        &["stl"],
        "model/stl",
        "model_metadata",
        ExtractorKind::Model {
            format: ModelFormat::Stl,
        },
        None,
    ),
];

pub const TEXT_EXTRACTOR_VERSION: &str = "v1";
pub const FALLBACK_BINARY_EXTRACTOR: &str = "fallback_binary";
pub const NO_EXTRACTABLE_TEXT_ERROR_MARKER: &str = "no extractable text";
const PDF_NO_TEXT_EXTRACTION_REASON: &str =
    "pdf text extractor returned no content; OCR/page rendering is not available";

const MAX_TABLE_SHEETS: usize = 64;
const MAX_TABLE_ROWS: usize = 100_000;
const MAX_TABLE_CELLS: usize = 1_000_000;
const MAX_EXTRACTED_TABLE_CHARS: usize = 2_000_000;
const MAX_OFFICE_ZIP_ENTRIES: usize = 10_000;
const MAX_OFFICE_UNCOMPRESSED_BYTES: u64 = 50 * 1024 * 1024;
const MAX_OFFICE_COMPRESSION_RATIO: u64 = 200;
const MAX_EXTRACTED_OFFICE_CHARS: usize = 2_000_000;
const MAX_PRESENTATION_SLIDES: usize = 512;
const BLEND_HEADER_LEN: usize = 12;
const MAX_ARCHIVE_ZIP_ENTRIES: usize = 10_000;
const MAX_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 50 * 1024 * 1024;
const MAX_ARCHIVE_COMPRESSION_RATIO: u64 = 200;
const MAX_EXTRACTED_ARCHIVE_CHARS: usize = 2_000_000;
const MAX_MODEL_JSON_BYTES: u64 = 10 * 1024 * 1024;
const MAX_MODEL_TEXT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_EXTRACTED_MODEL_CHARS: usize = 500_000;
const MAX_MODEL_LIST_ITEMS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractorSpec {
    extensions: &'static [&'static str],
    mime_type: &'static str,
    extractor_name: &'static str,
    kind: ExtractorKind,
    required_magic: Option<MagicSignature>,
}

impl ExtractorSpec {
    const fn new(
        extensions: &'static [&'static str],
        mime_type: &'static str,
        extractor_name: &'static str,
        kind: ExtractorKind,
        required_magic: Option<MagicSignature>,
    ) -> Self {
        Self {
            extensions,
            mime_type,
            extractor_name,
            kind,
            required_magic,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExtractorKind {
    Plaintext,
    Pdf,
    Html,
    Delimited { delimiter: u8 },
    Xlsx,
    Docx,
    Pptx,
    Image { format: ImageFormat },
    Blend,
    ZipArchive,
    Model { format: ModelFormat },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MagicSignature {
    Pdf,
    ZipPackage,
    Png,
    Jpeg,
    Webp,
    Tiff,
    Blend,
    Glb,
    Zstd,
    Gzip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Png,
    Jpeg,
    Webp,
    Tiff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFormat {
    GltfJson,
    GlbBinary,
    Obj,
    Stl,
}

/// Parse a file at `path`. Returns the normalized text + mime_type + raw byte
/// size of the source file (which is NOT the same as `text.len()` for PDF,
/// HTML, or Office-style container formats).
pub fn parse_file(path: &Path) -> Result<ParsedDocument, ParseError> {
    let (ext, extractor) = extractor_for_path(path)?;
    let byte_size = std::fs::metadata(path)?.len();
    let magic_prefix = read_magic_prefix(path)?;
    validate_magic(&ext, extractor, &magic_prefix)?;

    let text = match extractor.kind {
        ExtractorKind::Plaintext => parse_plaintext(path)?,
        ExtractorKind::Pdf => parse_pdf(path)?,
        ExtractorKind::Html => parse_html(path)?,
        ExtractorKind::Delimited { delimiter } => parse_delimited(path, delimiter)?,
        ExtractorKind::Xlsx => parse_xlsx(path)?,
        ExtractorKind::Docx => parse_docx(path)?,
        ExtractorKind::Pptx => parse_pptx(path)?,
        ExtractorKind::Image { format } => parse_image_metadata(path, extractor.mime_type, format)?,
        ExtractorKind::Blend => parse_blend_metadata(path, extractor.mime_type)?,
        ExtractorKind::ZipArchive => parse_zip_manifest(path, extractor.mime_type)?,
        ExtractorKind::Model { format } => parse_model_metadata(path, extractor.mime_type, format)?,
    };

    if text.trim().is_empty() {
        return Err(empty_extraction_error(extractor));
    }

    Ok(ParsedDocument {
        text,
        mime_type: extractor.mime_type.to_string(),
        byte_size,
        extractor_name: extractor.extractor_name.to_string(),
        extractor_version: TEXT_EXTRACTOR_VERSION.to_string(),
    })
}

fn empty_extraction_error(extractor: &ExtractorSpec) -> ParseError {
    match extractor.kind {
        ExtractorKind::Pdf => ParseError::NoExtractableText {
            mime_type: extractor.mime_type,
            reason: PDF_NO_TEXT_EXTRACTION_REASON,
        },
        _ => ParseError::Empty,
    }
}

pub fn mime_type_for_extension(extension: &str) -> Option<&'static str> {
    extractor_for_extension(extension).map(|extractor| extractor.mime_type)
}

pub fn extractor_name_for_mime(mime: &str) -> &'static str {
    REGISTRY
        .iter()
        .find(|extractor| extractor.mime_type == mime)
        .map(|extractor| extractor.extractor_name)
        .unwrap_or(FALLBACK_BINARY_EXTRACTOR)
}

fn extractor_for_path(path: &Path) -> Result<(String, &'static ExtractorSpec), ParseError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| ParseError::UnsupportedExtension(String::from("(no extension)")))?;
    let extractor = extractor_for_extension(&ext)
        .ok_or_else(|| ParseError::UnsupportedExtension(ext.clone()))?;
    Ok((ext, extractor))
}

fn extractor_for_extension(extension: &str) -> Option<&'static ExtractorSpec> {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    REGISTRY
        .iter()
        .find(|extractor| extractor.extensions.iter().any(|ext| *ext == extension))
}

fn read_magic_prefix(path: &Path) -> Result<Vec<u8>, ParseError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 8192];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn validate_magic(
    extension: &str,
    extractor: &ExtractorSpec,
    prefix: &[u8],
) -> Result<(), ParseError> {
    let detected = sniff_magic(prefix);
    if let Some(required) = extractor.required_magic {
        if !magic_satisfies(required, detected.map(|detected| detected.signature)) {
            return Err(ParseError::MimeMismatch {
                extension: extension.to_string(),
                expected_mime: extractor.mime_type.to_string(),
                detected_mime: detected
                    .map(|detected| detected.mime_type.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            });
        }
        return Ok(());
    }

    if let Some(detected) = detected
        && detected.is_strong_binary_conflict(extractor.mime_type)
    {
        return Err(ParseError::MimeMismatch {
            extension: extension.to_string(),
            expected_mime: extractor.mime_type.to_string(),
            detected_mime: detected.mime_type.to_string(),
        });
    }
    Ok(())
}

fn magic_satisfies(required: MagicSignature, detected: Option<MagicSignature>) -> bool {
    detected == Some(required)
        || matches!(
            (required, detected),
            (
                MagicSignature::Blend,
                Some(MagicSignature::Zstd | MagicSignature::Gzip)
            )
        )
}

#[derive(Debug, Clone, Copy)]
struct DetectedMagic {
    mime_type: &'static str,
    signature: MagicSignature,
}

impl DetectedMagic {
    fn is_strong_binary_conflict(self, expected_mime: &str) -> bool {
        match self.signature {
            MagicSignature::Pdf => expected_mime != "application/pdf",
            MagicSignature::ZipPackage => !is_zip_container_mime(expected_mime),
            MagicSignature::Png => expected_mime != "image/png",
            MagicSignature::Jpeg => expected_mime != "image/jpeg",
            MagicSignature::Webp => expected_mime != "image/webp",
            MagicSignature::Tiff => expected_mime != "image/tiff",
            MagicSignature::Blend => expected_mime != "application/x-blender",
            MagicSignature::Glb => expected_mime != "model/gltf-binary",
            MagicSignature::Zstd => expected_mime != "application/x-blender",
            MagicSignature::Gzip => expected_mime != "application/x-blender",
        }
    }
}

fn is_zip_container_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/zip"
    )
}

fn sniff_magic(prefix: &[u8]) -> Option<DetectedMagic> {
    if prefix.starts_with(b"%PDF-") {
        return Some(DetectedMagic {
            mime_type: "application/pdf",
            signature: MagicSignature::Pdf,
        });
    }
    if prefix.starts_with(b"PK\x03\x04")
        || prefix.starts_with(b"PK\x05\x06")
        || prefix.starts_with(b"PK\x07\x08")
    {
        return Some(DetectedMagic {
            mime_type: "application/zip",
            signature: MagicSignature::ZipPackage,
        });
    }
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(DetectedMagic {
            mime_type: "image/png",
            signature: MagicSignature::Png,
        });
    }
    if prefix.starts_with(b"\xff\xd8\xff") {
        return Some(DetectedMagic {
            mime_type: "image/jpeg",
            signature: MagicSignature::Jpeg,
        });
    }
    if prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP" {
        return Some(DetectedMagic {
            mime_type: "image/webp",
            signature: MagicSignature::Webp,
        });
    }
    if prefix.starts_with(b"II*\0") || prefix.starts_with(b"MM\0*") {
        return Some(DetectedMagic {
            mime_type: "image/tiff",
            signature: MagicSignature::Tiff,
        });
    }
    if prefix.starts_with(b"BLENDER") {
        return Some(DetectedMagic {
            mime_type: "application/x-blender",
            signature: MagicSignature::Blend,
        });
    }
    if prefix.starts_with(b"glTF") {
        return Some(DetectedMagic {
            mime_type: "model/gltf-binary",
            signature: MagicSignature::Glb,
        });
    }
    if prefix.starts_with(b"\x28\xb5\x2f\xfd") {
        return Some(DetectedMagic {
            mime_type: "application/zstd",
            signature: MagicSignature::Zstd,
        });
    }
    if prefix.starts_with(b"\x1f\x8b") {
        return Some(DetectedMagic {
            mime_type: "application/gzip",
            signature: MagicSignature::Gzip,
        });
    }
    None
}

fn parse_plaintext(path: &Path) -> Result<String, ParseError> {
    let bytes = std::fs::read(path)?;
    Ok(String::from_utf8(bytes)?)
}

fn parse_pdf(path: &Path) -> Result<String, ParseError> {
    pdf_extract::extract_text(path).map_err(|e| ParseError::Pdf(format!("{e}")))
}

fn parse_html(path: &Path) -> Result<String, ParseError> {
    let html = std::fs::read_to_string(path)?;
    html2text::from_read(html.as_bytes(), 80_000).map_err(|e| ParseError::Html(format!("{e}")))
}

fn parse_image_metadata(
    path: &Path,
    mime_type: &str,
    format: ImageFormat,
) -> Result<String, ParseError> {
    let bytes = std::fs::read(path)?;
    let (width, height) = image_dimensions(&bytes, format)?;
    let format_name = format.label();
    Ok(format!(
        "Image file\nFormat: {format_name}\nMIME type: {mime_type}\nDimensions: {width} x {height} pixels\nWidth: {width} pixels\nHeight: {height} pixels\n"
    ))
}

fn parse_blend_metadata(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let (header, compression) = read_blend_header(path)?;
    if !header.starts_with(b"BLENDER") {
        return Err(ParseError::Blend("invalid Blender header".to_string()));
    }
    let pointer_size = match header[7] {
        b'_' => "32-bit",
        b'-' => "64-bit",
        value => {
            return Err(ParseError::Blend(format!(
                "invalid pointer-size marker: 0x{value:02x}"
            )));
        }
    };
    let endian = match header[8] {
        b'v' => "little-endian",
        b'V' => "big-endian",
        value => {
            return Err(ParseError::Blend(format!(
                "invalid endian marker: 0x{value:02x}"
            )));
        }
    };
    let version = std::str::from_utf8(&header[9..12])
        .map_err(|e| ParseError::Blend(format!("version: {e}")))?;
    if !version.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseError::Blend(format!(
            "invalid Blender version: {version}"
        )));
    }
    let pretty_version = format!("{}.{}", &version[0..1], &version[1..3]);
    let compression_label = compression.label();
    Ok(format!(
        "Blender file\nFormat: Blender .blend\nMIME type: {mime_type}\nBlender version: {pretty_version}\nVersion code: {version}\nPointer size: {pointer_size}\nEndianness: {endian}\nCompression: {compression_label}\n"
    ))
}

fn read_blend_header(
    path: &Path,
) -> Result<([u8; BLEND_HEADER_LEN], BlendCompression), ParseError> {
    let prefix = read_magic_prefix(path)?;
    let compression = if prefix.starts_with(b"BLENDER") {
        BlendCompression::None
    } else if prefix.starts_with(b"\x28\xb5\x2f\xfd") {
        BlendCompression::Zstd
    } else if prefix.starts_with(b"\x1f\x8b") {
        BlendCompression::Gzip
    } else {
        return Err(ParseError::Blend("invalid Blender header".to_string()));
    };

    let mut header = [0u8; BLEND_HEADER_LEN];
    match compression {
        BlendCompression::None => {
            let mut file = File::open(path)?;
            file.read_exact(&mut header)
                .map_err(|e| ParseError::Blend(format!("header: {e}")))?;
        }
        BlendCompression::Zstd => {
            let file = File::open(path)?;
            let mut decoder = zstd::stream::read::Decoder::new(file)
                .map_err(|e| ParseError::Blend(format!("zstd header: {e}")))?;
            decoder
                .read_exact(&mut header)
                .map_err(|e| ParseError::Blend(format!("zstd header: {e}")))?;
        }
        BlendCompression::Gzip => {
            let file = File::open(path)?;
            let mut decoder = flate2::read::GzDecoder::new(file);
            decoder
                .read_exact(&mut header)
                .map_err(|e| ParseError::Blend(format!("gzip header: {e}")))?;
        }
    }
    Ok((header, compression))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlendCompression {
    None,
    Zstd,
    Gzip,
}

impl BlendCompression {
    fn label(self) -> &'static str {
        match self {
            BlendCompression::None => "none",
            BlendCompression::Zstd => "Zstandard",
            BlendCompression::Gzip => "Gzip",
        }
    }
}

fn parse_model_metadata(
    path: &Path,
    mime_type: &str,
    format: ModelFormat,
) -> Result<String, ParseError> {
    match format {
        ModelFormat::GltfJson => parse_gltf_json_metadata(path, mime_type),
        ModelFormat::GlbBinary => parse_glb_metadata(path, mime_type),
        ModelFormat::Obj => parse_obj_metadata(path, mime_type),
        ModelFormat::Stl => parse_stl_metadata(path, mime_type),
    }
}

fn parse_gltf_json_metadata(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_MODEL_JSON_BYTES {
        return Err(ParseError::Model(format!(
            "glTF JSON exceeds {MAX_MODEL_JSON_BYTES} bytes"
        )));
    }
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| ParseError::Model(format!("glTF JSON: {e}")))?;
    render_gltf_metadata(
        &value,
        GltfContainerMetadata {
            format_label: "glTF JSON",
            mime_type,
            glb_version: None,
            glb_declared_length: None,
            glb_json_chunk_bytes: None,
            glb_bin_chunk_bytes: None,
            glb_extra_chunk_count: None,
        },
    )
}

fn parse_glb_metadata(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let file_size = std::fs::metadata(path)?.len();
    let mut file = File::open(path)?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .map_err(|e| ParseError::Model(format!("GLB header: {e}")))?;
    if &header[0..4] != b"glTF" {
        return Err(ParseError::Model("invalid GLB magic".to_string()));
    }
    let version =
        read_le_u32(&header, 4).ok_or_else(|| ParseError::Model("GLB version".to_string()))?;
    let declared_length = read_le_u32(&header, 8)
        .ok_or_else(|| ParseError::Model("GLB declared length".to_string()))?;
    if u64::from(declared_length) != file_size {
        return Err(ParseError::Model(format!(
            "GLB declared length {declared_length} does not match file size {file_size}"
        )));
    }

    let mut offset = 12u64;
    let mut json_chunk = None;
    let mut json_chunk_bytes = 0u32;
    let mut bin_chunk_bytes = 0u64;
    let mut extra_chunk_count = 0usize;
    while offset < u64::from(declared_length) {
        let remaining = u64::from(declared_length) - offset;
        if remaining < 8 {
            return Err(ParseError::Model("truncated GLB chunk header".to_string()));
        }
        let mut chunk_header = [0u8; 8];
        file.read_exact(&mut chunk_header)
            .map_err(|e| ParseError::Model(format!("GLB chunk header: {e}")))?;
        offset += 8;
        let chunk_length = read_le_u32(&chunk_header, 0)
            .ok_or_else(|| ParseError::Model("GLB chunk length".to_string()))?;
        let chunk_type = &chunk_header[4..8];
        if u64::from(chunk_length) > u64::from(declared_length) - offset {
            return Err(ParseError::Model(format!(
                "GLB chunk length {chunk_length} exceeds remaining file bytes"
            )));
        }
        match chunk_type {
            b"JSON" => {
                if json_chunk.is_some() {
                    return Err(ParseError::Model(
                        "GLB has multiple JSON chunks".to_string(),
                    ));
                }
                if u64::from(chunk_length) > MAX_MODEL_JSON_BYTES {
                    return Err(ParseError::Model(format!(
                        "GLB JSON chunk exceeds {MAX_MODEL_JSON_BYTES} bytes"
                    )));
                }
                let mut bytes = vec![0u8; chunk_length as usize];
                file.read_exact(&mut bytes)
                    .map_err(|e| ParseError::Model(format!("GLB JSON chunk: {e}")))?;
                while bytes.last().is_some_and(|byte| *byte == b' ' || *byte == 0) {
                    bytes.pop();
                }
                json_chunk_bytes = chunk_length;
                json_chunk = Some(bytes);
            }
            b"BIN\0" => {
                bin_chunk_bytes = bin_chunk_bytes.saturating_add(u64::from(chunk_length));
                file.seek(SeekFrom::Current(i64::from(chunk_length)))
                    .map_err(|e| ParseError::Model(format!("GLB BIN chunk: {e}")))?;
            }
            _ => {
                extra_chunk_count = extra_chunk_count.saturating_add(1);
                file.seek(SeekFrom::Current(i64::from(chunk_length)))
                    .map_err(|e| ParseError::Model(format!("GLB extra chunk: {e}")))?;
            }
        }
        offset += u64::from(chunk_length);
    }

    let json_chunk =
        json_chunk.ok_or_else(|| ParseError::Model("GLB missing JSON chunk".to_string()))?;
    let value: serde_json::Value = serde_json::from_slice(&json_chunk)
        .map_err(|e| ParseError::Model(format!("GLB JSON chunk: {e}")))?;
    render_gltf_metadata(
        &value,
        GltfContainerMetadata {
            format_label: "GLB binary",
            mime_type,
            glb_version: Some(version),
            glb_declared_length: Some(declared_length),
            glb_json_chunk_bytes: Some(json_chunk_bytes),
            glb_bin_chunk_bytes: Some(bin_chunk_bytes),
            glb_extra_chunk_count: Some(extra_chunk_count),
        },
    )
}

#[derive(Debug, Clone, Copy)]
struct GltfContainerMetadata<'a> {
    format_label: &'a str,
    mime_type: &'a str,
    glb_version: Option<u32>,
    glb_declared_length: Option<u32>,
    glb_json_chunk_bytes: Option<u32>,
    glb_bin_chunk_bytes: Option<u64>,
    glb_extra_chunk_count: Option<usize>,
}

fn render_gltf_metadata(
    value: &serde_json::Value,
    container: GltfContainerMetadata<'_>,
) -> Result<String, ParseError> {
    let asset = value
        .get("asset")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ParseError::Model("glTF missing asset metadata".to_string()))?;
    let version = asset
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ParseError::Model("glTF missing asset.version".to_string()))?;

    let mut text = String::new();
    let mut chars = 0usize;
    push_limited_model_line(&mut text, &mut chars, "3D model\n".to_string())?;
    push_limited_model_line(
        &mut text,
        &mut chars,
        format!("Format: {}\n", container.format_label),
    )?;
    push_limited_model_line(
        &mut text,
        &mut chars,
        format!("MIME type: {}\n", container.mime_type),
    )?;
    push_limited_model_line(
        &mut text,
        &mut chars,
        format!("glTF version: {}\n", sanitize_model_field(version)),
    )?;
    if let Some(generator) = asset.get("generator").and_then(serde_json::Value::as_str) {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("Generator: {}\n", sanitize_model_field(generator)),
        )?;
    }
    if let Some(scene) = value.get("scene").and_then(serde_json::Value::as_u64) {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("Default scene index: {scene}\n"),
        )?;
    }

    for (key, label) in [
        ("scenes", "Scene"),
        ("nodes", "Node"),
        ("meshes", "Mesh"),
        ("materials", "Material"),
        ("animations", "Animation"),
        ("cameras", "Camera"),
        ("skins", "Skin"),
        ("images", "Image"),
    ] {
        push_json_name_lines(value, key, label, &mut text, &mut chars)?;
    }
    push_json_string_list_lines(
        value,
        "extensionsUsed",
        "Extension used",
        &mut text,
        &mut chars,
    )?;
    push_json_string_list_lines(
        value,
        "extensionsRequired",
        "Extension required",
        &mut text,
        &mut chars,
    )?;

    for (key, label) in [
        ("scenes", "Scene count"),
        ("nodes", "Node count"),
        ("meshes", "Mesh count"),
        ("materials", "Material count"),
        ("textures", "Texture count"),
        ("images", "Image count"),
        ("animations", "Animation count"),
        ("cameras", "Camera count"),
        ("skins", "Skin count"),
        ("buffers", "Buffer count"),
    ] {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("{label}: {}\n", json_array_len(value, key)),
        )?;
    }
    let total_buffer_bytes = value
        .get("buffers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|buffer| buffer.get("byteLength").and_then(serde_json::Value::as_u64))
        .fold(0u64, u64::saturating_add);
    push_limited_model_line(
        &mut text,
        &mut chars,
        format!("Total declared buffer bytes: {total_buffer_bytes}\n"),
    )?;

    if let Some(version) = container.glb_version {
        push_limited_model_line(&mut text, &mut chars, format!("GLB version: {version}\n"))?;
    }
    if let Some(length) = container.glb_declared_length {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("GLB declared length: {length} bytes\n"),
        )?;
    }
    if let Some(bytes) = container.glb_json_chunk_bytes {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("GLB JSON chunk bytes: {bytes}\n"),
        )?;
    }
    if let Some(bytes) = container.glb_bin_chunk_bytes {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("GLB BIN chunk bytes: {bytes}\n"),
        )?;
    }
    if let Some(count) = container.glb_extra_chunk_count {
        push_limited_model_line(
            &mut text,
            &mut chars,
            format!("GLB extra chunk count: {count}\n"),
        )?;
    }

    Ok(text)
}

fn parse_obj_metadata(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_MODEL_TEXT_BYTES {
        return Err(ParseError::Model(format!(
            "OBJ text exceeds {MAX_MODEL_TEXT_BYTES} bytes"
        )));
    }
    let text = String::from_utf8(std::fs::read(path)?)
        .map_err(|e| ParseError::Model(format!("OBJ is not valid UTF-8: {e}")))?;
    let mut objects = Vec::new();
    let mut groups = Vec::new();
    let mut material_libraries = Vec::new();
    let mut materials = Vec::new();
    let mut vertex_count = 0usize;
    let mut texcoord_count = 0usize;
    let mut normal_count = 0usize;
    let mut face_count = 0usize;
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("o ") {
            push_model_item(&mut objects, rest);
        } else if let Some(rest) = line.strip_prefix("g ") {
            push_model_item(&mut groups, rest);
        } else if let Some(rest) = line.strip_prefix("mtllib ") {
            push_model_item(&mut material_libraries, rest);
        } else if let Some(rest) = line.strip_prefix("usemtl ") {
            push_model_item(&mut materials, rest);
        } else if line.starts_with("v ") {
            vertex_count = vertex_count.saturating_add(1);
        } else if line.starts_with("vt ") {
            texcoord_count = texcoord_count.saturating_add(1);
        } else if line.starts_with("vn ") {
            normal_count = normal_count.saturating_add(1);
        } else if line.starts_with("f ") {
            face_count = face_count.saturating_add(1);
        }
    }

    let mut out = String::new();
    let mut chars = 0usize;
    push_limited_model_line(&mut out, &mut chars, "3D model\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, "Format: Wavefront OBJ\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, format!("MIME type: {mime_type}\n"))?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Object count: {}\n", objects.len()),
    )?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Group count: {}\n", groups.len()),
    )?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Vertex count: {vertex_count}\n"),
    )?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Texture coordinate count: {texcoord_count}\n"),
    )?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Normal count: {normal_count}\n"),
    )?;
    push_limited_model_line(&mut out, &mut chars, format!("Face count: {face_count}\n"))?;
    push_model_item_lines("Object", &objects, &mut out, &mut chars)?;
    push_model_item_lines("Group", &groups, &mut out, &mut chars)?;
    push_model_item_lines(
        "Material library",
        &material_libraries,
        &mut out,
        &mut chars,
    )?;
    push_model_item_lines("Material", &materials, &mut out, &mut chars)?;
    Ok(out)
}

fn parse_stl_metadata(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let size = std::fs::metadata(path)?.len();
    let prefix = read_magic_prefix(path)?;
    if prefix
        .get(..5)
        .is_some_and(|bytes| bytes.eq_ignore_ascii_case(b"solid"))
        && size <= MAX_MODEL_TEXT_BYTES
        && let Ok(text) = String::from_utf8(std::fs::read(path)?)
        && let Ok(metadata) = ascii_stl_metadata(&text)
    {
        return render_stl_ascii_metadata(mime_type, metadata);
    }
    render_stl_binary_metadata(path, mime_type, size)
}

#[derive(Debug, Clone)]
struct AsciiStlMetadata {
    name: Option<String>,
    facet_count: usize,
    vertex_line_count: usize,
}

fn ascii_stl_metadata(text: &str) -> Result<AsciiStlMetadata, ParseError> {
    let mut lines = text.lines();
    let first = lines
        .next()
        .map(str::trim)
        .ok_or_else(|| ParseError::Model("empty ASCII STL".to_string()))?;
    let Some(name) = first.strip_prefix("solid") else {
        return Err(ParseError::Model(
            "ASCII STL missing solid header".to_string(),
        ));
    };
    let mut facet_count = 0usize;
    let mut vertex_line_count = 0usize;
    let mut saw_end = false;
    for line in lines {
        let line = line.trim_start();
        if line.starts_with("facet normal") {
            facet_count = facet_count.saturating_add(1);
        } else if line.starts_with("vertex ") {
            vertex_line_count = vertex_line_count.saturating_add(1);
        } else if line.starts_with("endsolid") {
            saw_end = true;
        }
    }
    if facet_count == 0 || !saw_end {
        return Err(ParseError::Model("invalid ASCII STL".to_string()));
    }
    let name = sanitize_model_field(name);
    Ok(AsciiStlMetadata {
        name: (!name.is_empty()).then_some(name),
        facet_count,
        vertex_line_count,
    })
}

fn render_stl_ascii_metadata(
    mime_type: &str,
    metadata: AsciiStlMetadata,
) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut chars = 0usize;
    push_limited_model_line(&mut out, &mut chars, "3D model\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, "Format: STL ASCII\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, format!("MIME type: {mime_type}\n"))?;
    if let Some(name) = metadata.name {
        push_limited_model_line(&mut out, &mut chars, format!("Name: {name}\n"))?;
    }
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Facet count: {}\n", metadata.facet_count),
    )?;
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Vertex line count: {}\n", metadata.vertex_line_count),
    )?;
    Ok(out)
}

fn render_stl_binary_metadata(
    path: &Path,
    mime_type: &str,
    size: u64,
) -> Result<String, ParseError> {
    if size < 84 {
        return Err(ParseError::Model(
            "STL binary header is truncated".to_string(),
        ));
    }
    let mut file = File::open(path)?;
    let mut header = [0u8; 84];
    file.read_exact(&mut header)
        .map_err(|e| ParseError::Model(format!("STL binary header: {e}")))?;
    let triangle_count = read_le_u32(&header, 80)
        .ok_or_else(|| ParseError::Model("STL triangle count".to_string()))?;
    let expected = 84u64.saturating_add(u64::from(triangle_count).saturating_mul(50));
    if expected != size {
        return Err(ParseError::Model(format!(
            "STL binary size {size} does not match triangle count {triangle_count}"
        )));
    }
    let header_text = sanitize_model_field(&String::from_utf8_lossy(&header[..80]));
    let mut out = String::new();
    let mut chars = 0usize;
    push_limited_model_line(&mut out, &mut chars, "3D model\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, "Format: STL binary\n".to_string())?;
    push_limited_model_line(&mut out, &mut chars, format!("MIME type: {mime_type}\n"))?;
    if !header_text.is_empty() {
        push_limited_model_line(&mut out, &mut chars, format!("Header: {header_text}\n"))?;
    }
    push_limited_model_line(
        &mut out,
        &mut chars,
        format!("Triangle count: {triangle_count}\n"),
    )?;
    Ok(out)
}

fn json_array_len(value: &serde_json::Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

fn push_json_name_lines(
    value: &serde_json::Value,
    key: &str,
    label: &str,
    text: &mut String,
    chars: &mut usize,
) -> Result<(), ParseError> {
    let Some(items) = value.get(key).and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for item in items.iter().take(MAX_MODEL_LIST_ITEMS) {
        if let Some(name) = item.get("name").and_then(serde_json::Value::as_str) {
            push_limited_model_line(
                text,
                chars,
                format!("{label}: {}\n", sanitize_model_field(name)),
            )?;
        }
    }
    Ok(())
}

fn push_json_string_list_lines(
    value: &serde_json::Value,
    key: &str,
    label: &str,
    text: &mut String,
    chars: &mut usize,
) -> Result<(), ParseError> {
    let Some(items) = value.get(key).and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for item in items.iter().take(MAX_MODEL_LIST_ITEMS) {
        if let Some(name) = item.as_str() {
            push_limited_model_line(
                text,
                chars,
                format!("{label}: {}\n", sanitize_model_field(name)),
            )?;
        }
    }
    Ok(())
}

fn push_model_item(items: &mut Vec<String>, value: &str) {
    if items.len() >= MAX_MODEL_LIST_ITEMS {
        return;
    }
    let value = sanitize_model_field(value);
    if !value.is_empty() && !items.iter().any(|item| item == &value) {
        items.push(value);
    }
}

fn push_model_item_lines(
    label: &str,
    items: &[String],
    text: &mut String,
    chars: &mut usize,
) -> Result<(), ParseError> {
    for item in items {
        push_limited_model_line(text, chars, format!("{label}: {item}\n"))?;
    }
    Ok(())
}

fn push_limited_model_line(
    text: &mut String,
    chars: &mut usize,
    line: String,
) -> Result<(), ParseError> {
    *chars = chars.saturating_add(line.chars().count());
    if *chars > MAX_EXTRACTED_MODEL_CHARS {
        return Err(ParseError::Model(format!(
            "model metadata limit exceeded: max {MAX_EXTRACTED_MODEL_CHARS} chars"
        )));
    }
    text.push_str(&line);
    Ok(())
}

fn sanitize_model_field(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = false;
    for ch in value.trim_matches(char::from(0)).trim().chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
        if out.len() >= 200 {
            break;
        }
    }
    out.trim().to_string()
}

impl ImageFormat {
    fn label(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Webp => "WebP",
            ImageFormat::Tiff => "TIFF",
        }
    }
}

fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), ParseError> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Webp => webp_dimensions(bytes),
        ImageFormat::Tiff => tiff_dimensions(bytes),
    }
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), ParseError> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16] != b"IHDR" {
        return Err(ParseError::Image("invalid PNG header".to_string()));
    }
    let width = read_be_u32(bytes, 16).ok_or_else(|| ParseError::Image("PNG width".to_string()))?;
    let height =
        read_be_u32(bytes, 20).ok_or_else(|| ParseError::Image("PNG height".to_string()))?;
    nonzero_dimensions(width, height, "PNG")
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), ParseError> {
    if !bytes.starts_with(b"\xff\xd8") {
        return Err(ParseError::Image("invalid JPEG header".to_string()));
    }
    let mut offset = 2usize;
    while offset + 4 <= bytes.len() {
        while offset < bytes.len() && bytes[offset] != 0xff {
            offset = offset.saturating_add(1);
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset = offset.saturating_add(1);
        }
        if offset >= bytes.len() {
            break;
        }
        let marker = bytes[offset];
        offset = offset.saturating_add(1);
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > bytes.len() {
            break;
        }
        let segment_len = usize::from(
            read_be_u16(bytes, offset)
                .ok_or_else(|| ParseError::Image("JPEG segment length".to_string()))?,
        );
        if segment_len < 2 || offset + segment_len > bytes.len() {
            return Err(ParseError::Image("invalid JPEG segment length".to_string()));
        }
        if is_jpeg_sof_marker(marker) {
            if segment_len < 7 || offset + 7 > bytes.len() {
                return Err(ParseError::Image("invalid JPEG SOF segment".to_string()));
            }
            let height = u32::from(
                read_be_u16(bytes, offset + 3)
                    .ok_or_else(|| ParseError::Image("JPEG height".to_string()))?,
            );
            let width = u32::from(
                read_be_u16(bytes, offset + 5)
                    .ok_or_else(|| ParseError::Image("JPEG width".to_string()))?,
            );
            return nonzero_dimensions(width, height, "JPEG");
        }
        offset += segment_len;
    }
    Err(ParseError::Image(
        "JPEG dimensions not found before image data".to_string(),
    ))
}

fn is_jpeg_sof_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32), ParseError> {
    if bytes.len() < 20 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WEBP" {
        return Err(ParseError::Image("invalid WebP header".to_string()));
    }
    match &bytes[12..16] {
        b"VP8X" => {
            if bytes.len() < 30 {
                return Err(ParseError::Image("truncated WebP VP8X header".to_string()));
            }
            let width = read_le_u24(bytes, 24)
                .ok_or_else(|| ParseError::Image("WebP width".to_string()))?
                .saturating_add(1);
            let height = read_le_u24(bytes, 27)
                .ok_or_else(|| ParseError::Image("WebP height".to_string()))?
                .saturating_add(1);
            nonzero_dimensions(width, height, "WebP")
        }
        b"VP8L" => {
            if bytes.len() < 25 || bytes[20] != 0x2f {
                return Err(ParseError::Image("truncated WebP VP8L header".to_string()));
            }
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3fff).saturating_add(1);
            let height = ((bits >> 14) & 0x3fff).saturating_add(1);
            nonzero_dimensions(width, height, "WebP")
        }
        b"VP8 " => {
            if bytes.len() < 30 || &bytes[23..26] != b"\x9d\x01\x2a" {
                return Err(ParseError::Image("truncated WebP VP8 header".to_string()));
            }
            let width = u32::from(
                read_le_u16(bytes, 26)
                    .ok_or_else(|| ParseError::Image("WebP width".to_string()))?
                    & 0x3fff,
            );
            let height = u32::from(
                read_le_u16(bytes, 28)
                    .ok_or_else(|| ParseError::Image("WebP height".to_string()))?
                    & 0x3fff,
            );
            nonzero_dimensions(width, height, "WebP")
        }
        _ => Err(ParseError::Image("unsupported WebP chunk".to_string())),
    }
}

fn tiff_dimensions(bytes: &[u8]) -> Result<(u32, u32), ParseError> {
    let endian = if bytes.starts_with(b"II*\0") {
        TiffEndian::Little
    } else if bytes.starts_with(b"MM\0*") {
        TiffEndian::Big
    } else {
        return Err(ParseError::Image("invalid TIFF header".to_string()));
    };
    let ifd_offset = endian
        .read_u32(bytes, 4)
        .ok_or_else(|| ParseError::Image("TIFF IFD offset".to_string()))?
        as usize;
    let entry_count = usize::from(
        endian
            .read_u16(bytes, ifd_offset)
            .ok_or_else(|| ParseError::Image("TIFF IFD entry count".to_string()))?,
    );
    let entries_start = ifd_offset.saturating_add(2);
    let entries_len = entry_count
        .checked_mul(12)
        .ok_or_else(|| ParseError::Image("TIFF IFD entry count overflow".to_string()))?;
    if entries_start
        .checked_add(entries_len)
        .is_none_or(|end| end > bytes.len())
    {
        return Err(ParseError::Image("truncated TIFF IFD".to_string()));
    }

    let mut width = None;
    let mut height = None;
    for idx in 0..entry_count {
        let entry = entries_start + idx * 12;
        let tag = endian
            .read_u16(bytes, entry)
            .ok_or_else(|| ParseError::Image("TIFF tag".to_string()))?;
        if tag == 256 || tag == 257 {
            let value = tiff_ifd_scalar_value(bytes, entry, endian)?;
            if tag == 256 {
                width = Some(value);
            } else {
                height = Some(value);
            }
        }
    }
    let width = width.ok_or_else(|| ParseError::Image("TIFF width tag missing".to_string()))?;
    let height = height.ok_or_else(|| ParseError::Image("TIFF height tag missing".to_string()))?;
    nonzero_dimensions(width, height, "TIFF")
}

fn tiff_ifd_scalar_value(
    bytes: &[u8],
    entry: usize,
    endian: TiffEndian,
) -> Result<u32, ParseError> {
    let field_type = endian
        .read_u16(bytes, entry + 2)
        .ok_or_else(|| ParseError::Image("TIFF field type".to_string()))?;
    let count = endian
        .read_u32(bytes, entry + 4)
        .ok_or_else(|| ParseError::Image("TIFF value count".to_string()))?;
    if count != 1 {
        return Err(ParseError::Image(
            "TIFF dimension tag must contain one value".to_string(),
        ));
    }
    match field_type {
        3 => endian
            .read_u16(bytes, entry + 8)
            .map(u32::from)
            .ok_or_else(|| ParseError::Image("TIFF SHORT value".to_string())),
        4 => endian
            .read_u32(bytes, entry + 8)
            .ok_or_else(|| ParseError::Image("TIFF LONG value".to_string())),
        _ => Err(ParseError::Image(
            "TIFF dimension tag must be SHORT or LONG".to_string(),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
enum TiffEndian {
    Little,
    Big,
}

impl TiffEndian {
    fn read_u16(self, bytes: &[u8], offset: usize) -> Option<u16> {
        let data = bytes.get(offset..offset + 2)?;
        Some(match self {
            TiffEndian::Little => u16::from_le_bytes([data[0], data[1]]),
            TiffEndian::Big => u16::from_be_bytes([data[0], data[1]]),
        })
    }

    fn read_u32(self, bytes: &[u8], offset: usize) -> Option<u32> {
        let data = bytes.get(offset..offset + 4)?;
        Some(match self {
            TiffEndian::Little => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            TiffEndian::Big => u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        })
    }
}

fn nonzero_dimensions(width: u32, height: u32, format: &str) -> Result<(u32, u32), ParseError> {
    if width == 0 || height == 0 {
        return Err(ParseError::Image(format!(
            "{format} dimensions must be non-zero"
        )));
    }
    Ok((width, height))
}

fn read_be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let data = bytes.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([data[0], data[1]]))
}

fn read_le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let data = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([data[0], data[1]]))
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_le_u24(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 3)?;
    Some(u32::from(data[0]) | (u32::from(data[1]) << 8) | (u32::from(data[2]) << 16))
}

fn parse_delimited(path: &Path, delimiter: u8) -> Result<String, ParseError> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .has_headers(false)
        .from_path(path)
        .map_err(|e| ParseError::Csv(format!("{e}")))?;
    let mut text = String::new();
    let mut chars = 0usize;
    let mut cells_seen = 0usize;
    for (idx, record) in rdr.records().enumerate() {
        let record = record.map_err(|e| ParseError::Csv(format!("{e}")))?;
        if idx >= MAX_TABLE_ROWS {
            return Err(ParseError::Csv(format!(
                "row limit exceeded: max {MAX_TABLE_ROWS} rows"
            )));
        }
        cells_seen = cells_seen.saturating_add(record.len());
        if cells_seen > MAX_TABLE_CELLS {
            return Err(ParseError::Csv(format!(
                "cell limit exceeded: max {MAX_TABLE_CELLS} cells"
            )));
        }
        if record.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let cells = record.iter().map(str::trim).collect::<Vec<_>>();
        push_limited_table_line(
            &mut text,
            &mut chars,
            format!("row {}: {}\n", idx + 1, cells.join(" | ")),
            TableSource::Delimited,
        )?;
    }
    Ok(text)
}

fn parse_xlsx(path: &Path) -> Result<String, ParseError> {
    let _ = validate_office_package(path, OfficePackageKind::Xlsx)?;
    let mut workbook =
        calamine::open_workbook_auto(path).map_err(|e| ParseError::Spreadsheet(format!("{e}")))?;
    let sheet_names = workbook.sheet_names().to_owned();
    if sheet_names.len() > MAX_TABLE_SHEETS {
        return Err(ParseError::Spreadsheet(format!(
            "sheet limit exceeded: max {MAX_TABLE_SHEETS} sheets"
        )));
    }
    let mut text = String::new();
    let mut chars = 0usize;
    let mut rows_seen = 0usize;
    let mut cells_seen = 0usize;

    for sheet_name in sheet_names {
        let range = workbook
            .worksheet_range(&sheet_name)
            .map_err(|e| ParseError::Spreadsheet(format!("{sheet_name}: {e}")))?;
        let mut wrote_sheet = false;
        for (row_idx, row) in range.rows().enumerate() {
            rows_seen = rows_seen.saturating_add(1);
            if rows_seen > MAX_TABLE_ROWS {
                return Err(ParseError::Spreadsheet(format!(
                    "row limit exceeded: max {MAX_TABLE_ROWS} rows"
                )));
            }
            cells_seen = cells_seen.saturating_add(row.len());
            if cells_seen > MAX_TABLE_CELLS {
                return Err(ParseError::Spreadsheet(format!(
                    "cell limit exceeded: max {MAX_TABLE_CELLS} cells"
                )));
            }
            let cells = trim_trailing_empty_cells(
                row.iter()
                    .map(spreadsheet_cell_text)
                    .collect::<Vec<String>>(),
            );
            if cells.iter().all(|cell| cell.trim().is_empty()) {
                continue;
            }
            if !wrote_sheet {
                push_limited_table_line(
                    &mut text,
                    &mut chars,
                    format!("Sheet: {sheet_name}\n"),
                    TableSource::Xlsx,
                )?;
                wrote_sheet = true;
            }
            push_limited_table_line(
                &mut text,
                &mut chars,
                format!("R{}: {}\n", row_idx + 1, cells.join(" | ")),
                TableSource::Xlsx,
            )?;
        }
    }

    Ok(text)
}

fn parse_docx(path: &Path) -> Result<String, ParseError> {
    let mut archive = validate_office_package(path, OfficePackageKind::Docx)?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|e| ParseError::Office(format!("word/document.xml: {e}")))?
        .read_to_string(&mut xml)
        .map_err(|e| ParseError::Office(format!("word/document.xml: {e}")))?;
    parse_docx_document_xml(&xml)
}

fn parse_pptx(path: &Path) -> Result<String, ParseError> {
    let mut archive = validate_office_package(path, OfficePackageKind::Pptx)?;
    let slide_parts = pptx_slide_parts(&mut archive)?;
    if slide_parts.is_empty() {
        return Err(ParseError::Presentation(
            "pptx package missing slide parts".to_string(),
        ));
    }
    if slide_parts.len() > MAX_PRESENTATION_SLIDES {
        return Err(ParseError::Presentation(format!(
            "slide limit exceeded: max {MAX_PRESENTATION_SLIDES} slides"
        )));
    }

    let mut text = String::new();
    let mut chars = 0usize;
    for slide_part in &slide_parts {
        let mut xml = String::new();
        archive
            .by_name(&slide_part.part_name)
            .map_err(|e| ParseError::Presentation(format!("{}: {e}", slide_part.part_name)))?
            .read_to_string(&mut xml)
            .map_err(|e| ParseError::Presentation(format!("{}: {e}", slide_part.part_name)))?;
        let slide_text = parse_pptx_slide_xml(&xml)?;
        if slide_text.trim().is_empty() {
            continue;
        }
        push_limited_office_fragment(
            &mut text,
            &mut chars,
            &format!("Slide {}:\n", slide_part.display_number),
            OfficePackageKind::Pptx,
        )?;
        push_limited_office_fragment(&mut text, &mut chars, &slide_text, OfficePackageKind::Pptx)?;
    }
    Ok(text)
}

fn parse_zip_manifest(path: &Path, mime_type: &str) -> Result<String, ParseError> {
    let file = File::open(path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| ParseError::Archive(format!("{e}")))?;
    if archive.len() > MAX_ARCHIVE_ZIP_ENTRIES {
        return Err(ParseError::Archive(format!(
            "zip archive entry limit exceeded: max {MAX_ARCHIVE_ZIP_ENTRIES} entries"
        )));
    }

    let mut entries = Vec::with_capacity(archive.len());
    let mut total_uncompressed = 0u64;
    let mut total_compressed = 0u64;
    for idx in 0..archive.len() {
        let file = archive
            .by_index(idx)
            .map_err(|e| ParseError::Archive(format!("zip archive entry {idx}: {e}")))?;
        let name =
            safe_zip_entry_name(file.name(), file.enclosed_name().is_some()).map_err(|entry| {
                ParseError::Archive(format!("zip archive entry has unsafe path: {entry}"))
            })?;
        total_uncompressed = total_uncompressed.saturating_add(file.size());
        total_compressed = total_compressed.saturating_add(file.compressed_size());
        if total_uncompressed > MAX_ARCHIVE_UNCOMPRESSED_BYTES {
            return Err(ParseError::Archive(format!(
                "zip archive uncompressed size exceeds {MAX_ARCHIVE_UNCOMPRESSED_BYTES} bytes"
            )));
        }
        entries.push(ArchiveEntryManifest {
            name,
            kind: if file.is_dir() { "directory" } else { "file" },
            uncompressed_bytes: file.size(),
            compressed_bytes: file.compressed_size(),
            compression: format!("{:?}", file.compression()),
            encrypted: file.encrypted(),
        });
    }

    if (total_compressed == 0 && total_uncompressed > 0)
        || (total_compressed > 0
            && total_uncompressed > total_compressed.saturating_mul(MAX_ARCHIVE_COMPRESSION_RATIO))
    {
        return Err(ParseError::Archive(format!(
            "zip archive compression ratio exceeds {MAX_ARCHIVE_COMPRESSION_RATIO}:1"
        )));
    }

    let mut text = String::new();
    let mut chars = 0usize;
    push_limited_archive_line(&mut text, &mut chars, "Archive file\n".to_string())?;
    push_limited_archive_line(&mut text, &mut chars, "Format: ZIP archive\n".to_string())?;
    push_limited_archive_line(&mut text, &mut chars, format!("MIME type: {mime_type}\n"))?;
    push_limited_archive_line(
        &mut text,
        &mut chars,
        format!("Entries: {}\n", entries.len()),
    )?;
    push_limited_archive_line(
        &mut text,
        &mut chars,
        format!("Total uncompressed bytes: {total_uncompressed}\n"),
    )?;
    push_limited_archive_line(
        &mut text,
        &mut chars,
        format!("Total compressed bytes: {total_compressed}\n"),
    )?;
    for (idx, entry) in entries.iter().enumerate() {
        push_limited_archive_line(
            &mut text,
            &mut chars,
            format!(
                "Entry {}: {} | {} | uncompressed bytes: {} | compressed bytes: {} | method: {} | encrypted: {}\n",
                idx + 1,
                entry.name,
                entry.kind,
                entry.uncompressed_bytes,
                entry.compressed_bytes,
                entry.compression,
                entry.encrypted
            ),
        )?;
    }
    Ok(text)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveEntryManifest {
    name: String,
    kind: &'static str,
    uncompressed_bytes: u64,
    compressed_bytes: u64,
    compression: String,
    encrypted: bool,
}

fn push_limited_archive_line(
    text: &mut String,
    chars: &mut usize,
    line: String,
) -> Result<(), ParseError> {
    *chars = chars.saturating_add(line.chars().count());
    if *chars > MAX_EXTRACTED_ARCHIVE_CHARS {
        return Err(ParseError::Archive(format!(
            "archive manifest limit exceeded: max {MAX_EXTRACTED_ARCHIVE_CHARS} chars"
        )));
    }
    text.push_str(&line);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum OfficePackageKind {
    Xlsx,
    Docx,
    Pptx,
}

impl OfficePackageKind {
    fn label(self) -> &'static str {
        match self {
            OfficePackageKind::Xlsx => "xlsx",
            OfficePackageKind::Docx => "docx",
            OfficePackageKind::Pptx => "pptx",
        }
    }

    fn required_part(self) -> &'static str {
        match self {
            OfficePackageKind::Xlsx => "xl/workbook.xml",
            OfficePackageKind::Docx => "word/document.xml",
            OfficePackageKind::Pptx => "ppt/presentation.xml",
        }
    }

    fn missing_message(self) -> &'static str {
        match self {
            OfficePackageKind::Xlsx => "xlsx package missing required workbook metadata",
            OfficePackageKind::Docx => "docx package missing required document metadata",
            OfficePackageKind::Pptx => "pptx package missing required presentation metadata",
        }
    }

    fn content_label(self) -> &'static str {
        match self {
            OfficePackageKind::Xlsx => "workbook",
            OfficePackageKind::Docx => "document",
            OfficePackageKind::Pptx => "slide",
        }
    }

    fn parse_error(self, message: String) -> ParseError {
        match self {
            OfficePackageKind::Xlsx => ParseError::Spreadsheet(message),
            OfficePackageKind::Docx => ParseError::Office(message),
            OfficePackageKind::Pptx => ParseError::Presentation(message),
        }
    }
}

fn validate_office_package(
    path: &Path,
    kind: OfficePackageKind,
) -> Result<zip::ZipArchive<File>, ParseError> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| kind.parse_error(format!("{e}")))?;
    if archive.len() > MAX_OFFICE_ZIP_ENTRIES {
        return Err(kind.parse_error(format!(
            "{} package entry limit exceeded: max {MAX_OFFICE_ZIP_ENTRIES} entries",
            kind.label()
        )));
    }

    let mut total_uncompressed = 0u64;
    let mut total_compressed = 0u64;
    let mut has_content_types = false;
    let mut has_required_part = false;
    for idx in 0..archive.len() {
        let file = archive
            .by_index(idx)
            .map_err(|e| kind.parse_error(format!("{} package entry {idx}: {e}", kind.label())))?;
        let name =
            safe_zip_entry_name(file.name(), file.enclosed_name().is_some()).map_err(|entry| {
                kind.parse_error(format!(
                    "{} package entry has unsafe path: {entry}",
                    kind.label()
                ))
            })?;
        has_content_types |= name == "[Content_Types].xml";
        has_required_part |= name == kind.required_part();
        total_uncompressed = total_uncompressed.saturating_add(file.size());
        total_compressed = total_compressed.saturating_add(file.compressed_size());
        if total_uncompressed > MAX_OFFICE_UNCOMPRESSED_BYTES {
            return Err(kind.parse_error(format!(
                "{} package uncompressed size exceeds {MAX_OFFICE_UNCOMPRESSED_BYTES} bytes",
                kind.label()
            )));
        }
    }

    if !has_content_types || !has_required_part {
        return Err(kind.parse_error(kind.missing_message().to_string()));
    }
    if total_compressed > 0
        && total_uncompressed > total_compressed.saturating_mul(MAX_OFFICE_COMPRESSION_RATIO)
    {
        return Err(kind.parse_error(format!(
            "{} package compression ratio exceeds {MAX_OFFICE_COMPRESSION_RATIO}:1",
            kind.label()
        )));
    }
    Ok(archive)
}

fn safe_zip_entry_name(raw_name: &str, enclosed_name: bool) -> Result<String, String> {
    if !enclosed_name || raw_name.contains('\0') {
        return Err(raw_name.to_string());
    }
    let normalized = raw_name.replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() || normalized.starts_with('/') || normalized.starts_with("//") {
        return Err(raw_name.to_string());
    }
    for component in normalized.split('/') {
        if component == ".." || component.contains(':') {
            return Err(raw_name.to_string());
        }
    }
    Ok(normalized)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PptxSlidePart {
    part_name: String,
    display_number: usize,
}

fn pptx_slide_parts(archive: &mut zip::ZipArchive<File>) -> Result<Vec<PptxSlidePart>, ParseError> {
    let mut presentation_xml = String::new();
    archive
        .by_name("ppt/presentation.xml")
        .map_err(|e| ParseError::Presentation(format!("ppt/presentation.xml: {e}")))?
        .read_to_string(&mut presentation_xml)
        .map_err(|e| ParseError::Presentation(format!("ppt/presentation.xml: {e}")))?;
    let relationship_ids = pptx_presentation_slide_relationship_ids(&presentation_xml)?;
    if !relationship_ids.is_empty() {
        let relationships = pptx_slide_relationship_targets(archive)?;
        let mut slide_parts = Vec::new();
        let mut missing_ids = Vec::new();
        for (idx, relationship_id) in relationship_ids.iter().enumerate() {
            if let Some(part_name) = relationships
                .iter()
                .find_map(|(id, target)| (id == relationship_id).then(|| target.clone()))
            {
                slide_parts.push(PptxSlidePart {
                    part_name,
                    display_number: idx + 1,
                });
            } else {
                missing_ids.push(relationship_id.as_str());
            }
        }
        if !missing_ids.is_empty() {
            return Err(ParseError::Presentation(format!(
                "pptx presentation references missing slide relationship: {}",
                missing_ids.join(", ")
            )));
        }
        return Ok(slide_parts);
    }

    Ok(pptx_slide_part_names_by_filename(archive)?
        .into_iter()
        .enumerate()
        .map(|(idx, part_name)| PptxSlidePart {
            part_name,
            display_number: idx + 1,
        })
        .collect())
}

fn pptx_presentation_slide_relationship_ids(xml: &str) -> Result<Vec<String>, ParseError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut relationship_ids = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if xml_name_is(e.name().as_ref(), b"sldId") =>
            {
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| {
                        ParseError::Presentation(format!("presentation slide attribute: {e}"))
                    })?;
                    if attr.key.as_ref().ends_with(b":id") {
                        let value = attr
                            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                            .map_err(|e| {
                                ParseError::Presentation(format!(
                                    "presentation slide relationship id: {e}"
                                ))
                            })?
                            .into_owned();
                        relationship_ids.push(value);
                        break;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Presentation(format!("presentation XML: {e}"))),
            _ => {}
        }
    }
    Ok(relationship_ids)
}

fn pptx_slide_relationship_targets(
    archive: &mut zip::ZipArchive<File>,
) -> Result<Vec<(String, String)>, ParseError> {
    let mut xml = String::new();
    match archive.by_name("ppt/_rels/presentation.xml.rels") {
        Ok(mut rels) => {
            rels.read_to_string(&mut xml).map_err(|e| {
                ParseError::Presentation(format!("ppt/_rels/presentation.xml.rels: {e}"))
            })?;
        }
        Err(zip::result::ZipError::FileNotFound) => return Ok(Vec::new()),
        Err(e) => {
            return Err(ParseError::Presentation(format!(
                "ppt/_rels/presentation.xml.rels: {e}"
            )));
        }
    }

    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut relationships = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if xml_name_is(e.name().as_ref(), b"Relationship") =>
            {
                let mut id = None;
                let mut target = None;
                let mut relationship_type = None;
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| {
                        ParseError::Presentation(format!(
                            "presentation relationship attribute: {e}"
                        ))
                    })?;
                    let value = attr
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|e| {
                            ParseError::Presentation(format!("presentation relationship: {e}"))
                        })?
                        .into_owned();
                    match attr.key.as_ref() {
                        b"Id" => id = Some(value),
                        b"Target" => target = Some(value),
                        b"Type" => relationship_type = Some(value),
                        _ => {}
                    }
                }
                if relationship_type
                    .as_deref()
                    .is_some_and(|value| value.ends_with("/slide"))
                {
                    let id = id.ok_or_else(|| {
                        ParseError::Presentation(
                            "slide relationship missing Id attribute".to_string(),
                        )
                    })?;
                    let target = target.ok_or_else(|| {
                        ParseError::Presentation(
                            "slide relationship missing Target attribute".to_string(),
                        )
                    })?;
                    let part_name =
                        pptx_slide_relationship_target_part(&target).ok_or_else(|| {
                            ParseError::Presentation(format!(
                                "slide relationship has unsafe target: {target}"
                            ))
                        })?;
                    relationships.push((id, part_name));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ParseError::Presentation(format!(
                    "presentation rels XML: {e}"
                )));
            }
            _ => {}
        }
    }
    Ok(relationships)
}

fn pptx_slide_relationship_target_part(target: &str) -> Option<String> {
    let normalized = target.replace('\\', "/");
    let package_path = if normalized.starts_with('/') {
        normalized.trim_start_matches('/').to_string()
    } else {
        format!("ppt/{normalized}")
    };
    let mut parts = Vec::new();
    for part in package_path.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            value => parts.push(value),
        }
    }
    let part_name = parts.join("/");
    if part_name.starts_with("ppt/slides/") && part_name.ends_with(".xml") {
        Some(part_name)
    } else {
        None
    }
}

fn pptx_slide_part_names_by_filename(
    archive: &mut zip::ZipArchive<File>,
) -> Result<Vec<String>, ParseError> {
    let mut slide_names = Vec::new();
    for idx in 0..archive.len() {
        let file = archive
            .by_index(idx)
            .map_err(|e| ParseError::Presentation(format!("pptx package entry {idx}: {e}")))?;
        let name = file.name().replace('\\', "/");
        if name.starts_with("ppt/slides/slide")
            && name.ends_with(".xml")
            && pptx_slide_number(&name).is_some()
        {
            slide_names.push(name);
        }
    }
    slide_names.sort_by(|a, b| {
        pptx_slide_number(a)
            .cmp(&pptx_slide_number(b))
            .then_with(|| a.cmp(b))
    });
    Ok(slide_names)
}

fn pptx_slide_number(name: &str) -> Option<usize> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

fn parse_docx_document_xml(xml: &str) -> Result<String, ParseError> {
    parse_visible_office_text_xml(xml, OfficePackageKind::Docx)
}

fn parse_pptx_slide_xml(xml: &str) -> Result<String, ParseError> {
    parse_visible_office_text_xml(xml, OfficePackageKind::Pptx)
}

fn parse_visible_office_text_xml(xml: &str, kind: OfficePackageKind) -> Result<String, ParseError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut text = String::new();
    let mut paragraph = String::new();
    let mut chars = 0usize;
    let mut paragraph_depth = 0usize;
    let mut visible_text_depth = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let name = name.as_ref();
                if xml_name_is(name, b"p") {
                    paragraph_depth = paragraph_depth.saturating_add(1);
                } else if paragraph_depth > 0 && xml_name_is(name, b"t") {
                    visible_text_depth = visible_text_depth.saturating_add(1);
                }
            }
            Ok(Event::Text(e)) if visible_text_depth > 0 => {
                paragraph.push_str(&decode_office_text(e, kind)?);
            }
            Ok(Event::CData(e)) if visible_text_depth > 0 => {
                paragraph.push_str(&e.decode().map_err(|e| {
                    kind.parse_error(format!("{} CDATA: {e}", kind.content_label()))
                })?);
            }
            Ok(Event::GeneralRef(e)) if visible_text_depth > 0 => {
                paragraph.push_str(&resolve_office_reference(&e, kind)?);
            }
            Ok(Event::Empty(e)) => {
                let name = e.name();
                let name = name.as_ref();
                if paragraph_depth > 0 && xml_name_is(name, b"tab") {
                    paragraph.push('\t');
                } else if paragraph_depth > 0
                    && (xml_name_is(name, b"br") || xml_name_is(name, b"cr"))
                {
                    paragraph.push('\n');
                }
            }
            Ok(Event::End(e)) if xml_name_is(e.name().as_ref(), b"t") => {
                visible_text_depth = visible_text_depth.saturating_sub(1);
            }
            Ok(Event::End(e)) if xml_name_is(e.name().as_ref(), b"p") => {
                push_visible_office_paragraph(&mut text, &mut chars, &mut paragraph, kind)?;
                paragraph_depth = paragraph_depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(kind.parse_error(format!("{} XML: {e}", kind.content_label())));
            }
            _ => {}
        }
    }
    push_visible_office_paragraph(&mut text, &mut chars, &mut paragraph, kind)?;
    Ok(text)
}

fn decode_office_text(event: BytesText<'_>, kind: OfficePackageKind) -> Result<String, ParseError> {
    let decoded = event
        .xml10_content()
        .map_err(|e| kind.parse_error(format!("{} text: {e}", kind.content_label())))?;
    let unescaped = unescape(&decoded)
        .map_err(|e| kind.parse_error(format!("{} text: {e}", kind.content_label())))?;
    Ok(unescaped.into_owned())
}

fn resolve_office_reference(
    reference: &BytesRef<'_>,
    kind: OfficePackageKind,
) -> Result<String, ParseError> {
    if let Some(ch) = reference
        .resolve_char_ref()
        .map_err(|e| kind.parse_error(format!("{} reference: {e}", kind.content_label())))?
    {
        return Ok(ch.to_string());
    }
    let decoded = reference
        .decode()
        .map_err(|e| kind.parse_error(format!("{} reference: {e}", kind.content_label())))?;
    match decoded.as_ref() {
        "amp" => Ok("&".to_string()),
        "lt" => Ok("<".to_string()),
        "gt" => Ok(">".to_string()),
        "quot" => Ok("\"".to_string()),
        "apos" => Ok("'".to_string()),
        _ => Err(kind.parse_error(format!("unsupported XML entity reference: {decoded}"))),
    }
}

fn push_visible_office_paragraph(
    text: &mut String,
    chars: &mut usize,
    paragraph: &mut String,
    kind: OfficePackageKind,
) -> Result<(), ParseError> {
    let trimmed = paragraph.trim();
    if trimmed.is_empty() {
        paragraph.clear();
        return Ok(());
    }
    let line = format!("{trimmed}\n");
    push_limited_office_fragment(text, chars, &line, kind)?;
    paragraph.clear();
    Ok(())
}

fn push_limited_office_fragment(
    text: &mut String,
    chars: &mut usize,
    fragment: &str,
    kind: OfficePackageKind,
) -> Result<(), ParseError> {
    if fragment.is_empty() {
        return Ok(());
    }
    *chars = chars.saturating_add(fragment.chars().count());
    if *chars > MAX_EXTRACTED_OFFICE_CHARS {
        return Err(kind.parse_error(format!(
            "extracted text limit exceeded: max {MAX_EXTRACTED_OFFICE_CHARS} chars"
        )));
    }
    text.push_str(fragment);
    Ok(())
}

fn xml_name_is(name: &[u8], local: &[u8]) -> bool {
    name == local
        || name
            .strip_suffix(local)
            .is_some_and(|prefix| prefix.ends_with(b":"))
}

#[derive(Debug, Clone, Copy)]
enum TableSource {
    Delimited,
    Xlsx,
}

fn push_limited_table_line(
    text: &mut String,
    chars: &mut usize,
    line: String,
    source: TableSource,
) -> Result<(), ParseError> {
    *chars = chars.saturating_add(line.chars().count());
    if *chars > MAX_EXTRACTED_TABLE_CHARS {
        let msg = format!("extracted text limit exceeded: max {MAX_EXTRACTED_TABLE_CHARS} chars");
        return match source {
            TableSource::Delimited => Err(ParseError::Csv(msg)),
            TableSource::Xlsx => Err(ParseError::Spreadsheet(msg)),
        };
    }
    text.push_str(&line);
    Ok(())
}

fn spreadsheet_cell_text(cell: &calamine::Data) -> String {
    match cell {
        calamine::Data::Empty => String::new(),
        _ => cell.to_string(),
    }
}

fn trim_trailing_empty_cells(mut cells: Vec<String>) -> Vec<String> {
    while cells.last().is_some_and(|cell| cell.is_empty()) {
        cells.pop();
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body).unwrap();
        f.sync_all().unwrap();
        path
    }

    #[test]
    fn parse_markdown_file_returns_text() {
        let tmp = TempDir::new().unwrap();
        let body = "# Hello\n\nThis is a markdown file.";
        let path = write_file(&tmp, "note.md", body.as_bytes());

        let out = parse_file(&path).unwrap();
        assert_eq!(out.text, body);
        assert_eq!(out.mime_type, "text/markdown");
        assert_eq!(out.extractor_name, "markdown_text");
        assert_eq!(out.byte_size, body.len() as u64);
    }

    #[test]
    fn parse_plain_text_file() {
        let tmp = TempDir::new().unwrap();
        let body = "Hello world.\n";
        let path = write_file(&tmp, "x.txt", body.as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.text, body);
        assert_eq!(out.mime_type, "text/plain");
    }

    #[test]
    fn parse_rust_source() {
        let tmp = TempDir::new().unwrap();
        let body = "fn main() {\n    println!(\"hi\");\n}\n";
        let path = write_file(&tmp, "main.rs", body.as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.text, body);
        assert_eq!(out.mime_type, "text/x-rust");
        assert_eq!(out.extractor_name, "code_text");
    }

    #[test]
    fn parse_jsonl_as_plain_utf8_text() {
        let tmp = TempDir::new().unwrap();
        let body = "{\"role\":\"user\",\"content\":\"hello\"}\n";
        let path = write_file(&tmp, "chat.jsonl", body.as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.text, body);
        assert_eq!(out.mime_type, "application/x-ndjson");
        assert_eq!(out.extractor_name, "json_text");
    }

    #[test]
    fn parse_uppercase_extension_is_accepted() {
        let tmp = TempDir::new().unwrap();
        let body = "# upper";
        let path = write_file(&tmp, "README.MD", body.as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "text/markdown");
    }

    #[test]
    fn parse_html_strips_tags() {
        let tmp = TempDir::new().unwrap();
        let body = "<html><body><p>hello world</p><script>var x = 'nope';</script></body></html>";
        let path = write_file(&tmp, "page.html", body.as_bytes());
        let out = parse_file(&path).unwrap();
        assert!(
            out.text.contains("hello world"),
            "expected 'hello world' in: {:?}",
            out.text
        );
        assert!(
            !out.text.contains("nope"),
            "script body should not appear in text: {:?}",
            out.text
        );
        assert_eq!(out.mime_type, "text/html");
    }

    #[test]
    fn parse_csv_extracts_rows() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "people.csv",
            b"name,role\nAlice,Engineer\nBob,Designer\n",
        );
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "text/csv");
        assert_eq!(out.extractor_name, "csv_table");
        assert!(out.text.contains("row 1: name | role"));
        assert!(out.text.contains("row 2: Alice | Engineer"));
    }

    #[test]
    fn parse_csv_rejects_extracted_text_over_limit() {
        let tmp = TempDir::new().unwrap();
        let mut body = "x".repeat(MAX_EXTRACTED_TABLE_CHARS + 1);
        body.push('\n');
        let path = write_file(&tmp, "huge.csv", body.as_bytes());
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Csv(msg) => assert!(
                msg.contains("extracted text limit exceeded"),
                "unexpected CSV error: {msg}"
            ),
            other => panic!("expected Csv, got {other:?}"),
        }
    }

    #[test]
    fn parse_tsv_extracts_rows() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "people.tsv", b"name\trole\nAlice\tEngineer\n");
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "text/tab-separated-values");
        assert_eq!(out.extractor_name, "tsv_table");
        assert!(out.text.contains("row 1: name | role"));
        assert!(out.text.contains("row 2: Alice | Engineer"));
    }

    #[test]
    fn parse_xlsx_extracts_sheet_rows() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "people.xlsx", &minimal_xlsx());
        let out = parse_file(&path).unwrap();
        assert_eq!(
            out.mime_type,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(out.extractor_name, "xlsx_workbook");
        assert!(out.text.contains("Sheet: People"), "{}", out.text);
        assert!(out.text.contains("R1: name | role"), "{}", out.text);
        assert!(out.text.contains("R2: Alice | Engineer"), "{}", out.text);
    }

    #[test]
    fn parse_docx_extracts_paragraph_text() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "notes.docx", &minimal_docx());
        let out = parse_file(&path).unwrap();
        assert_eq!(
            out.mime_type,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(out.extractor_name, "docx_document");
        assert!(out.text.contains("Project Alpha"), "{}", out.text);
        assert!(out.text.contains("Alice owns the roadmap."), "{}", out.text);
        assert!(out.text.contains("Research & Development"), "{}", out.text);
    }

    #[test]
    fn parse_docx_ignores_non_visible_word_text() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "tracked.docx", &minimal_docx_with_non_visible_text());
        let out = parse_file(&path).unwrap();
        assert!(out.text.contains("Visible statement."), "{}", out.text);
        assert!(out.text.contains("Visible field result."), "{}", out.text);
        assert!(!out.text.contains("Deleted secret"), "{}", out.text);
        assert!(!out.text.contains("MERGEFIELD"), "{}", out.text);
        assert!(!out.text.contains("Hidden field"), "{}", out.text);
    }

    #[test]
    fn parse_pptx_extracts_slide_text() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "deck.pptx", &minimal_pptx());
        let out = parse_file(&path).unwrap();
        assert_eq!(
            out.mime_type,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        assert_eq!(out.extractor_name, "pptx_deck");
        assert!(out.text.contains("Slide 1:"), "{}", out.text);
        assert!(out.text.contains("Project Alpha"), "{}", out.text);
        assert!(out.text.contains("Alice owns the roadmap."), "{}", out.text);
        assert!(out.text.contains("Research & Development"), "{}", out.text);
    }

    #[test]
    fn parse_pptx_uses_presentation_relationship_order() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "deck.pptx", &minimal_reordered_pptx());
        let out = parse_file(&path).unwrap();
        let first = out
            .text
            .find("Slide 1:\nFirst in deck order.")
            .unwrap_or_else(|| panic!("missing first slide text: {}", out.text));
        let second = out
            .text
            .find("Slide 2:\nSecond in deck order.")
            .unwrap_or_else(|| panic!("missing second slide text: {}", out.text));
        assert!(first < second, "{}", out.text);
        assert!(!out.text.contains("Slide 10:"), "{}", out.text);
    }

    #[test]
    fn parse_pptx_rejects_missing_presentation_relationships() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "deck.pptx", &minimal_pptx_missing_slide_rels());
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Presentation(msg) => assert!(
                msg.contains("missing slide relationship: rIdMissing"),
                "unexpected presentation error: {msg}"
            ),
            other => panic!("expected Presentation, got {other:?}"),
        }
    }

    #[test]
    fn parse_zip_extracts_safe_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "bundle.zip",
            &write_stored_zip(&[
                (
                    "docs/project-alpha.txt",
                    "Project Alpha notes should not be extracted",
                ),
                ("assets/diagram.png", "not actually an image"),
            ]),
        );
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "application/zip");
        assert_eq!(out.extractor_name, "zip_manifest");
        assert!(out.text.contains("Archive file"), "{}", out.text);
        assert!(out.text.contains("Format: ZIP archive"), "{}", out.text);
        assert!(out.text.contains("Entries: 2"), "{}", out.text);
        assert!(
            out.text.contains("Entry 1: docs/project-alpha.txt | file"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("Entry 2: assets/diagram.png | file"),
            "{}",
            out.text
        );
        assert!(
            !out.text
                .contains("Project Alpha notes should not be extracted"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_zip_rejects_path_traversal_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "bundle.zip",
            &write_stored_zip(&[("../evil.txt", "nope")]),
        );
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Archive(msg) => assert!(
                msg.contains("unsafe path"),
                "unexpected archive error: {msg}"
            ),
            other => panic!("expected Archive, got {other:?}"),
        }
    }

    #[test]
    fn parse_zip_rejects_zip_bomb_ratio_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "bundle.zip",
            &zip_with_declared_sizes("huge.txt", MAX_ARCHIVE_COMPRESSION_RATIO + 1, 1),
        );
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Archive(msg) => assert!(
                msg.contains("compression ratio exceeds"),
                "unexpected archive error: {msg}"
            ),
            other => panic!("expected Archive, got {other:?}"),
        }
    }

    #[test]
    fn parse_png_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "diagram.png", &minimal_png(64, 32));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: PNG"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 64 x 32 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_jpeg_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "photo.jpg", &minimal_jpeg(48, 24));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/jpeg");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: JPEG"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 48 x 24 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_webp_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "graphic.webp", &minimal_webp_vp8x(80, 40));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/webp");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: WebP"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 80 x 40 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_webp_lossless_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "lossless.webp", &minimal_webp_vp8l(81, 41));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/webp");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: WebP"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 81 x 41 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_webp_lossy_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "lossy.webp", &minimal_webp_vp8(82, 42));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/webp");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: WebP"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 82 x 42 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_tiff_extracts_image_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scan.tiff", &minimal_tiff(96, 72));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "image/tiff");
        assert_eq!(out.extractor_name, "image_metadata");
        assert!(out.text.contains("Format: TIFF"), "{}", out.text);
        assert!(
            out.text.contains("Dimensions: 96 x 72 pixels"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_blend_extracts_header_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.blend", &minimal_blend(b'-', b'v', b"400"));
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "application/x-blender");
        assert_eq!(out.extractor_name, "blend_metadata");
        assert!(out.text.contains("Blender file"), "{}", out.text);
        assert!(out.text.contains("Blender version: 4.00"), "{}", out.text);
        assert!(out.text.contains("Pointer size: 64-bit"), "{}", out.text);
        assert!(
            out.text.contains("Endianness: little-endian"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Compression: none"), "{}", out.text);
    }

    #[test]
    fn parse_zstd_compressed_blend_extracts_header_metadata() {
        let tmp = TempDir::new().unwrap();
        let bytes = zstd::stream::encode_all(&minimal_blend(b'_', b'V', b"293")[..], 0)
            .expect("zstd encode");
        let path = write_file(&tmp, "scene.blend", &bytes);
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "application/x-blender");
        assert_eq!(out.extractor_name, "blend_metadata");
        assert!(out.text.contains("Blender version: 2.93"), "{}", out.text);
        assert!(out.text.contains("Pointer size: 32-bit"), "{}", out.text);
        assert!(out.text.contains("Endianness: big-endian"), "{}", out.text);
        assert!(out.text.contains("Compression: Zstandard"), "{}", out.text);
    }

    #[test]
    fn parse_gzip_compressed_blend_extracts_header_metadata() {
        let tmp = TempDir::new().unwrap();
        let bytes = gzip_encode(&minimal_blend(b'-', b'v', b"279"));
        let path = write_file(&tmp, "scene.blend", &bytes);
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "application/x-blender");
        assert_eq!(out.extractor_name, "blend_metadata");
        assert!(out.text.contains("Blender version: 2.79"), "{}", out.text);
        assert!(out.text.contains("Pointer size: 64-bit"), "{}", out.text);
        assert!(
            out.text.contains("Endianness: little-endian"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Compression: Gzip"), "{}", out.text);
    }

    #[test]
    fn parse_zstd_compressed_blend_rejects_non_blender_payload() {
        let tmp = TempDir::new().unwrap();
        let bytes = zstd::stream::encode_all(&b"not a blend file"[..], 0).expect("zstd encode");
        let path = write_file(&tmp, "scene.blend", &bytes);
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Blend(msg) => assert!(
                msg.contains("invalid Blender header"),
                "unexpected Blender error: {msg}"
            ),
            other => panic!("expected Blend, got {other:?}"),
        }
    }

    #[test]
    fn parse_blend_rejects_invalid_header_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.blend", &minimal_blend(b'?', b'v', b"400"));
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Blend(msg) => assert!(
                msg.contains("invalid pointer-size marker"),
                "unexpected Blender error: {msg}"
            ),
            other => panic!("expected Blend, got {other:?}"),
        }
    }

    #[test]
    fn parse_gltf_extracts_model_metadata() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.gltf", &minimal_gltf_json());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "model/gltf+json");
        assert_eq!(out.extractor_name, "model_metadata");
        assert!(out.text.contains("Format: glTF JSON"), "{}", out.text);
        assert!(out.text.contains("glTF version: 2.0"), "{}", out.text);
        assert!(out.text.contains("Generator: Solo fixture"), "{}", out.text);
        assert!(out.text.contains("Node count: 1"), "{}", out.text);
        assert!(out.text.contains("Mesh count: 1"), "{}", out.text);
        assert!(
            out.text.contains("Total declared buffer bytes: 12"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Scene: Main Scene"), "{}", out.text);
        assert!(out.text.contains("Node: Root Node"), "{}", out.text);
        assert!(out.text.contains("Mesh: Cube Mesh"), "{}", out.text);
        assert!(out.text.contains("Material: Blue Material"), "{}", out.text);
    }

    #[test]
    fn parse_glb_extracts_json_chunk_metadata_without_indexing_bin() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.glb", &minimal_glb());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "model/gltf-binary");
        assert_eq!(out.extractor_name, "model_metadata");
        assert!(out.text.contains("Format: GLB binary"), "{}", out.text);
        assert!(out.text.contains("GLB version: 2"), "{}", out.text);
        assert!(out.text.contains("GLB JSON chunk bytes:"), "{}", out.text);
        assert!(out.text.contains("GLB BIN chunk bytes:"), "{}", out.text);
        assert!(out.text.contains("glTF version: 2.0"), "{}", out.text);
        assert!(out.text.contains("Mesh: Cube Mesh"), "{}", out.text);
        assert!(
            !out.text.contains("binary payload should not be extracted"),
            "{}",
            out.text
        );
    }

    #[test]
    fn parse_obj_extracts_model_counts_and_names() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "mesh.obj", minimal_obj().as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "model/obj");
        assert_eq!(out.extractor_name, "model_metadata");
        assert!(out.text.contains("Format: Wavefront OBJ"), "{}", out.text);
        assert!(out.text.contains("Object count: 1"), "{}", out.text);
        assert!(out.text.contains("Group count: 1"), "{}", out.text);
        assert!(out.text.contains("Vertex count: 3"), "{}", out.text);
        assert!(
            out.text.contains("Texture coordinate count: 1"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Normal count: 1"), "{}", out.text);
        assert!(out.text.contains("Face count: 1"), "{}", out.text);
        assert!(out.text.contains("Object: Cube"), "{}", out.text);
        assert!(
            out.text.contains("Material library: cube.mtl"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Material: Blue"), "{}", out.text);
    }

    #[test]
    fn parse_ascii_stl_extracts_model_counts() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "mesh.stl", minimal_ascii_stl().as_bytes());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "model/stl");
        assert_eq!(out.extractor_name, "model_metadata");
        assert!(out.text.contains("Format: STL ASCII"), "{}", out.text);
        assert!(out.text.contains("Name: SoloPart"), "{}", out.text);
        assert!(out.text.contains("Facet count: 1"), "{}", out.text);
        assert!(out.text.contains("Vertex line count: 3"), "{}", out.text);
    }

    #[test]
    fn parse_binary_stl_extracts_triangle_count() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "mesh.stl", &minimal_binary_stl());
        let out = parse_file(&path).unwrap();
        assert_eq!(out.mime_type, "model/stl");
        assert_eq!(out.extractor_name, "model_metadata");
        assert!(out.text.contains("Format: STL binary"), "{}", out.text);
        assert!(out.text.contains("Header: Solo binary STL"), "{}", out.text);
        assert!(out.text.contains("Triangle count: 1"), "{}", out.text);
    }

    #[test]
    fn parse_glb_requires_glb_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.glb", b"{\"asset\":{\"version\":\"2.0\"}}");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "glb");
                assert_eq!(expected_mime, "model/gltf-binary");
                assert_eq!(detected_mime, "unknown");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_gltf_rejects_binary_glb_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "scene.gltf", &minimal_glb());
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "gltf");
                assert_eq!(expected_mime, "model/gltf+json");
                assert_eq!(detected_mime, "model/gltf-binary");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_image_extension_requires_matching_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "not-image.png", b"not an image");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "png");
                assert_eq!(expected_mime, "image/png");
                assert_eq!(detected_mime, "unknown");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_unsupported_extension_errors() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "blob.bin", b"\x00\x01\x02");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::UnsupportedExtension(ext) => assert_eq!(ext, "bin"),
            other => panic!("expected UnsupportedExtension, got {other:?}"),
        }
    }

    #[test]
    fn parse_file_without_extension_errors() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "noext", b"hello");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::UnsupportedExtension(ext) => assert_eq!(ext, "(no extension)"),
            other => panic!("expected UnsupportedExtension, got {other:?}"),
        }
    }

    #[test]
    fn parse_pdf_extension_requires_pdf_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "not-pdf.pdf", b"plain text");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "pdf");
                assert_eq!(expected_mime, "application/pdf");
                assert_eq!(detected_mime, "unknown");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_csv_rejects_pdf_magic_mismatch() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "fake.csv", b"%PDF-1.7\nnot actually csv");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "csv");
                assert_eq!(expected_mime, "text/csv");
                assert_eq!(detected_mime, "application/pdf");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_xlsx_requires_zip_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "not-xlsx.xlsx", b"not a zip package");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "xlsx");
                assert_eq!(
                    expected_mime,
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                );
                assert_eq!(detected_mime, "unknown");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_zip_requires_zip_magic() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "not-zip.zip", b"not a zip package");
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::MimeMismatch {
                extension,
                expected_mime,
                detected_mime,
            } => {
                assert_eq!(extension, "zip");
                assert_eq!(expected_mime, "application/zip");
                assert_eq!(detected_mime, "unknown");
            }
            other => panic!("expected MimeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_xlsx_rejects_generic_zip_package() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "not-a-workbook.xlsx",
            &write_stored_zip(&[("hello.txt", "hello")]),
        );
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Spreadsheet(msg) => assert!(
                msg.contains("missing required workbook metadata"),
                "unexpected spreadsheet error: {msg}"
            ),
            other => panic!("expected Spreadsheet, got {other:?}"),
        }
    }

    #[test]
    fn parse_docx_rejects_generic_zip_package() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "not-a-doc.docx",
            &write_stored_zip(&[("hello.txt", "hello")]),
        );
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Office(msg) => assert!(
                msg.contains("missing required document metadata"),
                "unexpected Office error: {msg}"
            ),
            other => panic!("expected Office, got {other:?}"),
        }
    }

    #[test]
    fn parse_pptx_rejects_generic_zip_package() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(
            &tmp,
            "not-a-deck.pptx",
            &write_stored_zip(&[("hello.txt", "hello")]),
        );
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::Presentation(msg) => assert!(
                msg.contains("missing required presentation metadata"),
                "unexpected presentation error: {msg}"
            ),
            other => panic!("expected Presentation, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_file_errors_with_empty_variant() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "empty.txt", b"");
        let err = parse_file(&path).unwrap_err();
        assert!(matches!(err, ParseError::Empty), "got: {err:?}");
    }

    #[test]
    fn parse_whitespace_only_file_errors_with_empty_variant() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "ws.txt", b"   \n\t\n  \n");
        let err = parse_file(&path).unwrap_err();
        assert!(matches!(err, ParseError::Empty), "got: {err:?}");
    }

    #[test]
    fn parse_returns_byte_size_correctly() {
        let tmp = TempDir::new().unwrap();
        let body = b"abcdefghij";
        let path = write_file(&tmp, "sized.txt", body);
        let out = parse_file(&path).unwrap();
        assert_eq!(out.byte_size, 10);
    }

    #[test]
    fn parse_invalid_utf8_errors() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "bad.txt", &[0xff, 0xfe, 0xfd]);
        let err = parse_file(&path).unwrap_err();
        assert!(matches!(err, ParseError::InvalidUtf8(_)), "got: {err:?}");
    }

    #[test]
    fn mime_type_registry_covers_registered_document_extractors() {
        assert_eq!(mime_type_for_extension("csv"), Some("text/csv"));
        assert_eq!(
            mime_type_for_extension(".tsv"),
            Some("text/tab-separated-values")
        );
        assert_eq!(
            mime_type_for_extension("xlsx"),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
        assert_eq!(
            mime_type_for_extension("docx"),
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
        );
        assert_eq!(extractor_name_for_mime("text/csv"), "csv_table");
        assert_eq!(
            extractor_name_for_mime(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            "docx_document"
        );
        assert_eq!(
            mime_type_for_extension("pptx"),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
        );
        assert_eq!(
            extractor_name_for_mime(
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            ),
            "pptx_deck"
        );
        assert_eq!(mime_type_for_extension("png"), Some("image/png"));
        assert_eq!(mime_type_for_extension("jpg"), Some("image/jpeg"));
        assert_eq!(mime_type_for_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(mime_type_for_extension("webp"), Some("image/webp"));
        assert_eq!(mime_type_for_extension("tif"), Some("image/tiff"));
        assert_eq!(mime_type_for_extension("tiff"), Some("image/tiff"));
        assert_eq!(extractor_name_for_mime("image/png"), "image_metadata");
        assert_eq!(extractor_name_for_mime("image/jpeg"), "image_metadata");
        assert_eq!(extractor_name_for_mime("image/webp"), "image_metadata");
        assert_eq!(extractor_name_for_mime("image/tiff"), "image_metadata");
        assert_eq!(
            mime_type_for_extension("blend"),
            Some("application/x-blender")
        );
        assert_eq!(
            extractor_name_for_mime("application/x-blender"),
            "blend_metadata"
        );
        assert_eq!(mime_type_for_extension("zip"), Some("application/zip"));
        assert_eq!(extractor_name_for_mime("application/zip"), "zip_manifest");
        assert_eq!(mime_type_for_extension("gltf"), Some("model/gltf+json"));
        assert_eq!(mime_type_for_extension("glb"), Some("model/gltf-binary"));
        assert_eq!(mime_type_for_extension("obj"), Some("model/obj"));
        assert_eq!(mime_type_for_extension("stl"), Some("model/stl"));
        assert_eq!(extractor_name_for_mime("model/gltf+json"), "model_metadata");
        assert_eq!(
            extractor_name_for_mime("model/gltf-binary"),
            "model_metadata"
        );
        assert_eq!(extractor_name_for_mime("model/obj"), "model_metadata");
        assert_eq!(extractor_name_for_mime("model/stl"), "model_metadata");
    }

    fn minimal_gltf_json() -> Vec<u8> {
        br#"{
  "asset": { "version": "2.0", "generator": "Solo fixture" },
  "scene": 0,
  "scenes": [{ "name": "Main Scene", "nodes": [0] }],
  "nodes": [{ "name": "Root Node", "mesh": 0 }],
  "meshes": [{ "name": "Cube Mesh", "primitives": [{ "attributes": { "POSITION": 0 } }] }],
  "materials": [{ "name": "Blue Material" }],
  "buffers": [{ "byteLength": 12 }],
  "animations": [{ "name": "Spin" }],
  "cameras": [{ "name": "Main Camera" }]
}"#
        .to_vec()
    }

    fn minimal_glb() -> Vec<u8> {
        let mut json = minimal_gltf_json();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut bin = b"binary payload should not be extracted".to_vec();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let total_len = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total_len as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    fn minimal_obj() -> &'static str {
        "mtllib cube.mtl\n\
         o Cube\n\
         g Front\n\
         v 0.0 0.0 0.0\n\
         v 1.0 0.0 0.0\n\
         v 0.0 1.0 0.0\n\
         vt 0.0 0.0\n\
         vn 0.0 0.0 1.0\n\
         usemtl Blue\n\
         f 1/1/1 2/1/1 3/1/1\n"
    }

    fn minimal_ascii_stl() -> &'static str {
        "solid SoloPart\n\
         facet normal 0 0 1\n\
           outer loop\n\
             vertex 0 0 0\n\
             vertex 1 0 0\n\
             vertex 0 1 0\n\
           endloop\n\
         endfacet\n\
         endsolid SoloPart\n"
    }

    fn minimal_binary_stl() -> Vec<u8> {
        let mut out = vec![0u8; 80];
        let header = b"Solo binary STL";
        out[..header.len()].copy_from_slice(header);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 50]);
        out
    }

    fn minimal_pdf() -> Vec<u8> {
        minimal_pdf_with_content_stream("BT\n/F1 24 Tf\n72 720 Td\n(Hello PDF) Tj\nET\n")
    }

    fn minimal_blank_pdf() -> Vec<u8> {
        minimal_pdf_with_content_stream("")
    }

    fn minimal_pdf_with_content_stream(stream: &str) -> Vec<u8> {
        let contents = format!(
            "5 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            stream.len(),
            stream
        );
        let objects: [&str; 5] = [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>\nendobj\n",
            "4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
            &contents,
        ];

        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n");
        let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
        for obj in &objects {
            offsets.push(buf.len());
            buf.extend_from_slice(obj.as_bytes());
        }
        let xref_offset = buf.len();
        buf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                objects.len() + 1,
                xref_offset
            )
            .as_bytes(),
        );
        buf
    }

    #[test]
    fn parse_pdf_extracts_known_text() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "hello.pdf", &minimal_pdf());

        let out = parse_file(&path).expect("text-bearing PDF fixture should parse");
        assert_eq!(out.mime_type, "application/pdf");
        assert!(
            out.text.to_lowercase().contains("hello"),
            "expected extracted PDF text to contain fixture text, got: {:?}",
            out.text
        );
    }

    #[test]
    fn parse_pdf_without_text_reports_ocr_gap() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "blank.pdf", &minimal_blank_pdf());
        let err = parse_file(&path).unwrap_err();
        match err {
            ParseError::NoExtractableText { mime_type, reason } => {
                assert_eq!(mime_type, "application/pdf");
                assert!(reason.contains("OCR/page rendering"), "{reason}");
            }
            other => panic!("expected NoExtractableText, got {other:?}"),
        }
    }

    fn minimal_xlsx() -> Vec<u8> {
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="People" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1">
<c r="A1" t="inlineStr"><is><t>name</t></is></c>
<c r="B1" t="inlineStr"><is><t>role</t></is></c>
</row>
<row r="2">
<c r="A2" t="inlineStr"><is><t>Alice</t></is></c>
<c r="B2" t="inlineStr"><is><t>Engineer</t></is></c>
</row>
</sheetData>
</worksheet>"#,
            ),
        ];
        write_stored_zip(&entries)
    }

    fn minimal_docx() -> Vec<u8> {
        minimal_docx_with_document_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>Project Alpha</w:t></w:r></w:p>
<w:p><w:r><w:t>Alice owns the roadmap.</w:t></w:r></w:p>
<w:p><w:r><w:t>Research &amp; Development</w:t></w:r></w:p>
</w:body>
</w:document>"#,
        )
    }

    fn minimal_docx_with_non_visible_text() -> Vec<u8> {
        minimal_docx_with_document_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p>
<w:r><w:t>Visible statement.</w:t></w:r>
<w:del><w:r><w:delText>Deleted secret should not index.</w:delText></w:r></w:del>
<w:r><w:instrText>MERGEFIELD Hidden field</w:instrText></w:r>
</w:p>
<w:p><w:fldSimple w:instr="MERGEFIELD HiddenAttribute"><w:r><w:t>Visible field result.</w:t></w:r></w:fldSimple></w:p>
</w:body>
</w:document>"#,
        )
    }

    fn minimal_docx_with_document_xml(document_xml: &str) -> Vec<u8> {
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
            ),
            ("word/document.xml", document_xml),
        ];
        write_stored_zip(&entries)
    }

    fn minimal_pptx() -> Vec<u8> {
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
            ),
            (
                "ppt/presentation.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody>
<a:bodyPr/><a:lstStyle/>
<a:p><a:r><a:t>Project Alpha</a:t></a:r></a:p>
<a:p><a:r><a:t>Alice owns the roadmap.</a:t></a:r></a:p>
<a:p><a:r><a:t>Research &amp; Development</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#,
            ),
        ];
        write_stored_zip(&entries)
    }

    fn minimal_reordered_pptx() -> Vec<u8> {
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide10.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
            ),
            (
                "ppt/presentation.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldIdLst>
<p:sldId id="256" r:id="rIdFirst"/>
<p:sldId id="257" r:id="rIdSecond"/>
</p:sldIdLst>
</p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rIdFirst" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide10.xml"/>
<Relationship Id="rIdSecond" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
</Relationships>"#,
            ),
            (
                "ppt/slides/slide10.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>First in deck order.</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#,
            ),
            (
                "ppt/slides/slide2.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>Second in deck order.</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#,
            ),
        ];
        write_stored_zip(&entries)
    }

    fn minimal_pptx_missing_slide_rels() -> Vec<u8> {
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
            ),
            (
                "ppt/presentation.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldIdLst><p:sldId id="256" r:id="rIdMissing"/></p:sldIdLst>
</p:presentation>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>Should not fall back by filename.</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#,
            ),
        ];
        write_stored_zip(&entries)
    }

    fn minimal_png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out
    }

    fn minimal_jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\xff\xd8");
        out.extend_from_slice(b"\xff\xc0");
        out.extend_from_slice(&17u16.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        out.extend_from_slice(b"\xff\xd9");
        out
    }

    fn minimal_webp_vp8x(width: u32, height: u32) -> Vec<u8> {
        assert!(width > 0 && width <= 16_777_216);
        assert!(height > 0 && height <= 16_777_216);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        write_u32(&mut out, 22);
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(b"VP8X");
        write_u32(&mut out, 10);
        out.extend_from_slice(&[0, 0, 0, 0]);
        write_u24(&mut out, width - 1);
        write_u24(&mut out, height - 1);
        out
    }

    fn minimal_webp_vp8l(width: u32, height: u32) -> Vec<u8> {
        assert!(width > 0 && width <= 16_384);
        assert!(height > 0 && height <= 16_384);
        let bits = (width - 1) | ((height - 1) << 14);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        write_u32(&mut out, 17);
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(b"VP8L");
        write_u32(&mut out, 5);
        out.push(0x2f);
        write_u32(&mut out, bits);
        out
    }

    fn minimal_webp_vp8(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        write_u32(&mut out, 22);
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(b"VP8 ");
        write_u32(&mut out, 10);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(b"\x9d\x01\x2a");
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        out
    }

    fn minimal_tiff(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"II*\0");
        write_u32(&mut out, 8);
        write_u16(&mut out, 2);
        write_tiff_long_entry(&mut out, 256, width);
        write_tiff_long_entry(&mut out, 257, height);
        write_u32(&mut out, 0);
        out
    }

    fn minimal_blend(pointer_size_marker: u8, endian_marker: u8, version: &[u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"BLENDER");
        out.push(pointer_size_marker);
        out.push(endian_marker);
        out.extend_from_slice(version);
        out
    }

    fn gzip_encode(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).expect("gzip write");
        encoder.finish().expect("gzip finish")
    }

    fn write_tiff_long_entry(out: &mut Vec<u8>, tag: u16, value: u32) {
        write_u16(out, tag);
        write_u16(out, 4);
        write_u32(out, 1);
        write_u32(out, value);
    }

    fn zip_with_declared_sizes(
        name: &str,
        uncompressed_size: u64,
        compressed_size: u64,
    ) -> Vec<u8> {
        assert!(uncompressed_size <= u64::from(u32::MAX));
        assert!(compressed_size <= u64::from(u32::MAX));
        let compressed_body = vec![0u8; compressed_size as usize];
        let crc32 = crc32(&compressed_body);
        let local_offset = 0u32;

        let mut out = Vec::new();
        write_u32(&mut out, 0x0403_4b50);
        write_u16(&mut out, 20);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, crc32);
        write_u32(&mut out, compressed_size as u32);
        write_u32(&mut out, uncompressed_size as u32);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&compressed_body);

        let central_offset = out.len() as u32;
        write_u32(&mut out, 0x0201_4b50);
        write_u16(&mut out, 20);
        write_u16(&mut out, 20);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, crc32);
        write_u32(&mut out, compressed_size as u32);
        write_u32(&mut out, uncompressed_size as u32);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, 0);
        write_u32(&mut out, local_offset);
        out.extend_from_slice(name.as_bytes());
        let central_size = out.len() as u32 - central_offset;

        write_u32(&mut out, 0x0605_4b50);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 1);
        write_u16(&mut out, 1);
        write_u32(&mut out, central_size);
        write_u32(&mut out, central_offset);
        write_u16(&mut out, 0);
        out
    }

    fn write_stored_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        #[derive(Clone)]
        struct CentralEntry {
            name: String,
            crc32: u32,
            size: u32,
            local_offset: u32,
        }

        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let body = body.as_bytes();
            let crc32 = crc32(body);
            let local_offset = out.len() as u32;
            write_u32(&mut out, 0x0403_4b50);
            write_u16(&mut out, 20);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u32(&mut out, crc32);
            write_u32(&mut out, body.len() as u32);
            write_u32(&mut out, body.len() as u32);
            write_u16(&mut out, name.len() as u16);
            write_u16(&mut out, 0);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);
            central.push(CentralEntry {
                name: (*name).to_string(),
                crc32,
                size: body.len() as u32,
                local_offset,
            });
        }

        let central_offset = out.len() as u32;
        for entry in &central {
            write_u32(&mut out, 0x0201_4b50);
            write_u16(&mut out, 20);
            write_u16(&mut out, 20);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u32(&mut out, entry.crc32);
            write_u32(&mut out, entry.size);
            write_u32(&mut out, entry.size);
            write_u16(&mut out, entry.name.len() as u16);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u16(&mut out, 0);
            write_u32(&mut out, 0);
            write_u32(&mut out, entry.local_offset);
            out.extend_from_slice(entry.name.as_bytes());
        }
        let central_size = out.len() as u32 - central_offset;

        write_u32(&mut out, 0x0605_4b50);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, central.len() as u16);
        write_u16(&mut out, central.len() as u16);
        write_u32(&mut out, central_size);
        write_u32(&mut out, central_offset);
        write_u16(&mut out, 0);
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn write_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn write_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn write_u24(out: &mut Vec<u8>, value: u32) {
        assert!(value <= 0x00ff_ffff);
        out.push((value & 0xff) as u8);
        out.push(((value >> 8) & 0xff) as u8);
        out.push(((value >> 16) & 0xff) as u8);
    }
}
