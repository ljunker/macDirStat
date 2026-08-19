use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[derive(Serialize, Deserialize)]
struct EncodedPath {
    display: String,
    bytes_base64: String,
}

pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    encoded(path).serialize(serializer)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = EncodedPath::deserialize(deserializer)?;
    decode(&encoded).map_err(D::Error::custom)
}

pub mod vec {
    use super::*;

    pub fn serialize<S>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        paths
            .iter()
            .map(PathBuf::as_path)
            .map(encoded)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<EncodedPath>::deserialize(deserializer)?
            .iter()
            .map(|path| decode(path).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(unix)]
fn encoded(path: &Path) -> EncodedPath {
    EncodedPath {
        display: path.to_string_lossy().into_owned(),
        bytes_base64: STANDARD.encode(path_bytes(path)),
    }
}

#[cfg(not(unix))]
fn encoded(path: &Path) -> EncodedPath {
    EncodedPath {
        display: path.to_string_lossy().into_owned(),
        bytes_base64: STANDARD.encode(path.to_string_lossy().as_bytes()),
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

#[cfg(unix)]
fn decode(encoded: &EncodedPath) -> Result<PathBuf, String> {
    let bytes = STANDARD
        .decode(&encoded.bytes_base64)
        .map_err(|error| format!("invalid path bytes: {error}"))?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn decode(encoded: &EncodedPath) -> Result<PathBuf, String> {
    let bytes = STANDARD
        .decode(&encoded.bytes_base64)
        .map_err(|error| format!("invalid path bytes: {error}"))?;
    let value = String::from_utf8(bytes).map_err(|error| format!("invalid path text: {error}"))?;
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn json_roundtrip_preserves_non_utf8_path() {
        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            #[serde(with = "crate::portable_path")]
            path: PathBuf,
        }
        let json = serde_json::to_string(&Wrapper { path: path.clone() }).unwrap();
        assert!(json.contains("bytes_base64"));
        let decoded: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.path, path);
    }
}
