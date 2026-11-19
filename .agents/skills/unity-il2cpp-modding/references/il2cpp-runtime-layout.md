# il2cpp runtime structures (Unity 6, measured in-process)

Offsets for exploring a live IL2CPP game from inside the process, when the managed metadata on disk is
encrypted and no offline dumper works. Everything here was read from a running Unity 6000.0.x VRChat
build (GameAssembly sha256 `648f61b1…`); treat the offsets as "this shape on this build", re-measure
after a game update, but the **order of operations** is build-independent.

Correction to earlier notes: the no-argument getter you find first is **`il2cpp_get_corlib`**, not
`il2cpp_domain_get`. Whatever it returns is the corlib `Il2CppImage`; the "assembly list" behind it is
the runtime's contiguous `Il2CppImage[]`, not `Il2CppAssembly**`. Every conclusion below follows from
that one fix.

## 1. Getter → corlib image

Every export name has an RVA in the generated symbol map, so every export is callable even when you
cannot name it. Filter by *compile-time shape*: singleton getters compile to an 8-byte thunk

```
48 8B 05 <disp32> C3      mov rax,[rip+disp32]; ret     (or 48 8D 05 = lea)
```

Measured: exactly 3 of 251 exports had that shape. Call each with no arguments, twice, and dump the
return plus one level of indirection. The useful one returns a heap pointer whose `+0x08` is a printable
name and whose following 0x48-strided siblings are more names (`mscorlib`, `DataModel`,
`UnityEngine.CoreModule`, …). That is `il2cpp_get_corlib` and image[0].

Proof that it is *not* the domain: `il2cpp_domain_get_assemblies(domain, &count)` writes 216, which
matches the image count, but the pointer returned by the getter is an image (its `+0x00` is an
`Il2CppAssembly*` whose own `+0x08` points back at it), and `il2cpp_image_get_class(image, index)`
accepts it. A getter is "domain" only if an identified `domain_get_assemblies` agrees with it.

## 2. The image array (stride 0x48)

Images are laid out back to back with stride 0x48, in assembly-table order (216 of them on the measured
build) — but **the getter's return is not the first image**. It is the *corlib* image, and other
assemblies, including the game's own `Assembly-CSharp`, sit a few entries **before** it (measured:
`Assembly-CSharp` at `0x3027c0a0`, `mscorlib` at `0x3027c1c0`, i.e. −4 × 0x48). So walk the stride in
both directions from the getter's pointer, with a sanity stop on the first non-nameable slot:

```rust
for step in 0i64..768 { for sign in [1, -1] { let img = corlib + sign*step*0x48; /* read name at +0x08 */ } }
```

Match **exact short names first, prefixes only as a fallback**: `Assembly-CSharp` is a prefix of
`Assembly-CSharp-firstpass`, and a prefix-first search silently resolves to the wrong image (its class
indices then belong to another assembly, so `image_get_class` returns plausible-looking classes with the
wrong names).

```
+0x00  Il2CppAssembly*            (assembly record; its +0x08 points back to this image)
+0x08  const char*  short name    ("UnityEngine.CoreModule")
+0x10  u32 typeStart, u32 typeCount         (measured 0 / 0xce1 for CoreModule)
+0x18  u32 exportedTypeStart, u32 exportedTypeCount
+0x20  pointer into the per-image metadata
+0x28  pointer (heap)
+0x30  const Il2CppCodeGenModule*  (moduleName at +0x00, methodPointerCount at +0x08,
                                    methodPointers at +0x10, adjustorThunkCount at +0x18, …)
+0x38  u64 = 1
+0x40  const char*  name with extension ("UnityEngine.CoreModule.dll")
```

`record + 0x28` inside an older "assembly record" view is this `codegenModule`, **not** the image — the
image is the record base. That single off-by-0x28 mistake produced several sessions of
"`class_from_name(image)` returns structures without strings".

## 3. Class table — enumerate without calling anything

`s_TypeInfoDefinitionTable` is a plain `Il2CppClass*[]` in GameAssembly `.data`, indexed by
`typeDefinitionIndex`, lazily filled (NULL until a type is materialised). Find the global from the
indexing code, not from its bytes:

```
mov rax,[rip+disp]     ; s_TypeInfoDefinitionTable  (measured .data RVA 0xe66c870)
lea rsi,[rax+rdi*8]    ; rdi = typeDefinitionIndex
```

