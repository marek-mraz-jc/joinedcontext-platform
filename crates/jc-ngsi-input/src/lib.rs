//! What an NGSI-LD write must carry before the gateway hands it to the broker, split out of
//! context-gateway in T-5323: the unit code of every quantity (`units`) and the JSON-LD context
//! a caller may send (`context_guard`). No I/O, no routes, no auth path: the gateway calls these
//! from its write and proxy handlers after its policy check.

pub mod context_guard;
pub mod units;
