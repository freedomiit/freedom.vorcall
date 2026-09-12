namespace Vorcall.Server.Admin;

// The admin CLI is a second process against the same database, so closing a live socket is the
// one thing it cannot do on its own: it asks the running server over HTTP instead. Key absent is
// a valid state — the endpoints are then not mapped at all and the CLI says so — because a
// deployment that never bans anybody should not have to carry a secret for it.
public sealed record AdminOptions
{
    public const string DefaultUrl = "http://localhost:5000";

    private AdminOptions(string? key, string url)
    {
        Key = key;
        Url = url;
    }

    public string? Key { get; }

    // In production the CLI runs as a sibling container, where the server is http://backend:5000.
    public string Url { get; }

    public static AdminOptions FromConfiguration(IConfiguration configuration)
    {
        var key = configuration["Vorcall:AdminKey"]?.Trim();
        var url = configuration["Vorcall:AdminUrl"]?.Trim();
        return new AdminOptions(
            string.IsNullOrEmpty(key) ? null : key,
            string.IsNullOrEmpty(url) ? DefaultUrl : url);
    }
}
