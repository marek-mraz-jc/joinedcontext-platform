---
sidebar_position: 5
title: Pipelines & Ingestion
description: Deploying, monitoring, and testing data ingestion pipelines, connecting data sources, and instantiating blueprints.
---

# Pipelines & Ingestion

A pipeline reads a data source and writes entities into a context space. This guide explains how to declare a data source, build the pipeline that reads it, test the mapping on a sample before anything is proposed, set up a whole feed from the flow gallery, and see whether a pipeline is running. It is written for data stewards, integration engineers and pipeline operators.

Every pipeline change follows the change approval flow: you configure the source or pipeline, test the mapping or probe the feed, click **Propose change**, and an approver reviews and merges the proposal in Approvals before execution begins.

## 1. Execution Classes and Change Flow

A pipeline runs in one of two ways, and **Execution** decides which:

- `resident` runs continuously: a feed that pushes (an MQTT broker, a WebSocket) or a **Period**
  shorter than 30 seconds.
- `scheduled` runs and finishes: a **Period** of 30 seconds or more, or a **Schedule** written as
  five cron fields such as `10 0 * * *`. Between runs it costs nothing.
- `auto` is the third choice and the easiest: the platform reads the period and picks `resident`
  under 30 seconds, `scheduled` at 30 seconds and above.

## 2. Connecting an External Data Source

### Add a Data Source Connection

#### By hand

1. Navigate to **Data sources** at `/projects/helsinki/datasources`.
2. In **Type**, choose the connection type (such as `HTTP endpoint` or `MQTT broker`).
3. Click **New data source**.
4. In **Name**, enter `hsl-citybikes-free`.
5. In **URL**, enter `https://gbfs.theta.fifteen.eu/gbfs/2.2/helsinki/en/free_bike_status.json`.
6. Set **Timeout** to `15s`. If authentication is required, select credential references in **Credentials** (secret references take a secret name and key; raw values are never stored).
7. Click **Check**. You should see **Planned change** and the probe results displaying fetched records and byte counts.
8. Click **Propose change**.
9. In `/projects/helsinki/approvals`, an approver clicks **Approve**.

The live journey `load.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Add an HTTP data source called hsl-citybikes-free that reads https://gbfs.theta.fifteen.eu/gbfs/2.2/helsinki/en/free_bike_status.json`

The assistant opens the creation dialog on `/projects/helsinki/datasources` with the URL pre-filled. You click **Check**, verify the probe report, and click **Propose change**. An approver merges the proposal.

The live journey `assistant-creates.spec.ts` replays these steps.

## 3. Building and Testing Pipelines in the Studio

### Build and Test an Ingestion Pipeline

#### By hand

1. Navigate to **Pipelines** at `/projects/helsinki/pipelines`.
2. Click **New pipeline**.
3. Under **1. Source**, choose **Read from** as `A data source of this project (external feed)` and pick `hsl-citybikes-free`.
4. In **Pipeline**, enter `citybikes-free` and set **Period** to `60s`.
5. Set **Target endpoint** to `helsinki-all`.
6. In **Compute**, choose **Kind** as `Bloblang mapping`.
7. Enter the Bloblang mapping expression that maps input records to NGSI-LD entities, or drop a sample file to auto-draft the mapping.
8. In **Output**, enter **Entity type** as `Vehicle` and set **Write mode** to `upsert`.
9. Under **Try it on a sample**, click **Test on the source's URL** and click **Test mapping**.
10. You should see **All messages map to entities**, and **Propose change** becomes enabled.
11. Click **Propose change**.
12. In `/projects/helsinki/approvals`, an approver merges the proposal.

The live journey `load.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Create a pipeline called citybikes-free that reads the hsl-citybikes-free data source every 60s and writes into the helsinki space through the helsinki-all endpoint`

The assistant opens the studio dialog at `/projects/helsinki/pipelines` with the parameters configured. You verify the mapping test and click **Propose change**.

The live journey `assistant-creates.spec.ts` replays these steps.

## 4. Calculating KPIs with Pipeline Presets

### Configure an Indicator Pipeline

#### By hand

