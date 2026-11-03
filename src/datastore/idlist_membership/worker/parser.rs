use sha2::{Digest, Sha256};

use super::WorkerError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileChange {
    pub(super) add: bool,
    pub(super) member: String,
}

pub(super) struct DecodedLine {
    pub(super) change: Option<FileChange>,
    pub(super) end_offset: u64,
}

pub(super) struct CompleteLineDecoder {
    pending: Vec<u8>,
    next_offset: u64,
}

impl CompleteLineDecoder {
    pub(super) fn new(start: u64) -> Self {
        Self {
            pending: Vec::new(),
            next_offset: start,
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<DecodedLine>, WorkerError> {
        self.pending.extend_from_slice(chunk);
        let mut decoded = Vec::new();
        let mut cursor = 0usize;
        // Only newline-terminated records receive an end offset. Bytes after the
        // final newline stay buffered so a retry cannot resume in the middle of a member ID.
        while let Some(relative_newline) = self.pending[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let newline = cursor + relative_newline;
            let mut line = &self.pending[cursor..newline];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            decoded.push(DecodedLine {
                change: (!line.is_empty())
                    .then(|| parse_change_line(line))
                    .transpose()?,
                end_offset: self.next_offset + newline as u64 + 1,
            });
            cursor = newline + 1;
        }
        if cursor > 0 {
            self.pending.drain(..cursor);
            self.next_offset += cursor as u64;
        }
        Ok(decoded)
    }

    pub(super) fn has_partial_line(&self) -> bool {
        !self.pending.is_empty()
    }
}

fn parse_change_line(line: &[u8]) -> Result<FileChange, WorkerError> {
    let (&prefix, member) = line
        .split_first()
        .ok_or_else(|| WorkerError::InvalidFile("empty line".to_string()))?;
    let add = match prefix {
        b'+' => true,
        b'-' => false,
        _ => {
            return Err(WorkerError::InvalidFile(format!(
                "line has invalid prefix 0x{prefix:02x}"
            )))
        }
    };
    if member.is_empty() {
        return Err(WorkerError::InvalidFile(
            "line has an empty member".to_string(),
        ));
    }
    Ok(FileChange {
        add,
        member: String::from_utf8(member.to_vec())
            .map_err(|_| WorkerError::InvalidFile("member is not UTF-8".to_string()))?,
    })
}

pub(super) fn content_range_starts_at(value: &str, expected_start: u64) -> bool {
    value
        .strip_prefix("bytes ")
        .and_then(|range| range.split_once('-'))
        .and_then(|(start, _)| start.parse::<u64>().ok())
        == Some(expected_start)
}

pub(super) fn partition_for(member: &str, partitions: u64) -> u64 {
    // A stable digest makes partition checkpoints portable across pods and
    // process restarts; Rust's default HashMap hasher is deliberately randomized.
    let digest = Sha256::digest(member.as_bytes());
    let hash = u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 has 8 bytes"));
    hash % partitions.max(1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::datastore::idlist_membership::schema::task_meta_fields;
    use crate::datastore::idlist_membership::worker::{IdListWorkerConfig, TaskMetadata, TaskOp};

    #[test]
    fn parses_add_and_remove_lines() {
        assert_eq!(
            parse_change_line(b"+abc").unwrap(),
            FileChange {
                add: true,
                member: "abc".to_string()
            }
        );
        assert_eq!(
            parse_change_line(b"-abc").unwrap(),
            FileChange {
                add: false,
                member: "abc".to_string()
            }
        );
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse_change_line(b"abc").is_err());
        assert!(parse_change_line(b"+").is_err());
        assert!(parse_change_line(&[b'+', 0xff]).is_err());
    }

    #[test]
    fn content_range_start_must_match_exactly() {
        assert!(content_range_starts_at("bytes 10-19/20", 10));
        assert!(!content_range_starts_at("bytes 100-199/200", 10));
        assert!(!content_range_starts_at("items 10-19/20", 10));
    }

    #[test]
    fn decoder_carries_partial_trailing_line_without_advancing_checkpoint() {
        let mut decoder = CompleteLineDecoder::new(100);
        let first = decoder.push(b"+alpha\n+par").unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].end_offset, 107);
        assert_eq!(first[0].change.as_ref().unwrap().member, "alpha");
        assert!(decoder.has_partial_line());

        let second = decoder.push(b"tial\n-beta\r\n").unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].change.as_ref().unwrap().member, "partial");
        assert_eq!(second[1].change.as_ref().unwrap().member, "beta");
        assert!(!decoder.has_partial_line());
    }

    #[test]
    fn task_metadata_requires_load_fields_but_remove_is_minimal() {
        let remove = HashMap::from([(task_meta_fields::OP.to_string(), "remove".to_string())]);
        assert_eq!(TaskMetadata::from_hash(&remove).unwrap().op, TaskOp::Remove);

        let load = HashMap::from([
            (task_meta_fields::OP.to_string(), "load".to_string()),
            (task_meta_fields::FILE_ID.to_string(), "f1".to_string()),
            (
                task_meta_fields::CREATION_TIME.to_string(),
                "123".to_string(),
            ),
            (task_meta_fields::URL.to_string(), "https://x".to_string()),
            (task_meta_fields::SIZE.to_string(), "9".to_string()),
        ]);
        assert_eq!(
            TaskMetadata::from_hash(&load).unwrap().version_tag(),
            "f1@123"
        );
    }

    #[test]
    fn partitioning_is_deterministic_and_in_range() {
        let first = partition_for("member", 7);
        assert_eq!(first, partition_for("member", 7));
        assert!(first < 7);
        assert_eq!(partition_for("member", 1), 0);
    }

    #[test]
    fn default_config_has_bounded_positive_values() {
        let config = IdListWorkerConfig::default();
        assert!(config.pipeline_batch_size > 0);
        assert!(config.scan_count > 0);
        assert!(config.diff_partition_max_members > 0);
        assert!(config.lock_ttl_s >= 3);
    }
}
