//! The ELF64 image parser.
//!
//! Pure: it reads through [`ReadAt`], which is a byte slice in the host tests
//! and one snapshot of our own (or the game's) memory in the locator. Nothing
//! here dereferences a pointer or asks the OS anything, so a hostile or
//! corrupt image can only produce an [`ElfError`] or a short [`Summary`].
//!
//! Every table read is bounded twice: by the caller's snapshot budget and by
//! the caps below, so a claimed `e_shnum` of 65535 cannot make the probe
//! allocate.

use std::fmt;

/// ELF magic, the only signature the locator looks for.
pub const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// ELFCLASS64.
pub const ELFCLASS64: u8 = 2;
/// ELFDATA2LSB.
pub const ELFDATA2LSB: u8 = 1;
/// `ET_DYN`, the type a position-independent client module has.
pub const ET_DYN: u16 = 3;

/// `PT_LOAD`.
pub const PT_LOAD: u32 = 1;
/// `SHT_DYNSYM`.
pub const SHT_DYNSYM: u32 = 11;
/// `SHN_UNDEF`: the symbol is imported, not defined here.
pub const SHN_UNDEF: u16 = 0;

/// Section-table cap: a hostile or corrupt `e_shnum` must not allocate.
pub const MAX_SECTIONS: usize = 4_096;
/// Program-header cap.
pub const MAX_PROGRAMS: usize = 512;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// Random-access reader over an image. Returning `None` means "not readable /
/// outside the budget"; no implementation may panic on a bad offset.
pub trait ReadAt {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>>;
}

/// [`ReadAt`] over a byte slice (host tests and offline files).
pub struct SliceRead<'a> {
    data: &'a [u8],
}

impl<'a> SliceRead<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl ReadAt for SliceRead<'_> {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        self.data.get(start..end).map(<[u8]>::to_vec)
    }
}

/// Why an image could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// Fewer than 64 bytes: not even an ELF64 header.
    TooShort,
    /// No `\x7fELF`.
    BadMagic,
    /// `EI_CLASS` is not `ELFCLASS64`.
    NotElf64,
    /// `EI_DATA` is not little-endian.
    NotLittleEndian,
    /// `e_phentsize`/`e_phnum` are unusable.
    BadProgramHeaders,
    /// `e_shentsize`/`e_shnum` are unusable.
    BadSections,
    /// A table the parser needed is outside the snapshot budget.
    Truncated(&'static str),
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("shorter than an ELF64 header"),
            Self::BadMagic => f.write_str("no \\x7fELF magic"),
            Self::NotElf64 => f.write_str("not ELFCLASS64"),
            Self::NotLittleEndian => f.write_str("not little-endian"),
            Self::BadProgramHeaders => f.write_str("unusable program-header table"),
            Self::BadSections => f.write_str("unusable section-header table"),
            Self::Truncated(what) => write!(f, "{what} outside the snapshot budget"),
        }
    }
}

/// The 64-byte ELF64 header, little-endian fields only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElfHeader {
    /// `EI_CLASS` (always [`ELFCLASS64`] after a successful parse).
    pub class: u8,
    /// `EI_DATA` (always [`ELFDATA2LSB`]).
    pub data: u8,
    /// `EI_OSABI`; 3 = GNU/Linux, 9 = FreeBSD, 0 = SysV.
    pub osabi: u8,
    /// `e_type` ([`ET_DYN`] for a shared object / PIE).
    pub kind: u16,
    /// `e_machine` (62 = `EM_X86_64`).
    pub machine: u16,
    /// `e_entry`.
    pub entry: u64,
    /// `e_phoff`.
    pub phoff: u64,
    /// `e_shoff`.
    pub shoff: u64,
    /// `e_flags`.
    pub flags: u32,
    /// `e_phentsize`.
    pub phentsize: u16,
    /// `e_phnum`.
    pub phnum: u16,
    /// `e_shentsize`.
    pub shentsize: u16,
    /// `e_shnum`.
    pub shnum: u16,
    /// `e_shstrndx`.
    pub shstrndx: u16,
}

/// One `Elf64_Phdr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramHeader {
    /// `p_type`.
    pub kind: u32,
    /// `p_flags` (`PF_X` 1, `PF_W` 2, `PF_R` 4).
    pub flags: u32,
    /// `p_offset`.
    pub offset: u64,
    /// `p_vaddr`.
    pub vaddr: u64,
    /// `p_filesz`.
    pub filesz: u64,
    /// `p_memsz`.
    pub memsz: u64,
    /// `p_align`.
    pub align: u64,
}

