using System.Numerics;

namespace Vorcall.Server.Permissions;

// The `Permission` bit set of proto/vorcall.proto as flags, so the engine can name one bit. Values
// are the wire values and must stay identical to the proto enum.
[Flags]
public enum Perm : ulong
{
    None = 0,
    ManageServer = 1UL << 0,
    ManageChannels = 1UL << 1,
    ManageRoles = 1UL << 2,
    ManageMembers = 1UL << 3,
    ManageMessages = 1UL << 4,
    ManageInvites = 1UL << 5,
    KickMembers = 1UL << 6,
    BanMembers = 1UL << 7,
    ViewChannel = 1UL << 8,
    SendMessages = 1UL << 9,
    AttachFiles = 1UL << 10,
    AddReactions = 1UL << 11,
    MentionEveryone = 1UL << 12,
    Connect = 1UL << 13,
    Speak = 1UL << 14,
    ShareScreen = 1UL << 15,
    MuteMembers = 1UL << 16,
    DeafenMembers = 1UL << 17,
    MoveMembers = 1UL << 18,
    PrioritySpeaker = 1UL << 19,
    ChangeNickname = 1UL << 20,
    Soundpad = 1UL << 21,
    ManageSounds = 1UL << 22,
}

// The proto spellings without the PERMISSION_ prefix: PROTOCOL.md § Roles and permissions makes one
// of them the `detail` of every PERMISSION_DENIED, so the wire depends on these exact strings.
public static class PermNames
{
    // Indexed by bit position, which is what keeps a name and its bit from drifting apart.
    private static readonly string[] Spellings =
    [
        "MANAGE_SERVER",
        "MANAGE_CHANNELS",
        "MANAGE_ROLES",
        "MANAGE_MEMBERS",
        "MANAGE_MESSAGES",
        "MANAGE_INVITES",
        "KICK_MEMBERS",
        "BAN_MEMBERS",
        "VIEW_CHANNEL",
        "SEND_MESSAGES",
        "ATTACH_FILES",
        "ADD_REACTIONS",
        "MENTION_EVERYONE",
        "CONNECT",
        "SPEAK",
        "SHARE_SCREEN",
        "MUTE_MEMBERS",
        "DEAFEN_MEMBERS",
        "MOVE_MEMBERS",
        "PRIORITY_SPEAKER",
        "CHANGE_NICKNAME",
        "SOUNDPAD",
        "MANAGE_SOUNDS",
    ];

    // Empty for anything that is not one defined bit: only a single bit has a name on the wire.
    public static string Name(Perm bit)
    {
        var mask = (ulong)bit;
        if (mask == 0 || (mask & (mask - 1)) != 0 || (mask & Perms.All) == 0)
        {
            return string.Empty;
        }

        return Spellings[BitOperations.TrailingZeroCount(mask)];
    }

    public static bool TryParse(string name, out Perm bit)
    {
        for (var index = 0; index < Spellings.Length; index++)
        {
            if (Spellings[index] == name)
            {
                bit = (Perm)(1UL << index);
                return true;
            }
        }

        bit = Perm.None;
        return false;
    }

    // Ascending, so a caller that reports one bit out of several always reports the same one.
    // An undefined bit has no name and is skipped.
    public static IEnumerable<Perm> Bits(ulong mask)
    {
        var defined = mask & Perms.All;
        for (var index = 0; defined != 0; index++, defined >>= 1)
        {
            if ((defined & 1) != 0)
            {
                yield return (Perm)(1UL << index);
            }
        }
    }
}
