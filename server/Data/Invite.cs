namespace Vorcall.Server.Data;

public class Invite
{
    public long Id { get; set; }

    // As with refresh tokens, only the hash is stored: a leaked database hands out no invites.
    public string CodeHash { get; set; } = string.Empty;

    public DateTime CreatedAt { get; set; }

    // Null for an invite the admin CLI minted, which has no account behind it.
    public long? CreatedBy { get; set; }

    public DateTime ExpiresAt { get; set; }

    public DateTime? UsedAt { get; set; }

    public long? UsedByUserId { get; set; }

    // Set by "invites revoke": an invite that was never used and must never be usable. Kept
    // apart from UsedAt so the listing can still say which of the two ended it.
    public DateTime? RevokedAt { get; set; }
}
