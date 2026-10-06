#![allow(dead_code)]

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};

#[derive(Debug, PartialEq, Eq)]
struct EntrySnapshot {
    name: String,
    hash: String,
    size: u64,
    entry_type: u32,
}

impl<'de> Deserialize<'de> for EntrySnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = EntrySnapshot;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Kubo listing entry object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut name: Option<String> = None;
                let mut hash: Option<String> = None;
                let mut size: Option<u64> = None;
                let mut entry_type: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "Name" => {
                            if name.is_some() {
                                return Err(de::Error::duplicate_field("Name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "Hash" => {
                            if hash.is_some() {
                                return Err(de::Error::duplicate_field("Hash"));
                            }
                            hash = Some(map.next_value()?);
                        }
                        "Size" => {
                            if size.is_some() {
                                return Err(de::Error::duplicate_field("Size"));
                            }
                            size = Some(map.next_value()?);
                        }
                        "Type" => {
                            if entry_type.is_some() {
                                return Err(de::Error::duplicate_field("Type"));
                            }
                            entry_type = Some(map.next_value()?);
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }

                Ok(EntrySnapshot {
                    name: name.ok_or_else(|| de::Error::missing_field("Name"))?,
                    hash: hash.ok_or_else(|| de::Error::missing_field("Hash"))?,
                    size: size.ok_or_else(|| de::Error::missing_field("Size"))?,
                    entry_type: entry_type.ok_or_else(|| de::Error::missing_field("Type"))?,
                })
            }
        }

        deserializer.deserialize_map(EntryVisitor)
    }
}

struct LsObject {
    links: Vec<EntrySnapshot>,
}

impl<'de> Deserialize<'de> for LsObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LsObjectVisitor;

        impl<'de> Visitor<'de> for LsObjectVisitor {
            type Value = LsObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an IPFS ls object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut links: Option<Vec<EntrySnapshot>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "Links" {
                        if links.is_some() {
                            return Err(de::Error::duplicate_field("Links"));
                        }
                        links = Some(map.next_value()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }

                Ok(LsObject {
                    links: links.ok_or_else(|| de::Error::missing_field("Links"))?,
                })
            }
        }

        deserializer.deserialize_map(LsObjectVisitor)
    }
}

struct LsEnvelope {
    entries: Vec<EntrySnapshot>,
}

impl<'de> Deserialize<'de> for LsEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LsEnvelopeVisitor;

        impl<'de> Visitor<'de> for LsEnvelopeVisitor {
            type Value = LsEnvelope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an IPFS ls response object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut objects: Option<Vec<LsObject>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "Objects" {
                        if objects.is_some() {
                            return Err(de::Error::duplicate_field("Objects"));
                        }
                        objects = Some(map.next_value()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }

                let mut objects = objects.ok_or_else(|| de::Error::missing_field("Objects"))?;
                if objects.len() != 1 {
                    return Err(de::Error::custom("expected exactly one Objects item"));
                }

                Ok(LsEnvelope {
                    entries: objects.pop().expect("length checked above").links,
                })
            }
        }

        deserializer.deserialize_map(LsEnvelopeVisitor)
    }
}

struct MfsEnvelope {
    entries: Vec<EntrySnapshot>,
}

impl<'de> Deserialize<'de> for MfsEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MfsEnvelopeVisitor;

        impl<'de> Visitor<'de> for MfsEnvelopeVisitor {
            type Value = MfsEnvelope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an MFS ls response object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries: Option<Vec<EntrySnapshot>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "Entries" {
                        if entries.is_some() {
                            return Err(de::Error::duplicate_field("Entries"));
                        }
                        entries = Some(
                            map.next_value::<Option<Vec<EntrySnapshot>>>()?
                                .unwrap_or_default(),
                        );
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }

                Ok(MfsEnvelope {
                    entries: entries.ok_or_else(|| de::Error::missing_field("Entries"))?,
                })
            }
        }

        deserializer.deserialize_map(MfsEnvelopeVisitor)
    }
}

pub fn check_ls_response(data: &[u8], parsed: Option<&[ipfs::LsEntry]>) {
    let oracle = serde_json::from_slice::<LsEnvelope>(data).map(|value| value.entries);
    match (oracle, parsed) {
        (Ok(expected), Some(actual)) => {
            assert_ls_entries(&expected, actual);
            let canonical = serde_json::json!({
                "Objects": [{
                    "Links": actual.iter().map(|entry| serde_json::json!({
                        "Name": &entry.name,
                        "Hash": &entry.hash,
                        "Size": entry.size,
                        "Type": entry.entry_type,
                    })).collect::<Vec<_>>()
                }]
            });
            let encoded = serde_json::to_vec(&canonical).expect("canonical ls JSON");
            let reparsed = ipfs::parse_kubo_ls_response(&encoded).expect("canonical ls response");
            assert_ls_entries(&expected, &reparsed);
        }
        (Err(_), None) => {}
        (Ok(_), None) => panic!("production rejected a structurally valid IPFS ls response"),
        (Err(error), Some(_)) => {
            panic!("production accepted an oracle-invalid IPFS ls response: {error}")
        }
    }
}

pub fn check_mfs_ls_response(data: &[u8], parsed: Option<&[ipfs::MfsEntry]>) {
    let oracle = serde_json::from_slice::<MfsEnvelope>(data).map(|value| value.entries);
    match (oracle, parsed) {
        (Ok(expected), Some(actual)) => {
            assert_mfs_entries(&expected, actual);
            let canonical = serde_json::json!({
                "Entries": actual.iter().map(|entry| serde_json::json!({
                    "Name": &entry.name,
                    "Hash": &entry.hash,
                    "Size": entry.size,
                    "Type": entry.entry_type,
                })).collect::<Vec<_>>()
            });
            let encoded = serde_json::to_vec(&canonical).expect("canonical MFS ls JSON");
            let reparsed =
                ipfs::parse_kubo_mfs_ls_response(&encoded).expect("canonical MFS ls response");
            assert_mfs_entries(&expected, &reparsed);
        }
        (Err(_), None) => {}
        (Ok(_), None) => panic!("production rejected a structurally valid MFS ls response"),
        (Err(error), Some(_)) => {
            panic!("production accepted an oracle-invalid MFS ls response: {error}")
        }
    }
}

fn assert_ls_entries(expected: &[EntrySnapshot], actual: &[ipfs::LsEntry]) {
    assert_eq!(expected.len(), actual.len(), "IPFS ls entry count changed");
    for (expected, actual) in expected.iter().zip(actual) {
        assert_eq!(expected.name, actual.name);
        assert_eq!(expected.hash, actual.hash);
        assert_eq!(expected.size, actual.size);
        assert_eq!(expected.entry_type, actual.entry_type);
    }
}

fn assert_mfs_entries(expected: &[EntrySnapshot], actual: &[ipfs::MfsEntry]) {
    assert_eq!(expected.len(), actual.len(), "MFS ls entry count changed");
    for (expected, actual) in expected.iter().zip(actual) {
        assert_eq!(expected.name, actual.name);
        assert_eq!(expected.hash, actual.hash);
        assert_eq!(expected.size, actual.size);
        assert_eq!(expected.entry_type, actual.entry_type);
    }
}
