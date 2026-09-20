//! What leaves the platform on the gateway's own account (T-0156, R46).
//!
//! Everything else in this crate answers a request. A notification is the one thing the
//! platform sends without being asked, so it is the one place where the enforcement point
//! has to run on the way out rather than on the way in.

pub mod notifications;
