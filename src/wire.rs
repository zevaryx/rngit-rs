//! MessagePack request bodies and reply parsing.

use rmpv::Value;

use crate::error::{Error, Result, Status};

/// A request body: a map with integer keys for the repository (0) and group
/// (2), and string keys for everything else.
#[derive(Debug, Default)]
pub struct Request(Vec<(Value, Value)>);

impl Request {
    pub fn repo(path: &str) -> Self {
        Request(vec![(Value::from(0), Value::from(path))])
    }

    pub fn group(name: &str) -> Self {
        Request(vec![(Value::from(2), Value::from(name))])
    }

    pub fn put(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.push((Value::from(key), value.into()));
        self
    }

    pub fn bin(self, key: &str, bytes: Vec<u8>) -> Self {
        self.put(key, Value::Binary(bytes))
    }

    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::new();
        // Writing a Value to a Vec can't fail.
        let _ = rmpv::encode::write_value(&mut out, &Value::Map(self.0));
        out
    }
}

/// Split a reply into its status byte and payload. A non-OK status becomes
/// an error carrying the node's text.
pub fn split_reply(reply: &[u8]) -> Result<&[u8]> {
    let (&byte, payload) = reply.split_first().ok_or_else(|| Error::Protocol("empty reply".into()))?;
    match Status::from_byte(byte) {
        Status::Ok => Ok(payload),
        status => Err(Error::Remote { status, message: String::from_utf8_lossy(&payload[..payload.len().min(512)]).into_owned() }),
    }
}

/// Decode one MessagePack value and reject trailing bytes.
pub fn decode(payload: &[u8]) -> Result<Value> {
    let mut cursor = payload;
    let value = rmpv::decode::read_value(&mut cursor).map_err(|e| Error::Protocol(format!("bad MessagePack: {e}")))?;
    if !cursor.is_empty() {
        return Err(Error::Protocol("trailing bytes after MessagePack value".into()));
    }
    Ok(value)
}

/// Read access to a decoded map that never panics on missing or wrong-typed
/// fields. Everything a node sends is untrusted.
#[derive(Debug, Clone, Copy)]
pub struct MapRef<'a>(&'a [(Value, Value)]);

impl<'a> MapRef<'a> {
    pub fn new(value: &'a Value) -> Result<Self> {
        match value {
            Value::Map(entries) => Ok(MapRef(entries)),
            _ => Err(Error::Protocol("expected a map".into())),
        }
    }

    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v).filter(|v| !v.is_nil())
    }

    pub fn str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Value::as_str).map(str::to_string)
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(Value::as_u64)
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Value::as_f64)
    }

    pub fn bytes(&self, key: &str) -> Option<&'a [u8]> {
        match self.get(key)? {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }

    pub fn array(&self, key: &str) -> &'a [Value] {
        match self.get(key) {
            Some(Value::Array(items)) => items,
            _ => &[],
        }
    }

    pub fn map(&self, key: &str) -> Option<MapRef<'a>> {
        MapRef::new(self.get(key)?).ok()
    }
}

/// Hex of a byte string for display, empty when absent.
pub fn hex_or_empty(bytes: Option<&[u8]>) -> String {
    bytes.map(hex::encode).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_integer_key_for_repo() {
        let body = Request::repo("public/demo.git").put("for_push", false).encode();
        let value = decode(&body).unwrap();
        let Value::Map(entries) = value else { panic!() };
        assert_eq!(entries[0].0.as_u64(), Some(0));
        assert_eq!(entries[0].1.as_str(), Some("public/demo.git"));
        assert_eq!(entries[1].0.as_str(), Some("for_push"));
    }

    #[test]
    fn split_reply_maps_statuses() {
        assert_eq!(split_reply(b"\x00hi").unwrap(), b"hi");
        for (byte, status) in [(1u8, Status::NotAllowed), (2, Status::BadRequest), (3, Status::NotFound), (0xff, Status::ServerError)] {
            let mut reply = vec![byte];
            reply.extend_from_slice(b"why");
            match split_reply(&reply) {
                Err(Error::Remote { status: s, message }) => {
                    assert_eq!(s, status);
                    assert_eq!(message, "why");
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(split_reply(b"").is_err());
    }

    #[test]
    fn decode_rejects_trailing_and_truncated() {
        assert!(decode(b"\x01\x02").is_err());
        assert!(decode(b"\x82\xa1a").is_err());
    }

    #[test]
    fn map_ref_tolerates_wrong_types() {
        let value = Value::Map(vec![(Value::from("n"), Value::from("text")), (Value::from("nil"), Value::Nil)]);
        let map = MapRef::new(&value).unwrap();
        assert_eq!(map.u64("n"), None);
        assert_eq!(map.str("n").as_deref(), Some("text"));
        assert!(map.get("nil").is_none());
        assert!(map.array("missing").is_empty());
        assert!(MapRef::new(&Value::Nil).is_err());
    }
}
