//! Owned copy of the resident service table. No host Rust value is retained.
//!
//! The memory diagnostics below (`parse_addr`, `qwords`, `bytes`, `decode_hex`)
//! live here because every one of them either parses a console argument or
//! formats bytes obtained through [`Host`]; none of them touches game memory
//! directly.

use hromatite_abi::{HostApiV1, MAX_CALL_ARGS};

/// Parse a console address: `0x`/`0X` prefix optional, hexadecimal only.
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

/// `q <addr> [count]`: qwords with their ASCII hint and module tag.
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

/// `b <addr> [count]`: one hex line with the address and byte count.
pub fn bytes(host: &Host, address: usize, count: usize) -> String {
    let bytes = host.read_upto(address, count.min(512));
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{address:#x}: {} ({} bytes)", hex.join(" "), bytes.len())
}

/// Decode a hex byte string, ignoring whitespace and `_` group separators.
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

#[derive(Clone, Copy)]
pub struct Host {
    api: HostApiV1,
}

impl Host {
    pub fn new(api: HostApiV1) -> Self {
        Self { api }
    }

    /// SHA-256 of the attached `GameAssembly.dll`, lowercase hex.
    pub fn game_sha_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.api.game_assembly_sha256 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    pub fn game_assembly_range(&self) -> (usize, usize) {
        (
            self.api.game_assembly_base as usize,
            self.api.game_assembly_size as usize,
        )
    }

    pub fn log(&self, message: &str) {
        (self.api.log)(c"con".as_ptr(), 3, message.as_ptr().cast(), message.len());
    }

    pub fn read_upto(&self, address: usize, len: usize) -> Vec<u8> {
        if len == 0 {
            return Vec::new();
        }
        let mut out = vec![0u8; len.min(64 * 1024)];
        let read = (self.api.read)(address as u64, out.as_mut_ptr(), out.len());
        out.truncate(read.min(out.len()));
        out
    }

    pub fn read_bytes(&self, address: usize, len: usize) -> Option<Vec<u8>> {
        let out = self.read_upto(address, len);
        (out.len() == len).then_some(out)
    }

    pub fn read_u8(&self, address: usize) -> Option<u8> {
        Some(*self.read_bytes(address, 1)?.first()?)
    }

    pub fn read_u16(&self, address: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            self.read_bytes(address, 2)?.try_into().ok()?,
        ))
    }

    pub fn read_u32(&self, address: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            self.read_bytes(address, 4)?.try_into().ok()?,
        ))
    }

    pub fn read_u64(&self, address: usize) -> Option<u64> {
        Some(u64::from_le_bytes(
            self.read_bytes(address, 8)?.try_into().ok()?,
        ))
    }

    pub fn read_usize(&self, address: usize) -> Option<usize> {
        usize::try_from(self.read_u64(address)?).ok()
    }

    pub fn write_bytes(&self, address: usize, data: &[u8]) -> bool {
        !data.is_empty()
            && (self.api.write)(address as u64, data.as_ptr(), data.len()) == data.len()
    }

    pub fn write_usize(&self, address: usize, value: usize) -> bool {
        self.write_bytes(address, &value.to_le_bytes())
    }

    /// Invoke native code through the resident guard; this module never installs VEH.
    pub fn call(&self, address: usize, args: &[usize]) -> Result<usize, i32> {
        if args.len() > MAX_CALL_ARGS {
            return Err(hromatite_abi::STATUS_BAD_ARGUMENT);
        }
        let mut wide = [0u64; MAX_CALL_ARGS];
        for (out, value) in wide.iter_mut().zip(args) {
            *out = *value as u64;
        }
        let mut result = 0u64;
        let status =
            (self.api.guarded_call)(address as u64, wide.as_ptr(), args.len(), &mut result);
        if status == 0 {
            Ok(result as usize)
        } else {
            Err(status)
        }
    }

    pub fn ascii_at(&self, address: usize, max: usize) -> Option<String> {
        if address < 0x10000 {
            return None;
        }
        let mut out = String::with_capacity(max.min(64));
        for index in 0..max {
            let byte = self.read_u8(address.checked_add(index)?)?;
            if byte == 0 {
                return Some(out);
            }
            if !(32..127).contains(&byte) {
                return None;
            }
            out.push(byte as char);
        }
        Some(out)
    }

    pub fn ascii_lossy(&self, address: usize, max: usize) -> String {
        if address < 0x10000 {
            return String::new();
        }
        let mut out = String::with_capacity(max.min(64));
        for index in 0..max {
            let Some(address) = address.checked_add(index) else {
                break;
            };
            let Some(byte) = self.read_u8(address) else {
                break;
            };
            if byte == 0 {
                break;
            }
            out.push(if (32..127).contains(&byte) {
                byte as char
            } else {
                '.'
            });
        }
        out
    }

    pub fn utf16_lossy(&self, address: usize, max: usize) -> String {
        let mut units = Vec::with_capacity(max.min(128));
        for index in 0..max {
            let Some(address) = index
                .checked_mul(2)
                .and_then(|off| address.checked_add(off))
            else {
                break;
            };
            let Some(unit) = self.read_u16(address) else {
                break;
            };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16_lossy(&units)
    }

    pub fn module_tag(&self, value: usize) -> String {
        for (base, size, short, name) in [
            (
                self.api.game_assembly_base,
                self.api.game_assembly_size,
                "GA",
                "GameAssembly.dll",
            ),
            (
                self.api.unity_player_base,
                self.api.unity_player_size,
                "UP",
                "UnityPlayer.dll",
            ),
        ] {
            let value = value as u64;
            if value >= base && value < base.saturating_add(size) {
                return format!("{short}({name}+{:#x})", value - base);
            }
        }
        String::new()
    }
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
