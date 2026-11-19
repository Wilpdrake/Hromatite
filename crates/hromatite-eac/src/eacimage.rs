//! Read-only locator and parser for an in-process **ELF64** image.
//!
//! The live EAC client module inside VRChat.exe is not a PE module: it is a
//! 6.65 MB memfd ELF mapped into our own address space, so
//! `GetModuleHandleA` cannot name it and the PE export walk used everywhere
//! else has nothing to walk. What it does have is a magic number at the start of
//! its mapping, which is enough for a cheap, read-only locator:
//!
//! 1. walk our own committed regions with `VirtualQuery` (`safe`-style, no
//!    dereference without a prior OS check);
//! 2. read **four bytes** at each region start — never page by page by default;
//! 3. only when the magic matches, take a bounded snapshot and parse it.
//!
//! Page-by-page scanning is the thing that froze the game before
//! (`Fatal error in GC — SuspendThread loop failed`): a long tight memory scan
//! on the tick thread starves the GC's `SuspendThread`. Everything here is
//! therefore budgeted, and the budget is a *hard* stop that the output reports:
//!
//! * `MAX_REGIONS` regions per call (a Unity game has a few thousand);
//! * an optional page walk (`pages`) capped by `MAX_PAGES` and yielding to the
//!   GC every `YIELD_EVERY` probes with [`hromatite_win::memory::scan_yield`];
//! * a byte budget for the snapshot of one candidate (`read=<bytes>`), so a
//!   random 1 GB region cannot be copied.
//!
//! The parser itself is pure and host-tested: it takes any [`ReadAt`], which is
//! a byte slice in the tests and our own process memory in the locator.

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

/// Regions walked per call before the walk gives up (and says so).
pub const MAX_REGIONS: usize = 65_536;
/// Pages probed inside large executable regions when the page walk is asked for.
pub const MAX_PAGES: usize = 4_096;
/// Page size used by the optional page walk.
pub const PAGE: usize = 4_096;
/// Probe once every this many pages, then yield to the game's GC.
pub const YIELD_EVERY: usize = 256;
/// Default snapshot budget for one candidate region (2 MiB).
pub const DEFAULT_READ_BYTES: usize = 2 << 20;
/// Upper bound a caller may raise the snapshot budget to (64 MiB).
pub const MAX_READ_BYTES: usize = 64 << 20;
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

/// One region of our own address space, as `VirtualQuery` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// `BaseAddress`.
    pub base: usize,
    /// `RegionSize`.
    pub size: usize,
    /// `Protect`.
    pub protect: u32,
    /// True when the region is committed and neither guarded nor inaccessible.
    pub readable: bool,
}

impl Region {
    /// `rw-`, `r-x`, … in the `/proc/self/maps` style the docs use.
    pub fn permissions(&self) -> String {
        const EXECUTE: u32 = 0x10 | 0x20 | 0x40 | 0x80;
        let mut out = String::with_capacity(3);
        out.push(if self.protect & 0x01 == 0 { 'r' } else { '-' });
        out.push(if self.protect & 0x04 != 0 || self.protect & 0x08 != 0 {
            'w'
        } else {
            '-'
        });
        out.push(if self.protect & EXECUTE != 0 {
            'x'
        } else {
            '-'
        });
        out
    }
}

/// Knobs of one `eac.module` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Walk page starts inside large executable regions too.
    pub pages: bool,
    /// Bytes one candidate image may contribute to the snapshot.
    pub read_bytes: usize,
    /// Import names printed per candidate.
    pub import_limit: usize,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            pages: false,
            read_bytes: DEFAULT_READ_BYTES,
            import_limit: 24,
        }
    }
}

impl Probe {
    /// Parse the command argument: `pages`, `read=<bytes|NNm>`, `imports=<n>`.
    ///
    /// Unknown tokens are ignored on purpose: a diagnostic must not fail
    /// because of a typo, and the defaults are the safe ones.
    pub fn parse(arg: &str) -> Self {
        let mut probe = Self::default();
        for token in arg.split_whitespace() {
            match token {
                "pages" => probe.pages = true,
                other => {
                    if let Some(value) = other.strip_prefix("read=") {
                        if let Some(bytes) = parse_size(value) {
                            probe.read_bytes = bytes.clamp(4096, MAX_READ_BYTES);
                        }
                    } else if let Some(value) = other.strip_prefix("imports=")
                        && let Ok(limit) = value.parse::<usize>()
                    {
                        probe.import_limit = limit.min(256);
                    }
                }
            }
        }
        probe
    }
}

