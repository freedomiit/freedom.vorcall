namespace Vorcall.Server.Updates;

// Where the signed release manifest and the client binaries live. In production this is the
// read-only bind mount docker-compose.prod.yml points at ./releases on the host. Unlike the other
// Vorcall options there is nothing to reject: an absent directory only means no release has been
// published yet, which the endpoints answer with 404.
public sealed record UpdatesOptions
{
    public const string DefaultReleasesDir = "/releases";

    private UpdatesOptions(string releasesDir) => ReleasesDir = releasesDir;

    public string ReleasesDir { get; }

    public static UpdatesOptions FromConfiguration(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:ReleasesDir"]?.Trim();
        return new UpdatesOptions(string.IsNullOrEmpty(configured) ? DefaultReleasesDir : configured);
    }
}
