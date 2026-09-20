# Roles as code, evaluated on every merge request of the organization repository (PF-51,
# PF-52, CC-59). `jcctl roles render` writes this file and `policies/roles.json` from the
# Role and RoleBinding manifests under users/; edit those, never this.
#
# input (built by .gitea/workflows/ci.yaml):
#   author: the forge login of the merge request's author
#   author_email: their e-mail, when the forge knows it
#   groups: the groups the author belongs to (empty when the forge cannot tell)
#   changes: one per changed manifest: path, action (propose | delete), kind, name, project,
#            manifest (the whole document, for the constraints)
# data (policies/roles.json): roles (name -> { rules }) for the organization's own roles,
#   projectRoles (project -> name -> { rules }) for a project's own roles (PF-68),
#   bindings ([{ name, subjects, role, scope }])
package main

import rego.v1

deny contains msg if {
	some change in input.changes
	not allowed(change)
	msg := sprintf("%s: no role binding grants %s on %s to %s (PF-52)", [change.path, change.action, change.kind, input.author])
}

allowed(change) if {
	some binding in data.bindings
	subject_matches(binding)
	scope_covers(binding.scope, change)
	some rule in rules_of(binding, change)
	change.kind in rule.kinds
	change.action in rule.verbs
	constraints_hold(rule, change.manifest)
}

# The organization's role, or the role of the project this change belongs to; a name is never
# both, which `jcctl validate` refuses before this file is written (PF-68, PF-69).
rules_of(binding, _) := rules if {
	rules := data.roles[binding.role].rules
}

rules_of(binding, change) := rules if {
	not data.roles[binding.role]
	rules := data.projectRoles[change.project][binding.role].rules
}

subject_matches(binding) if {
	some subject in binding.subjects
	subject.user == input.author
}

subject_matches(binding) if {
	some subject in binding.subjects
	subject.user == input.author_email
}

subject_matches(binding) if {
	some subject in binding.subjects
	subject.group in input.groups
}

scope_covers(scope, _) if {
	scope.organization
}

scope_covers(scope, change) if {
	scope.project == change.project
}

scope_covers(scope, change) if {
	scope.contextSpace == change.manifest.spec.contextSpaceRef
}

scope_covers(scope, change) if {
	scope.contextSpace == change.manifest.spec.contextSpaceRef.name
}

constraints_hold(rule, _) if {
	not rule.constraints
}

constraints_hold(rule, manifest) if {
	every constraint in rule.constraints {
		constraint_holds(constraint, manifest)
	}
}

constraint_holds(constraint, manifest) if {
	constraint.equals
	value_at(manifest, constraint.field) == constraint.equals
}

constraint_holds(constraint, manifest) if {
	constraint["in"]
	value_at(manifest, constraint.field) in constraint["in"]
}

constraint_holds(constraint, manifest) if {
	constraint.notIn
	not value_at(manifest, constraint.field) in constraint.notIn
}

value_at(doc, path) := object.get(doc, split(path, "."), null)
