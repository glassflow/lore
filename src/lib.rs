pub mod answer;
pub mod auth;
pub mod chunk;
pub mod cli;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod http;
pub mod llm;
pub mod mcp;
pub mod otlp;
pub mod repo;
pub mod search;
pub mod sessions;
pub mod store;
pub mod web;

/// The version lore reports: the release tag the binary was built for (`LORE_VERSION` at build
/// time, set by the release image build), or the crate version.
pub const VERSION: &str = match option_env!("LORE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_the_build_tag_or_the_crate_version() {
        assert!(!VERSION.is_empty());
        match option_env!("LORE_VERSION") {
            Some(v) if !v.is_empty() => assert_eq!(VERSION, v),
            _ => assert_eq!(VERSION, env!("CARGO_PKG_VERSION")),
        }
    }
}
