---
sidebar_position: 4
title: Data Modeling & The LinkML Editor
description: Authoring, importing, adapting, and versioning data models using LinkML, model projections, and mappings.
---

# Data Modeling & The LinkML Editor

In the joinedcontext platform, all data structure contracts are modeled using LinkML (Linked Open Data Modeling Language). This guide explains how to import standard Smart Data Models, author custom classes and slots, configure endpoint projections, build mappings, and govern schema versions. It is written for data architects, stewards, and integration engineers.

Every change to a data model follows the standard change flow: you edit the model, click **Check**, click **Save model**, and an approver reviews and merges the proposal in Approvals before schemas are compiled.

## 1. LinkML Models and Schema Evolution

LinkML provides a single source of truth for entity definitions. From one model definition, the platform derives:

- Strict JSON Schema draft-07 schemas for API payload validation.
- Standard JSON-LD `@context` files for semantic interoperability.
- SHACL shapes, OWL and RDF renderings, documentation pages and one validated example entity, served beside the schema on every endpoint that publishes the model.

Schema changes are classified by their impact:

- **Additive change:** Adding optional slots or classes routes through the Yellow lane.
- **Breaking change:** Renaming slots, changing types, or removing properties raises the version major, requires explicit confirmation, and routes through the Red lane.

## 2. Importing a Data Model

### Import a Smart Data Model

#### By hand

1. Navigate to **Data models** at `/projects/helsinki/models`.
2. Select the **Import** tab.
3. Under **Smart Data Models catalogue**, type `WeatherObserved` in **Search models and attributes**.
4. Select `WeatherObserved` from the list.
5. In the preview panel, review the attributes. Click **Only required** to retain mandatory slots, or select specific attributes. Unchecked upstream slots are marked deprecated so partner payloads validate.
6. In **Space for this model**, pick `helsinki`.
7. Click **Import WeatherObserved**. The source loads directly into the editor.
8. In the **Editor** tab, click **Check**. You should see the severity chip confirm the update.
9. Click **Save model**. An approver reviews and merges the proposal in Approvals.

#### By asking the assistant

Type into the assistant composer:
`Import the WeatherObserved model from Smart Data Models into the helsinki project`

The assistant fetches the model, configures the draft, and opens `/projects/helsinki/models` with the editor populated. You click **Check**, click **Save model**, and an approver approves the change.

### Infer a Model from a File

#### By hand

1. On `/projects/helsinki/models`, open the **Import** tab.
2. Under **Model from a file**, drop a CSV or JSON sample file into the upload zone.
3. You should see the preview dialog displaying inferred classes, columns, detected data types, and catalogue matches.
4. Click **Populate the editor**.
5. In the **Editor** tab, enter `Name` as `city-weather` and select `Space` as `helsinki`.
6. Click **Check**, then click **Save model**. An approver merges the change in Approvals.

#### By asking the assistant

Attach a CSV or JSON file in the assistant panel and type:
`Draft a data model from this sample file.`

The assistant analyzes the sample structure, populates the LinkML editor, and presents the draft at `/projects/helsinki/models`.

## 3. Editing Classes and Slots

### Author Attributes in the Visual Editor

#### By hand

1. On `/projects/helsinki/models`, select the **Editor** tab and switch to the **Structure** view.
2. Under **Classes**, select `WeatherObserved` or type a name into **New class** and click **Add**.
3. Under **Slots**, click a slot to edit, or enter a name in **New slot** and click **Add**.
4. In the slot detail panel, configure:
   - **Range:** Choose `float`, `string`, `integer`, or an enum.
   - **NGSI-LD kind:** Choose `Property`, `Relationship`, or `GeoProperty`.
   - **Unit:** Select a UN/CEFACT unit code (such as `CEL` for Celsius or `MTS` for meters per second).
   - **Required:** Check if payloads must include this attribute.
5. Switch to the **Source** view to read and edit the model's own YAML, with its problems marked as you type.
6. Switch to the **Preview** view and select tabs to inspect generated JSON Schema, `@context`, documentation, and sample normalized entities.
7. Click **Check**, then click **Save model**. An approver merges the change.

#### By asking the assistant

Type into the assistant composer:
`Add an optional float slot called airQualityIndex to the WeatherObserved class`

The assistant applies the operation to the model draft and refreshes the editor view. You click **Check** and **Save model** to propose the change.

## 4. Configuring Model Projections for Endpoints

### Expose a Model Subset on an Endpoint

#### By hand

