//! git interoperability on the server: object generation, the smart-HTTP
//! and SSH protocols for clone, fetch and push, cached clone packs, and
//! Git LFS.

pub mod http;
pub mod lfs;
pub mod objects;
pub mod pack;
pub mod packcache;
pub mod ssh;
