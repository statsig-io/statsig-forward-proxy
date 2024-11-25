use std::{fmt, str::FromStr};
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum CompressionEncoder {
    PlainText,
    Gzip,
}

impl fmt::Display for CompressionEncoder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CompressionEncoder::PlainText => write!(f, "plain_text"),
            CompressionEncoder::Gzip => write!(f, "gzip"),
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
            _ => Ok(CompressionEncoder::PlainText),
        }
    }

    type Err = ();
}

impl Eq for CompressionEncoder {}
