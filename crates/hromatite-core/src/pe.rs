//! Checked PE input parsing. Every offset originates in the input file, so all
//! reads are bounds-checked and malformed data returns an error, never a panic.

#![forbid(unsafe_code)]

const MAX_SECTIONS: usize = 96;
const MAX_IMAGE_SIZE: usize = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub va: u32,
    pub vsize: u32,
    pub raw: u32,
    pub rsize: u32,
    pub characteristics: u32,
}

impl Section {
    pub fn is_executable(&self) -> bool {
        self.characteristics & 0x2000_0000 != 0
    }
}

pub struct PeImage {
    pub data: Vec<u8>,
    pub machine: u16,
    pub is_dll: bool,
    pub image_base: u64,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub entry: u32,
    pub sections: Vec<Section>,
    pub exports: (u32, u32),
    pub reloc: (u32, u32),
    pub imports: (u32, u32),
    pub tls: (u32, u32),
    pub is_64: bool,
}

fn take(data: &[u8], offset: usize, len: usize) -> Result<&[u8], String> {
    let end = offset.checked_add(len).ok_or("PE offset overflow")?;
    data.get(offset..end).ok_or_else(|| {
        format!(
            "PE range {offset:#x}..{end:#x} is truncated (file is {:#x} bytes)",
            data.len()
        )
    })
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        take(data, offset, 2)?
            .try_into()
            .map_err(|_| "PE u16 conversion")?,
    ))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        take(data, offset, 4)?
            .try_into()
            .map_err(|_| "PE u32 conversion")?,
    ))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        take(data, offset, 8)?
            .try_into()
            .map_err(|_| "PE u64 conversion")?,
    ))
}

impl PeImage {
    pub fn from_file(path: &str) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
        Self::from_bytes(data)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self, String> {
        if data.len() < 0x200 || take(&data, 0, 2)? != b"MZ" {
            return Err("not a PE (no MZ)".into());
        }
        let pe_offset = u32_at(&data, 0x3C)? as usize;
        if take(&data, pe_offset, 4)? != b"PE\0\0" {
            return Err("not a PE (no PE sig)".into());
        }
        let machine = u16_at(&data, pe_offset.checked_add(4).ok_or("PE header overflow")?)?;
        let nsec = u16_at(&data, pe_offset.checked_add(6).ok_or("PE header overflow")?)? as usize;
        let characteristics = u16_at(
            &data,
            pe_offset.checked_add(22).ok_or("PE header overflow")?,
        )?;
        let is_dll = characteristics & 0x2000 != 0;
        if nsec > MAX_SECTIONS {
            return Err(format!("too many PE sections: {nsec}"));
        }
        let opt_size = u16_at(
            &data,
            pe_offset.checked_add(20).ok_or("PE header overflow")?,
        )? as usize;
        let opt = pe_offset.checked_add(24).ok_or("PE header overflow")?;
        let magic = u16_at(&data, opt)?;
        let is_64 = match magic {
            0x20B => true,
            0x10B => false,
            other => return Err(format!("unsupported PE optional-header magic {other:#x}")),
        };
        let dir = opt
            .checked_add(if is_64 { 112 } else { 96 })
            .ok_or("PE directory overflow")?;
        let dir_end = dir.checked_add(10 * 8).ok_or("PE directory overflow")?;
        let opt_end = opt
            .checked_add(opt_size)
            .ok_or("PE optional header overflow")?;
        if opt_end < dir_end {
            return Err("PE optional header is shorter than its data directories".into());
        }
        take(&data, opt, opt_size)?;

        let image_base = if is_64 {
            u64_at(&data, opt + 24)?
        } else {
            u32_at(&data, opt + 28)? as u64
        };
        let entry = u32_at(&data, opt + 16)?;
        let size_of_image = u32_at(&data, opt + 56)?;
        let size_of_headers = u32_at(&data, opt + 60)?;
        if size_of_image == 0 || size_of_image as usize > MAX_IMAGE_SIZE {
            return Err(format!("invalid SizeOfImage {size_of_image:#x}"));
        }
        if size_of_headers > size_of_image {
            return Err("SizeOfHeaders exceeds SizeOfImage".into());
        }
        let exports = (u32_at(&data, dir)?, u32_at(&data, dir + 4)?);
        let reloc = (u32_at(&data, dir + 5 * 8)?, u32_at(&data, dir + 5 * 8 + 4)?);
        let imports = (u32_at(&data, dir + 8)?, u32_at(&data, dir + 12)?);
        let tls = (u32_at(&data, dir + 9 * 8)?, u32_at(&data, dir + 9 * 8 + 4)?);

        let section_table = opt_end;
        take(
            &data,
            section_table,
            nsec.checked_mul(40).ok_or("PE section-table overflow")?,
        )?;
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let at = section_table + i * 40;
            let vsize = u32_at(&data, at + 8)?;
            let va = u32_at(&data, at + 12)?;
            let rsize = u32_at(&data, at + 16)?;
            let raw = u32_at(&data, at + 20)?;
            let characteristics = u32_at(&data, at + 36)?;
            let raw_end = (raw as usize)
                .checked_add(rsize as usize)
                .ok_or("section raw range overflow")?;
            let va_end = (va as usize)
                .checked_add(vsize as usize)
                .ok_or("section VA range overflow")?;
            if raw_end > data.len() || va_end > size_of_image as usize {
                return Err(format!("section {i} lies outside its file/image bounds"));
            }
            sections.push(Section {
                va,
                vsize,
                raw,
                rsize,
                characteristics,
            });
        }

