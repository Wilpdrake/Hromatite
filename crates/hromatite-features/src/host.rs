//! Owned copy of the resident service table. No host Rust value is retained.

use hromatite_abi::{HostApiV1, MAX_CALL_ARGS};

#[derive(Clone, Copy)]
pub struct Host {
    api: HostApiV1,
}

impl Host {
    pub fn new(api: HostApiV1) -> Self {
        Self { api }
    }

    pub fn log(&self, message: &str) {
        (self.api.log)(
            b"con\0".as_ptr().cast(),
            3,
            message.as_ptr().cast(),
            message.len(),
        );
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
        Some(usize::try_from(self.read_u64(address)?).ok()?)
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
