namespace Vorcall.Server.Data;

public class Invite
{
    public long Id { get; set; }

    // As with refresh tokens, only the hash is stored: a leaked database hands out no invites.
    public string CodeHash { get; set; } = string.Empty;

    public DateTime CreatedAt { get; set; }

    public DateTime ExpiresAt { get; set; }

    public DateTime? UsedAt { get; set; }

    public long? UsedByUserId { get; set; }
}
