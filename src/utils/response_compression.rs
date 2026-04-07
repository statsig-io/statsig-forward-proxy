use std::io::{Cursor, Read, Write};

use flate2::{write::GzEncoder, Compression};

use super::compress_encoder::CompressionEncoder;

pub fn preferred_response_compression(encodings: &[CompressionEncoder]) -> CompressionEncoder {
    if encodings.contains(&CompressionEncoder::StatsigBrotli) {
        CompressionEncoder::StatsigBrotli
    } else if encodings.contains(&CompressionEncoder::Brotli) {
        CompressionEncoder::Brotli
    } else if encodings.contains(&CompressionEncoder::Gzip) {
        CompressionEncoder::Gzip
    } else {
        CompressionEncoder::PlainText
    }
}

pub fn compress_response_payload(payload: &[u8], encoding: CompressionEncoder) -> Option<Vec<u8>> {
    match encoding {
        CompressionEncoder::PlainText => Some(payload.to_vec()),
        CompressionEncoder::Gzip => {
            let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
            if gzip.write_all(payload).is_err() {
                return None;
            }
            gzip.finish().ok()
        }
        CompressionEncoder::Brotli | CompressionEncoder::StatsigBrotli => {
            let mut reader = brotli::CompressorReader::new(Cursor::new(payload), 4096, 5, 22);
            let mut compressed = Vec::new();
            if reader.read_to_end(&mut compressed).is_ok() {
                Some(compressed)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn content_encoding_header_value(encoding: CompressionEncoder) -> Option<&'static str> {
    match encoding {
        CompressionEncoder::StatsigBrotli => Some("statsig-br"),
        CompressionEncoder::Brotli => Some("br"),
        CompressionEncoder::Gzip => Some("gzip"),
        CompressionEncoder::Deflate => Some("deflate"),
        CompressionEncoder::Compress => Some("compress"),
        CompressionEncoder::Zstd => Some("zstd"),
        CompressionEncoder::PlainText | CompressionEncoder::Identity => None,
    }
}
