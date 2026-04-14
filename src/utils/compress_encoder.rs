use std::{fmt, str::FromStr};

#[derive(Debug, PartialEq, Copy, Clone, Ord, PartialOrd)]
pub enum CompressionEncoder {
    PlainText,
    Gzip,
    Brotli,
    StatsigBrotli,
    Deflate,
    Compress,
    Identity,
    Zstd,
}

impl fmt::Display for CompressionEncoder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CompressionEncoder::PlainText => write!(f, "plain_text"),
            CompressionEncoder::Gzip => write!(f, "gzip"),
            CompressionEncoder::Brotli => write!(f, "br"),
            CompressionEncoder::StatsigBrotli => write!(f, "statsig-br"),
            CompressionEncoder::Deflate => write!(f, "deflate"),
            CompressionEncoder::Compress => write!(f, "compress"),
            CompressionEncoder::Identity => write!(f, "identity"),
            CompressionEncoder::Zstd => write!(f, "zstd"),
        }
    }
}

impl std::hash::Hash for CompressionEncoder {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.to_string().hash(state);
    }
}

impl FromStr for CompressionEncoder {
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "gzip" | "x-gzip" => Ok(CompressionEncoder::Gzip),
            "br" => Ok(CompressionEncoder::Brotli),
            "statsig_br" | "statsig-br" => Ok(CompressionEncoder::StatsigBrotli),
            "deflate" => Ok(CompressionEncoder::Deflate),
            "compress" => Ok(CompressionEncoder::Compress),
            "identity" => Ok(CompressionEncoder::Identity),
            "zstd" => Ok(CompressionEncoder::Zstd),
            "plain_text" => Ok(CompressionEncoder::PlainText),
            _ => Err(()),
        }
    }

    type Err = ();
}

impl Eq for CompressionEncoder {}

pub fn format_compression_encodings(encodings: &[CompressionEncoder]) -> String {
    encodings
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn encoding_priority(encoding: CompressionEncoder) -> u8 {
    match encoding {
        CompressionEncoder::StatsigBrotli => 0,
        CompressionEncoder::Gzip => 1,
        CompressionEncoder::Brotli => 2,
        CompressionEncoder::Zstd => 3,
        CompressionEncoder::Deflate => 4,
        CompressionEncoder::Compress => 5,
        CompressionEncoder::Identity => 6,
        CompressionEncoder::PlainText => 7,
    }
}

fn parse_accept_encoding_token(raw: &str) -> Option<(CompressionEncoder, bool)> {
    let mut parts = raw.split(';');
    let token = parts.next()?.trim().to_ascii_lowercase();

    let encoding: Option<CompressionEncoder> = CompressionEncoder::from_str(token.as_str()).ok();

    encoding?;

    for param in parts {
        let mut kv = param.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim();
        if !key.eq_ignore_ascii_case("q") {
            continue;
        }

        let value = kv.next().unwrap_or("").trim();
        if let Ok(q) = value.parse::<f32>() {
            if q <= 0.0 {
                return Some((encoding.unwrap(), false));
            }
        }
    }

    Some((encoding.unwrap(), true))
}

mod parsed_accept_encoding;

pub use parsed_accept_encoding::ParsedAcceptEncoding;

#[cfg(test)]
mod tests {
    use super::CompressionEncoder;

    #[test]
    fn normalization_preserves_existing_encoder_names() {
        let encoded = CompressionEncoder::StatsigBrotli.to_string();
        assert_eq!(encoded, "statsig-br");
    }
}
