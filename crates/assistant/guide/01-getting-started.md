---
sidebar_position: 2
title: "Getting Started"
description: Your first hour in the Portal, task by task, each one done by hand and asked of the assistant in one sentence.
---

# Getting Started

This page is a walkthrough of your first hour in the Portal, in the order a first hour usually goes: see what a project already holds, read its data, bring a new feed in, publish it, and let another project read it. Every step names the page, the button and what to type, in the words the screen uses. Each task is shown twice, because the Portal offers both: by hand, filling the form yourself, and by asking, one sentence to the assistant, which fills the same form and leaves it for you to check.

Nothing in this guide goes live when you press Save. Every write is proposed as a change that somebody approves, which is why each form's last button says **Propose change** and not "Create". Section 6 is where a change becomes real.

## 1. Sign in and find your way around

1. Open the Portal. The sign-in page is your organization's own; enter the user name and password you were given. Once you are in, the Portal opens on a project.
2. The bar at the top names the project you are looking at. If you belong to more than one, use it to switch: everything below it, every page and every list, belongs to the project named there.
3. The left navigation is the whole Portal: **Context Spaces**, **Endpoints**, **Data sources**, **Pipelines**, **Dashboards**, **Applications**, **Approvals**, **Access**, **Explore data**, **Data models**, **Flows**, **Copies**, **Open data** and **Activity**.
4. The assistant sits in the corner behind **Open the assistant**. It can read the pages you can read and fill in the forms you can fill in, and it can propose a change, exactly like you; it cannot approve one.

If a page says you may not see something, that is your role talking, not a fault. **Access** shows what your roles let you do here.

## 2. See what the project already holds

Start with what is there. Nothing in this section changes anything.

1. Open **Context Spaces**. A context space is a named collection of context data; every entity lives in exactly one. The demonstration project `helsinki` has one, also called `helsinki`.
2. On its row, choose **Look inside**. The page lists the entity types the space's data model defines, with **Entities**, the count of each, and a sample of them. Underneath, a line about seeing the space through its endpoints lists the doors into it, and **Policies** lists what each door lets through.
3. If the count says *not readable anonymously*, the space has no public door. That is normal and not an error; the next section reads it with your own session instead.

**By asking the assistant:** open the assistant and type *what does this project hold?* It opens the same pages and answers from them.

## 3. Read the data with your own session

1. Open **Explore data**. Choose the **Context space**, then **Read through endpoint**, then the entity type.
2. The table is the entities as that endpoint serves them. The endpoint's policy decides what you see, so two people can open the same page and see different columns. That is the point, not a bug.
3. Choose a row to open one entity, and **Export this page** to take the page away as a file.

**By asking the assistant:** *show me the bike stations that have no bikes right now*. The assistant reads through an endpoint you may read and shows you the rows.

## 4. Bring a new feed in

Two pieces, in this order: a data source says where to fetch from, and a pipeline says what to do with what arrives. They are separate because several pipelines commonly read one source.

### The data source

1. Open **Data sources** and choose **New data source**.
2. Pick the **Type**: *HTTP endpoint*, *MQTT broker*, *WebSocket stream* or *GTFS-realtime feed*. The rest of the form follows the type you picked.
3. Fill in **Name** (lower-case letters, digits and hyphens: this is the name the pipelines will use), **Title** in your own words, and the connection: for an HTTP endpoint that is the **URL**, the **Method** and a **Timeout**.
4. If the feed needs a credential, the form asks for a **Secret name** and a **Key in the secret**, never for the credential itself. Credentials stay in the secret store; the form takes a reference to one. If the name you need is not there yet, ask your administrator for it before you go on.
5. Choose **Check** to have the platform try the connection, then **Propose change**.

### The pipeline

1. Open **Pipelines** and start a new one. A pipeline reads one source, transforms what arrives, and writes through an endpoint.
2. **Source** is one input and only one: a **Data source** of this project, or a **Source endpoint** read by a query. Not both.
3. **Execution** is *auto*, *resident* or *scheduled*. *Resident* runs continuously in the project's runner; *scheduled* runs as a job and stops in between, and then **Schedule** is required, five cron fields such as `10 0 * * *`; *auto* lets the platform pick from the **Period** you give, a duration such as `15s` or `1h`.
4. The studio draws the pipeline as a lane of steps and lets you edit each step. Use its test run: it shows you what one message becomes, and paints the step that failed.
5. **Propose change**.