/// `4096`, `512k`, `4m` → bytes.
fn parse_size(value: &str) -> Option<usize> {
    let (digits, factor) = match value.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024 * 1024),
        _ => (value, 1),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_mul(factor))
}

// ---------------------------------------------------------------- unix view
//
// The anti-cheat client image is mapped into this process by *native* code
// (`dlopen` of a `/memfd:` file by the launcher), not by Wine: `VirtualQuery`
// reports the whole span as unmapped and `ReadProcessMemory` cannot touch it.
// The Linux view of our own address space is the only way to name it, and Wine
// exposes that view at `\\?\unix\...` — verified live 2026-11-18: the payload
// read `\\?\unix\proc\self\maps` while a nonexistent path returned
// "File not found".

/// One line of `/proc/self/maps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapEntry {
    /// First address of the mapping.
    pub start: u64,
    /// One past the last address.
    pub end: u64,
    /// `rwxp`-style permission string.
    pub perms: String,
    /// File offset the mapping starts at.
    pub offset: u64,
    /// Everything after the inode column (`/memfd:… (deleted)`, a path, empty).
    pub path: String,
}

impl MapEntry {
    /// Mapping length in bytes.
    pub fn size(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// A readable, file-backed memfd mapping that starts a file.
    ///
    /// This is how the anti-cheat client image looks: `/memfd:<uuid> (deleted)`,
    /// first segment `r--p` at offset 0. Anonymous regions and the `(deleted)`
    /// tail of a PE are excluded by the `offset == 0` rule.
    pub fn is_memfd_image(&self) -> bool {
        self.path.contains("memfd") && self.perms.starts_with('r') && self.offset == 0
    }
}

/// Parse the text of `/proc/self/maps`; a malformed line is skipped, never fatal.
pub fn parse_maps(text: &str) -> Vec<MapEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
        ) else {
            continue;
        };
        let Some(perms) = fields.next() else { continue };
        let Some(offset) = fields.next().and_then(|o| u64::from_str_radix(o, 16).ok()) else {
            continue;
        };
        // device and inode columns, then the (possibly absent) path
        let _device = fields.next();
        let _inode = fields.next();
        let path = fields.collect::<Vec<_>>().join(" ");
        out.push(MapEntry {
            start,
            end,
            perms: perms.to_string(),
            offset,
            path,
        });
    }
    out
}

/// Bases of memfd-backed images, ascending and deduplicated.
pub fn memfd_bases(maps: &[MapEntry]) -> Vec<u64> {
    let mut bases: Vec<u64> = maps
        .iter()
        .filter(|m| m.is_memfd_image())
        .map(|m| m.start)
        .collect();
    bases.sort_unstable();
    bases.dedup();
    bases
}

/// `(first, last)` addresses of every mapping that shares the memfd path of the
/// mapping starting at `base` — the image's real span, not one segment.
pub fn image_span(maps: &[MapEntry], base: u64) -> (u64, u64) {
    let Some(first) = maps.iter().find(|m| m.start == base) else {
        return (base, base);
    };
    let mut lo = first.start;
    let mut hi = first.end;
    for map in maps.iter().filter(|m| m.path == first.path) {
        lo = lo.min(map.start);
        hi = hi.max(map.end);
    }
    (lo, hi)
}

#[cfg(all(windows, feature = "runtime"))]
mod live {
    use super::*;
    use hromatite_win::memory as safe;

    /// [`ReadAt`] over one region of our own address space, with a byte budget.
    pub struct RegionRead {
        base: usize,
        size: usize,
        budget: usize,
        spent: usize,
    }

    impl RegionRead {
        fn new(base: usize, size: usize, budget: usize) -> Self {
            Self {
                base,
                size,
                budget,
                spent: 0,
            }
        }

        /// Bytes actually copied out of the game's memory by this reader.
        pub fn spent(&self) -> usize {
            self.spent
        }
    }

