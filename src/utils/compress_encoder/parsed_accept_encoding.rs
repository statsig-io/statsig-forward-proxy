use super::{encoding_priority, parse_accept_encoding_token, CompressionEncoder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAcceptEncoding {
    acceptable_encodings: Vec<CompressionEncoder>,
    saw_non_empty_token: bool,
    saw_unrecognized_token: bool,
}

impl ParsedAcceptEncoding {
    pub fn from_raw_value(value: &str) -> Self {
        let mut parsed = Self {
            acceptable_encodings: Vec::new(),
            saw_non_empty_token: false,
            saw_unrecognized_token: false,
        };

        for raw in value.split(',') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            parsed.saw_non_empty_token = true;

            match parse_accept_encoding_token(raw) {
                Some((encoding, acceptable)) => {
                    if acceptable {
                        parsed.acceptable_encodings.push(encoding);
                    }
                }
                None => {
                    parsed.saw_unrecognized_token = true;
                }
            }
        }

        parsed
            .acceptable_encodings
            .sort_by_key(|encoding| encoding_priority(*encoding));
        parsed.acceptable_encodings.dedup();

        parsed
    }

    pub fn from_header_values<'a>(header: impl Iterator<Item = &'a str>) -> Self {
        let raw = header.collect::<Vec<_>>().join(",");
        Self::from_raw_value(&raw)
    }

    pub fn logger_tag(&self) -> Option<String> {
        if !self.saw_non_empty_token {
            return Some("none".to_string());
        }

        if self.acceptable_encodings.is_empty() {
            return Some("unknown".to_string());
        }

        let mut normalized = self
            .acceptable_encodings
            .iter()
            .map(|encoding| encoding.to_string())
            .collect::<Vec<_>>()
            .join("+");

        if self.saw_unrecognized_token {
            normalized.push_str("+unknown");
        }

        Some(normalized)
    }

    pub fn acceptable_encodings(&self) -> Vec<CompressionEncoder> {
        let mut encodings = self.acceptable_encodings.clone();
        encodings.retain(|encoding| {
            !matches!(
                encoding,
                CompressionEncoder::PlainText | CompressionEncoder::Identity
            )
        });
        encodings
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedAcceptEncoding;
    use crate::utils::compress_encoder::CompressionEncoder;
    use rocket::http::HeaderMap;

    #[test]
    fn filters_q_zero_from_acceptable_encodings() {
        let parsed = ParsedAcceptEncoding::from_raw_value("gzip;q=0, statsig-br;q=1, br");
        assert_eq!(
            parsed.acceptable_encodings(),
            vec![
                CompressionEncoder::StatsigBrotli,
                CompressionEncoder::Brotli
            ]
        );
    }

    #[test]
    fn normalize_accept_encoding_canonicalizes_order_and_dedupes() {
        let value = ParsedAcceptEncoding::from_raw_value("br;q=1, gzip ; q=1, br").logger_tag();
        assert_eq!(value, Some("gzip+br".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_supports_statsig_aliases() {
        let value = ParsedAcceptEncoding::from_raw_value("statsig_br, gzip").logger_tag();
        assert_eq!(value, Some("statsig-br+gzip".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_respects_q_zero() {
        let value = ParsedAcceptEncoding::from_raw_value("gzip;q=0, br;q=1").logger_tag();
        assert_eq!(value, Some("br".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_returns_unknown_for_non_empty_unrecognized_input() {
        let value = ParsedAcceptEncoding::from_raw_value("foo").logger_tag();
        assert_eq!(value, Some("unknown".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_returns_none_tag_for_empty_input() {
        let value = ParsedAcceptEncoding::from_raw_value("").logger_tag();
        assert_eq!(value, Some("none".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_preserves_known_and_marks_unknown_tokens() {
        let value = ParsedAcceptEncoding::from_raw_value("gzip,foo,br").logger_tag();
        assert_eq!(value, Some("gzip+br+unknown".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_handles_multiple_header_instances() {
        // Mirror `http_server.rs` behavior: multiple header instances are collected and joined.
        let mut map = HeaderMap::new();
        map.add_raw("Accept-Encoding", "gzip,br");
        map.add_raw("Accept-Encoding", "statsig-br,plain_text");

        let value =
            ParsedAcceptEncoding::from_header_values(map.get("Accept-Encoding")).logger_tag();

        assert_eq!(value, Some("statsig-br+gzip+br+plain_text".to_string()));
    }

    #[test]
    fn normalize_accept_encoding_supports_identity() {
        let value = ParsedAcceptEncoding::from_raw_value("identity").logger_tag();
        assert_eq!(value, Some("identity".to_string()));
    }
}
