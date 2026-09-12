namespace Vorcall.Server.Data;

public class User
{
    public long Id { get; set; }

    public string Username { get; set; } = string.Empty;

    // Uppercased with the invariant culture: the uniqueness key and the login lookup key.
    public string UsernameNormalized { get; set; } = string.Empty;

    public string PasswordHash { get; set; } = string.Empty;

    public DateTime CreatedAt { get; set; }

    // Self-reported by the client in Hello and never enforced: null for an account that has not
    // connected since the field existed, or that sent nothing.
    public string? LastClientVersion { get; set; }

    public string? LastClientPlatform { get; set; }

    public DateTime? LastSeenAt { get; set; }

    // Set by "users ban": login, refresh and every bearer-authenticated request are refused
    // while it is not null, and the row outlives any running server, so a ban holds across a
    // restart without anything in memory.
    public DateTime? DisabledAt { get; set; }
}