impl ProgramHeader {
    /// `PF_R`/`PF_W`/`PF_X` as `r-x`, `rw-`, …
    pub fn permissions(&self) -> String {
        let mut out = String::with_capacity(3);
        out.push(if self.flags & 4 != 0 { 'r' } else { '-' });
        out.push(if self.flags & 2 != 0 { 'w' } else { '-' });
        out.push(if self.flags & 1 != 0 { 'x' } else { '-' });
        out
    }

    /// One log line.
    pub fn line(&self, index: usize) -> String {
        let kind = match self.kind {
            PT_LOAD => "LOAD".to_string(),
            2 => "DYNAMIC".to_string(),
            3 => "INTERP".to_string(),
            4 => "NOTE".to_string(),
            6 => "PHDR".to_string(),
            7 => "TLS".to_string(),
            0x6474_e551 => "GNU_STACK".to_string(),
            0x6474_e552 => "GNU_RELRO".to_string(),
            other => format!("0x{other:x}"),
        };
        format!(
            "ph[{index}] {kind} {} off=0x{:x} va=0x{:x} filesz=0x{:x} memsz=0x{:x} align=0x{:x}",
            self.permissions(),
            self.offset,
            self.vaddr,
            self.filesz,
            self.memsz,
            self.align
        )
    }
}

/// One `Elf64_Shdr` with its resolved name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// `.dynsym`, `.text`, … (empty when the name index is out of range).
    pub name: String,
    /// `sh_type`.
    pub kind: u32,
    /// `sh_flags`.
    pub flags: u64,
    /// `sh_addr`.
    pub addr: u64,
    /// `sh_offset`.
    pub offset: u64,
    /// `sh_size`.
    pub size: u64,
    /// `sh_link` (the string table for `.dynsym`).
    pub link: u32,
    /// `sh_entsize`.
    pub entsize: u64,
}

/// Everything the locator reports about one candidate image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Parsed ELF64 header.
    pub header: ElfHeader,
    /// Program headers, in table order.
    pub programs: Vec<ProgramHeader>,
    /// Section headers, in table order (empty when the image has none inside
    /// the snapshot budget).
    pub sections: Vec<Section>,
    /// Undefined (imported) symbol names, bounded by [`Summary::imports_shown`].
    pub imports: Vec<String>,
    /// Total number of undefined symbols seen.
    pub imports_total: usize,
    /// True when the symbol count was cut by the caller's limit.
    pub imports_truncated: bool,
    /// Lines describing what could not be parsed, if anything.
    pub notes: Vec<String>,
}

impl Summary {
    /// Number of import names kept in [`Self::imports`].
    pub fn imports_shown(&self) -> usize {
        self.imports.len()
    }

    /// Human-readable lines, one per fact.
    pub fn lines(&self) -> Vec<String> {
        let header = &self.header;
        let mut lines = vec![
            format!(
                "elf: class=ELF64 data=LE osabi={} type={} ({}) machine={} ({}) entry=0x{:x}",
                header.osabi,
                header.kind,
                if header.kind == ET_DYN {
                    "ET_DYN"
                } else {
                    "other"
                },
                header.machine,
                if header.machine == 62 {
                    "x86-64"
                } else {
                    "other"
                },
                header.entry
            ),
            format!(
                "elf: phoff=0x{:x} phnum={} phentsize={} shoff=0x{:x} shnum={} shentsize={} shstrndx={}",
                header.phoff,
                header.phnum,
                header.phentsize,
                header.shoff,
                header.shnum,
                header.shentsize,
                header.shstrndx
            ),
        ];
        for (index, program) in self.programs.iter().enumerate() {
            lines.push(program.line(index));
        }
        let loads = self
            .programs
            .iter()
            .filter(|program| program.kind == PT_LOAD)
            .count();
        lines.push(format!(
            "elf: {} program header(s), {loads} LOAD, {} section(s)",
            self.programs.len(),
            self.sections.len()
        ));
        if self.imports_total == 0 {
            lines.push("elf: dynsym: no undefined symbol (static or already resolved)".to_string());
        } else {
            lines.push(format!(
                "elf: dynsym: {} import(s){}",
                self.imports_total,
                if self.imports_truncated {
                    format!(" (first {} shown)", self.imports.len())
                } else {
                    String::new()
                }
            ));
        }
        if !self.imports.is_empty() {
            lines.push(format!("elf: imports: {}", self.imports.join(", ")));
        }
        let dynsym = self
            .sections
            .iter()
            .find(|section| section.kind == SHT_DYNSYM);
        if let Some(section) = dynsym {
            lines.push(format!(
                "elf: {} off=0x{:x} size={} entsize={} link={}",
                if section.name.is_empty() {
                    "<dynsym>"
                } else {
                    section.name.as_str()
                },
                section.offset,
                section.size,
                section.entsize,
                section.link
            ));
        }
        lines.extend(self.notes.iter().cloned());
        lines
    }
}

