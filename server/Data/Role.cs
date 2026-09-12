namespace Vorcall.Server.Data;

public class Role
{
    public long Id { get; set; }

    public string Name { get; set; } = string.Empty;

    // 0xRRGGBB, null for "no colour": a member's name is painted by the highest-positioned role
    // that has one.
    public int? Color { get; set; }

    // A role shows at most one icon, an emoji or an image; empty when it shows none.
    public string IconEmoji { get; set; } = string.Empty;

    public long? IconImageId { get; set; }

    // Higher outranks lower; the everyone role is pinned at 0.
    public int Position { get; set; }

    // The permission bit set. Signed here and bigint in SQL against the wire's uint64: only the
    // low 21 bits are ever used, so the two are the same number.
    public long Permissions { get; set; }

    public bool Hoist { get; set; }

    // The undeletable role every account holds implicitly. Exactly one row has it.
    public bool IsEveryone { get; set; }
}
