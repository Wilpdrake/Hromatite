//! Basic memory diagnostics and deliberate native operations.

use crate::host::Host;

pub fn parse_addr(text: &str) -> Option<usize> {
    let text = text
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if text.is_empty() {
        return None;
    }
    usize::from_str_radix(text, 16).ok()
}

pub fn qwords(host: &Host, address: usize, count: usize) -> Vec<String> {
    let mut out = Vec::new();
    for index in 0..count.min(64) {
        let Some(current) = index
            .checked_mul(8)
            .and_then(|offset| address.checked_add(offset))
        else {
            break;
        };
        match host.read_u64(current) {
            Some(value) => {
                let pointer = value as usize;
                let ascii = host.ascii_lossy(pointer, 24);
                let ascii = if ascii.is_empty() {
                    String::new()
                } else {
                    format!(" '{ascii}'")
                };
                let tag = host.module_tag(pointer);
                out.push(format!(
                    "  {current:#x}: {value:#016x}{ascii}{}{tag}",
                    if tag.is_empty() { "" } else { " " }
                ));
            }
            None => {
                out.push(format!("  {current:#x}: (unmapped)"));
                break;
            }
        }
    }
    out
}

pub fn bytes(host: &Host, address: usize, count: usize) -> String {
    let bytes = host.read_upto(address, count.min(512));
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{address:#x}: {} ({} bytes)", hex.join(" "), bytes.len())
}

pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '_')
        .collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
        return None;
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&cleaned[index..index + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{decode_hex, parse_addr};

    #[test]
    fn addresses_and_hex_writes_validate_their_format() {
        assert_eq!(parse_addr("0x10"), Some(16));
        assert_eq!(parse_addr(""), None);
        assert_eq!(decode_hex("01_00 ff"), Some(vec![1, 0, 255]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("gg"), None);
    }
}
