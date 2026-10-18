//! Shared core of Cairn: storage schema, query parsing, classification,
//! extraction (run inside sandboxed workers), vector store and IPC protocol.

pub mod chunk;
pub mod classify;
pub mod db;
pub mod extract;
pub mod ipc;
pub mod paths;
pub mod query;
pub mod sandbox;
pub mod timeutil;
pub mod vecstore;
