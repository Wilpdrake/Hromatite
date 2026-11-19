# Naming the obfuscated il2cpp exports inside Ghidra

When the ask is "find the exports and their real names" / "let's name the functions", the deliverable is
**renames inside the analysis database**, not an offline table — a human has to read that listing later.
A build-keyed JSON map is the side artifact.

## 0. Inventory the export table offline first

Parse the PE export directory yourself; you need name→RVA per build anyway.

- PE32+ data directories start at `opt + 112` (PE32: `+96`). Using 96 on a PE32+ image resolves the export
  directory to garbage and every later read fails with an unrelated-looking unpack error.
- `AddressOfNameOrdinals` entries are **u16**. Reading `u32` there yields ordinal values like `253191888`
  and an out-of-buffer unpack instead of a wrong table.
- Measured VRChat build: 251 export names over **225 distinct addresses** — alias groups (up to 8 names on
  one address) are one function; rename once and pick the name that matches the code.
- The export directory is name-sorted, so name order carries no meaning; sort by **RVA** to see the layout:
  one contiguous il2cpp-api block (~`0xc27990`–`0xc2a990` on that build) plus a scatter of early entries
  (array/gc/thread helpers) and the non-il2cpp exports (zlib `CreateZStream`/`Flush`/`ReadZStream`,
  sockets `CreateNLSocket`/`ReadEvents`, COM `DllGetActivationFactory`/`DllCanUnloadNow`).

## 1. Build a shape table before touching Ghidra

Slice the first ~320 bytes of every export and disassemble flat as binary:

```bash
objdump -D -b binary -m i386:x86-64 --adjust-vma=0x<rva> /tmp/slice.bin
```

