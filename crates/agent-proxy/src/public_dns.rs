//! The egress resolver: a host on the allow-list reaches the public internet and nothing else
//! (AG-65, T-1304). The guard is the platform's one outbound guard, [`jc_core::outbound`]
//! (T-4776): `reqwest` connects only to what [`PublicOnly`] returns, so no second DNS answer (a
//! rebinding) slips between the check and the connection.

pub use jc_core::outbound::{is_public, refused, PrivateAddress, PublicOnly};
