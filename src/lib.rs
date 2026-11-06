//! HubbCap: version control that gets out of your way.
//!
//! See the design doc for the model. In short: `cap done` saves and shares
//! everything, and when two people change the same file the hub keeps both
//! versions (a superposition) instead of making anyone merge first.

#![forbid(unsafe_code)]

pub mod fsutil;
pub mod hash;
pub mod hub;
pub mod model;
pub mod ops;
pub mod store;
pub mod workcopy;
