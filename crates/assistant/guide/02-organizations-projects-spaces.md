---
sidebar_position: 3
title: Organizations, Projects & Context Spaces
description: Managing organizational domain boundaries, collaborative projects, context spaces, quotas, and working copies.
---

# Organizations, Projects & Context Spaces

The joinedcontext platform organizes all city operations into structured domain boundaries. This guide explains how to manage projects, create and inspect isolated context spaces, monitor resource quotas, and work safely on project copies without altering live data. It is written for project stewards, data architects, and city administrators.

Every change to a project follows the same rule in the Portal: you fill in a form, the platform runs a validation check, you click the button that proposes the change, and an approver reviews and merges the proposal in Approvals before any change takes effect.

## 1. Domain Boundaries

The platform organizes city data into three structural levels:

1. **Organization:** Represents the entire city administration or regional authority (such as `hel.fi`). It manages top-level governance, shared role definitions, and default quotas.
2. **Project:** Represents a collaborative departmental workspace (such as `helsinki`). It defines access rights, resource quotas, and owns pipelines, dashboards, and spaces.
3. **Context Space:** Represents an isolated data partition. Every entity belongs to exactly one context space and is inaccessible to outside callers unless explicitly exposed by an Endpoint.

## 2. Managing Projects and Quotas

### Open a Project

#### By hand

1. Open the project switcher in the navigation header and select **New project**.
2. In the dialog, enter the project identifier in **Name** using lowercase letters, digits, and hyphens (such as `helsinki-mobility`).
3. Enter a title in **Display name** (such as `Helsinki Mobility`) and a summary in **Description**.
4. Click **Open project**.
5. You should see a proposal banner linking to Approvals. An administrator reviews the proposal before the project initializes.

#### By asking the assistant

Type into the assistant composer:
`Open a new project called helsinki-mobility for urban transport`

The assistant validates your permissions, drafts the manifest, and opens the creation form. You click **Open project**, and an administrator approves the proposal.

### Monitoring Quotas

#### By hand

1. Navigate to **Context Spaces** at `/projects/helsinki/spaces`.
2. Scroll to the **Quota** section beneath the spaces table.
3. You should see quota usage progress bars for **Context Spaces**, **Resident pipelines**, **Public endpoints**, and **Apps**.
4. When a limit is reached, creating an additional resource is blocked until an administrator raises the limit or an unused resource is removed.

#### By asking the assistant

Type into the assistant composer:
`What are the remaining quotas for the helsinki project?`

The assistant queries the project usage and reports the active counts and limits for each dimension.

## 3. Creating and Inspecting Context Spaces

### Create a Context Space

#### By hand

1. Navigate to **Context Spaces** at `/projects/helsinki/spaces`.
2. Click **New Context Space**.
3. In the dialog, enter `air-quality` into **Name**.
4. In **Title**, enter `Air Quality & Weather`.
5. In **Data model**, enter the model reference `helsinki`.
6. Click **Check**. You should see the draft verdict chip display `Checked`.
7. Click **Propose change**.
8. Navigate to `/projects/helsinki/approvals` where an approver selects the proposal and clicks **Approve**.
9. The space state transitions to **Live**.

The live journey `verdict-gate.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Create a context space called air-quality in the helsinki project`

The assistant navigates to `/projects/helsinki/spaces` and opens the dialog with **Name** filled. You click **Check**, verify the green chip, and click **Propose change**. An approver merges the proposal in Approvals.

The live journey `assistant-creates.spec.ts` replays these steps.

### Inspect What a Space Holds

#### By hand

1. Navigate to `/projects/helsinki/spaces`.
2. Locate the row for `helsinki` and click **Look inside**.
3. You are taken to `/projects/helsinki/spaces/helsinki`.
4. Under **Entity types**, you should see the defined types (`BikeHireDockingStation`, `Vehicle`, `Event`), the live entity count, and sample records.
5. Under **Data**, select an entity type from **Entity type** to browse records in the data grid.
6. Under **Endpoints** and **Policies**, review the public and organizational views configured over this space.

#### By asking the assistant

Type into the assistant composer:
`What entity types and how many entities are inside the helsinki space?`

The assistant inspects the space surface and answers with the active types and live record counts.

## 4. Completing a Space from a Sample

### Complete a Space from a Feed

#### By hand

1. On `/projects/helsinki/spaces`, click **Complete this space** to open `/projects/helsinki/spaces/complete`.
2. In **Space name**, enter `city-bikes`.
3. In **Endpoint URL**, enter `https://gbfs.theta.fifteen.eu/gbfs/2.2/helsinki/en/station_information.json`, or attach a CSV file using **Files**.
4. Click **Complete**.
5. You should see drafted resource cards for DataModel, ContextSpace, DataSource, and Pipeline, each marked with a check status chip.
6. Click **Propose all**. An approver reviews and approves the bundle in Approvals.

The live journey `space_complete.spec.ts` replays these steps.

#### By asking the assistant

Drop a sample file into the assistant panel and type:
`Complete a space called city-bikes from this sample.`

The assistant analyzes the sample, drafts the schemas and pipelines, and navigates to `/projects/helsinki/spaces/complete` with the drafts ready for your review and proposal.

## 5. Working on Project Copies

### Make Changes in a Copy

#### By hand

1. Navigate to **Copies** at `/projects/helsinki/workspaces`.
2. Click **Work on a copy**.
3. In **Name**, enter `emp-test`. Select `The whole project` under **Covers**, and click **Start the copy**.
4. You should see the copy banner across the top stating that you are working on a copy and nothing is live.
5. Navigate to `/projects/helsinki/pipelines?workspace=emp-test` and click **Pause** on `hel-news`. Inside a copy the edit is kept in the copy and opens no proposal of its own.
6. Open `/projects/helsinki/workspaces/emp-test/compare` to inspect what the copy changes.
7. Open `/projects/helsinki/workspaces/emp-test/bring-back` and click **Propose as one change**.
8. In `/projects/helsinki/approvals`, an approver reviews the diff and approves the change.

The live journey `copy-employee.spec.ts` replays these steps.

#### By asking the assistant

Type into the assistant composer:
`Work on a copy of the helsinki project called agent-test, and in that copy pause the hel-news pipeline.`

The assistant starts the copy, creates the checked draft inside the copy, and stops at the door. Proposing the copy back to the project or discarding it must be executed by you.

The live journey `copy-agent.spec.ts` replays these steps.

## Related

- [Getting Started](./01-getting-started.md): first steps in the Portal.
- [Data Modeling](./03-data-models.md): visual LinkML modeling and schema versioning.
- [Endpoints & Sharing](./05-endpoints-and-sharing.md): exposing space data to consumers and tools.
- [Users, Roles & Approvals](./07-users-roles-approvals.md): role assignments, service accounts, and change reviews.
