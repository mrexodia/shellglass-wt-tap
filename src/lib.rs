//! Windows Terminal private-ABI render tap.
//!
//! Everything in this crate is hook/broker policy. Frame presentation is
//! delegated to the sibling `shellglass` library through `SourceSession`.

// Keep the migrated implementation's paths local while making the ownership
// boundary explicit: these modules are shellglass's stable library surface.
pub mod model {
    pub use shellglass::model::*;
}
pub mod proto {
    pub use shellglass::proto::*;
}
pub mod source {
    pub use shellglass::source::*;
}

#[cfg(feature = "accessibility")]
pub mod accessibility;
pub mod native_broker;
pub mod native_protocol;
#[cfg(windows)]
pub mod windows_native;
