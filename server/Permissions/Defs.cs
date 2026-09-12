namespace Vorcall.Server.Permissions;

// The engine's view of the stored world: plain values, no EF entities and no protobuf, so the rules
// of PROTOCOL.md § Roles and permissions can be resolved and tested without either.

public sealed record RoleDef(long Id, int Position, ulong Permissions, bool Everyone);

// Exactly one of RoleId / UserId is set; IsValidOverride is what enforces that on write.
public sealed record OverrideDef(long? RoleId, long? UserId, ulong Allow, ulong Deny);

public sealed record ChannelDef(long Id, bool IsGeneral, IReadOnlyList<OverrideDef> Overrides);

// RoleIds never contains the everyone role: every member holds that one implicitly.
public sealed record MemberDef(long UserId, IReadOnlyCollection<long> RoleIds);

// Every role of the server, in any order; exactly one has Everyone = true.
public sealed record Hierarchy(long OwnerId, IReadOnlyList<RoleDef> Roles);
