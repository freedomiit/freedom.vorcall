namespace Vorcall.Server.Data;

public class RefreshToken
{
    public long Id { get; set; }

    public long UserId { get; set; }

    // Only the SHA-256 of the opaque token is stored; the plaintext lives in the response body
    // and nowhere else.
    public string TokenHash { get; set; } = string.Empty;

    // Every token rotated out of one login shares a family, so replaying an old one can revoke
    // the whole chain at once.
    public Guid FamilyId { get; set; }

    public DateTime CreatedAt { get; set; }

    public DateTime LastUsedAt { get; set; }

    public DateTime ExpiresAt { get; set; }

    public DateTime? RevokedAt { get; set; }

    public long? ReplacedById { get; set; }

    public User User { get; set; } = null!;
}