Per export record: size up to the first top-level `ret`, first instruction (a `jmp <addr>` means a thin
wrapper — capture the **target's** first instructions too), memory operands off the argument registers
(`[rcx+0xNN]`, `[rdx+…]`, `[r8+…]`, `[r9+…]`), call targets, and RIP-referenced globals.

Gotcha that silently empties the whole table: objdump rows are `   c2a330:\t<bytes>\t<asm>`, so strip the
trailing colon before `int(x, 16)`. Without it every row fails to parse and you conclude "no disassembly".

One line per export is enough for a worker to decide in one or two Ghidra calls, and it costs no analysis
server time.

## 2. Rename recipe (Ghidra MCP)

```json
{"renames":[{"target_type":"function","identifier":"0x180c2a060","new_name":"il2cpp_type_get_type"}]}
```

- `identifier` is the **address string** (`ImageBase + RVA`, e.g. `0x180000000 + 0xc2a060`); batch 10–30
  renames per call. Renaming sets the function name; the export label keeps its obfuscated name, which is fine
  — listings and the decompiler show the new one.
- **Ghidra analysis can leave an export address without a function.** If the rename answers
  `Function not found`, do **not** call `create_function`: on bytes Ghidra never disassembled it creates a
  1-byte function, and the MCP surface has no `delete_function`, so that address is permanently half-broken
  (rename works, `get_disassembly` and `get_pseudocode` return 0 instructions and re-creating it is refused as
  "already exists"). Report `no_function` and move on.
- `get_pseudocode` is **asynchronous** on this server: it returns a `Task ID` with `Status: PENDING`, and the
  body is fetched with `get_task_status(task_id)`. `get_function_info`, `get_disassembly`, `get_xrefs_to`,
  `rename_symbols_batch` are synchronous. Queue 5–8 decompiles, then collect their statuses.
- `get_xrefs_to` reports **0 references for operands that never got a reference made**, including ordinary
  rip-relative global reads. Never conclude "nothing reads this global" from it; open the pseudocode.
- Save the project when the pass is done — renames live in memory otherwise.

## 3. Identification rules

- **Body shape beats stub order.** Measured stub→canonical offsets: 12→11 (`get_corlib`), 16→15 (`free`),
  83→84 (`native_stack_trace`), 122→121 (`gchandle_get_target`), 211→211 (`image_get_class`). The offset
  oscillates between +1 and −1, so order only generates candidates; a name needs a call or the shape.
- Arg 1 is `rcx`. Tiny getters map straight onto the measured layouts:

```
Il2CppClass  +0x00 image   +0x10 parent   +0x18 namespaze  +0x20 byval_arg (Il2CppType)
             +0x40 element_class   +0x48 interfaces  +0x12c interfaces_count
             +0x50 methods  +0x120 method_count      +0x80 fields  +0x124 fields_count (u16)
             +0x88 events  +0x126 events_count       +0x90 properties  +0x122 properties_count
             +0x98 name    +0xa8 nested_types  +0x128 nested_type_count
             +0xb0 vtable  +0x12e vtable_count      +0xc8 interface_offsets
             +0xf8 instance_size  +0x104 element_size  +0x10c static_fields_size
             +0x118 flags  +0x11c token  +0x130 depth  +0x132 rank (u8)
             +0x133 minimumAlignment  +0x135/+0x136 bit flags
MethodInfo   +0x00 methodPointer  +0x08 virtualMethodPointer  +0x10 invoker  +0x18 name
             +0x20 klass  +0x28 return_type  +0x30 parameters  +0x38 metadataHandle
             +0x48 token  +0x4c flags (u16)  +0x4e iflags  +0x50 slot  +0x52 parameters_count (u8)
FieldInfo    stride 0x28: +0x00 type  +0x08 offset (int32)  +0x10 name  +0x18 token  +0x20 parent
Il2CppType   +0x00 data  +0x08 attrs (u16)  +0x0a type (int8)
Il2CppImage  +0x00 assembly  +0x08 short name  +0x14 custom-attribute range count (NOT typeStart)
             +0x20 metadata handle (typeStart is the first dword of imageDef, i.e. *(+0x20))
             +0x30 codegenModule  +0x40 "<name>.dll"
```

  The `FieldInfo` pair is the classic swap: `field_get_offset` compiles to `movslq 0x8(%rcx)` and the token is
  the `+0x18` field that `field_has_attribute` feeds into the metadata cache. Grep those two bodies before
  trusting any published table.
- Six near-identical `(klass, void** iter)` iterators exist (fields, methods, events, properties, interfaces,
  nested_types). Separate them by the class offset they touch and by their internal callee — never by
  neighbour order, which is exactly where they look interchangeable.
- Type predicates read the `+0x0a` tag: `PTR 0x0f`, `ARRAY 0x14`, `VAR 0x13`, `MVAR 0x1e`, `VALUETYPE 0x11`,
  `CLASS 0x12`; `class_is_generic` reads `byval_arg.type` at `class+0x2a` and compares with VAR/MVAR.
  Attrs-bit questions (`is_static`, `is_byref`, `is_valuetype`) go through the decompiler: the bitfield
  (`attrs:16, type:8, num_mods:6, byref:1`) is easier to read there than to derive from `shr` counts.
- Naming convention: `il2cpp_<name>` when shape or behaviour proves it, `il2cpp_cand_<name>` when only the
  stub order supports it, leave it unnamed otherwise. Never dress an order-derived guess as proof.

## 4. Parallel pass (one worker per address band)

Split the distinct addresses into contiguous RVA bands (~29 addresses each) and give each agent one band.
Renames are per-address and bands are disjoint, so no coordination is needed — provided you forbid each
child from touching anything outside its own list, including the internal targets of its thunks.

Per child: a shared brief file (layouts, proven anchors, rename recipe, async-pseudocode note, naming
convention, JSON schema), its band file, and a **pre-computed shape file**. Explicitly forbid
`create_function`, `disassemble_bytes`, `patch_bytes`, `save_project`, `close_program` — one stray
`create_function` leaves a permanently broken address that no later pass can repair.

Run **one pilot band first**: it proves the MCP toolset is reachable from a child session before the other
bands load the same analysis server. Then merge the per-band JSONs: dedupe by address (alias groups appear
once), count by confidence, and list everything not renamed so the gaps are explicit.

Name the internal helpers (thunk targets) in a **separate later pass**, not alongside the wrappers: one
target backs many wrappers, so the same address would otherwise be claimed with different names by
different workers.

## 5. Close with a canonical-list diff, then a second wave

The banded pass never reaches 100 %: widget getters, bit-flag readers, global setters and whatever Unity 6
added on top of the published API survive it. Finish in this order.

1. **Merge** the per-band JSONs, dedupe by address, count by confidence.
2. **Diff against the canonical API list**: `canonical_names − assigned_names`. Measured on one build: 183 of
   225 addresses named, 49 canonical names unclaimed. That leftover list is the second wave's search space,
   and it is stronger than "look at the shape again" — a `movzwl 0x8(%rcx)` getter cannot be
   `image_get_filename` once the list says filename is still free and attrs are not.
3. **Check duplicates and non-canonical names** in the merged map. One name on two addresses means one of
   them is wrong (a `strlen`+copy pair looks like `string_new` both as a thunk and inline); an `il2cpp_*`
   name absent from the canonical list is an invented identity — demote it to `cand_` or drop it.
4. **Second wave** on the leftovers, grouped by difficulty rather than by address: plain getters in one band,
   global setter/getter pairs in another, Unity-6-only subsystems last (internal hook/registration vectors,
   GC heap walkers). Those last have no canonical name at all — give them descriptive `unity_*` names at
   `inferred` confidence, or leave them.
5. Anything still unnamed stays unnamed and is reported with the reason. A short honest gap list beats a
   `cand_` name that a later reader will trust.

## 6. Second pass: independent verification (do this before reporting done)

A first pass is *candidate generation*: name taken from shape, order and one decompile. Treat the whole map
as unverified until a second pass re-derives every entry **without trusting the previous evidence**.

Protocol per address: read the callee chain, the struct offsets touched, how the arguments are used, string
literals and IAT slots; then write a verdict `confirm | uncertain | wrong | nofunc` plus the correcting name.
Measured on one build (218 addresses, 10 reviewers, 20–22 each): **207 confirm, 7 uncertain, 4 wrong** — a
~2 % error rate is what to expect, so “everything looked fine” is not a plausible result.

Where the errors actually come from (all four measured misses were an *adjacent* thing):

- **neighbouring bit flags.** `+0x135` and `+0x136` each hold several one-bit predicates; `is_enum` reads
  `(+0x135 >> 2) & 1`, `is_blittable` `+0x136.3`, `import_or_windows_runtime` `+0x136.4`. Do not assign a
  bit-getter from its slot between two known ones — settle the order from the code that *unpacks the bits
  from metadata* (`Il2CppTypeDefinition.flags` shifted into the class bitfield during class construction) or
  from the single predicate that provably sets the bit.
- **name vs filename.** `image_get_name` is `+0x08`, `image_get_filename` is `+0x40` (`"<name>.dll"`). Both
  are one-instruction getters, so only the string content settles it.
- **a forwarder is not a getter.** `mov dl,1; jmp Class::FromIl2CppType` style thunks and `call Object::New;
  ret` forwarders carry no shape of their own; a `class_from_type`-looking slot was really `object_new`
  because the callee was `Object::New` (allocates `instance_size`, writes `klass` at `obj+0`). Always name a
  thunk after what the callee does, not after its position.
- **a reviewer's layout claim needs an independent anchor.** One report concluded from three call sites that
  `MethodInfo` was `klass@0x28 / return_type@0x20`; `il2cpp_method_get_param` reads `parameters@0x30`, which
  only fits the upstream order (`klass@0x20`, `return_type@0x28`). Refute by finding one function whose
  offsets are pinned by observed behaviour, and prefer that over majority vote among call sites.

Give reviewers the known traps as *hypotheses to re-check*, not as facts: the value of the pass is that a
fresh agent notices a name contradicts the code, and that only happens if it is told to derive, not compare.

## 7. ICF aliasing: several APIs share one address

The linker folds identical bodies, so a canonical name legitimately has **no address of its own**. On one
build, 24 of 232 canonical names were in that state while only 5 addresses were unnamed. Consequences:

- The canonical-list diff (§5.2) under-reports coverage — do not "fix" it by inventing addresses.
- Recover the alias set from the offset each API must read. Measured pairs/groups: `mov (%rcx)` →
  `object_get_class`, `class_get_image`, `image_get_assembly`, `assembly_get_image`, `property_get_parent`;
  `mov 0x18(%rcx)` → `class_get_namespace` + `method_get_name`; `mov 0x20(%rcx)` → `field_get_parent` +
  `method_get_class`; `mov 0x10(%rcx)` → `class_get_parent` + `field_get_name`; `mov 0x40(%rcx)` →
  `image_get_filename` + `class_get_element_class`; `mov $0x10` → `object_header_size` +
  `allocation_granularity` + `offset_of_array_bounds…`; `mov $1,%dl; jmp FromIl2CppType` →
  `class_from_il2cpp_type` + `class_from_type`.
- Store the alias list beside the address in the map artifact, so the next build's diff does not re-chase them.
- Corollary for whole-game tooling: no separate implementation exists to hook — an alias call lands in the
  same body, which is fine for `GetProcAddress` but matters when you reason about call graphs.

## 8. Working the MCP server at scale

- `get_pseudocode` is asynchronous; bursting ~10 requests trips the server's circuit breaker and stalls the
  tool for ~60 s. Queue 2–4, collect with `get_task_status`, then queue the next batch.
- `get_function_info` / `get_disassembly` / `rename_symbols_batch` are synchronous — cheap enough to use as
  the read-back proof that a rename landed (`get_function_info(<addr>)` shows the new name).
- Renames live in the open project only. `save_project` refuses while any tool call is in flight, and a
  killed session loses the pass, so either save between waves or tell the owner to hit save before closing.
  After a reload, spot-check a few addresses with `get_function_info` — a project that was reopened without
  saving shows the obfuscated names again.
