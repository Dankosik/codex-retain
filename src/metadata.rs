//! Validate the first rollout record without retaining its unused JSON fields.

use anyhow::{Context, Result, ensure};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

/// Read only the first metadata record, with the same bounds for every format.
pub(crate) fn first_record(file: &std::fs::File, compressed: bool) -> Result<Vec<u8>> {
    use std::io::{BufRead, BufReader, Read};
    let reader: Box<dyn Read + '_> = if compressed {
        let mut decoder =
            zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(16384, file))?;
        decoder.window_log_max(23)?;
        Box::new(decoder)
    } else {
        Box::new(file)
    };
    let mut line = Vec::new();
    BufReader::with_capacity(16384, reader.take(1024 * 1024 + 1)).read_until(b'\n', &mut line)?;
    ensure!(
        line.len() <= 1024 * 1024 && line.last() == Some(&b'\n'),
        "missing or oversized rollout metadata"
    );
    Ok(line)
}

pub(crate) fn validate_metadata(line: &[u8], thread_id: &str) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_slice(line);
    let metadata = Selection {
        field: Field::Root,
        thread_id,
    }
    .deserialize(&mut deserializer)
    .context("invalid rollout metadata")?;
    deserializer.end().context("invalid rollout metadata")?;
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
}
