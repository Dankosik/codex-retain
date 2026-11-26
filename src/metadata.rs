//! Validate the first rollout record without retaining its unused JSON fields.

use anyhow::{Context, Result, bail, ensure};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::io::{self, Read};

const MAX_FIRST_RECORD: usize = 1024 * 1024;
const READ_CHUNK: usize = 16 * 1024;
const MAX_READ_CHUNK: usize = 256 * 1024;

/// Reusable bounded reader for rollout first records.
/// Reuses the line buffer and zstd decompression context across files to avoid
/// per-file heap allocations and 16 KiB BufReader churn.
pub(crate) struct RecordReader {
    buf: Vec<u8>,
    zstd_in: Vec<u8>,
    zstd_decoder: Option<zstd::stream::raw::Decoder<'static>>,
}

impl Default for RecordReader {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordReader {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            zstd_in: Vec::new(),
            zstd_decoder: None,
        }
    }

    pub(crate) fn read_first_record(
        &mut self,
        file: &std::fs::File,
        compressed: bool,
    ) -> Result<&[u8]> {
        if compressed {
            self.read_compressed(file)
        } else {
            self.read_plain(file)
        }
    }

    fn read_plain(&mut self, mut file: &std::fs::File) -> Result<&[u8]> {
        if self.buf.len() < READ_CHUNK {
            self.buf.resize(READ_CHUNK, 0);
        }
        let max_total = MAX_FIRST_RECORD + 1;
        let mut filled = 0usize;
        let mut chunk_size = READ_CHUNK;
        loop {
            if filled >= max_total {
                bail!("missing or oversized rollout metadata");
            }
            let chunk_end = (filled + chunk_size).min(max_total);
            if self.buf.len() < chunk_end {
                self.buf.resize(chunk_end, 0);
            }
            let count = match file.read(&mut self.buf[filled..chunk_end]) {
                Ok(0) => bail!("missing or oversized rollout metadata"),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            let prev = filled;
            filled += count;
            if let Some(rel) = memchr::memchr(b'\n', &self.buf[prev..filled]) {
                let end = prev + rel + 1;
                ensure!(
                    end <= MAX_FIRST_RECORD,
                    "missing or oversized rollout metadata"
                );
                return Ok(&self.buf[..end]);
            }
            if chunk_size < MAX_READ_CHUNK {
                chunk_size = (chunk_size * 2).min(MAX_READ_CHUNK);
            }
        }
    }

    fn read_compressed(&mut self, mut file: &std::fs::File) -> Result<&[u8]> {
        use zstd::stream::raw::{DParameter, Decoder, InBuffer, Operation, OutBuffer};

        if let Some(decoder) = self.zstd_decoder.as_mut() {
            if let Err(error) = decoder.reinit() {
                self.zstd_decoder = None;
                return Err(error.into());
            }
        } else {
            let mut decoder = Decoder::new()?;
            decoder.set_parameter(DParameter::WindowLogMax(23))?;
            self.zstd_decoder = Some(decoder);
        }

        if self.zstd_in.len() < READ_CHUNK {
            self.zstd_in.resize(READ_CHUNK, 0);
        }
        if self.buf.len() < READ_CHUNK {
            self.buf.resize(READ_CHUNK, 0);
        }

        let max_total = MAX_FIRST_RECORD + 1;
        let mut in_pos = 0usize;
        let mut in_len = 0usize;
        let mut out_filled = 0usize;
        let mut out_chunk = READ_CHUNK;
        let mut in_frame = false;
        let mut eof = false;

        loop {
            if in_pos == in_len && !eof {
                let count = loop {
                    match file.read(&mut self.zstd_in[..READ_CHUNK]) {
                        Ok(count) => break count,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                };
                if count == 0 {
                    eof = true;
                } else {
                    in_pos = 0;
                    in_len = count;
                }
            }

            if out_filled >= max_total {
                bail!("missing or oversized rollout metadata");
            }
            let chunk_end = (out_filled + out_chunk).min(max_total);
            if self.buf.len() < chunk_end {
                self.buf.resize(chunk_end, 0);
            }

            let decoder = self.zstd_decoder.as_mut().expect("initialized above");
            let mut input = InBuffer::around(&self.zstd_in[in_pos..in_len]);
            let mut output = OutBuffer::around(&mut self.buf[out_filled..chunk_end]);
            let remaining = match decoder.run(&mut input, &mut output) {
                Ok(remaining) => remaining,
                Err(error) => {
                    self.zstd_decoder = None;
                    return Err(error.into());
                }
            };
            let consumed = input.pos();
            let written = output.pos();
            in_pos += consumed;
            if consumed > 0 || written > 0 {
                in_frame = remaining != 0;
            }
            if written > 0 {
                let prev = out_filled;
                out_filled += written;
                if let Some(rel) = memchr::memchr(b'\n', &self.buf[prev..out_filled]) {
                    let end = prev + rel + 1;
                    ensure!(
                        end <= MAX_FIRST_RECORD,
                        "missing or oversized rollout metadata"
                    );
                    return Ok(&self.buf[..end]);
                }
                if out_chunk < MAX_READ_CHUNK {
                    out_chunk = (out_chunk * 2).min(MAX_READ_CHUNK);
                }
                continue;
            }
            if consumed == 0 {
                if eof {
                    if in_frame {
                        self.zstd_decoder = None;
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "incomplete zstd frame",
                        )
                        .into());
                    }
                    bail!("missing or oversized rollout metadata");
                }
                if in_pos < in_len {
                    self.zstd_decoder = None;
                    bail!("stalled zstd decompression while reading rollout metadata");
                }
            }
        }
    }
}

