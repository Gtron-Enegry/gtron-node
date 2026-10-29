//! What a process that holds a TRON key needs to sign safely: it asks a node to build the
//! transaction ([`tron`]), decodes what came back and refuses unless it is exactly the
//! requested contract ([`txcheck`]), signs locally ([`keys`]), and records the signed
//! transaction before broadcasting it ([`store`]) so a retry re-sends it instead of
//! signing a second one. [`secretfile`] checks files that hold keys and tokens.
//!
//! Shared by the platform's signer and by gtron-node, which shops run next to their wallet.

pub mod keys;
pub mod secretfile;
pub mod store;
pub mod tron;
pub mod txcheck;
