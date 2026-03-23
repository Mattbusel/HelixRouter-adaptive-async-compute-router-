//! HTTP content negotiation: media types, encodings, and language tags.
//!
//! Implements the `Accept`, `Accept-Encoding`, and `Accept-Language` header
//! parsing and preference resolution algorithms described in RFC 7231.

use std::collections::HashMap;
use std::fmt;

// ── MediaType ─────────────────────────────────────────────────────────────────

/// A parsed MIME media type with optional parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaType {
    /// Primary type (e.g. `"application"`).
    pub type_: String,
    /// Subtype (e.g. `"json"`).
    pub subtype: String,
    /// Optional parameters such as `charset`.
    pub params: HashMap<String, String>,
}

impl MediaType {
    /// Parse a media-type string such as `"application/json; charset=utf-8"`.
    ///
    /// Returns `None` if the string is not in the form `type/subtype`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.splitn(2, ';');
        let mime = parts.next()?.trim();
        let mut mime_parts = mime.splitn(2, '/');
        let type_ = mime_parts.next()?.trim().to_lowercase();
        let subtype = mime_parts.next()?.trim().to_lowercase();

        let mut params = HashMap::new();
        if let Some(param_str) = parts.next() {
            for param in param_str.split(';') {
                let param = param.trim();
                if let Some(eq) = param.find('=') {
                    let key = param[..eq].trim().to_lowercase();
                    let val = param[eq + 1..].trim().trim_matches('"').to_string();
                    params.insert(key, val);
                }
            }
        }

        Some(MediaType { type_, subtype, params })
    }

    /// Return the `q` quality factor (default 1.0).
    pub fn quality(&self) -> f64 {
        self.params
            .get("q")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0)
    }

    /// Return `true` if `self` matches `pattern`, supporting `*` wildcards in
    /// either the type or subtype position.
    pub fn matches(&self, pattern: &MediaType) -> bool {
        let type_ok = pattern.type_ == "*" || pattern.type_ == self.type_;
        let sub_ok = pattern.subtype == "*" || pattern.subtype == self.subtype;
        type_ok && sub_ok
    }

    /// Render the media type as a canonical string.
    pub fn to_str(&self) -> String {
        let mut s = format!("{}/{}", self.type_, self.subtype);
        for (k, v) in &self.params {
            if k != "q" {
                s.push_str(&format!("; {k}={v}"));
            }
        }
        s
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

// ── AcceptHeader ──────────────────────────────────────────────────────────────

/// Parsed `Accept` header: an ordered list of `(MediaType, q-value)` pairs.
pub struct AcceptHeader {
    /// Media types sorted by q-value descending.
    pub types: Vec<(MediaType, f64)>,
}

impl AcceptHeader {
    /// Parse an `Accept` header value such as
    /// `"text/html, application/json;q=0.9, */*;q=0.8"`.
    pub fn parse(header: &str) -> Self {
        let mut types: Vec<(MediaType, f64)> = header
            .split(',')
            .filter_map(|s| {
                let mt = MediaType::parse(s.trim())?;
                let q = mt.quality();
                Some((mt, q))
            })
            .collect();
        types.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        AcceptHeader { types }
    }

    /// Return the best matching media type from `available`, or `None`.
    pub fn preferred<'a>(&self, available: &'a [MediaType]) -> Option<&'a MediaType> {
        for (accepted, _q) in &self.types {
            for candidate in available {
                if candidate.matches(accepted) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Return `true` if the header accepts the given media type.
    pub fn accepts(&self, media_type: &MediaType) -> bool {
        self.types
            .iter()
            .any(|(accepted, q)| *q > 0.0 && media_type.matches(accepted))
    }
}

// ── ContentEncoding ───────────────────────────────────────────────────────────

/// HTTP content encodings supported for `Accept-Encoding` negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    /// No encoding (`identity`).
    Identity,
    /// gzip compression.
    Gzip,
    /// DEFLATE compression.
    Deflate,
    /// Brotli compression.
    Br,
    /// Zstandard compression.
    Zstd,
}

impl fmt::Display for ContentEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentEncoding::Identity => write!(f, "identity"),
            ContentEncoding::Gzip => write!(f, "gzip"),
            ContentEncoding::Deflate => write!(f, "deflate"),
            ContentEncoding::Br => write!(f, "br"),
            ContentEncoding::Zstd => write!(f, "zstd"),
        }
    }
}

