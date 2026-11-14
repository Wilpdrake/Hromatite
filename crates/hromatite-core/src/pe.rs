//! Just enough PE parsing to map an image by hand: headers, sections,
//! relocations, import descriptors. No sec-adjacent cleverness.

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub va: u32,
    pub vsize: u32,
    pub raw: u32,
    pub rsize: u32,
}

pub struct PeImage {
    pub data: Vec<u8>,
    pub image_base: u64,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub entry: u32,
    pub sections: Vec<Section>,
    pub reloc: (u32, u32),
    pub imports: (u32, u32),
    pub tls: (u32, u32),
    pub is_64: bool,
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}

impl PeImage {
    pub fn from_file(path: &str) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
        if data.len() < 0x200 || &data[0..2] != b"MZ" {
            return Err("not a PE (no MZ)".into());
        }
        let e_lfanew = u32_at(&data, 0x3C) as usize;
        if &data[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
            return Err("not a PE (no PE sig)".into());
        }
        let nsec = u16_at(&data, e_lfanew + 6) as usize;
        let opt_size = u16_at(&data, e_lfanew + 20) as usize;
        let opt = e_lfanew + 24;
        let magic = u16_at(&data, opt);
        let is_64 = magic == 0x20B;
        let image_base = if is_64 { u64_at(&data, opt + 24) } else { u32_at(&data, opt + 28) as u64 };
        let entry = u32_at(&data, opt + 16);
        let size_of_image = u32_at(&data, opt + 56);
        let size_of_headers = u32_at(&data, opt + 60);
        let dd = if is_64 { opt + 112 } else { opt + 96 };
        let reloc = (u32_at(&data, dd + 5 * 8), u32_at(&data, dd + 5 * 8 + 4));
        let imports = (u32_at(&data, dd + 8), u32_at(&data, dd + 12));
        let tls = (u32_at(&data, dd + 9 * 8), u32_at(&data, dd + 9 * 8 + 4));
        let mut sections = Vec::new();
        let st = opt + opt_size;
        for i in 0..nsec {
            let b = st + i * 40;
            sections.push(Section {
                vsize: u32_at(&data, b + 8),
                va: u32_at(&data, b + 12),
                rsize: u32_at(&data, b + 16),
                raw: u32_at(&data, b + 20),
            });
        }
        Ok(Self {
            data,
            image_base,
            size_of_image,
            size_of_headers,
            entry,
            sections,
            reloc,
            imports,
            tls,
            is_64,
        })
    }
}
