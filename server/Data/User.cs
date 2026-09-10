namespace Vorcall.Server.Data;

public class User
{
    public long Id { get; set; }

    public string Username { get; set; } = string.Empty;

    // Uppercased with the invariant culture: the uniqueness key and the login lookup key.
    public string UsernameNormalized { get; set; } = string.Empty;

    public string PasswordHash { get; set; } = string.Empty;

    public DateTime CreatedAt { get; set; }
}