the neighbouring `MetadataCache::GetTypeInfoFrom…` also shows `mov eax,[reg+0xe4]; sub eax,0x24` and
`imul … ,0x58` — `0x58` = `sizeof(Il2CppTypeDefinition)`, which is how you know the index is a type
definition and not a handle.

Measured layouts (Unity 6000.0.67f1 — differs from the published 2020 headers):

```
Il2CppClass  +0x00  const Il2CppImage*
             +0x18  const char* namespaze
             +0x50  const MethodInfo**   (array of POINTERS, ends at a null slot)
             +0x80  FieldInfo[]          (inline records)
             +0x98  const char* name
             +0x124 uint16 fieldsCount
FieldInfo    stride 0x28
             +0x00  const Il2CppType*   +0x08 uint32 token   +0x10 const char* name
             +0x18  uint32 offset/index  +0x20 Il2CppClass* parent
MethodInfo   +0x00  methodPointer      +0x18 const char* name
```

Without bias: `name` is at `+0x98`, not `+0x10`/`+0x18` — a class-shaped record whose `+0x10` is a
pointer and `+0x18` is `"UnityEngine"` is the same struct seen from a different base. Validate a sweep
by requiring BOTH a printable name and a namespace, then confirm one hit by an independent route (call a
method pointer, or compare against what `il2cpp_image_get_class(image, i)` returns at the same index).

Cost: ~38 k classes in ~0.3 s over `/proc/<pid>/mem`, ~2 s from inside the game. No calls, so it is safe
from any thread, including a frame detour.

**The table is sparse because classes are built on demand** — an untouched type is a NULL slot, so a
NUL scan can miss the very class you need (game-specific classes are often cold). `il2cpp_image_get_class(image, i)`
*creates* the class as a side effect: sweeping `i` over an image materialises its whole type list
(measured: 37 778 classes of `Assembly-CSharp` in ~1 s from inside the game, 0 faults). Do this once at
the start of a recon session, then all later lookups are plain table reads.

