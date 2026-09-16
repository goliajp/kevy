//! The command reference: help text, argument hints and Tab completion,
//! all read from one model of a server's `COMMAND DOCS`.

pub(crate) mod help_text;
pub(crate) mod marks;
pub(crate) mod model;
mod query;
mod render;

#[cfg(test)]
mod tests;