    impl ReadAt for RegionRead {
        fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
            let remaining = self.budget.checked_sub(self.spent)?;
            if remaining == 0 {
                return None;
            }
            let offset = usize::try_from(offset).ok()?;
            if offset >= self.size {
                return None;
            }
            let room = self.size - offset;
            let len = len.min(remaining).min(room);
            if len == 0 {
                return None;
            }
            let address = self.base.checked_add(offset)?;
            let bytes = safe::read_bytes(address, len)?;
            self.spent += bytes.len();
            Some(bytes)
        }
    }

    /// Walk the target's own regions, applying `visit` to every committed one.
    ///
    /// The walk is bounded by [`MAX_REGIONS`] and yields with
    /// [`safe::scan_yield`] so the game's GC keeps winning `SuspendThread`.
    fn walk(mut visit: impl FnMut(Region) -> bool) -> (usize, bool) {
        let mut address = 0usize;
        let mut seen = 0usize;
        let mut stopped_early = false;
        while address < usize::MAX - 0x1000 {
            let Some(info) = safe::query_region(address) else {
                break;
            };
            let Some(next) = info.base.checked_add(info.size) else {
                break;
            };
            if next <= address {
                break;
            }
            address = next;
            seen += 1;
            if seen > MAX_REGIONS {
                // The region budget is a hard stop: a partial walk is reported
                // as such instead of pretending the process has no ELF image.
                stopped_early = true;
                break;
            }
            if seen.is_multiple_of(64) {
                safe::scan_yield();
            }
            let region = Region {
                base: info.base,
                size: info.size,
                protect: info.protect,
                readable: info.is_readable(),
            };
            if !visit(region) {
                stopped_early = true;
                break;
            }
        }
        (seen, stopped_early)
    }

    /// True when the region starts with `\x7fELF`.
    fn starts_with_magic(base: usize) -> bool {
        safe::read_bytes(base, MAGIC.len()).is_some_and(|bytes| bytes == MAGIC)
    }

    /// Describe one candidate and return its report lines.
    fn describe_region(region: Region, probe: &Probe) -> (Vec<String>, usize) {
        let mut reader = RegionRead::new(region.base, region.size, probe.read_bytes);
        let mut lines = vec![format!(
            "eac.module: ELF image at 0x{:x} size=0x{:x} protect={}",
            region.base,
            region.size,
            region.permissions()
        )];
        let result = describe(&mut reader, probe.import_limit);
        let spent = reader.spent();
        lines.push(format!(
            "eac.module: snapshot budget {} B, copied {spent} B",
            probe.read_bytes
        ));
        match result {
            Ok(summary) => lines.extend(summary.lines()),
            Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
        }
        (lines, spent)
    }

    /// Run the locator and return the lines to log.
    pub fn probe_command(probe: &Probe) -> Vec<String> {
        let started = std::time::Instant::now();
        let mut committed = 0usize;
        let mut magic_hits = 0usize;
        let mut copied = 0usize;
        let mut pages_walked = 0usize;
        let mut report: Vec<String> = Vec::new();

        let (seen, stopped_early) = walk(|region| {
            if !region.readable {
                return true;
            }
            committed += 1;
            if starts_with_magic(region.base) {
                magic_hits += 1;
                let (lines, spent) = describe_region(region, probe);
                copied += spent;
                report.extend(lines);
                if magic_hits >= 2 {
                    return false;
                }
                return true;
            }
            if probe.pages
                && region.size >= 2 * PAGE
                && region.protect & (0x10 | 0x20 | 0x40 | 0x80) != 0
            {
                // Only the page starts of a large executable region, and only
                // within the global page budget.
                let pages = (region.size / PAGE).saturating_sub(1);
                for index in 1..=pages {
                    if pages_walked >= MAX_PAGES {
                        return false;
                    }
                    pages_walked += 1;
                    if pages_walked.is_multiple_of(YIELD_EVERY) {
                        safe::scan_yield();
                    }
                    let at = region.base + index * PAGE;
                    if starts_with_magic(at)
                        && let Some(region) = region_at(at)
                    {
                        magic_hits += 1;
                        let (lines, spent) = describe_region(region, probe);
                        copied += spent;
                        report.extend(lines);
                        if magic_hits >= 2 {
                            return false;
                        }
                    }
                }
            }
            true
        });

        let mut lines = vec![format!(
            "eac.module: walked {seen} region(s), {committed} committed/readable, \
             {magic_hits} ELF magic hit(s), pages_walked={pages_walked}, \
             copied={copied} B, {} ms, stopped_early={stopped_early}",
            started.elapsed().as_millis()
        )];
        if magic_hits == 0 {
            lines.push(
                "eac.module: no ELF image found at a region start (try `eac.module pages`)"
                    .to_string(),
            );
        }
        lines.extend(report);
        lines
    }

    /// `VirtualQuery` for one address: the region containing it.
    fn region_at(address: usize) -> Option<Region> {
        let info = safe::query_region(address)?;
        Some(Region {
            base: info.base,
            size: info.size,
            protect: info.protect,
            readable: info.is_readable(),
        })
    }
}


    // ----------------------------------------------------- the unix view

    /// `/proc/self/maps` of *this* process, through the Wine unix namespace.
    const UNIX_MAPS: &str = r"\\?\unix\proc\self\maps";
    /// The same process's memory as a seekable file.
    const UNIX_MEM: &str = r"\\?\unix\proc\self\mem";

    /// [`ReadAt`] over `/proc/self/mem`, with a byte budget.
    ///
    /// `/proc/self/mem` answers at *virtual* offsets, so `base + offset` names
    /// the same byte the payload would read with `safe::*` — if the mapping were
    /// visible to `VirtualQuery`, which the memfd image is not.
    pub struct UnixMemRead {
        file: std::fs::File,
        base: u64,
        budget: usize,
        spent: usize,
    }

    impl UnixMemRead {
        /// Open `/proc/self/mem` and anchor it at `base`.
        pub fn open(base: u64, budget: usize) -> Option<Self> {
            let file = std::fs::File::open(UNIX_MEM).ok()?;
            Some(Self {
                file,
                base,
                budget,
                spent: 0,
            })
        }

        /// Bytes copied out of the image by this reader.
        pub fn spent(&self) -> usize {
            self.spent
        }
    }

    impl ReadAt for UnixMemRead {
        fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            if self.spent.checked_add(len)? > self.budget {
                return None;
            }
            let at = self.base.checked_add(offset)?;
            self.file.seek(SeekFrom::Start(at)).ok()?;
            let mut buffer = vec![0u8; len];
            self.file.read_exact(&mut buffer).ok()?;
            self.spent += len;
            Some(buffer)
        }
    }

    /// Locate memfd images through the Linux view and describe them.
    ///
    /// Costs one small file read plus a 64-byte probe per candidate; nothing is
    /// walked page by page, and no budget can be exceeded.
    pub fn locate_unix(probe: &Probe) -> Vec<String> {
        let text = match std::fs::read_to_string(UNIX_MAPS) {
            Ok(text) => text,
            Err(error) => {
                return vec![format!("eac.module: cannot read {UNIX_MAPS}: {error}")];
            }
        };
        let maps = parse_maps(&text);
        let mut lines = vec![format!(
            "eac.module: unix view: {} mapping(s) in /proc/self/maps, {} bytes",
            maps.len(),
            text.len()
        )];
        if let Some(first) = maps.first() {
            lines.push(format!(
                "eac.module: unix view: first=0x{:x} {} perms={} path={}",
                first.start,
                first.size(),
                first.perms,
                if first.path.is_empty() {
                    "<anon>"
                } else {
                    &first.path
                }
            ));
        }
        let mut found = 0usize;
        for base in memfd_bases(&maps) {
            let Some(mut reader) = UnixMemRead::open(base, probe.read_bytes) else {
                lines.push("eac.module: cannot open /proc/self/mem".to_string());
                break;
            };
            let Some(head) = reader.read_at(0, MAGIC.len()) else {
                continue;
            };
            if head != MAGIC {
                continue;
            }
            found += 1;
            let (lo, hi) = image_span(&maps, base);
            let segments: Vec<String> = maps
                .iter()
                .filter(|m| m.start >= lo && m.end <= hi)
                .map(|m| format!("{}+0x{:x}({})", m.perms, m.size(), m.offset))
                .collect();
            lines.push(format!(
                "eac.module: unix ELF image at 0x{base:x} span=0x{:x} (0x{lo:x}..0x{hi:x}) \
                 segments={}",
                hi.saturating_sub(lo),
                segments.join(" ")
            ));
            let result = describe(&mut reader, probe.import_limit);
            lines.push(format!(
                "eac.module: snapshot budget {} B, copied {} B",
                probe.read_bytes,
                reader.spent()
            ));
            match result {
                Ok(summary) => lines.extend(summary.lines()),
                Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
            }
            if found >= 2 {
                break;
            }
        }
        if found == 0 {
            lines.push("eac.module: unix view: no memfd ELF image".to_string());
        }
        lines
    }