pub(crate) fn validate_metadata(line: &[u8], thread_id: &str) -> Result<()> {
    let selection = Selection {
        field: Field::Root,
        thread_id,
    };
    let metadata = if let Ok(text) = simdutf8::basic::from_utf8(line) {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let metadata = selection
            .deserialize(&mut deserializer)
            .context("invalid rollout metadata")?;
        deserializer.end().context("invalid rollout metadata")?;
        metadata
    } else {
        // Fall back to SliceRead on invalid UTF-8 to preserve exact serde_json
        // diagnostics (line, column, and error precedence).
        let mut deserializer = serde_json::Deserializer::from_slice(line);
        let metadata = selection
            .deserialize(&mut deserializer)
            .context("invalid rollout metadata")?;
        deserializer.end().context("invalid rollout metadata")?;
        metadata
    };
    ensure!(
        metadata.record_type && metadata.thread_id,
        "rollout metadata identity mismatch"
    );
    ensure!(
        metadata.history_mode,
        "rollout metadata has unsupported history mode"
    );
    ensure!(
        metadata.history_base,
        "rollout contains a shared history reference"
    );
    Ok(())
}

struct Metadata {
    record_type: bool,
    thread_id: bool,
    history_mode: bool,
    history_base: bool,
}

impl Default for Metadata {
    fn default() -> Self {
        // Value indexing treats absent fields and fields on non-objects as null.
        Self {
            record_type: false,
            thread_id: false,
            history_mode: true,
            history_base: true,
        }
    }
}

#[derive(Clone, Copy)]
enum Field {
    Root,
    Payload,
    RecordType,
    ThreadId,
    HistoryMode,
    HistoryBase,
    Discard,
}

#[derive(Clone, Copy)]
struct Selection<'a> {
    field: Field,
    thread_id: &'a str,
}

impl Selection<'_> {
    fn non_null(self) -> Metadata {
        Metadata {
            history_mode: !matches!(self.field, Field::HistoryMode),
            history_base: !matches!(self.field, Field::HistoryBase),
            ..Metadata::default()
        }
    }

    fn child(self, field: Field) -> Self {
        Self { field, ..self }
    }
}

