# The executable cases of PF-52: `conftest verify -p policies` runs them in CI.
package main

import rego.v1

roles := {
	"pipeline-developer": {"rules": [
		{"kinds": ["Pipeline", "DataSource", "Mapping"], "verbs": ["propose"]},
		{"kinds": ["Endpoint"], "verbs": ["propose"], "constraints": [{"field": "spec.audience", "notIn": ["public"]}]},
	]},
	"org-admin": {"rules": [{"kinds": ["Endpoint", "Pipeline", "Role", "RoleBinding"], "verbs": ["propose", "approve", "delete"]}]},
}

bindings := [
	{"name": "ovzdusie-developers", "subjects": [{"group": "air-quality-team"}, {"user": "jana.kovacova@banskabystrica.sk"}], "role": "pipeline-developer", "scope": {"project": "ovzdusie"}},
	{"name": "admins", "subjects": [{"user": "admin"}], "role": "org-admin", "scope": {"organization": "banskabystrica"}},
]

pipeline_change := {"path": "projects/ovzdusie/pipelines/aq/pipeline.yaml", "action": "propose", "kind": "Pipeline", "name": "aq", "project": "ovzdusie", "manifest": {"kind": "Pipeline", "spec": {"class": "resident"}}}

endpoint_change(audience) := {"path": "projects/ovzdusie/spaces/ovzdusie/endpoints/air.yaml", "action": "propose", "kind": "Endpoint", "name": "air", "project": "ovzdusie", "manifest": {"kind": "Endpoint", "spec": {"contextSpaceRef": "ovzdusie", "audience": audience}}}

test_a_viewer_cannot_propose if {
	count(deny) == 1 with input as {"author": "viewer", "groups": [], "changes": [pipeline_change]}
		with data.roles as roles
		with data.bindings as bindings
}

test_a_developer_proposes_a_pipeline_by_group if {
	count(deny) == 0 with input as {"author": "jana", "groups": ["air-quality-team"], "changes": [pipeline_change]}
		with data.roles as roles
		with data.bindings as bindings
}

test_a_developer_proposes_a_pipeline_by_email if {
	count(deny) == 0 with input as {"author": "jana", "author_email": "jana.kovacova@banskabystrica.sk", "groups": [], "changes": [pipeline_change]}
		with data.roles as roles
		with data.bindings as bindings
}

test_a_developer_cannot_publish_a_public_endpoint if {
	count(deny) == 1 with input as {"author": "jana", "groups": ["air-quality-team"], "changes": [endpoint_change("public")]}
		with data.roles as roles
		with data.bindings as bindings
}

test_a_developer_proposes_an_organization_endpoint if {
	count(deny) == 0 with input as {"author": "jana", "groups": ["air-quality-team"], "changes": [endpoint_change("organization")]}
		with data.roles as roles
		with data.bindings as bindings
}

test_deletion_needs_delete if {
	deleted := object.union(pipeline_change, {"action": "delete"})
	count(deny) == 1 with input as {"author": "jana", "groups": ["air-quality-team"], "changes": [deleted]}
		with data.roles as roles
		with data.bindings as bindings
	count(deny) == 0 with input as {"author": "admin", "groups": [], "changes": [deleted]}
		with data.roles as roles
		with data.bindings as bindings
}

test_a_project_binding_stops_at_its_project if {
	other := object.union(pipeline_change, {"project": "doprava", "path": "projects/doprava/pipelines/aq/pipeline.yaml"})
	count(deny) == 1 with input as {"author": "jana", "groups": ["air-quality-team"], "changes": [other]}
		with data.roles as roles
		with data.bindings as bindings
}

test_organization_scope_covers_every_project if {
	other := object.union(pipeline_change, {"project": "doprava"})
	count(deny) == 0 with input as {"author": "admin", "groups": [], "changes": [other]}
		with data.roles as roles
		with data.bindings as bindings
}

project_roles := {"ovzdusie": {"air-analyst": {"rules": [{"kinds": ["DataSource"], "verbs": ["propose"]}]}}}

project_role_bindings := [{"name": "ovzdusie-analysts", "subjects": [{"user": "peter"}], "role": "air-analyst", "scope": {"project": "ovzdusie"}}]

datasource_change(project) := {"path": sprintf("projects/%s/sources/air.yaml", [project]), "action": "propose", "kind": "DataSource", "name": "air", "project": project, "manifest": {"kind": "DataSource", "spec": {}}}

test_a_project_role_grants_inside_its_project if {
	count(deny) == 0 with input as {"author": "peter", "groups": [], "changes": [datasource_change("ovzdusie")]}
		with data.roles as roles
		with data.projectRoles as project_roles
		with data.bindings as project_role_bindings
}

test_a_project_role_grants_nothing_in_another_project if {
	count(deny) == 1 with input as {"author": "peter", "groups": [], "changes": [datasource_change("doprava")]}
		with data.roles as roles
		with data.projectRoles as project_roles
		with data.bindings as project_role_bindings
}

# Letting data out to the public is a right of its own (PF-71, PF-72): the seeded steward
# approves every endpoint but a public one, the publisher approves only a public one, and
# org-admin, unconstrained, approves both.

publishing_roles := {
	"steward": {"rules": [
		{"kinds": ["Pipeline", "DataSource"], "verbs": ["propose", "approve"]},
		{"kinds": ["Endpoint"], "verbs": ["propose", "approve"], "constraints": [{"field": "spec.audience", "notIn": ["public"]}]},
	]},
	"publisher": {"rules": [
		{"kinds": ["Endpoint", "Pipeline"], "verbs": ["read"]},
		{"kinds": ["Endpoint"], "verbs": ["approve"], "constraints": [{"field": "spec.audience", "in": ["public"]}]},
	]},
	"org-admin": {"rules": [{"kinds": ["Endpoint", "Pipeline", "Role", "RoleBinding"], "verbs": ["propose", "approve", "delete"]}]},
}

publishing_bindings := [
	{"name": "ovzdusie-steward", "subjects": [{"user": "lead"}], "role": "steward", "scope": {"project": "ovzdusie"}},
	{"name": "ovzdusie-publisher", "subjects": [{"user": "mayor"}], "role": "publisher", "scope": {"project": "ovzdusie"}},
	{"name": "admins", "subjects": [{"user": "admin"}], "role": "org-admin", "scope": {"organization": "banskabystrica"}},
]

approve_endpoint(audience) := object.union(endpoint_change(audience), {"action": "approve"})

test_a_steward_cannot_approve_a_public_endpoint if {
	count(deny) == 1 with input as {"author": "lead", "groups": [], "changes": [approve_endpoint("public")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
	count(deny) == 0 with input as {"author": "lead", "groups": [], "changes": [approve_endpoint("organization")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
}

test_a_publisher_approves_only_the_public_one if {
	count(deny) == 0 with input as {"author": "mayor", "groups": [], "changes": [approve_endpoint("public")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
	count(deny) == 1 with input as {"author": "mayor", "groups": [], "changes": [approve_endpoint("organization")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
}

test_org_admin_approves_both if {
	count(deny) == 0 with input as {"author": "admin", "groups": [], "changes": [approve_endpoint("public")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
	count(deny) == 0 with input as {"author": "admin", "groups": [], "changes": [approve_endpoint("organization")]}
		with data.roles as publishing_roles
		with data.bindings as publishing_bindings
}