/// Parse the 64-byte header at offset 0.
pub fn parse_header<R: ReadAt + ?Sized>(reader: &mut R) -> Result<ElfHeader, ElfError> {
    let raw = reader.read_at(0, 64).ok_or(ElfError::TooShort)?;
    if raw.len() < 64 {
        return Err(ElfError::TooShort);
    }
    if raw[0..4] != MAGIC {
        return Err(ElfError::BadMagic);
    }
    if raw[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    if raw[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }
    let field = |at: usize| -> Result<u16, ElfError> { u16_at(&raw, at).ok_or(ElfError::TooShort) };
    let wide = |at: usize| -> Result<u64, ElfError> { u64_at(&raw, at).ok_or(ElfError::TooShort) };
    let header = ElfHeader {
        class: raw[4],
        data: raw[5],
        osabi: raw[7],
        kind: field(16)?,
        machine: field(18)?,
        entry: wide(24)?,
        phoff: wide(32)?,
        shoff: wide(40)?,
        flags: u32_at(&raw, 48).ok_or(ElfError::TooShort)?,
        phentsize: field(54)?,
        phnum: field(56)?,
        shentsize: field(58)?,
        shnum: field(60)?,
        shstrndx: field(62)?,
    };
    if header.phnum != 0 && header.phentsize as usize != 56 {
        return Err(ElfError::BadProgramHeaders);
    }
    if header.phnum as usize > MAX_PROGRAMS {
        return Err(ElfError::BadProgramHeaders);
    }
    if header.shnum != 0 && header.shentsize as usize != 64 {
        return Err(ElfError::BadSections);
    }
    if header.shnum as usize > MAX_SECTIONS {
        return Err(ElfError::BadSections);
    }
    Ok(header)
}

/// Parse the program-header table.
pub fn parse_program_headers<R: ReadAt + ?Sized>(
    reader: &mut R,
    header: &ElfHeader,
) -> Result<Vec<ProgramHeader>, ElfError> {
    let count = header.phnum as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let bytes = 56usize;
    let total = count
        .checked_mul(bytes)
        .ok_or(ElfError::BadProgramHeaders)?;
    let raw = reader
        .read_at(header.phoff, total)
        .ok_or(ElfError::Truncated("program headers"))?;
    if raw.len() < total {
        return Err(ElfError::BadProgramHeaders);
    }
    let mut programs = Vec::with_capacity(count);
    for index in 0..count {
        let at = index * bytes;
        programs.push(ProgramHeader {
            kind: u32_at(&raw, at).ok_or(ElfError::BadProgramHeaders)?,
            flags: u32_at(&raw, at + 4).ok_or(ElfError::BadProgramHeaders)?,
            offset: u64_at(&raw, at + 8).ok_or(ElfError::BadProgramHeaders)?,
            vaddr: u64_at(&raw, at + 16).ok_or(ElfError::BadProgramHeaders)?,
            filesz: u64_at(&raw, at + 32).ok_or(ElfError::BadProgramHeaders)?,
            memsz: u64_at(&raw, at + 40).ok_or(ElfError::BadProgramHeaders)?,
            align: u64_at(&raw, at + 48).ok_or(ElfError::BadProgramHeaders)?,
        });
    }
    Ok(programs)
}

/// Parse the section-header table, resolving names through `e_shstrndx`.
pub fn parse_sections<R: ReadAt + ?Sized>(
    reader: &mut R,
    header: &ElfHeader,
) -> Result<Vec<Section>, ElfError> {
    let count = header.shnum as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let bytes = 64usize;
    let total = count.checked_mul(bytes).ok_or(ElfError::BadSections)?;
    let raw = reader
        .read_at(header.shoff, total)
        .ok_or(ElfError::Truncated("section headers"))?;
    if raw.len() < total {
        return Err(ElfError::BadSections);
    }
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let at = index * bytes;
        sections.push(Section {
            name: String::new(),
            kind: u32_at(&raw, at + 4).ok_or(ElfError::BadSections)?,
            flags: u64_at(&raw, at + 8).ok_or(ElfError::BadSections)?,
            addr: u64_at(&raw, at + 16).ok_or(ElfError::BadSections)?,
            offset: u64_at(&raw, at + 24).ok_or(ElfError::BadSections)?,
            size: u64_at(&raw, at + 32).ok_or(ElfError::BadSections)?,
            link: u32_at(&raw, at + 40).ok_or(ElfError::BadSections)?,
            entsize: u64_at(&raw, at + 56).ok_or(ElfError::BadSections)?,
        });
    }
    // Names live in the section named by `e_shstrndx`; a bad index just leaves
    // every name empty (the rest of the parse is still useful).
    if let Some(strings) = section_bytes(reader, &sections, header.shstrndx as usize, 1 << 20) {
        for (index, section) in sections.iter_mut().enumerate() {
            let name_at = u32_at(&raw, index * bytes).unwrap_or(0) as usize;
            section.name = cstr_at(&strings, name_at).unwrap_or_default();
        }
    }
    Ok(sections)
}

