//! Terminal presentation for the commands: [`render`] formats the read verbs' results (human and
//! agent-shaped), [`tui`] is the in-process list+preview picker `curate` reviews sessions in, and
//! [`banner`] plays the wait animation `ask` runs behind a borrowed agent.

pub mod banner;
pub mod render;
pub mod tui;