// ----------------------------------------------------- the native view
//
// Measured live 2026-11-18: in Wine a `\\?\unix\proc\self\...` path is opened by
// the *wineserver*, so `self` names wineserver (111 mappings, first one
// `.../bin/wineserver`), not the game. The game's own Linux pid has to be found
// by name; its `maps` is world-readable, and the memfd image it names is
// readable by a plain load even though `VirtualQuery` reports it unmapped —
// Wine simply does not track mappings that native code created.

/// `/proc` of the host namespace, as seen through Wine.
#[cfg(feature = "runtime")]
const UNIX_PROC: &str = r"\\?\unix\proc";

/// The Linux pid of `VRChat.exe`, found by reading `cmdline` of `/proc/<pid>`.
#[cfg(feature = "runtime")]
pub fn find_process_pid() -> Option<u32> {
    let entries = std::fs::read_dir(UNIX_PROC).ok()?;
    for (index, entry) in entries.flatten().enumerate() {
        if index > 4096 {
            break;
        }
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("{UNIX_PROC}/{pid}/cmdline")) else {
            continue;
        };
        if cmdline
            .split(|byte| *byte == 0)
            .any(|arg| arg.ends_with(b"VRChat.exe"))
        {
            return Some(pid);
        }
    }
    None
}

/// `maps` text of one pid.
#[cfg(feature = "runtime")]
pub fn maps_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("{UNIX_PROC}/{pid}/maps")).ok()
}

