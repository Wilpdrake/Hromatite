//! Hot-reload: the shared pure contract plus the resident router.
//!
//! The generation state machine, configuration parsing and the wire formats
//! live in `hromatite-abi::hot` - the same code the host console and the
//! feature image compile, so producer and consumer cannot drift apart again.
//! [`router`] is the Windows side that maps images, runs canaries and retires
//! generations; it carries its `unsafe` in the image-mapping module.

pub use hromatite_abi::hot::*;

pub mod router;