1. Navigate to `/projects/helsinki/endpoints` and click **New endpoint** (or open an existing endpoint).
2. Under **Model projection**, choose **Draw a new one** or select an existing projection.
3. Under classes, check the entity type to expose (such as `BikeHireDockingStation`).
4. Tick the individual slots to expose (such as `availableBikeNumber`). The identity slots `id` and `type` remain locked.
5. If the endpoint accepts data writes, check **Writable** and enter query constraints.
6. Click **Check**, then click **Propose change**.
7. In `/projects/helsinki/approvals`, an approver approves the proposal.

#### By asking the assistant

Type into the assistant composer:
`Create a projection for endpoint helsinki-bikes exposing only id, type, and availableBikeNumber`

The assistant selects the requested classes and slots, generates the projection manifest, and fills the form ready for your check and proposal.

## 5. Mappings Between Models

### Create a Schema Transformation Mapping

#### By hand

1. In `/projects/helsinki/models`, select the **Mappings** tab.
2. Under **Models**, select the source model in **Source model** and target in **Target model**.
3. Review the slot alignment table. For unmapped slots, choose an attribute from **Source slot** or configure a unit conversion in **Unit conversion**.
4. In **Golden test**, paste a sample JSON object into **Input example (JSON)**.
5. You should see **Transformed output** render the resulting transformed entity according to your rules.
6. Click **Propose the mapping**.
7. In `/projects/helsinki/approvals`, an approver reviews the diff and golden test files, then approves the change.

#### By asking the assistant

The assistant cannot automatically verify and propose schema mappings without operator review. Configure and verify mapping transformations manually in the **Mappings** tab.

### Edit a Mapping with the Form

The slot-by-slot rules stay on the **Mappings** tab. The form holds everything around them: which models the mapping connects, its golden tests and any code you write by hand. Every field says what it is for beside it, in your language.

#### By hand

1. Open `/projects/banskabystrica/mappings`, open the row menu of the mapping, for example `ovzdusie-to-airqualityobserved`, and click **Edit**.
2. Under **Between which models**, **Name** and **Context space** are fixed: a different name would be a new mapping. Under **Source model** and **Target model**, set **Data model** to the published name, such as `ovzdusie-senzory` and `airqualityobserved`, and **Major version** to a whole number such as `1`. The target cannot be the source at the same version.
3. Under **Golden tests**, each row pairs an **Input example** with the **Expected output** it must produce, as paths beside the mapping, for example `./tests/ovzdusie-to-airqualityobserved.input.json`. The check compares each pair, so a rule change that breaks one shows up before anyone approves it.
4. Under **Native blocks**, add code only when a rule cannot say what you need. Pick the **Target slot** it fills, such as `pm10`, the **Language**, and write the **Code**, for example `root = this.pm10_raw.number() / 10`. Hand-written code always goes to a stricter review, because no check can read it.
5. Leave **Compiled artifacts** as they are: the compiler writes those files for you.
6. Click **Propose change**. You should see a notice with a link to the proposal; an approver decides it in `/projects/banskabystrica/approvals`.

## 6. Deciding a Model Version's Lifecycle

A model's classes and attributes come from the LinkML editor. Two things about a version come from you: where it is in its life, and whether its entities may carry attributes the model does not declare.

### Change a Version's Lifecycle with the Form

#### By hand

1. Open `/projects/banskabystrica/datamodels`, open the row menu of the model, for example `ovzdusie-senzory`, and click **Edit**.
2. Under **The model version**, read **Name**, **Context space**, **Version** (such as `1.2.0`) and **LinkML source** (such as `ovzdusie-senzory.linkml.yaml`). You change these in the LinkML editor on the **Models** page, not here.
3. Under **Your decisions**, choose the **Lifecycle**. A draft may still change. A published version is fixed, so its readers can rely on it. A deprecated version still answers but warns every reader, and a retired one answers no one.
4. Switch **Accept undeclared attributes** on only when the space has to accept attributes the model does not name yet. Switched off, an entity carrying such an attribute is refused.
5. Click **Propose change**. The approver sees exactly these two decisions in the proposal.

## Related

- [Organizations, Projects & Context Spaces](./02-organizations-projects-spaces.md): managing data partitions.
- [Pipelines & Ingestion](./04-pipelines.md): using mappings in data transformation streams.
- [Endpoints & Sharing](./05-endpoints-and-sharing.md): publishing projected model schemas.
- [Users, Roles & Approvals](./07-users-roles-approvals.md): reviewing model change proposals.
