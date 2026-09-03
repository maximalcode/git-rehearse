//! Pure parsing and transformation for repository-local merge configuration.

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) key: Vec<u8>,
    pub(super) value: Option<Vec<u8>>,
}

pub(super) fn parse(output: &[u8]) -> Result<Vec<Entry>, String> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut entries = Vec::new();
    while let Some(origin) = fields.next() {
        if origin.is_empty() {
            continue;
        }
        let record = fields.next().ok_or_else(|| {
            format!(
                "repository-local merge config from {:?} was truncated",
                String::from_utf8_lossy(origin)
            )
        })?;
        if let Some(separator) = record.iter().position(|byte| *byte == b'\n') {
            entries.push(Entry {
                key: record[..separator].to_vec(),
                value: Some(record[separator + 1..].to_vec()),
            });
        } else {
            entries.push(Entry {
                key: record.to_vec(),
                value: None,
            });
        }
    }
    Ok(entries)
}

pub(super) fn append(config: &[u8], entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mut transformed = config.to_vec();
    if !transformed.is_empty() && !transformed.ends_with(b"\n") {
        transformed.push(b'\n');
    }
    for entry in entries {
        append_entry(&mut transformed, entry)?;
    }
    Ok(transformed)
}

fn append_entry(config: &mut Vec<u8>, entry: &Entry) -> Result<(), String> {
    let separator = entry
        .key
        .iter()
        .rposition(|byte| *byte == b'.')
        .ok_or_else(|| {
            format!(
                "repository-local merge config key has no variable: {:?}",
                String::from_utf8_lossy(&entry.key)
            )
        })?;
    let (section, variable) = (&entry.key[..separator], &entry.key[separator + 1..]);
    let header = if section == b"merge" {
        b"[merge]".to_vec()
    } else {
        let Some(driver) = section.strip_prefix(b"merge.") else {
            return Err(format!(
                "repository-local merge config key is not a merge setting: {:?}",
                String::from_utf8_lossy(&entry.key)
            ));
        };
        let mut header = b"[merge \"".to_vec();
        for byte in driver {
            if *byte == b'\\' || *byte == b'"' {
                header.push(b'\\');
            }
            header.push(*byte);
        }
        header.extend_from_slice(b"\"]");
        header
    };
    config.extend_from_slice(&header);
    config.extend_from_slice(b"\n\t");
    config.extend_from_slice(variable);
    if let Some(value) = &entry.value {
        config.extend_from_slice(b" = ");
        append_quoted_value(config, value);
    }
    config.push(b'\n');
    Ok(())
}

fn append_quoted_value(config: &mut Vec<u8>, value: &[u8]) {
    config.push(b'"');
    for byte in value {
        match byte {
            b'\n' => config.extend_from_slice(b"\\n"),
            b'\t' => config.extend_from_slice(b"\\t"),
            b'\x08' => config.extend_from_slice(b"\\b"),
            b'\\' => config.extend_from_slice(b"\\\\"),
            b'"' => config.extend_from_slice(b"\\\""),
            byte => config.push(*byte),
        }
    }
    config.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::{Entry, append, parse};

    #[test]
    fn parses_driver_commands_without_decoding_their_bytes() {
        let entries = parse(b"file:.git/config\0merge.raw.driver\nprintf '\xff' > %A\0")
            .expect("config output parses");
        assert_eq!(
            entries,
            vec![Entry {
                key: b"merge.raw.driver".to_vec(),
                value: Some(b"printf '\xff' > %A".to_vec()),
            }]
        );
    }

    #[test]
    fn appends_a_valueless_key_without_inventing_an_empty_value() {
        let config = b"[core]\r\n\tbare = true\r\n";
        assert_eq!(
            append(
                config,
                &[Entry {
                    key: b"merge.raw.driver".to_vec(),
                    value: None,
                }]
            )
            .expect("transforms"),
            b"[core]\r\n\tbare = true\r\n[merge \"raw\"]\n\tdriver\n"
        );
    }

    #[test]
    fn appends_a_non_utf8_value_without_decoding_it() {
        let transformed = append(
            b"[core]\n",
            &[Entry {
                key: b"merge.raw.driver".to_vec(),
                value: Some(b"printf '\xff' > %A".to_vec()),
            }],
        )
        .expect("transforms");
        assert_eq!(
            transformed,
            b"[core]\n[merge \"raw\"]\n\tdriver = \"printf '\xff' > %A\"\n"
        );
    }
}
