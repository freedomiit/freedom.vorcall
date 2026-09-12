namespace Vorcall.Server.Permissions;

// The two scopes of PROTOCOL.md § Roles and permissions. All const: these values are part of the
// wire contract, not configuration, and vorcall-core/src/permissions.rs repeats them.
public static class Perms
{
    // The 21 defined bits.
    public const ulong All = 0x1FFFFF;

    // Held server-wide; an override carrying one of these has that bit masked away.
    public const ulong ServerScoped =
        (ulong)Perm.ManageServer
        | (ulong)Perm.ManageRoles
        | (ulong)Perm.ManageMembers
        | (ulong)Perm.ManageInvites
        | (ulong)Perm.KickMembers
        | (ulong)Perm.BanMembers
        | (ulong)Perm.ChangeNickname;

    public const ulong ChannelScoped = All & ~ServerScoped;

    // @everyone's permissions on a fresh server: VIEW_CHANNEL | SEND_MESSAGES | ATTACH_FILES |
    // ADD_REACTIONS | CONNECT | SPEAK | SHARE_SCREEN | CHANGE_NICKNAME.
    public const ulong EveryoneDefault = 1109760;

    // A multi-bit argument asks for every one of them.
    public static bool Has(ulong set, Perm bit) => (set & (ulong)bit) == (ulong)bit;

    // An undefined bit is dropped rather than refused, so a newer client asking for a bit this
    // build does not know stores nothing instead of being turned away.
    public static ulong Clean(ulong raw) => raw & All;
}