/// One mapped file segment of the image: `offset` in the file, `size` bytes,
/// at `address` in our address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// File offset the segment starts at.
    pub offset: u64,
    /// Segment length in bytes.
    pub size: u64,
    /// Address the segment is mapped at.
    pub address: u64,
}

/// Translation table from `maps` entries that belong to one image.
///
/// ELF offsets handed to [`ReadAt`] are *file* offsets (the header and program
/// headers are addressed that way), and the loader maps the last segment at a
/// different `p_vaddr` than its `p_offset` — for the live EAC image the program
/// headers live at file offset `0x655258` but at address `base + 0x65e258`.
/// Only `maps` knows the difference.
pub fn segments_of(maps: &[MapEntry], base: u64) -> Vec<Segment> {
    let Some(first) = maps.iter().find(|m| m.start == base) else {
        return Vec::new();
    };
    let mut segments: Vec<Segment> = maps
        .iter()
        .filter(|m| m.path == first.path)
        .map(|m| Segment {
            offset: m.offset,
            size: m.size(),
            address: m.start,
        })
        .collect();
    segments.sort_by_key(|segment| segment.offset);
    segments
}

/// [`ReadAt`] over raw addresses of our own process, bounded by the segments.
///
/// The segments come from `/proc/<pid>/maps`, so every read is inside a mapping
/// the kernel reports; the VEH guard turns a stale mapping into `None` instead
/// of a dead game.
#[cfg(feature = "runtime")]
pub struct NativeMemRead {
    segments: Vec<Segment>,
    budget: usize,
    spent: usize,
}

#[cfg(feature = "runtime")]
impl NativeMemRead {
    /// Reader over the image whose segments are `segments`.
    pub fn new(segments: Vec<Segment>, budget: usize) -> Self {
        Self {
            segments,
            budget,
            spent: 0,
        }
    }

    /// Bytes copied out of the image by this reader.
    pub fn spent(&self) -> usize {
        self.spent
    }

    /// Address of a file offset, when a mapped segment contains all of `[offset, offset+len)`.
    fn address_of(&self, offset: u64, len: u64) -> Option<u64> {
        let end = offset.checked_add(len)?;
        self.segments.iter().find_map(|segment| {
            let segment_end = segment.offset.checked_add(segment.size)?;
            (segment.offset <= offset && end <= segment_end)
                .then(|| segment.address.checked_add(offset - segment.offset))
                .flatten()
        })
    }
}

#[cfg(feature = "runtime")]
impl ReadAt for NativeMemRead {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let at = self.address_of(offset, u64::try_from(len).ok()?)?;
        if self.spent.checked_add(len)? > self.budget {
            return None;
        }
        let mut buffer = vec![0u8; len];
        let result = hromatite_win::veh::call_with(|| {
            // SAFETY: `at` comes from a `maps` entry of this process and `len`
            // stays inside that mapping; the vectored handler converts a fault
            // into `Err` instead of killing the game.
            unsafe {
                core::ptr::copy_nonoverlapping(at as *const u8, buffer.as_mut_ptr(), len);
            }
        });
        if result.is_err() {
            return None;
        }
        self.spent += len;
        Some(buffer)
    }
}

