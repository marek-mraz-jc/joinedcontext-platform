# Domain-standard representations and the data model they need

Generated from `Representation::requirement()` (jc-core); a test fails when this file and the code differ. `UPDATE_SCHEMAS=1 cargo test -p jc-core --test endpoint_tests` rewrites it. An Endpoint may list a representation below only when its space's data model has the type with every mandatory source attribute as a required slot; `id`, `type` and `modifiedAt` belong to every entity (EP-125).

## `ical` (RFC 5545)

Type: `Event` (https://smartdatamodels.org/dataModel.Event/Event)

| Field | Section | Source attribute | Mandatory |
|---|---|---|---|
| `UID` | §3.6.1, §3.8.4.7 | `id` | yes |
| `DTSTAMP` | §3.6.1, §3.8.7.2 | `modifiedAt` | yes |
| `DTSTART` | §3.6.1, §3.8.2.4 | `startDate` | yes |
| `SUMMARY` | §3.8.1.12 | `name` | yes |
| `DTEND` | §3.8.2.2 | `endDate` | no |
| `DESCRIPTION` | §3.8.1.5 | `description` | no |
| `LOCATION` | §3.8.1.7 | `address` | no |
| `GEO` | §3.8.1.6 | `location` | no |
| `URL` | §3.8.4.6 | `url` | no |