**By asking the assistant:** *load the city's air quality feed from this URL into a new space every fifteen minutes*. The assistant drafts the data source, the pipeline and the space together and leaves them filled in for you to read. The notice above the form says the assistant filled it in; check it before you propose, because the change goes out under your name.

Or start from a flow: **Flows** is a gallery of whole use cases. **Set up** asks only for what that use case needs and writes the space, the source, the pipeline and the endpoint together.

## 5. Publish it

An endpoint is where a space is published: who may call it, in which representations, under which address.

1. Open **Endpoints** and choose **New endpoint**.
2. Fill in **Name** and **Title**, and pick the **Context Space** it publishes.
3. **Slug** is the address, and **Generate slug** makes one for you. It never changes, even when the endpoint is renamed, so a link you give out keeps working.
4. **Audience** is one of three: *Selected projects: only the projects you tick*, *Organization: every project of the organization*, or *Public: anyone with the address*. Choosing *Selected projects* adds **Allowed projects**, where you tick them.
5. **Representations** is how the same data is served: *NGSI-LD API*, *GeoJSON*, *CSV*, *Excel (xlsx)*, *ZIP download*, *OGC API Features*, *SensorThings API*, *JSON*, and *MCP for AI assistants*. Tick the ones you want; every one you tick becomes a link on the endpoint's row.
6. Under **Which classes and attributes it shows**, tick the classes and the attributes this endpoint exposes. What you do not tick is not served. **Hide attributes** takes out single attributes from what is otherwise shown.
7. **Show the manifests this proposes** is worth opening once: it is exactly what will be written, before you write it.
8. **Propose change**.

Making an endpoint public is a bigger step than the others, and the approval in the next section will ask for more.

## 6. Get the change approved, and watch it happen

1. Open **Approvals**. Every change to the project sits here until an approver merges it. Each one shows its **Author**, its **Risk lane** and, field by field, what it would change.
2. An approver opens it and chooses **Approve** or **Reject**. You cannot approve your own proposal, and letting an endpoint or an app out to the public needs the publisher role; a steward alone cannot. A red-lane change asks for the resource name to be typed back before it will go.
3. Once approved, the page says the change is being deployed, and then that it is applied. Open the endpoint's row and its links answer.
4. **Activity** is the history of all of this afterwards: what changed, who proposed it, who approved it.

## 7. Let another project read it

Two projects never write into each other. What crosses is a published endpoint.

1. As the project that owns the data, open the endpoint and set its **Audience** to *Selected projects*, then tick the project you are sharing with. Propose and have it approved.
2. As the project being shared with, open **Endpoints** and find **Shared with this project**: the endpoints other projects of the organization have opened to this one. Choose **Use in this project** and the space is referenced here under an alias.
3. That reference is proposed as a change like every other write. Once it is approved, this project can read the other's data, still through the other's own endpoint and still under the other's policy. The owner revokes it by changing the audience back, and nothing of yours has to be rebuilt.

**By asking the assistant:** *share the bike stations with the mobility project*. The assistant drafts the publication and tells you what it would open to whom.

## 8. Before you change anything real

Three habits that save an afternoon.

- **Work on a copy**. **Copies** is the project on the side: change several resources together, try them, and bring them all back as one change. Nothing in a copy is live until you do.
- Read the proposal, not the form. Every dialog can show you the manifests it is about to write. A change you have read is a change you can explain to the person approving it.
- Let the assistant draft, and check it yourself. It fills the same forms under the same rules, and it cannot approve. What it proposes goes out under your name.

## Related

- [02-organizations-projects-spaces](./02-organizations-projects-spaces.md) — what a project and a context space are, before you make one.
- [04-pipelines](./04-pipelines.md) — the pipeline studio in full, including the test run.
- [05-endpoints-and-sharing](./05-endpoints-and-sharing.md) — audiences, representations and sharing in detail.
- [07-users-roles-approvals](./07-users-roles-approvals.md) — who may propose, who may approve, and why they are never the same person.
- [08-working-with-ai-agents](./08-working-with-ai-agents.md) — what the assistant may do on your behalf.
