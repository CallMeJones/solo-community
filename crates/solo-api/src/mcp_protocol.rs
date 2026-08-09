// SPDX-License-Identifier: Apache-2.0

//! Shared MCP protocol-version constants for Solo transports.

/// Streamable HTTP request header defined by the MCP transport spec.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// Latest MCP spec version Solo intentionally advertises on HTTP.
///
/// Keep this explicit instead of inheriting it accidentally from the SDK so
/// HTTP header validation, initialize responses, and tests move together.
pub const MCP_PROTOCOL_VERSION_LATEST: &str = "2025-11-25";

/// Compatibility fallback from the Streamable HTTP transport spec.
pub const MCP_PROTOCOL_VERSION_2025_03_26: &str = "2025-03-26";

/// Protocol versions accepted by Solo's HTTP MCP transport.
pub const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &[MCP_PROTOCOL_VERSION_2025_03_26, MCP_PROTOCOL_VERSION_LATEST];

pub fn is_supported_protocol_version(version: &str) -> bool {
    MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}