/// Locate the memfd image through `maps` + direct reads, and describe it.
#[cfg(feature = "runtime")]
pub fn locate_native(probe: &Probe) -> Vec<String> {
    let Some(pid) = find_process_pid() else {
        return vec!["eac.module: VRChat.exe not found in /proc (wine unix view)".to_string()];
    };
    let Some(text) = maps_of(pid) else {
        return vec![format!("eac.module: cannot read /proc/{pid}/maps")];
    };
    let maps = parse_maps(&text);
    let bases = memfd_bases(&maps);
    let mut lines = vec![format!(
        "eac.module: pid {pid}, {} mapping(s), {} memfd candidate(s)",
        maps.len(),
        bases.len()
    )];
    let mut found = 0usize;
    for base in bases {
        let (lo, hi) = image_span(&maps, base);
        let segments_of_image = segments_of(&maps, base);
        let mut reader = NativeMemRead::new(segments_of_image.clone(), probe.read_bytes);
        let Some(head) = reader.read_at(0, MAGIC.len()) else {
            lines.push(format!("eac.module: 0x{base:x}: not readable"));
            continue;
        };
        if head != MAGIC {
            continue;
        }
        found += 1;
        let segments: Vec<String> = maps
            .iter()
            .filter(|m| m.start >= lo && m.end <= hi)
            .map(|m| format!("{}+0x{:x}({})", m.perms, m.size(), m.offset))
            .collect();
        let file_map: Vec<String> = segments_of_image
            .iter()
            .map(|segment| format!("file 0x{:x} -> 0x{:x}", segment.offset, segment.address))
            .collect();
        lines.push(format!(
            "eac.module: native ELF image at 0x{base:x} span=0x{:x} (0x{lo:x}..0x{hi:x}) segments={}",
            hi.saturating_sub(lo),
            segments.join(" ")
        ));
        lines.push(format!(
            "eac.module: file mapping: {}",
            file_map.join(", ")
        ));
        let result = describe(&mut reader, probe.import_limit);
        lines.push(format!(
            "eac.module: snapshot budget {} B, copied {} B",
            probe.read_bytes,
            reader.spent()
        ));
        match result {
            Ok(summary) => lines.extend(summary.lines()),
            Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
        }
        if found >= 2 {
            break;
        }
    }
    if found == 0 {
        lines.push("eac.module: no readable memfd ELF image in our own maps".to_string());
    }
    lines
}