**Parent is `+0x10`** on this build (measured chain `Camera → Behaviour → Component → Object →
System.Object`). Use it, do not guess: an inherited member is simply absent from the derived class's
`+0x50` array (there is no `get_transform` in `Camera`'s 407 methods — it lives on `Component`), so walk
the chain and pick the first class whose method array contains the name.

## 4. Identifying `class_from_name` / `image_get_class` by shape

Both are thin wrappers, so their bodies identify them; stub order does not (it drifts by one mid-list).

```
image_get_class(image, index):
  mov rax,[rcx+0x20]   ; per-image metadata handle
  mov r8d,[rax]        ; typeStart
  add r8d,edx          ; + index
  imul rcx,rax,0x58    ; sizeof(Il2CppTypeDefinition)
  jmp  <MetadataCache getter>

class_get_fields(klass, iter):
  mov rbx,rcx / mov rdi,rdx ; if (*iter == 0) … cmpw $0,0x124(%rbx) ; mov rax,[rbx+0x80]
```

Do not be fooled by look-alikes: `call <alloc>; copy (2*index) bytes from arg1` is
`il2cpp_string_new_utf16(str, len)`, and `mov rax,[rcx]` / `mov rax,[rcx+8]` are name/counter getters
whose returns cannot be told apart without resolving the strings.

Verification that is worth the two minutes: call `image_get_class` at the index your sweep found for a
known class and require the same pointer; then call a `get_main`-style `methodPointer` and check that the
returned object's first qword equals that class.

## 4b. Calling convention for the methods you find (x64, measured)

`MethodInfo.methodPointer` is the compiled native body, so call it as C, not as a wrapper:

| Shape | Registers | Rust call |
|---|---|---|
| static, no args (`Camera.get_main`) | — | `call0(f)` |
| instance getter (`Component.get_transform`) | `rcx` = `this` | `call1(f, obj)` |
| instance `void` method with args | `rcx` = `this`, `rdx`/`r8` = args | `call2/call3(f, obj, …)` |
| method returning a **struct** (`Transform.get_position` → `Vector3`) | `rcx` = hidden output pointer, `rdx` = `this` | `call2(f, buf_ptr, obj)` |

Consequences, both measured:

- A struct-returning getter called as `call1(f, this)` faults (`0xC0000005`) because the callee reads
  `rdx` (NULL) — the `this` you passed in `rcx` was taken as the destination. Never conclude "the method
  pointer is wrong" from that fault; fix the argument order.
- A 12-byte `Vector3` is written through the out pointer; read three `f32` from your own stack buffer.
  Sanity-check the values (`localPosition` near 0 while `position` is metres away is normal for a camera
  parented to a player) before believing a layout.

Struct-returning calls are exactly how reads become *usable*: `Camera.main` → `Component.get_transform`
→ `Transform.get_position` gives the live camera, and the same three calls with `VRCPlayerApi` give a
player's position (see §7b).

## 5. Guarded walker (copy this shape)

```rust
fn readable(a: usize) -> bool {                 // committed + no PAGE_NOACCESS + no PAGE_GUARD
    /* VirtualQuery, check State==MEM_COMMIT and (Protect & 0xFF) */
}
// per iteration, BEFORE every raw read
if !readable(p) { break; }
let q = unsafe { *(p as *const u64) };
```

An unguarded word read one stride past the last valid entry faults inside the injected thread and the
whole game exits; the injector then reports the *next* step failing (e.g. "0 pages XORed, N failed"),
which looks like a completely different bug.

## 6. Host-side reading of `/proc/<pid>/mem` (fastest recon loop)

With `ptrace_scope` 0 (or the same uid and a dumpable process) the file opens `rb` and every structure
above can be explored from the host, without injecting anything and without touching the game:

```python
mem = open(f'/proc/{pid}/mem', 'rb', 0)
def rd(a, n):                     # /proc reads can be short or EIO on a racing page
    mem.seek(a); out = b''
    while len(out) < n:
        d = mem.read(n - len(out))
        if not d: break
        out += d
    return out
```

Reopen the handle after an `EIO` — a single failed read leaves the buffered file object unusable and
every later read returns short, which surfaces as a `struct.error` on `unpack`, not as a read error, and
sends you looking for a layout bug that is not there.

Base + offsets: get `GameAssembly.dll`'s base from `/proc/<pid>/maps` and double-check it against content
(the export-name string at its RVA, or `gc_threads − 0xe6bb180`); wine maps the PE header as a 4 KB stub
and the body in anonymous regions, so "the region starting with MZ" is the wrong base.

## 7a. Runtime containers you will meet while reading values

```
Il2CppString   +0x10 uint32 length        +0x14 UTF-16 chars      (no .NET header games needed)
Il2CppArray    +0x18 uintptr length       +0x20 elements          (klass @+0x00)
List<T>        +0x10 Il2CppArray* _items  +0x18 int32 _size       (NOT +0x18 length: that is the array's)
```

An SDK call typed `T[]` in C# is often a `List<T>` at runtime; read `_items` and `_size` from the list,
then `length` and the element base from the array. Reading `+0x18` of the *list* as a length gives a
plausible small number on a good day and garbage otherwise.

## 7b. Prefer the game's *public SDK* surface over its own class names

Obfuscation renames the game's own classes but usually leaves the modding SDK untouched. Measured on a
BeeByte-protected Unity 6 game: 37 778 types in `Assembly-CSharp`, not one of them named `Player` or
`PlayerManager`, while `VRC.SDKBase.VRCPlayerApi` (assembly `VRCSDKBase`) exposes the whole player model
under normal names — `get_AllPlayers` (a `List<VRCPlayerApi>`), `GetPlayerCount`, `GetPlayerId(player)`,
`GetPosition(player)`, `GetPlayerObjects(player)`, `IsValid`.

Recipe that worked end-to-end (count, then per player id + position):

1. `class_or_materialise(ns, name, assembly)` — table lookup first, and if NULL sweep
   `image_get_class(image_of(assembly), i)` until the namespace+name match (this is what makes cold SDK
   classes usable).
2. Call the static accessor, read `List<T>` → `_items`/`_size` → array `length`/elements @+0x20.
3. Per element call the accessors with the conventions of §4b.
4. Cross-check one data point against an independent path (camera world position vs. player position:
   the eye sits ~1.2–1.7 m above the player) — that is the cheapest proof that both paths read the same
   scene and that your struct-return buffer is not stale.

## 7. Exploration channel that makes this cheap

Every offset above was found with a handful of commands from the host (`m3i`, `m3f <ns> [name]`,
`m3c <class>`, `q <addr> [n]`, `find <hex64>`, `findb <text>`) against a payload that stays alive and
polls a command file. Without that channel each hypothesis costs a rebuild + a game restart.

`find`/`findb` results are valid only in the instant they are produced — the game keeps allocating, so a
follow-up `q` a second later can show recycled bytes. Follow up immediately, or re-run the search.
