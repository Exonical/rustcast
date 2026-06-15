//! Protocol versioning.

/// Current protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Protocol identifier used in ALPN negotiation.
pub const PROTOCOL_ALPN: &[u8] = b"flux/1";

/// Minimum protocol version this build is compatible with.
pub const MIN_COMPATIBLE_VERSION: u32 = 1;

/// Check if a remote protocol version is compatible with this build.
pub fn is_compatible(remote_version: u32) -> bool {
    remote_version >= MIN_COMPATIBLE_VERSION && remote_version <= PROTOCOL_VERSION
}
