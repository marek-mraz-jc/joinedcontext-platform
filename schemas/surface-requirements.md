# Domain-standard representations and their mappings

Generated from `jc_core::kinds::ical_mapping` (jc-core); a test fails when this file and the code differ. `UPDATE_SCHEMAS=1 cargo test -p jc-core --test it endpoint_tests::` rewrites it. An Endpoint that serves a representation below maps its fields from NGSI-LD attributes in `spec.representations.<name>`; `jcctl validate` and the Portal check the mapping against the space's data model (EP-125, API/02 §8b).

## `ical` (RFC 5545)

| Field | Section | Required | Source accepts | Remaps |
|---|---|---|---|---|
| `UID` | §3.6.1, §3.8.4.7 | yes, fixed: entity `id` | nothing: the platform sets it | none |
| `DTSTAMP` | §3.6.1, §3.8.7.2 | yes, fixed: entity `modifiedAt` | nothing: the platform sets it | none |
| `DTSTART` | §3.6.1, §3.8.2.4 | yes | date or datetime | `firstOf`, `date`, `timezone` |
| `SUMMARY` | §3.8.1.12 | yes | a string, an enum or a LanguageProperty | `firstOf`, `join`, `language`, `truncate` |
| `DTEND` | §3.8.2.2 | no | date or datetime | `firstOf`, `date`, `timezone`, `endInclusive` |
| `DURATION` | §3.8.2.5 | no | a string | `firstOf`, `constant` |
| `DESCRIPTION` | §3.8.1.5 | no | a string, an enum or a LanguageProperty | `firstOf`, `join`, `language`, `truncate` |
| `LOCATION` | §3.8.1.7 | no | a string, an enum, a LanguageProperty or a structured address | `firstOf`, `join`, `language` |
| `GEO` | §3.8.1.6 | no | a GeoProperty | `firstOf` |
| `URL` | §3.8.4.6 | no | uri | `firstOf` |
| `CATEGORIES` | §3.8.1.2 | no | a string, an enum or a LanguageProperty | `firstOf`, `join`, `constant`, `language`, `map` |
| `STATUS` | §3.8.1.11 | no | a string, an enum or a LanguageProperty | `firstOf`, `language`, `map` |
| `ORGANIZER` | §3.8.4.3 | no | uri, or an object with a uri `url` and a `name` | `firstOf` |
| `LAST-MODIFIED` | §3.8.7.3 | no | date or datetime | `firstOf`, `date` |
| `CLASS` | §3.8.1.3 | no | a constant | `constant` |

### Preset `sdm-event`

| Field | Source |
|---|---|
| `DTSTART` | `startDate` |
| `DTEND` | `endDate` |
| `SUMMARY` | `name` |
| `DESCRIPTION` | `description` |
| `LOCATION` | `address` |
| `GEO` | `location` |
| `URL` | `url` |
| `ORGANIZER` | `organizer` |

### Preset `schema-org`

| Field | Source |
|---|---|
| `DTSTART` | `startDate` |
| `DTEND` | `endDate` |
| `SUMMARY` | `name` |
| `DESCRIPTION` | `description` |
| `LOCATION` | `address` |
| `GEO` | `location` |
| `URL` | `url` |
| `STATUS` | `eventStatus`, mapped (10 values), default `CONFIRMED` |
| `ORGANIZER` | `organizer` |

## `sta` (OGC SensorThings API v1.1)

A fixed requirement: the space's model needs any type with a slot of numeric `range` that declares its `unit` (https://docs.ogc.org/is/18-088/18-088.html).

| Field | Section | Source attribute | Mandatory |
|---|---|---|---|
| `Thing` | §9.2.1 | `id` | yes |
| `Datastream` | §9.2.5 | `a numeric slot with its unit` | yes |
| `Datastream.unitOfMeasurement` | §9.2.5 | `a numeric slot with its unit` | yes |
| `Observation.result` | §9.2.7 | `a numeric slot with its unit` | yes |
| `Observation.phenomenonTime` | §9.2.7 | `observedAt` | yes |
| `ObservedProperty` | §9.2.4 | `type` | yes |
| `Location` | §9.2.2 | `location` | no |
