//! Minimal access to the verified corlib image for the active game build.
//!
//! The historical name `find_domain` is retained for callers, but Unity's
//! `il2cpp_get_corlib` returns an `Il2CppImage*`, not an `Il2CppDomain*`.

#![deny(unsafe_op_in_unsafe_fn)]

/// `il2cpp_get_corlib` (`wYRnnMQuvvH`). Moved by the 2026-11-17 patch; the new
/// value comes from aligning the export block of both builds (182 of 184 gaps
/// identical, two anchors — `il2cpp_gchandle_get_target` and
/// `il2cpp_native_stack_trace` — map onto themselves).
const CORLIB_IMAGE_GETTER_RVA: u32 = 0x00c2_9c70;

fn corlib_getter_addr(base: usize, size: usize) -> Option<usize> {
    let address = base.checked_add(CORLIB_IMAGE_GETTER_RVA as usize)?;
    let end = base.checked_add(size)?;
    (address.checked_add(8)? <= end).then_some(address)
}

/// Return the corlib image through the build-verified no-argument export.
///
/// Compatibility helper: older callers use the historical `find_domain` name
/// and pass the result to `m3::image_of` to locate neighbouring images.
pub fn find_domain() -> Option<usize> {
    let (base, size) = crate::procs::own_module(crate::GAME_ASSEMBLY)?;
    let getter = corlib_getter_addr(base, size)?;
    // SAFETY: the RVA identifies a verified no-argument il2cpp_get_corlib
    // export for this GameAssembly build; VEH still contains a stale mapping.
    let image = unsafe { crate::veh::call0(getter) }.ok()?;
    (image >= 0x10000 && crate::safe::readable_at(image)).then_some(image)
}

#[cfg(test)]
mod accessor_tests {
    use super::{CORLIB_IMAGE_GETTER_RVA, corlib_getter_addr};

    #[test]
    fn corlib_accessor_uses_the_verified_export_within_the_image() {
        assert_eq!(
            corlib_getter_addr(0, 0x0100_0000),
            Some(CORLIB_IMAGE_GETTER_RVA as usize)
        );
        assert_eq!(corlib_getter_addr(0, 0x1000), None);
        assert_eq!(corlib_getter_addr(usize::MAX, 0x0100_0000), None);
    }
}
