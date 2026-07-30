//! Registration point for every connector. This is the **one** file (besides
//! the new connector's own file) that changes when a provider is added — see
//! `crate::connector` for the interface contract.

use crate::connector::{Connector, Registry};

// M1 will add, e.g.:
//   mod claude_code;
//   mod codex_cli;
// and register them below. Left empty until M1 lands so the interface can be
// reviewed on its own first.

pub fn all() -> Registry {
    let connectors: Vec<Box<dyn Connector>> = vec![
        // Box::new(claude_code::ClaudeCodeConnector::default()),
        // Box::new(codex_cli::CodexCliConnector::default()),
    ];
    Registry::new(connectors)
}