1. In `/projects/helsinki/pipelines`, click **New pipeline**.
2. In **Preset**, select `Analyse (KPI)`.
3. In **Source endpoint**, select `helsinki-all`.
4. Set **Indicator name** to `bikes-available-avg`.
5. Set **Entity type** to `BikeHireDockingStation` and **Attribute** to `availableBikeNumber`.
6. In **Aggregate**, select `Average`.
7. In **Period**, enter `15m`.
8. Click **Test on the endpoint's page**. You should see **Computed value** calculate and display a numeric result.
9. Click **Propose change**.
10. In `/projects/helsinki/approvals`, an approver reviews and merges the proposal.

The live journey `analyse.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Create a KPI pipeline called bikes-available-avg calculating average availableBikeNumber every 15m from endpoint helsinki-all`

The assistant populates the KPI preset in the studio, compiles the indicator formula, and computes the preview value. You click **Propose change**, and an approver merges the change.

## 5. Deploying Flows from Blueprints

### Instantiate a Blueprint

#### By hand

1. Navigate to **Flows** at `/projects/helsinki/flows`.
2. Browse the blueprint gallery. Check the review risk chip (**Approved automatically**, **One approver**, or **Full approval**).
3. Click **Set up** on a card (such as an MQTT sensor ingestion blueprint).
4. Fill in the parameters generated from the blueprint schema (such as broker address, target space, and model).
5. Click **Create**.
6. The platform expands the blueprint into manifests and opens a change proposal.
7. In `/projects/helsinki/approvals`, an approver approves the proposal.

#### By asking the assistant

The assistant can guide you to `/projects/helsinki/flows` and recommend blueprints matching your data source, but parameter forms must be submitted and reviewed by you.

## 6. Pausing, Resuming, and Stream Health

### Manage Live Pipelines

#### By hand

1. On `/projects/helsinki/pipelines`, inspect live operational numbers in the table: message throughput rate (**msg/s**), received messages, error counts, and p99 latency.
2. In the row actions menu, click **Pause** on a running pipeline.
3. This creates a proposal setting `spec.enabled: false`.
4. In `/projects/helsinki/approvals`, an approver clicks **Approve**. The stream halts cleanly.
5. To restart the stream, click **Resume** on the paused row and approve the resulting proposal.

The live journey `change.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Pause the hel-news pipeline`

The assistant opens `/projects/helsinki/pipelines?edit=hel-news` with the form toggled to pause the pipeline. You click **Propose change**, and an approver confirms it in Approvals.

The live journey `change.spec.ts` replays these steps.

## 7. Removing Entities the Source Dropped

A pipeline writes what its source sends and never deletes. When a feed stops listing a vehicle, the vehicle stays in the space with its last position. If the source sends the whole picture every run, you can tell the pipeline that absence means removal.

Use it for full-snapshot sources: a vehicle list, a register of stations, a catalogue export. An entry missing from the snapshot is gone.

Leave it off for incremental and event feeds: sensor readings, change streams, a feed that only sends what changed. There a quiet entity has nothing new to say, and expiry would delete it. That is why expiry is off unless you switch it on.

### Switch On Expiry

#### By hand

1. On `/projects/helsinki/pipelines`, open the pipeline and switch on **Remove entities the source no longer sends**.
2. Enter the window (for example `14d`) and the entity types this pipeline writes.
3. Click **Propose change**. The Portal refuses the proposal while another pipeline writes into the same space, and while no Policy grants the pipeline's account `deleteBatch` on those types; the message names the Policy to extend.
4. An approver confirms it in Approvals. The pipeline page and the list then say "Entities not updated for 14 days are removed", and the deletions appear in the activity trail under the pipeline's account.

## Related

- [Organizations, Projects & Context Spaces](./02-organizations-projects-spaces.md): managing target spaces.
- [Data Modeling](./03-data-models.md): authoring schemas for pipeline validation.
- [Endpoints & Sharing](./05-endpoints-and-sharing.md): publishing ingested context data.
- [Users, Roles & Approvals](./07-users-roles-approvals.md): reviewing pipeline change proposals.