impl ContentEncoding {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "identity" | "" => Some(ContentEncoding::Identity),
            "gzip" => Some(ContentEncoding::Gzip),
            "deflate" => Some(ContentEncoding::Deflate),
            "br" => Some(ContentEncoding::Br),
            "zstd" => Some(ContentEncoding::Zstd),
            _ => None,
        }
    }
}

// ── EncodingNegotiator ────────────────────────────────────────────────────────

/// Selects the best `Content-Encoding` given an `Accept-Encoding` header.
pub struct EncodingNegotiator;

impl EncodingNegotiator {
    /// Choose the best encoding from `available` that the client accepts.
    ///
    /// Falls back to `Identity` if nothing matches.
    pub fn negotiate(accept_encoding: &str, available: &[ContentEncoding]) -> ContentEncoding {
        // Parse quality values from the header
        let mut accepted: Vec<(String, f64)> = accept_encoding
            .split(',')
            .map(|s| {
                let s = s.trim();
                if let Some(semi) = s.find(';') {
                    let enc = s[..semi].trim().to_lowercase();
                    let q_part = s[semi + 1..].trim();
                    let q = q_part
                        .strip_prefix("q=")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(1.0);
                    (enc, q)
                } else {
                    (s.to_lowercase(), 1.0)
                }
            })
            .collect();
        accepted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (enc_str, q) in &accepted {
            if *q <= 0.0 {
                continue;
            }
            for &enc in available {
                if enc_str == "*" || enc_str == enc.to_string().as_str() {
                    return enc;
                }
            }
        }
        ContentEncoding::Identity
    }

    /// Return `true` if the client's `Accept-Encoding` header permits `encoding`.
    pub fn supported_by_client(header: &str, encoding: ContentEncoding) -> bool {
        let enc_str = encoding.to_string();
        for part in header.split(',') {
            let part = part.trim();
            let (name, q) = if let Some(semi) = part.find(';') {
                let name = part[..semi].trim();
                let q_part = part[semi + 1..].trim();
                let q = q_part
                    .strip_prefix("q=")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(1.0);
                (name, q)
            } else {
                (part, 1.0)
            };
            if (name == "*" || name.to_lowercase() == enc_str) && q > 0.0 {
                return true;
            }
        }
        false
    }
}

// ── LanguageTag ───────────────────────────────────────────────────────────────

/// A BCP 47-style language tag with optional region subtag and quality value.
#[derive(Debug, Clone)]
pub struct LanguageTag {
    /// Primary language subtag, e.g. `"en"`.
    pub language: String,
    /// Optional region subtag, e.g. `"US"`.
    pub region: Option<String>,
    /// Quality factor (default 1.0).
    pub quality: f64,
}

impl LanguageTag {
    /// Parse a single language tag such as `"en-US;q=0.9"`.
    pub fn parse(tag: &str) -> Self {
        let (tag_part, q) = if let Some(semi) = tag.find(';') {
            let t = &tag[..semi];
            let q_part = tag[semi + 1..].trim();
            let q = q_part
                .strip_prefix("q=")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            (t.trim(), q)
        } else {
            (tag.trim(), 1.0)
        };

        let mut parts = tag_part.splitn(2, '-');
        let language = parts.next().unwrap_or("").to_lowercase();
        let region = parts.next().map(|r| r.to_uppercase());

        LanguageTag { language, region, quality: q }
    }

    /// Return `true` if this tag matches `pattern` (e.g. `"en"` matches `"en-US"`).
    pub fn matches(&self, pattern: &str) -> bool {
        let pattern = pattern.to_lowercase();
        if let Some(dash) = pattern.find('-') {
            let lang = &pattern[..dash];
            let reg = pattern[dash + 1..].to_uppercase();
            self.language == lang && self.region.as_deref() == Some(reg.as_str())
        } else {
            self.language == pattern
        }
    }
}

// ── AcceptLanguage ────────────────────────────────────────────────────────────

/// Parsed `Accept-Language` header.
pub struct AcceptLanguage {
    /// Language tags sorted by quality descending.
    pub tags: Vec<LanguageTag>,
}

