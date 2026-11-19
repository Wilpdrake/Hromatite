# Resident hot reload under Wine/Proton

Use this for replacing a feature DLL without relaunching an IL2CPP game whose hook already runs in-process. Split the resident GC/tick/VEH/logging facilities from the versioned feature module. Do not load another full copy of the runtime.

## Architecture and ABI

- Keep the existing game-thread tick and GC guard permanently installed; feature routing is the only part swapped. No second injector, `proton run`, polling thread or duplicate VEH/guard should exist.
- Use a `#[repr(C)]` versioned function table. Borrow POD buffers with explicit lengths for one synchronous call only. Never pass `String`, `Vec`, trait objects, references to static runtime objects, or allocator ownership across the image boundary.
- Copy the host's values and call its guarded services. A feature must not install a panic hook, `catch_unwind`, static TLS, or its own exception handler. The resident guards every lifecycle function.
- Lifecycle calls must be serialized on the already proven game thread. No lock may remain held across a guarded call: a VEH longjmp abandons locals without running their destructors. Generation caches reset on replacement; do not preserve raw il2cpp object pointers.

## Mapper rules

- Manual-map locally on the existing tick, not through `CreateRemoteThread`. Resolve relocs/imports/TLS and the ABI export **before** erasing PE headers. Keep `RW -> RX`; never leave RWX.
- **Keep erased PE headers readable and writable (zeroed `RW`), not `PAGE_NOACCESS`.** Mingw/Rust CRT startup may read its image base: NOACCESS on erased headers caused a real DllMain attach AV under Wine. Explicit RW is compatible with the already-proven remote mapper.
- Resolve ordinary imports only through the named DLL or its PE export table. Resort to an all-module export search for `api-ms-*` names when the apiset fake image is absent; support ordinals and forwarders without treating an intra-export-directory RVA as code. **Never call `LoadLibraryW` during tick/reload**: a missing dependency fails the canary instead of acquiring the OS loader lock on the game thread.
- Validate the digest over the same in-memory PE snapshot passed to the mapper; hashing one file read and mapping a second read creates a TOCTOU identity mismatch.
- If matching a build hash, hash the actual loaded/on-disk `GameAssembly.dll`, not just two compile-time constants. A streaming hasher avoids pulling the entire file into page cache.
- Reload transitions must call the real game callback before mapping/init/shutdown. Preserve ordinary pre-original telemetry otherwise, and require the original exactly once, including error paths.
- Check phase deadlines after the whole canary returns, not just before it starts. A weak deadline cannot preempt a blocking foreign call or loader; keep known blocking operations out of the tick instead.
- Track how many TLS callbacks actually returned. Retire completed callbacks in reverse order, then `DllMain(PROCESS_DETACH)`, `VirtualFree`, and owned dependency references. A callback that faulted must not be re-entered for detach.
- No automatic `Drop` unmap. The owner must explicitly retire the image after `shutdown` and drain. If shutdown faults, preserve the failed image as quarantined rather than executing unmapped or half-detached code. Limited lifetime partial mappings are preferable to dangling callers.
- This local mapper is deliberately not a general Windows loader: no loader-managed static-TLS index, thread notifications, SEH registration, or loader-lock work. Do not create a worker inside the feature.

## Transaction

Retain at most active + rollback previous + one pending + one failed image. Validate SHA-256/ABI/game-build compatibility before mapping.

1. Reserve a unique immutable generation. Never overwrite a loaded filename.
2. Reject duplicate/concurrent requests; stop admitting new feature calls.
3. Drain in-flight calls across ticks, with a bounded wall-clock budget.
4. Map and validate the new API table.
5. Run guarded init, configuration, and a cheap canary while the current generation remains callable.
6. Atomically publish the new generation, retaining old current for rollback.
7. Only after grace and `in_flight == 0`, shut down and retire older previous/failed images. If the new tick fails, route back to previous; if none exists, restore the established resident fallback.

The pure model and the real image storage must change in matching transitions. In particular, move the failed/current image into `failed` *before* listing it as active previous, retain pending on canary error, and do not retire previous before `canary_succeeded` permits it. A status counter alone cannot prove quiescence; use it with the state transitions.

## Verification

- Test state transitions independently: full swap, duplicate requests, pending deadline, in-flight drain, failed canary, failed current rollback, and retirement timing.
- Exercise the real DLL under a scratch Proton prefix with the local loader: `init -> canary -> shutdown/retire` repeated A/B. Check actual generation markers returned by the feature, not synthetic text.
- Include malformed PE and header-access regression tests. A successful release link does not prove loader behavior under Wine.
- Preserve established resident console/GC/LateUpdate commands while a feature is active. Resident commands must not be handed to replaceable code.
- For first live acceptance the resident needs one normal injection; an old running payload cannot learn this protocol from console. Do not restart the game for other generations.

## Measured implementation notes (Hromatite, Unity 6 VRChat)

- **A `#[cfg(target_os = "windows")] mod imp` is invisible to host tests.** They type-check only the portable half, so a `?` on an `Option` inside a `Result` function compiled on the host and failed only in `cargo build --workspace --release`. Gate every edit that touches the Windows-only runtime on the real target build, not on `cargo test` alone.
- Make generation ids canonical decimal everywhere at once: the console parser, the `features-N-SHA12.dll` filename, the published manifest and the publisher's `--generation`. Leading zeros (`09`) and an empty flag value must fail before the game hash is read, before Cargo runs and before any staging file exists; validate the flag in the option parser, otherwise `[ -n "$GEN" ]` silently treats `--generation ""` as "not provided".
- Generation `0` is the bootstrap sentinel loaded from `HROMATITE_FEATURE_DLL` at the first tick; published generations start at `1`, so the model must let version `0` load with no active generation while the command path rejects `reload 0`.
- The whole swap (drain → map → init → configure → canary `feature` → publish → grace → retire) runs inside one existing frame tick, default 250 ms. There is no user-visible drain window: a command arriving mid-swap either reaches the old image or falls through to the built-in console for that one tick.
- Feature images are **private anonymous RW→RX** with erased-but-readable headers. That is the mapper signature an anti-cheat looks for; the file-backed `NtCreateSection`/`SEC_IMAGE` view is the alternative, and it is a separate phase, not a tweak.
- The host publisher must hash the real `GameAssembly.dll` and refuse a mismatch *before* building, and the manifest stores the **Linux** host path (`Z:` is applied game-side). Verified under wine: four generations mapped at low addresses (`0xa40000`/`0xb30000`), per-generation `feature: generation=N abi=1` canary, clean `shutdown/retire`, malformed PE contained as an error rather than an AV.
