# Solo editions

Solo Community is the complete open-source, local-first product. It stores one
encrypted Memory Library in `solo.db` and supports unlimited memories,
documents, and logical projects inside that library.

Community does not include database switching, tenant/profile administration,
a chat-agent runtime, hosted connectivity accounts, paid automation, or organization controls. Its
CLI, HTTP/OpenAPI, MCP, Desktop, configuration, and release artifacts are tested
to contain no selector that can route to a second memory database.

Pro and Enterprise are composed in separate private repositories from exact
public Core and Web commits. The paid implementations are not hidden behind a
Community feature flag and are not present in this repository or its Git
history. Shared fixes land in Community first, and the paid build then advances
its pinned public commits.

The public engine exposes narrow, generic composition interfaces so downstream
applications can reuse the same memory implementation. Those interfaces do not
contain Solo's paid modules, licensing system, hosted services, or organization
control plane.

Community is Apache-2.0 licensed. That permits independent modification and
redistribution; open-source local software cannot be made unpatchable DRM.
Official paid value is protected by keeping the proprietary implementation and
service operations private, and by Solo's trademark, signed releases, updates,
support, and commercial terms.
