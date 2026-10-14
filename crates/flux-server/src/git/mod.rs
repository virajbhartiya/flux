//! git interoperability on the server: object generation and (soon) the
//! smart-HTTP protocol for clone, fetch and push, with cached clone packs.

pub mod http;
pub mod objects;
pub mod pack;
pub mod packcache;
