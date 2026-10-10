---
sidebar_position: 13
title: Managing Your Organization and Projects
description: Where the organization's settings, members, roles and projects live, and where one project's settings live.
---

# Managing Your Organization and Projects

You manage access in two places. The **Organization** page holds what is the same in every project. **Project settings** holds what belongs to one project. Every change you make on either page is a proposed change that an approver merges, as everywhere else in the Portal ([07](07-users-roles-approvals.md)).

> Status (2026-09-22): the two pages are specified here and built by T-2605 and T-2606. Until they ship, everything below sits on **Project → Access**, as [07 §1](07-users-roles-approvals.md#1-granting-roles-to-team-members) describes.

## 1. The Organization page

Open **Organization** in the top bar. You see it whatever project you are in, because nothing on it belongs to a project.

| Tab | What you do there |
|---|---|
| **Settings** | Change the organization's domain and see whether it is verified, its languages and contacts, who may open a project, whether everyone in the organization sees every project, the project quota and how long a deleted project's name stays reserved. |
| **Members** | See who is bound at organization scope and with which role; add a person or a group, or remove them. Only a person who may read organization bindings sees this list. |
| **Roles** | Read the organization's roles in words; the ones every organization starts with are marked *seeded*. Propose a new role. |
| **Groups** | Keep the groups of people you bind at once; a member who has never signed in is marked. |
| **Service accounts** | The organization's own service accounts and their keys. |
| **Projects** | Every project you may read. Open a new one, or delete one: the dialog lists everything the deletion removes and asks you to type the project's name. |

## 2. Project settings

Open a project and choose **Settings**, the last item of its menu.

| Tab | What you do there |
|---|---|
| **General** | The project's title, description and quotas. |
| **Members** | Who is bound in this project or in one of its spaces. Give a person or a group an organization role or one of this project's roles. |
| **Roles** | This project's own roles, for example one that proposes pipelines but not endpoints. You can only write a role whose rights you hold yourself. |
| **Service accounts** | The project's service accounts and keys. |
| **Your access** | What you may read, propose, approve and delete in this project. |
| **Delete project** | Delete the project, with the list of what goes with it. |

An old link to `/projects/{project}/access` opens **Settings → Members**.

## 3. Which role is which

| You want someone to… | Use | Where |
|---|---|---|
| hold the same rights in every project | an organization role, bound at organization scope | Organization → Members |
| hold an organization role in one project only | the organization role, bound at project scope | Project settings → Members |
| hold rights your project defines | a project role | Project settings → Roles, then Members |
| use one application in a certain way | an application role | the application's own page |

Keycloak only knows who a person is and how they sign in. You never edit a role or a membership there.

## Related

- [07-users-roles-approvals.md](07-users-roles-approvals.md) — granting a role, approvals and drift, step by step.
- [Architecture/09 §14](../Architecture/09-portal.md#14-organization-and-project-management) — the specification of both pages.
- [Architecture/12 §2a](../Architecture/12-identity-and-access.md#2a-roles-as-code) — roles, bindings and groups as manifests.
