# Solo Memory Policy - Cursor

Use Solo as project and user memory while coding.

## Before Coding

Call `memory_context` when work may depend on prior architecture,
debugging history, user preferences, release process, or project
decisions. Query with the repository or feature name when possible.

Use the configured Solo profile for this workspace. Keep private, work,
and client memory separate unless the user explicitly asks for
cross-profile context.

Use `memory_facts_about` or `memory_entities` for named projects,
modules, people, services, or libraries when exact identity matters.

## What To Remember

Remember:

- architectural decisions and why they were made;
- bugs found and their root causes;
- release or packaging procedures;
- project-specific constraints;
- user coding preferences;
- recurring commands that worked;
- important test failures and fixes.

Prefer concise memory entries. Include project/module names in the
content so future recall is precise.

## What Not To Remember

Do not store secrets, credentials, tokens, private environment values,
large raw logs, generated files, or temporary failures with no future
value.

## Corrections

If a remembered project decision is stale, use `memory_update` or store
a new correction that clearly supersedes the old fact. If Solo surfaces
a contradiction, ask the user which decision is current before resolving
it.
