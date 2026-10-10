---
sidebar_position: 7
title: Dashboards & Geospatial Visualization
description: Building map dashboards and layers over the endpoints of a project, and exploring the entities behind them.
---

# Dashboards & Geospatial Visualization

A dashboard is a page of maps and numbers built from the endpoints of your project. This guide explains how to build one, how to style its layers, what a public dashboard may read, how to add a chart or a grid beside the map, and how to look at the entities behind a point. It is written for analysts, dashboard authors and data stewards.

Every dashboard change goes through the change approval flow: you build or edit a dashboard or layer, click **Propose change**, and an approver reviews and merges the proposal in Approvals before any visualization goes live.

## 1. What a Dashboard Is, and What a Public One May Read

A dashboard holds pages; a page holds layers on a map and widgets beside it. A layer is one
endpoint's entities of one type with a style, its encodings and a filter, so a dashboard reads
only what an endpoint already publishes and its policy already allows.

One rule decides what you may put on a dashboard: a dashboard whose **Visibility** is `public`
may read public endpoints only. If a layer of a public dashboard points at an endpoint published
to the organization or to selected projects, that layer is not drawn and the dashboard says which
one it left out.

The map needs WebGL. In a browser without it the dashboard says so instead of drawing an empty
frame.

## 2. Building a Map Dashboard

### Create a Dashboard and Styled Layers

#### By hand

1. Navigate to **Dashboards** at `/projects/helsinki/dashboards`.
2. Click **New dashboard**.
3. In **Name**, enter `city-bikes-overview`. In **Title**, enter `City Bikes Overview`. Set **Visibility** to `project` (or `public`). Click **Propose change**.
4. Click **New layer**.
5. In **Name**, enter `bike-stations-layer`.
6. Select **Endpoint** `helsinki-bikes` and **Entity type** `BikeHireDockingStation`.
7. Choose **Style**: `circle`, `line`, `fill`, `heatmap`, `hexagon` or `icon`.
8. Under **Colour by**, pick the numeric property `availableBikeNumber`, a **Palette**, and the **Domain [min, max]** the colours run between (such as 0 and 30).
9. Under **Popup properties**, choose attributes to show when a map point is selected.
10. Click **Propose change**.
11. In `/projects/helsinki/approvals`, an approver approves the dashboard and layer proposals.
12. You should see the interactive map render the vector points with live color scaling, and the **Layers** legend allows toggling layer visibility.

The live journey `assistant-creates.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Make a dashboard called city-bikes-overview showing the bikes of the helsinki space`

The assistant opens `/projects/helsinki/dashboards` with the dashboard and layers drafted. You review the planned changes and click **Propose change**. An approver merges the proposal.

The live journey `assistant-creates.spec.ts` replays these steps.

## 3. Adding Widgets: Charts and Grids

### Add Temporal Charts and Data Grids

#### By hand

1. On `/projects/helsinki/dashboards`, select your dashboard and click **Edit**.
2. Under **Pages**, add or manage pages. When a dashboard contains multiple pages, pill navigation tabs appear above the canvas.
3. Under **Widgets**, add an analytics widget:
   - A chart of one property over time: set **Widget type** to `temporal-chart`, choose the **Endpoint** `helsinki-bikes`, give the **Entity id** (such as `urn:ngsi-ld:BikeHireDockingStation:hel.fi:helsinki:station-1`) and the **Property** to draw (`availableBikeNumber`).
   - A grid of entities: set **Widget type** to `grid`, choose the **Endpoint** and the **Entity type**, and fill **The grid's configuration**.
4. Click **Propose change**.
5. In `/projects/helsinki/approvals`, an approver merges the proposal.
6. You should see the temporal chart plotting value trends over time and the data grid listing records below the map canvas.

#### By asking the assistant

Type into the assistant composer:
`Add a temporal chart widget for availableBikeNumber on station-1 to the city-bikes-overview dashboard`

The assistant configures the widget parameters and opens the dashboard editor. You review the settings and click **Propose change**.

## 4. Exploring Data in the Entity Explorer

### Search, Inspect, and Export Entities

#### By hand

1. Navigate to **Explore data** at `/projects/helsinki/explore`.
2. In **Context space**, select `helsinki`.
3. In **Read through endpoint**, choose `helsinki-all` (or any endpoint publishing the space).
4. In **Entity type**, select `BikeHireDockingStation`.
5. You should see the entity grid populate with live records, pagination controls, and attribute values.
6. Use column filter inputs to narrow records by attribute, or click **Draw area** on the map panel to draw a bounding area and filter geographically.
7. Click any entity row ID to open the full JSON-LD entity document in a detail panel.
8. Click **Export this page** to download the active page of records as a JSON file.

The live journey `load.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Show me bike stations with available bikes in the helsinki space`

The assistant executes the query and navigates to `/projects/helsinki/explore?space=helsinki&endpoint=helsinki-bikes&type=BikeHireDockingStation&q=availableBikeNumber>0` with the grid filtered to matching records.

## Related

- [Getting Started](./01-getting-started.md): first steps in the Portal.
- [Endpoints & Sharing](./05-endpoints-and-sharing.md): configuring endpoint sources for dashboards.
- [Users, Roles & Approvals](./07-users-roles-approvals.md): reviewing dashboard and layer change proposals.
- [Apps on Demand](./11-apps.md): deciding between interactive dashboards and purpose-built apps.
