use std::{fmt, str::FromStr};
#[derive(Debug, PartialEq, Copy, Clone, Ord, PartialOrd)]
pub enum CompressionEncoder {
    PlainText,
    Gzip,
    Brotli,
    StatsigBrotli,
}

impl fmt::Display for CompressionEncoder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CompressionEncoder::PlainText => write!(f, "plain_text"),
            CompressionEncoder::Gzip => write!(f, "gzip"),
            CompressionEncoder::Brotli => write!(f, "br"),
            CompressionEncoder::StatsigBrotli => write!(f, "statsig-br"),
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
            "gzip" => Ok(CompressionEncoder::Gzip),
            "br" => Ok(CompressionEncoder::Brotli),
            "statsig-br" => Ok(CompressionEncoder::StatsigBrotli),
            _ => Ok(CompressionEncoder::PlainText),
        }
    }

    type Err = ();
}

impl Eq for CompressionEncoder {}

pub fn format_compression_encodings(encodings: &Vec<CompressionEncoder>) -> String {
    encodings
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn convert_compression_encodings_from_header_map<'a>(
    header: impl Iterator<Item = &'a str>,
) -> Vec<CompressionEncoder> {
    let mut res: Vec<CompressionEncoder> = header
        .flat_map(|value| {
            let res: Vec<CompressionEncoder> = value
                .split(',')
                .filter_map(|v| (CompressionEncoder::from_str(v.trim())).ok())
                .collect();
            res
        })
        .collect();
    res.sort();
    res
}