/// Read a whole section's bytes, bounded.
fn section_bytes<R: ReadAt + ?Sized>(
    reader: &mut R,
    sections: &[Section],
    index: usize,
    limit: usize,
) -> Option<Vec<u8>> {
    let section = sections.get(index)?;
    if section.kind == 8 || section.size == 0 {
        return None; // SHT_NOBITS has no file bytes
    }
    let len = usize::try_from(section.size).ok()?.min(limit);
    let bytes = reader.read_at(section.offset, len)?;
    (!bytes.is_empty()).then_some(bytes)
}

/// NUL-terminated string at `at` inside `table`.
fn cstr_at(table: &[u8], at: usize) -> Option<String> {
    let tail = table.get(at..)?;
    let end = tail.iter().position(|byte| *byte == 0)?;
    let text = &tail[..end];
    // ELF symbol names are ASCII; anything else is shown escaped rather than
    // rejected, so a corrupt table cannot hide the rest of the report.
    Some(text.iter().map(|byte| escape(*byte)).collect())
}

fn escape(byte: u8) -> char {
    if (0x20..0x7f).contains(&byte) {
        byte as char
    } else {
        '?'
    }
}

/// Parse everything the report needs, best effort: a broken section table
/// leaves `sections` empty and adds a note instead of failing the whole probe.
pub fn describe<R: ReadAt + ?Sized>(
    reader: &mut R,
    import_limit: usize,
) -> Result<Summary, ElfError> {
    let header = parse_header(reader)?;
    let programs = parse_program_headers(reader, &header)?;
    let mut notes = Vec::new();
    let sections = match parse_sections(reader, &header) {
        Ok(sections) => sections,
        Err(error) => {
            notes.push(format!("elf: section headers unreadable ({error})"));
            Vec::new()
        }
    };
    let (imports, imports_total, imports_truncated) =
        imported_symbols(reader, &sections, import_limit, &mut notes);
    Ok(Summary {
        header,
        programs,
        sections,
        imports,
        imports_total,
        imports_truncated,
        notes,
    })
}