/// `eac.module [pages] [read=<bytes>] [imports=<n>]`: locate the in-process
/// ELF image(s) and describe them. Read-only; never writes a byte.
#[cfg(feature = "runtime")]
pub fn command(arg: &str) -> Vec<String> {
    let probe = Probe::parse(arg);
    #[cfg(windows)]
    {
        // `win` = the Windows view (VirtualQuery, blind to native mappings);
        // `self` = the `proc/self` view (which names the wineserver, see above);
        // default = pid + `maps` + direct reads, the only view that finds it.
        if arg.split_whitespace().any(|token| token == "win") {
            return live::probe_command(&probe);
        }
        if arg.split_whitespace().any(|token| token == "self") {
            return locate_unix(&probe);
        }
        let mut lines = locate_native(&probe);
        if !lines.iter().any(|line| line.contains("native ELF image")) {
            lines.push("eac.module: falling back to the Windows view (VirtualQuery)".to_string());
            lines.extend(live::probe_command(&probe));
        }
        lines
    }
    #[cfg(not(windows))]
    {
        let _ = probe;
        vec!["eac.module: host target has no address space to walk (build for x86_64-pc-windows-gnu)".to_string()]
    }
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

    #[test]
    fn probe_arguments_are_parsed_defensively() {
        let probe = Probe::parse("pages read=512k imports=7 typo");
        assert!(probe.pages);
        assert_eq!(probe.read_bytes, 512 * 1024);
        assert_eq!(probe.import_limit, 7);

        let probe = Probe::parse("read=1m");
        assert_eq!(probe.read_bytes, 1024 * 1024);
        let probe = Probe::parse("read=1 imports=999");
        assert_eq!(probe.read_bytes, 4096, "clamped up to one page");
        assert_eq!(probe.import_limit, 256, "clamped");
        let probe = Probe::parse("");
        assert_eq!(probe, Probe::default());
        assert!(parse_size("2m") == Some(2 << 20));
        assert!(parse_size("nope").is_none());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn the_module_command_always_answers_and_never_touches_memory_on_the_host() {
        let lines = command("pages read=1m typo");
        assert!(!lines.is_empty(), "the command must print something");
        #[cfg(not(windows))]
        assert!(
            lines[0].contains("host target"),
            "host builds cannot walk an address space: {lines:?}"
        );
    }

    #[test]
    fn region_permissions_are_rendered_like_proc_maps() {
        let region = Region {
            base: 0,
            size: 0x1000,
            protect: 0x04,
            readable: true,
        };
        assert_eq!(region.permissions(), "rw-");
        let region = Region {
            protect: 0x20,
            ..region
        };
        assert_eq!(region.permissions(), "r-x");
        let region = Region {
            protect: 0x01,
            readable: false,
            ..region
        };
        assert_eq!(
            region.permissions(),
            "---",
            "PAGE_NOACCESS is no access at all"
        );
    }

    #[test]
    fn proc_maps_parser_names_the_memfd_image() {
        // Two lines taken verbatim from the live game process (2026-11-18).
        let text = "7c4eb05a0000-7c4eb05a1000 rw-p 0011f000 00:01 281203 /wine/x86_64-unix/ntdll.so\n\
                    7c4eb05a1000-7c4eb05c1000 r--p 00000000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    7c4eb05c1000-7c4eb0704000 r-xp 00020000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    7c4eb07b9000-7c4eb07bb000 rw-p 00217000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    100000000-104000000 rw-p 00000000 00:00 0";
        let maps = parse_maps(text);
        assert_eq!(maps.len(), 5);
        assert_eq!(memfd_bases(&maps), vec![0x7c4eb05a1000], "only offset 0 starts a file");
        let (lo, hi) = image_span(&maps, 0x7c4eb05a1000);
        assert_eq!(lo, 0x7c4eb05a1000);
        assert_eq!(hi, 0x7c4eb07bb000, "the rw-p tail belongs to the same image");
        assert_eq!(hi - lo, 0x21a000);
        assert!(!maps[0].is_memfd_image(), "an ELF .so of Wine is not a memfd image");
        assert!(!maps[4].is_memfd_image(), "an anonymous region has no path");
    }

    #[test]
    fn malformed_maps_lines_are_skipped_not_fatal() {
        let maps = parse_maps("garbage\n7c4e-zzzz r-xp 0 0:0 0 x\n7c4e0000-7c4e1000 r--p 00000000 00:01 1 /memfd:abc (deleted)\n");
        assert_eq!(maps.len(), 1, "only the well-formed line survives");
        assert_eq!(maps[0].size(), 0x1000);
        assert_eq!(memfd_bases(&maps), vec![0x7c4e0000]);
        assert_eq!(image_span(&maps, 0xdead), (0xdead, 0xdead), "unknown base is empty");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn segment_table_translates_file_offsets_the_way_the_loader_mapped_them() {
        // Live lines (2026-11-18): the last LOAD has p_offset 0x219000 but
        // p_vaddr 0x222000, and the program headers live at file offset 0x655258.
        let text = "7c4eb05a1000-7c4eb05c1000 r--p 00000000 00:01 1 /memfd:x (deleted)\n\
                    7c4eb05c1000-7c4eb0704000 r-xp 00020000 00:01 1 /memfd:x (deleted)\n\
                    7c4eb07c3000-7c4ec0000000 r-xp 00219000 00:01 1 /memfd:x (deleted)";
        let maps = parse_maps(text);
        let base = 0x7c4eb05a1000u64;
        let segments = segments_of(&maps, base);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2].offset, 0x219000);
        assert_eq!(segments[2].address, 0x7c4eb07c3000, "p_vaddr, not p_offset");
        let reader = NativeMemRead::new(segments, 4096);
        assert_eq!(
            reader.address_of(0x655258, 56),
            Some(0x7c4eb07c3000 + (0x655258 - 0x219000)),
            "the program headers are addressed by file offset, mapped at p_vaddr"
        );
        assert_eq!(
            reader.address_of(0x2186c0, 64),
            None,
            "section headers fall in an unmapped gap between two LOAD segments"
        );
        assert_eq!(reader.address_of(0x1000, 0x100), Some(base + 0x1000));
    }
}
