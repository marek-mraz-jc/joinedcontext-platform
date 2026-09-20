# Air quality observed

<!-- Generated from the LinkML source by Model Tools. Do not edit: `jcctl model
     generate` overwrites this file and CI fails on any difference (DM-01, DM-02). -->

One air quality reading of one station, as the demo instance publishes it.

- Namespace: `https://hel.fi/ns/air-quality`
- Rendered by: `linkml-1.11.1`
- License: https://creativecommons.org/licenses/by/4.0/

## Classes

### AirQualityObserved

One reading of one air quality station.

IRI: `hel:AirQualityObserved`

Specialises `Entity`.

| Attribute | NGSI-LD kind | Range | Required | Unit | IRI | Description |
|---|---|---|---|---|---|---|
| `dateObserved` | Property | `datetime` | yes |  | `hel:dateObserved` | When the reading was taken. |
| `pm25` | Property | `float` | yes | µg/m³ (unece:GQ, qudt-unit:MicroGM-PER-M3) | `hel:pm25` | Mass concentration of particles smaller than 2.5 micrometres. |
| `temperature` | Property | `float` |  | °C (unece:CEL, qudt-unit:DEG_C) | `hel:temperature` | Air temperature at the station. |
| `stationName` | LanguageProperty | `string` |  |  | `hel:stationName` | What the station is called, per locale. |
| `refStation` | Relationship | `string` |  |  | `hel:refStation` | The station that produced the reading. |
| `id` | Property | `string` | yes |  | `ngsi-ld:hasId` | The entity id, urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}. |
| `type` | Property | `string` | yes |  | `ngsi-ld:hasType` | The entity type, one class of a published data model. |
| `location` | GeoProperty | `string` |  |  | `geojson:geometry` | Where the entity is, as GeoJSON geometry. |
| `observedAt` | Property | `datetime` |  |  | `ngsi-ld:observedAt` | When the observation the entity reports was made. |

### Entity

The root every NGSI-LD entity class specialises.

IRI: `ngsi-ld:Entity`

| Attribute | NGSI-LD kind | Range | Required | Unit | IRI | Description |
|---|---|---|---|---|---|---|
| `id` | Property | `string` | yes |  | `ngsi-ld:hasId` | The entity id, urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}. |
| `type` | Property | `string` | yes |  | `ngsi-ld:hasType` | The entity type, one class of a published data model. |
| `location` | GeoProperty | `string` |  |  | `geojson:geometry` | Where the entity is, as GeoJSON geometry. |
| `observedAt` | Property | `datetime` |  |  | `ngsi-ld:observedAt` | When the observation the entity reports was made. |