/// Undefined (imported) symbol names from `.dynsym`.
///
/// Returns `(names, total_undefined, truncated)`.
fn imported_symbols<R: ReadAt + ?Sized>(
    reader: &mut R,
    sections: &[Section],
    limit: usize,
    notes: &mut Vec<String>,
) -> (Vec<String>, usize, bool) {
    let Some(symtab) = sections.iter().find(|section| section.kind == SHT_DYNSYM) else {
        if !sections.is_empty() {
            notes.push("elf: no .dynsym section".to_string());
        }
        return (Vec::new(), 0, false);
    };
    let Some(strings) = section_bytes(reader, sections, symtab.link as usize, 1 << 20) else {
        notes.push("elf: .dynsym string table unreadable".to_string());
        return (Vec::new(), 0, false);
    };
    let entry = if symtab.entsize >= 24 {
        24usize
    } else {
        notes.push("elf: .dynsym entsize != 24".to_string());
        return (Vec::new(), 0, false);
    };
    let count = usize::try_from(symtab.size / 24).unwrap_or(0);
    if count == 0 {
        return (Vec::new(), 0, false);
    }
    // Bounded read: never more than 8 MiB of symbol table, whatever `sh_size`
    // claims.
    let cap = (8 << 20) / 24;
    let wanted = count.min(cap);
    let Some(raw) = reader.read_at(symtab.offset, wanted.saturating_mul(entry)) else {
        notes.push("elf: .dynsym unreadable".to_string());
        return (Vec::new(), 0, false);
    };
    let available = raw.len() / entry;
    let mut names = Vec::new();
    let mut total = 0usize;
    for index in 0..available {
        let at = index * entry;
        let name_at = u32_at(&raw, at).unwrap_or(0) as usize;
        let defined = u16_at(&raw, at + 6).unwrap_or(0);
        if name_at == 0 || defined != SHN_UNDEF {
            continue;
        }
        total += 1;
        if names.len() < limit
            && let Some(name) = cstr_at(&strings, name_at)
            && !name.is_empty()
        {
            names.push(name);
        }
    }
    let truncated = count > available || total > names.len();
    (names, total, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A minimal but real ELF64 image: header, two `PT_LOAD`, a section table
    /// with `.dynstr`/`.dynsym`/`.shstrtab`, and two imported symbols.
    fn sample() -> Vec<u8> {
        const PHOFF: usize = 0x40;
        const PHNUM: usize = 2;
        const SHOFF: usize = 0x100;
        const SHNUM: usize = 4;
        const DYNSTR_OFF: usize = 0x300;
        const DYNSYM_OFF: usize = 0x380;
        const SHSTR_OFF: usize = 0x400;
        const DYNSYM_ENTRIES: usize = 4;

        let mut image = vec![0u8; 0x500];
        image[0..4].copy_from_slice(&MAGIC);
        image[4] = ELFCLASS64;
        image[5] = ELFDATA2LSB;
        image[6] = 1;
        image[7] = 3;
        image[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
        image[18..20].copy_from_slice(&62u16.to_le_bytes());
        image[24..32].copy_from_slice(&0x1_0000u64.to_le_bytes());
        image[32..40].copy_from_slice(&(PHOFF as u64).to_le_bytes());
        image[40..48].copy_from_slice(&(SHOFF as u64).to_le_bytes());
        image[52..54].copy_from_slice(&64u16.to_le_bytes());
        image[54..56].copy_from_slice(&56u16.to_le_bytes());
        image[56..58].copy_from_slice(&(PHNUM as u16).to_le_bytes());
        image[58..60].copy_from_slice(&64u16.to_le_bytes());
        image[60..62].copy_from_slice(&(SHNUM as u16).to_le_bytes());
        image[62..64].copy_from_slice(&3u16.to_le_bytes()); // .shstrtab

        // ph[0]: PT_LOAD r-x, ph[1]: PT_LOAD rw-.
        let ph = |image: &mut Vec<u8>, index: usize, kind: u32, flags: u32, offset: u64| {
            let at = PHOFF + index * 56;
            image[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
            image[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            image[at + 16..at + 24].copy_from_slice(&offset.to_le_bytes());
            image[at + 32..at + 40].copy_from_slice(&0x1000u64.to_le_bytes());
            image[at + 40..at + 48].copy_from_slice(&0x2000u64.to_le_bytes());
            image[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        };
        ph(&mut image, 0, PT_LOAD, 5, 0);
        ph(&mut image, 1, PT_LOAD, 6, 0x200);

        // Section names: "\0.dynstr\0.dynsym\0.shstrtab\0".
        let names: &[u8] = b"\0.dynstr\0.dynsym\0.shstrtab\0";
        image[SHSTR_OFF..SHSTR_OFF + names.len()].copy_from_slice(names);

        let section =
            |image: &mut Vec<u8>, index: usize, name: u32, kind: u32, offset: u64, size: u64| {
                let at = SHOFF + index * 64;
                image[at..at + 4].copy_from_slice(&name.to_le_bytes());
                image[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
                image[at + 24..at + 32].copy_from_slice(&offset.to_le_bytes());
                image[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
            };
        section(&mut image, 1, 1, 3, DYNSTR_OFF as u64, 32);
        section(&mut image, 2, 9, SHT_DYNSYM, DYNSYM_OFF as u64, 96);
        let at = SHOFF + 2 * 64;
        image[at + 40..at + 44].copy_from_slice(&1u32.to_le_bytes()); // sh_link = .dynstr
        image[at + 56..at + 64].copy_from_slice(&24u64.to_le_bytes());
        section(&mut image, 3, 17, 3, SHSTR_OFF as u64, names.len() as u64);

        // "\0memcpy\0__libc_start\0": offsets 1 and 8.
        let strings: &[u8] = b"\0memcpy\0__libc_start\0";
        image[DYNSTR_OFF..DYNSTR_OFF + strings.len()].copy_from_slice(strings);

        let symbol = |image: &mut Vec<u8>, index: usize, name: u32, shndx: u16| {
            let at = DYNSYM_OFF + index * 24;
            image[at..at + 4].copy_from_slice(&name.to_le_bytes());
            image[at + 6..at + 8].copy_from_slice(&shndx.to_le_bytes());
        };
        symbol(&mut image, 0, 0, 0);
        symbol(&mut image, 1, 1, SHN_UNDEF); // memcpy, imported
        symbol(&mut image, 2, 1, 1); // memcpy, defined here
        symbol(&mut image, 3, 8, SHN_UNDEF); // __libc_start, imported
        debug_assert_eq!(DYNSYM_ENTRIES * 24, 96);
        image
    }

    #[test]
    fn the_header_parser_rejects_what_is_not_an_elf64_image() {
        let mut short = SliceRead::new(b"\x7fELF\x02\x01");
        assert_eq!(parse_header(&mut short), Err(ElfError::TooShort));

        let mut pe = SliceRead::new(&[0x4d, 0x5a, 0, 0, 0, 0, 0, 0][..]);
        assert_eq!(parse_header(&mut pe), Err(ElfError::TooShort));

        let mut not_elf = SliceRead::new(&[0u8; 64][..]);
        assert_eq!(parse_header(&mut not_elf), Err(ElfError::BadMagic));

        let mut bytes = sample();
        bytes[4] = 1; // ELFCLASS32
        let mut reader = SliceRead::new(&bytes);
        assert_eq!(parse_header(&mut reader), Err(ElfError::NotElf64));

        let mut bytes = sample();
        bytes[5] = 2; // big endian
        let mut reader = SliceRead::new(&bytes);
        assert_eq!(parse_header(&mut reader), Err(ElfError::NotLittleEndian));
    }

    #[test]
    fn a_full_image_yields_its_program_headers_sections_and_imports() {
        let image = sample();
        let mut reader = SliceRead::new(&image);
        let summary = describe(&mut reader, 8).expect("valid sample");
        assert_eq!(summary.header.class, ELFCLASS64);
        assert_eq!(summary.header.kind, ET_DYN);
        assert_eq!(summary.header.machine, 62);
        assert_eq!(summary.header.phnum, 2);
        assert_eq!(summary.programs.len(), 2);
        assert_eq!(summary.programs[0].permissions(), "r-x");
        assert_eq!(summary.programs[1].permissions(), "rw-");
        assert_eq!(
            summary.sections.len(),
            4,
            "null + .dynstr + .dynsym + .shstrtab"
        );
        assert_eq!(summary.imports, vec!["memcpy", "__libc_start"]);
        assert_eq!(summary.imports_total, 2);
        assert!(!summary.imports_truncated);
        let text = summary.lines().join("\n");
        assert!(text.contains("type=3 (ET_DYN)"), "{text}");
        assert!(text.contains("ph[0] LOAD r-x"), "{text}");
        assert!(text.contains("imports: memcpy, __libc_start"), "{text}");
    }

    #[test]
    fn the_import_list_is_bounded_but_the_count_is_not() {
        let image = sample();
        let mut reader = SliceRead::new(&image);
        let summary = describe(&mut reader, 1).expect("valid sample");
        assert_eq!(summary.imports, vec!["memcpy"]);
        assert_eq!(summary.imports_total, 2);
        assert!(summary.imports_truncated);
    }

    #[test]
    fn a_broken_section_table_degrades_to_a_note_not_a_failure() {
        let mut image = sample();
        // `e_shoff` past the end of the snapshot budget.
        image[40..48].copy_from_slice(&0xffff_ffffu64.to_le_bytes());
        let mut reader = SliceRead::new(&image);
        let summary = describe(&mut reader, 8).expect("header and phdrs still parse");
        assert!(summary.sections.is_empty());
        assert!(
            summary
                .notes
                .iter()
                .any(|note| note.contains("section headers")),
            "{:?}",
            summary.notes
        );
        assert_eq!(summary.programs.len(), 2, "phdrs are independent of shdrs");
    }
}