        Ok(Self {
            data,
            machine,
            is_dll,
            image_base,
            size_of_image,
            size_of_headers,
            entry,
            sections,
            exports,
            reloc,
            imports,
            tls,
            is_64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut data = vec![0u8; 0x400];
        data[..2].copy_from_slice(b"MZ");
        data[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        data[0x80..0x84].copy_from_slice(b"PE\0\0");
        data[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        data[0x86..0x88].copy_from_slice(&0u16.to_le_bytes());
        data[0x96..0x98].copy_from_slice(&0x2022u16.to_le_bytes());
        data[0x94..0x96].copy_from_slice(&0xF0u16.to_le_bytes());
        data[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        data[0xB0..0xB8].copy_from_slice(&0x400000u64.to_le_bytes());
        data[0xA8..0xAC].copy_from_slice(&0x1000u32.to_le_bytes());
        data[0xD0..0xD4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[0xD4..0xD8].copy_from_slice(&0x200u32.to_le_bytes());
        data
    }

    #[test]
    fn parses_a_well_formed_header() {
        let image = PeImage::from_bytes(fixture()).unwrap();
        assert!(image.is_64);
        assert_eq!(image.machine, 0x8664);
        assert!(image.is_dll);
        assert_eq!(image.image_base, 0x400000);
        assert_eq!(image.entry, 0x1000);
        assert_eq!(image.exports, (0, 0));
        assert_eq!(image.sections.len(), 0);
    }

    #[test]
    fn records_the_export_directory_for_pre_erasure_lookup() {
        let mut data = fixture();
        // PE32+ optional header starts at 0x98; data directories at +112.
        data[0x108..0x10c].copy_from_slice(&0x2345u32.to_le_bytes());
        data[0x10c..0x110].copy_from_slice(&0x40u32.to_le_bytes());
        let image = PeImage::from_bytes(data).unwrap();
        assert_eq!(image.exports, (0x2345, 0x40));
    }

    #[test]
    fn every_truncation_is_an_error_not_a_panic() {
        let data = fixture();
        for len in 0..0x200 {
            let result = PeImage::from_bytes(data[..len].to_vec());
            assert!(result.is_err(), "prefix {len:#x} unexpectedly parsed");
        }
        assert!(PeImage::from_bytes(data[..0x200].to_vec()).is_ok());
        assert!(PeImage::from_bytes(data).is_ok());
    }

    #[test]
    fn rejects_bad_optional_magic_and_section_counts() {
        let mut data = fixture();
        data[0x98..0x9A].copy_from_slice(&0x999u16.to_le_bytes());
        assert!(PeImage::from_bytes(data).is_err());
        let mut data = fixture();
        data[0x86..0x88].copy_from_slice(&97u16.to_le_bytes());
        assert!(PeImage::from_bytes(data).is_err());
    }
}