impl AcceptLanguage {
    /// Parse an `Accept-Language` header such as `"en-US,en;q=0.9,fr;q=0.8"`.
    pub fn parse(header: &str) -> Self {
        let mut tags: Vec<LanguageTag> = header
            .split(',')
            .map(|s| LanguageTag::parse(s.trim()))
            .collect();
        tags.sort_by(|a, b| {
            b.quality
                .partial_cmp(&a.quality)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        AcceptLanguage { tags }
    }

    /// Return the best matching language from `available`.
    pub fn preferred(&self, available: &[&str]) -> Option<String> {
        for tag in &self.tags {
            for &lang in available {
                if tag.matches(lang) {
                    return Some(lang.to_string());
                }
                // Also try prefix match: "en-US" matches available "en"
                if lang.to_lowercase() == tag.language {
                    return Some(lang.to_string());
                }
            }
        }
        None
    }
}

// ── NegotiationResult ─────────────────────────────────────────────────────────

/// Combined result of full content negotiation.
#[derive(Debug, Clone)]
pub struct NegotiationResult {
    /// Chosen content type, if any.
    pub content_type: Option<String>,
    /// Chosen content encoding.
    pub encoding: ContentEncoding,
    /// Chosen language, if any.
    pub language: Option<String>,
}

// ── ContentNegotiator ─────────────────────────────────────────────────────────

/// High-level content negotiation facade.
pub struct ContentNegotiator;

impl ContentNegotiator {
    /// Negotiate a content type from an `Accept` header and a list of available
    /// MIME type strings.  Returns the matched type string, or `None`.
    pub fn negotiate_type(accept: &str, available_types: &[&str]) -> Option<String> {
        let accept_hdr = AcceptHeader::parse(accept);
        let available: Vec<MediaType> = available_types
            .iter()
            .filter_map(|s| MediaType::parse(s))
            .collect();
        accept_hdr.preferred(&available).map(|mt| mt.to_str())
    }

    /// Negotiate a content encoding from an `Accept-Encoding` header.
    pub fn negotiate_encoding(accept_encoding: &str) -> ContentEncoding {
        let available = [
            ContentEncoding::Br,
            ContentEncoding::Zstd,
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Identity,
        ];
        EncodingNegotiator::negotiate(accept_encoding, &available)
    }

    /// Negotiate a language from an `Accept-Language` header and a list of
    /// available language strings.
    pub fn negotiate_language(
        accept_language: &str,
        available: &[&str],
    ) -> Option<String> {
        AcceptLanguage::parse(accept_language).preferred(available)
    }

    /// Perform full three-way negotiation.
    pub fn negotiate_all(
        accept: &str,
        accept_encoding: &str,
        accept_language: &str,
        available_types: &[&str],
        available_langs: &[&str],
    ) -> NegotiationResult {
        NegotiationResult {
            content_type: Self::negotiate_type(accept, available_types),
            encoding: Self::negotiate_encoding(accept_encoding),
            language: Self::negotiate_language(accept_language, available_langs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_media_type() {
        let mt = MediaType::parse("application/json; charset=utf-8").unwrap();
        assert_eq!(mt.type_, "application");
        assert_eq!(mt.subtype, "json");
        assert_eq!(mt.params.get("charset").map(String::as_str), Some("utf-8"));
    }

    #[test]
    fn accept_header_preferred() {
        let hdr = AcceptHeader::parse("text/html, application/json;q=0.9, */*;q=0.8");
        let available = vec![
            MediaType::parse("application/json").unwrap(),
        ];
        let pref = hdr.preferred(&available);
        assert!(pref.is_some());
        assert_eq!(pref.unwrap().subtype, "json");
    }

    #[test]
    fn encoding_negotiator_gzip() {
        let enc = EncodingNegotiator::negotiate(
            "gzip, deflate;q=0.8",
            &[ContentEncoding::Gzip, ContentEncoding::Deflate],
        );
        assert_eq!(enc, ContentEncoding::Gzip);
    }

    #[test]
    fn language_preferred() {
        let al = AcceptLanguage::parse("en-US,en;q=0.9,fr;q=0.8");
        let preferred = al.preferred(&["fr", "en"]);
        assert_eq!(preferred.as_deref(), Some("en"));
    }

    #[test]
    fn full_negotiation() {
        let result = ContentNegotiator::negotiate_all(
            "application/json",
            "gzip",
            "en-US",
            &["application/json", "text/html"],
            &["en", "fr"],
        );
        assert_eq!(result.content_type.as_deref(), Some("application/json"));
        assert_eq!(result.encoding, ContentEncoding::Gzip);
        assert!(result.language.is_some());
    }
}
