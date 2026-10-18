//! A Keycloak token verified against its realm's JWKS, kept current in the background (PF-45,
//! PF-46), behind the `auth` feature. The context-gateway and the functions runtime share it,
//! so neither links the other (T-4605).

pub mod jwks;
pub mod token;
