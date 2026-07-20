//! Template span pipeline (spec §6.2): lex → anchor → write-back → verify.

pub mod anchor;
pub mod lexer;
pub mod verify;
pub mod writeback;
