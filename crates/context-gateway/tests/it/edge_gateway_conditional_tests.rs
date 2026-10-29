//! Edge cases of `pdp::conditional::state_dependent` (T-1908; R45, GW16): a grant decides from
//! the stored entity exactly when it narrows by filter or by area. The gateway's own conditional
//! write (`required`, `batch_refusal`, `matches`) went with its NGSI-LD pipeline (T-3644): the
//! broker's view compares the entity tag (T-3789). What is left reads the Relationship targets a
//! write names.

use context_gateway::pdp::conditional::state_dependent;
use context_gateway::pdp::evaluator::Constraints;

const AREA: &str = "georel=within;geometry=Polygon;coordinates=[[[21.0,48.0],[21.5,48.0],[21.5,48.5],[21.0,48.5],[21.0,48.0]]]";

fn plain() -> Constraints {
    Constraints::default()
}

fn filtered() -> Constraints {
    Constraints {
        q: Some("temperature>10".to_owned()),
        ..Constraints::default()
    }
}

fn in_an_area() -> Constraints {
    Constraints {
        geo_areas: vec![AREA.to_owned()],
        ..Constraints::default()
    }
}

#[test]
fn a_grant_is_state_dependent_when_it_narrows_by_filter_or_by_area_and_not_otherwise() {
    assert!(
        !state_dependent(&plain()),
        "a grant on types alone is decided from the payload"
    );
    assert!(state_dependent(&filtered()));
    assert!(state_dependent(&in_an_area()));

    // An empty filter string is still a filter the broker would apply, so it still decides from
    // the stored entity; treating it as absent would skip the read.
    let empty_filter = Constraints {
        q: Some(String::new()),
        ..Constraints::default()
    };
    assert!(state_dependent(&empty_filter));

    // An empty list of areas narrows nothing, which is the documented meaning of the field.
    let no_areas = Constraints {
        geo_areas: Vec::new(),
        ..Constraints::default()
    };
    assert!(!state_dependent(&no_areas));
}
