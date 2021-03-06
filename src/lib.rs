#![feature(rand, old_io, core, collections, hash)]

#![allow(unused_features)]
#![feature(test)]

extern crate url;
extern crate openssl;
extern crate "rustc-serialize" as rustc_serialize;
extern crate "sha1-hasher" as sha1;
extern crate rand;
#[macro_use] extern crate bitflags;

#[cfg(test)]
extern crate test;

pub use socket::WebSocket;
pub use message::{WSMessage, WSStatusCode};

pub mod nonce;
pub mod message;
pub mod stream;
pub mod socket;