impl<'de> DeserializeSeed<'de> for Selection<'_> {
    type Value = Metadata;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Metadata, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Selection<'_> {
    type Value = Metadata;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Metadata, E> {
        Ok(Metadata::default())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Metadata, E> {
        Ok(self.non_null())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Metadata, E> {
        Ok(self.non_null())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Metadata, E> {
        Ok(self.non_null())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Metadata, E> {
        Ok(self.non_null())
    }

    fn visit_str<E>(self, value: &str) -> Result<Metadata, E> {
        let mut metadata = self.non_null();
        match self.field {
            Field::RecordType => metadata.record_type = value == "session_meta",
            Field::ThreadId => metadata.thread_id = value == self.thread_id,
            Field::HistoryMode => metadata.history_mode = value == "legacy",
            _ => {}
        }
        Ok(metadata)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Metadata, A::Error> {
        // Do not use IgnoredAny: every unused value must still receive the same
        // UTF-8, number-range and recursion-limit validation as serde_json::Value.
        while values
            .next_element_seed(self.child(Field::Discard))?
            .is_some()
        {}
        Ok(self.non_null())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> Result<Metadata, A::Error> {
        let mut metadata = self.non_null();
        while let Some(key) = values.next_key::<Key>()? {
            let field = match (self.field, key) {
                (Field::Root, Key::RecordType) => Field::RecordType,
                (Field::Root, Key::Payload) => Field::Payload,
                (Field::Payload, Key::ThreadId) => Field::ThreadId,
                (Field::Payload, Key::HistoryMode) => Field::HistoryMode,
                (Field::Payload, Key::HistoryBase) => Field::HistoryBase,
                _ => Field::Discard,
            };
            let value = values.next_value_seed(self.child(field))?;
            // Assign rather than reject duplicate keys: Value keeps the last
            // occurrence, including replacement of the whole payload object.
            match field {
                Field::RecordType => metadata.record_type = value.record_type,
                Field::ThreadId => metadata.thread_id = value.thread_id,
                Field::HistoryMode => metadata.history_mode = value.history_mode,
                Field::HistoryBase => metadata.history_base = value.history_base,
                Field::Payload => {
                    metadata.thread_id = value.thread_id;
                    metadata.history_mode = value.history_mode;
                    metadata.history_base = value.history_base;
                }
                _ => {}
            }
        }
        Ok(metadata)
    }
}

enum Key {
    RecordType,
    Payload,
    ThreadId,
    HistoryMode,
    HistoryBase,
    Other,
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;

        impl Visitor<'_> for KeyVisitor {
            type Value = Key;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object key")
            }

            fn visit_str<E>(self, key: &str) -> Result<Key, E> {
                Ok(match key {
                    "type" => Key::RecordType,
                    "payload" => Key::Payload,
                    "id" => Key::ThreadId,
                    "history_mode" => Key::HistoryMode,
                    "history_base" => Key::HistoryBase,
                    _ => Key::Other,
                })
            }
        }

        // The slice deserializer borrows ordinary keys and string values and
        // reuses its scratch buffer for escapes; none survive their callback.
        deserializer.deserialize_str(KeyVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD_ID: &str = "a55ef791-cfc1-447c-99f0-d7037caa54a8";

    fn reference(line: &[u8], thread_id: &str) -> Result<()> {
        let meta: serde_json::Value =
            serde_json::from_slice(line).context("invalid rollout metadata")?;
        ensure!(
            meta["type"] == "session_meta" && meta["payload"]["id"] == thread_id,
            "rollout metadata identity mismatch"
        );
        let mode = &meta["payload"]["history_mode"];
        ensure!(
            mode.is_null() || mode == "legacy",
            "rollout metadata has unsupported history mode"
        );
        ensure!(
            meta["payload"]["history_base"].is_null(),
            "rollout contains a shared history reference"
        );
        Ok(())
    }

    fn assert_equivalent(line: &[u8]) {
        let expected = reference(line, THREAD_ID).map_err(|error| format!("{error:#}"));
        let actual = validate_metadata(line, THREAD_ID).map_err(|error| format!("{error:#}"));
        assert_eq!(actual, expected, "input: {line:?}");
    }

    fn record(payload: &str) -> String {
        format!(r#"{{"type":"session_meta","payload":{payload}}}"#)
    }

    #[test]
    fn supported_metadata_and_escaped_fields() {
        for suffix in [
            "",
            r#", "history_mode":null, "history_base":null"#,
            r#", "history_mode":"legacy""#,
            r#", "history_mode":"leg\u0061cy", "history_base":null"#,
        ] {
            let line = record(&format!(r#"{{"id":"{THREAD_ID}"{suffix}}}"#));
            validate_metadata(line.as_bytes(), THREAD_ID).unwrap();
            assert_equivalent(line.as_bytes());
        }
        let escaped = br#"{"t\u0079pe":"session_\u006deta","pay\u006coad":{"i\u0064":"\u006155ef791-cfc1-447c-99f0-d7037caa54a8","history_\u006dode":"legacy","history_b\u0061se":null}}"#;
        validate_metadata(escaped, THREAD_ID).unwrap();
        assert_equivalent(escaped);
    }

    #[test]
    fn wrong_json_types_preserve_validation_and_error_precedence() {
        let wrong_types = ["null", "false", "0", "1.25", r#""other""#, "[]", "{}"];
        for value in wrong_types {
            assert_equivalent(value.as_bytes());
            assert_equivalent(record(value).as_bytes());
            for field in ["id", "history_mode", "history_base"] {
                let line = record(&format!(r#"{{"id":"{THREAD_ID}","{field}":{value}}}"#));
                assert_equivalent(line.as_bytes());
            }
            let line = format!(
                r#"{{"type":{value},"payload":{{"id":"{THREAD_ID}","history_mode":"paginated","history_base":{{}}}}}}"#
            );
            assert_equivalent(line.as_bytes());
        }
        for line in [
            "{}",
            r#"{"type":"session_meta"}"#,
            r#"{"type":"session_meta","payload":{"history_mode":"paginated"}}"#,
        ] {
            assert_equivalent(line.as_bytes());
        }
        let line = record(&format!(
            r#"{{"id":"{THREAD_ID}","history_mode":"paginated","history_base":{{}}}}"#
        ));
        assert_eq!(
            validate_metadata(line.as_bytes(), THREAD_ID)
                .unwrap_err()
                .to_string(),
            "rollout metadata has unsupported history mode"
        );
    }

    #[test]
    fn duplicate_fields_and_whole_payloads_keep_last_value() {
        let valid = format!(r#"{{"id":"{THREAD_ID}"}}"#);
        for other in [
            "null".to_owned(),
            "[]".to_owned(),
            "{}".to_owned(),
            format!(r#"{{"id":"{THREAD_ID}","history_mode":"paginated"}}"#),
            format!(r#"{{"id":"{THREAD_ID}","history_base":{{}}}}"#),
        ] {
            for (first, last) in [(&valid, &other), (&other, &valid)] {
                let line =
                    format!(r#"{{"type":"session_meta","payload":{first},"payload":{last}}}"#);
                assert_equivalent(line.as_bytes());
            }
        }
        for (field, valid, invalid) in [
            ("type", r#""session_meta""#.to_owned(), "null"),
            ("id", format!(r#""{THREAD_ID}""#), "{}"),
            ("history_mode", r#""legacy""#.to_owned(), r#""paginated""#),
            ("history_base", "null".to_owned(), "{}"),
        ] {
            for (first, last) in [(valid.as_str(), invalid), (invalid, valid.as_str())] {
                let duplicate = format!(r#""{field}":{first},"{field}":{last}"#);
                let line = if field == "type" {
                    format!(r#"{{{duplicate},"payload":{{"id":"{THREAD_ID}"}}}}"#)
                } else {
                    record(&format!(r#"{{"id":"{THREAD_ID}",{duplicate}}}"#))
                };
                assert_equivalent(line.as_bytes());
            }
        }
        let line = record(&format!(
            r#"{{"id":"wrong","i\u0064":"{THREAD_ID}","history_mode":"paginated","history_\u006dode":null}}"#
        ));
        validate_metadata(line.as_bytes(), THREAD_ID).unwrap();
        assert_equivalent(line.as_bytes());
    }

    #[test]
    fn ignored_values_are_fully_validated() {
        for invalid in [
            "[0,]",
            "{\"a\":}",
            "{0:1}",
            "[true false]",
            "1e9999",
            "-1e9999",
            "01",
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\x41""#,
            "\"a\u{1f}b\"",
            r#"{"\uD800":1}"#,
        ] {
            let line = format!(
                r#"{{"type":"session_meta","payload":{{"id":"{THREAD_ID}"}},"ignored":{invalid}}}"#
            );
            assert!(validate_metadata(line.as_bytes(), THREAD_ID).is_err());
            assert_equivalent(line.as_bytes());
            // Invalid JSON wins even when a later duplicate would replace it.
            let line = record(&format!(r#"{{"id":{invalid},"id":"{THREAD_ID}"}}"#));
            assert_equivalent(line.as_bytes());
        }
        for invalid_utf8 in [b"\xff".as_slice(), b"\xc0\xaf", b"\xed\xa0\x80"] {
            let mut line =
                format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_ID}"}},"ignored":""#)
                    .into_bytes();
            line.extend_from_slice(invalid_utf8);
            line.extend_from_slice(b"\"}");
            assert_equivalent(&line);
            assert!(validate_metadata(&line, THREAD_ID).is_err());
        }
    }

    #[test]
    fn trailing_content_and_incomplete_records_are_rejected() {
        let valid = record(&format!(r#"{{"id":"{THREAD_ID}"}}"#));
        for suffix in ["", "\n", " \r\n\t", " null", "{}", ",", "x"] {
            let line = format!("{valid}{suffix}");
            assert_equivalent(line.as_bytes());
        }
        for length in 0..valid.len() {
            let line = &valid.as_bytes()[..length];
            assert!(validate_metadata(line, THREAD_ID).is_err());
            assert_equivalent(line);
        }
    }

    #[test]
    fn ignored_values_preserve_json_recursion_limit() {
        for depth in [1, 32, 124, 125, 126, 127, 128, 129, 256] {
            for (open, close) in [("[", "]"), ("{\"x\":", "}")] {
                let nested = format!("{}null{}", open.repeat(depth), close.repeat(depth));
                let line = format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{THREAD_ID}"}},"ignored":{nested}}}"#
                );
                assert_equivalent(line.as_bytes());
            }
        }
    }

    #[test]
    fn large_unused_objects_and_arrays_do_not_change_metadata() {
        let item = r#"{"unused":{"tools":["αβγ","\uD834\uDD1E",false,null,1e100]},"id":"wrong","history_mode":"paginated"}"#;
        let ignored = std::iter::repeat_n(item, 4096)
            .collect::<Vec<_>>()
            .join(",");
        let line = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{THREAD_ID}","unused":[{ignored}]}},"ignored":{{"id":false,"history_base":{{}}}}}}"#
        );
        validate_metadata(line.as_bytes(), THREAD_ID).unwrap();
        assert_equivalent(line.as_bytes());
    }

    #[test]
    fn record_reader_reuses_buffers_and_preserves_plain_and_zstd_bounds() {
        use std::io::{Seek, Write};

        let make_file = |bytes: &[u8]| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            file.rewind().unwrap();
            file
        };

        let mut reader = RecordReader::new();

        // Plain: small, multi-chunk (>16 KiB), exact 1 MiB, 1 MiB + 1, missing newline, empty.
        let small = b"first line\nsecond line\n";
        assert_eq!(
            reader.read_first_record(&make_file(small), false).unwrap(),
            b"first line\n"
        );
        let mut multi_chunk = vec![b'a'; 40 * 1024];
        multi_chunk.push(b'\n');
        let mut multi_with_tail = multi_chunk.clone();
        multi_with_tail.extend_from_slice(b"tail\n");
        assert_eq!(
            reader
                .read_first_record(&make_file(&multi_with_tail), false)
                .unwrap(),
            multi_chunk.as_slice()
        );
        let mut exact_max = vec![b'x'; MAX_FIRST_RECORD - 1];
        exact_max.push(b'\n');
        assert_eq!(
            reader
                .read_first_record(&make_file(&exact_max), false)
                .unwrap()
                .len(),
            MAX_FIRST_RECORD
        );
        let mut over_max = vec![b'x'; MAX_FIRST_RECORD];
        over_max.push(b'\n');
        assert!(
            reader
                .read_first_record(&make_file(&over_max), false)
                .is_err()
        );
        assert!(
            reader
                .read_first_record(&make_file(b"no-newline"), false)
                .is_err()
        );
        assert!(reader.read_first_record(&make_file(b""), false).is_err());

        // Compressed: reuse across valid frames, concatenated frames, exact 1 MiB, >1 MiB,
        // truncated frame, oversized window (log=24) AFTER a valid frame, and recovery after error.
        let enc = |data: &[u8]| zstd::stream::encode_all(data, 1).unwrap();
        assert_eq!(
            reader
                .read_first_record(&make_file(&enc(small)), true)
                .unwrap(),
            b"first line\n"
        );
        assert_eq!(
            reader
                .read_first_record(&make_file(&enc(&multi_with_tail)), true)
                .unwrap(),
            multi_chunk.as_slice()
        );
        let mut concat = enc(b"part1-");
        concat.extend_from_slice(&enc(b"part2\nrest"));
        assert_eq!(
            reader.read_first_record(&make_file(&concat), true).unwrap(),
            b"part1-part2\n"
        );
        assert_eq!(
            reader
                .read_first_record(&make_file(&enc(&exact_max)), true)
                .unwrap()
                .len(),
            MAX_FIRST_RECORD
        );
        assert!(
            reader
                .read_first_record(&make_file(&enc(&over_max)), true)
                .is_err()
        );
        let valid_encoded = enc(b"no-newline-in-zst");
        assert!(
            reader
                .read_first_record(&make_file(&valid_encoded), true)
                .is_err()
        );
        assert!(
            reader
                .read_first_record(&make_file(&valid_encoded[..valid_encoded.len() - 2]), true)
                .is_err()
        );
        assert!(reader.read_first_record(&make_file(b""), true).is_err());

        // Oversized window (log=24 > 23) must STILL be rejected after reusing the decoder.
        let mut oversized_window = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x70];
        let payload = b"hello\n";
        let block_header = ((payload.len() as u32) << 3) | 1;
        oversized_window.extend_from_slice(&block_header.to_le_bytes()[..3]);
        oversized_window.extend_from_slice(payload);
        assert!(
            reader
                .read_first_record(&make_file(&oversized_window), true)
                .is_err()
        );
        // After an error, subsequent valid zstd frames must still decode cleanly.
        assert_eq!(
            reader
                .read_first_record(&make_file(&enc(small)), true)
                .unwrap(),
            b"first line\n"
        );
    }
}
