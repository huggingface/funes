//! The memory funes reads from and writes to: a local Lance directory, or a shared dataset on the
//! Hugging Face Hub.
//!
//! Three concerns live here, in layers. [`dataset`], [`fetch_store`] and [`capture_store`] are
//! mechanics — Lance and object stores, knowing nothing about the Hub. [`hf_dataset`] is transport:
//! the Hub commits an append or reindex lands. [`hub`] is the domain: what a memory is, what state
//! it's in, and how to open it. [`card`] and [`lock`] serve a published memory's dataset card and
//! the local writer lock.

pub mod capture_store;
pub mod card;
pub mod dataset;
pub mod fetch_store;
pub mod hf_dataset;
pub mod hub;
pub mod lock;
