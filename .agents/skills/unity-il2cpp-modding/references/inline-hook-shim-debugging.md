# Decrypt-on-entry shim: layout, stage markers, failure modes

Goal shape: the payload image sits `PAGE_READWRITE` and XOR-encrypted between ticks; one small page
outside the image holds the shim that decrypts on entry, runs the tick, and re-encrypts on exit.

## Page plan

| Page | Protection | Contents |
|---|---|---|
| shim (tick) | `r-xp` | entry code emitted at build time, position-independent, no Rust statics |
| shim (VEH) | `r-xp` | handler that re-encrypts if the tick faults |
| data | `rw-p` | `lock, due, hits, leaked, windows, crypted, stage, fault_addr, fault_code`, image base/size, code range |
| payload image | `rw-p` at rest, `rx` only for `.text` while a window is open | the mapped DLL |

State fields are read from the host by scanning anonymous `rw-p` regions for the shim's known data page
log line, then `read /proc/<pid>/mem` at `+0x00 lock`, `+0x08 due`, `+0x10 hits`, `+0x18 leaked`,
`+0x28 windows`, `+0x30 crypted`, `+0x38 stage`, `+0x40 fault_addr`, `+0x48 fault_code`.

## Stage markers (write one byte before each phase)

`1` entered shim → `2` window opened (already open?) → `3` decrypt done → `4` protections set →
`5` detour about to be called → `6` detour returned → `7` re-encrypt → `8` RX applied → `0xF` VEH fired.
A stuck stage localises the bug without a debugger: `7` forever means the detour is itself paging code
back in; `F` with `fault_addr` inside the shim means the emitted byte stream is wrong.

## Bugs that actually bite (each cost a live game run)

1. **Instruction length.** `mov qword [rdi+0x38], imm32` is 8 bytes (`48 C7 47 38` + 4). Emitting the
   5-byte form shifts every later instruction; the first fault appears at `shim+0x2A`, far from the bug.
2. **Counter clobber.** The `crypt` loop uses `rdi` as the page counter; the detour then receives the
   counter instead of the data page and faults on its first store (`shim+0x179`). Reload `rdi` after.
3. **Protection range.** Setting `RX` over the whole image covers `.data`; a Rust `static` mutex there
   faults on `lock cmpxchg`. Protect only executable sections, and pass `text_hi` = end of code, not the
   end of the keep-plaintext range.
4. **FlushInstructionCache inside the shim** can block with the window open (x86 I-cache is coherent and
   `VirtualProtect` serialises); drop the call before hunting elsewhere.
5. **Probe placed outside the image.** A Rust `static` may be linked past `SizeOfImage`; a shim that
   decides "inside image ⇒ encrypted" then decrypts plaintext. Keep the probe inside the data page.
6. **Range from `VirtualQuery`** covers only the queried sub-section; the shim then decrypts a subset with
   the wrong page keys. Pass `(base, size, code_lo, code_hi)` from the injector through the stub register.
7. **Seal and hook together.** Sealing after the entry returns encrypts the patched site the hook jumps
   to; the next frame executes ciphertext. Run sealed mode with an inline payload, or pause the seal.

## Partial seal (safe intermediate)

When the full shim is not ready, seal everything except the range from `.text` start to the end of the
writable sections: headers/`.rdata`/`.reloc` become ciphertext (region is no longer PE-shaped), `.text`
and `.data` stay live, and the normal in-image detour keeps ticking. Verified: `seal: 3 pages XORed,
0 failed` with ticks continuing and the image header reading as ciphertext on the host. It is a weaker
hiding (code stays plaintext) but it never kills the game, so use it as the A/B baseline.
