//! Read-only locator and parser for an in-process **ELF64** image.
//!
//! The live EAC client module inside VRChat.exe is not a PE module: it is a
//! 6.65 MB memfd ELF mapped into our own address space, so
//! `GetModuleHandleA` cannot name it and the PE export walk used everywhere
//! else has nothing to walk. What it does have is a magic number at the start of
//! its mapping, which is enough for a cheap, read-only locator:
//!
//! 1. decide *where* candidate images are ([`locator`]: `maps`, memfd paths,
//!    segment translation);
//! 2. read **four bytes** at each candidate — never page by page by default;
//! 3. only when the magic matches, take a bounded snapshot and parse it
//!    ([`elf`]).
//!
//! Page-by-page scanning is the thing that froze the game before
//! (`Fatal error in GC — SuspendThread loop failed`): a long tight memory scan
//! on the tick thread starves the GC's `SuspendThread`. Everything here is
//! therefore budgeted, and the budget is a *hard* stop that the output reports
//! (see `live`).
//!
//! The module is split so the pure half can be linked by a build without the
//! `runtime` feature:
//!
//! * [`elf`] — the ELF64 parser over any [`elf::ReadAt`]; pure, host-tested.
//! * [`locator`] — `/proc/self/maps` parsing, memfd candidate selection and the
//!   file-offset → address translation; pure, host-tested.
//! * `live` — the walkers that actually touch memory (the `VirtualQuery` walk,
//!   the `proc/self` view, the pid + `maps` + direct-read view). `runtime` only.
//! * `report` — the `eac.module` entry point that produces the report lines.
//!   `runtime` only.
//!
//! Every name that used to live at `hromatite_eac::eacimage::X` is re-exported
//! here, so the split is invisible to callers.

mod elf;
mod locator;
// The live half is the only part that needs the OS: `hromatite-win`'s safe
// readers and the VEH guard. Off, the pure core still parses and locates.
#[cfg(feature = "runtime")]
mod live;
#[cfg(feature = "runtime")]
mod report;

pub use elf::{
    ELFCLASS64, ELFDATA2LSB, ET_DYN, ElfError, ElfHeader, MAGIC, MAX_PROGRAMS, MAX_SECTIONS,
    PT_LOAD, ProgramHeader, ReadAt, SHN_UNDEF, SHT_DYNSYM, Section, SliceRead, Summary, describe,
    parse_header, parse_program_headers, parse_sections,
};
pub use locator::{
    DEFAULT_READ_BYTES, MAX_PAGES, MAX_READ_BYTES, MAX_REGIONS, MapEntry, PAGE, Probe, Region,
    Segment, YIELD_EVERY, image_span, memfd_bases, parse_maps, segments_of,
};

#[cfg(feature = "runtime")]
pub use live::{NativeMemRead, UnixMemRead, find_process_pid, locate_native, locate_unix, maps_of};
#[cfg(all(feature = "runtime", windows))]
pub use live::{RegionRead, probe_command};
#[cfg(feature = "runtime")]
pub use report::command;
