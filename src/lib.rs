#![no_std]
#![allow(async_fn_in_trait)]

extern crate alloc;

pub mod adapters;
pub mod constants;
pub mod domain;
pub mod drivers;
pub mod inter_task;
pub mod ports;
pub mod proto;
pub mod tasks;
