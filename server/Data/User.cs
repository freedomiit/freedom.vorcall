namespace Vorcall.Server.Data;

public class User
{
    public long Id { get; set; }

    public string Username { get; set; } = string.Empty;

    // Uppercased with the invariant culture: the uniqueness key and the login lookup key.
    public string UsernameNormalized { get; set; } = string.Empty;

    public string PasswordHash { get; set; } = string.Empty;

    public DateTime CreatedAt { get; set; }

    // The name shown instead of Username everywhere; null when the account shows its username.
    public string? Nickname { get; set; }

    public long? AvatarImageId { get; set; }

    public long? BannerImageId { get; set; }

    // The profile's "about me".
    public string Description { get; set; } = string.Empty;

    // 0xRRGGBB, null for "none": the profile card's accent.
    public int? AccentColor { get; set; }

    // Moderation state, persisted so it survives a reconnect and enforced by the relay.
    public bool ServerMuted { get; set; }

    public bool ServerDeafened { get; set; }

    // Self-reported by the client in Hello and never enforced: null for an account that has not
    // connected since the field existed, or that sent nothing.
    public string? LastClientVersion { get; set; }

    public string? LastClientPlatform { get; set; }

    public DateTime? LastSeenAt { get; set; }
}
