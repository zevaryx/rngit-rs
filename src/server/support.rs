//! What the parts of the node that answer management requests share.

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use rmpv::Value;

use super::Refusal;
use crate::error::Status;
use crate::transport::Response;

pub(super) fn refuse(status: Status, text: &str) -> Refusal {
    Refusal::new(status, text)
}

/// Seconds since 1970.
pub(super) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Seconds since 1970, with the fraction, as stock rngit writes times.
pub(super) fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Write a file by way of a temporary one, so a failure leaves nothing half written.
pub(super) fn write_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(".{}.part", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// An OK reply with a MessagePack value after the status byte.
pub(super) fn packed(value: &Value) -> Result<Response, Refusal> {
    let mut out = vec![Status::Ok.to_byte()];
    rmpv::encode::write_value(&mut out, value).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
    Ok(Response::Packet(out))
}

pub(super) fn text(value: &str) -> Value {
    Value::from(value)
}

/// Check the lines of a permissions file a client sends, as stock rngit
/// does: each is `perm:who`, blank, or a `#` comment. The first that isn't
/// is named.
pub(super) fn check_rules(content: &str) -> Result<(), Refusal> {
    for (number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let one_word = line.split_once(':').is_some_and(|(perm, who)| !perm.trim().contains(char::is_whitespace) && !who.contains(':'));
        if !one_word || line.contains(',') || super::Rule::parse(line).is_empty() {
            return Err(refuse(Status::BadRequest, &format!("Invalid permission \"{line}\" on line {}", number + 1)));
        }
    }
    Ok(())
}
