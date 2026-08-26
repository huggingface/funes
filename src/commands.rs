//! One module per command: what funes does when you run it. They orchestrate the layers below —
//! read sessions through [`crate::traces`], chunk and embed them, write to or read from a
//! [`crate::memory`], and present the result through [`crate::ui`] — and they are where the
//! decisions live. Every state they act on is one the layers below report; nothing here infers it.

pub mod ask;
pub mod curate;
pub mod index;
pub mod mcp;
pub mod push;
pub mod recall;
pub mod scrub;
pub mod sketch;
pub mod update;
