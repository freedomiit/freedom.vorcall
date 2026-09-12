using System.Security.Cryptography;
using Serilog;

namespace Vorcall.Server.Auth;

// The two secrets that have no safe default: the pre-shared door key and the token signing key.
//
// A self-hosted Vorcall has to come up on `docker compose up -d` with nothing configured, so a
// secret that configuration does not supply is generated once and kept in Vorcall:DataDir. A
// value in configuration always wins, which is why a deployment that sets both (production, the
// test host, `appsettings.Development.json`) never touches these files at all.
//
// The generated door key is logged once, because the operator has to read it out to hand it to
// the people who will connect. The signing key never is: nothing needs to see it.
public static class SecretStore
{
    public const string DefaultDataDir = "/data";

    // HS256 with a key shorter than the digest weakens the signature; JwtOptions refuses anything
    // under this, so this is what we generate.
    private const int SigningKeyBytes = 32;

    // 32 hex characters: long enough that guessing it is hopeless, short enough to read out loud
    // or paste into the client's Server field.
    private const int ServerKeyBytes = 16;

    public static string DataDir(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:DataDir"]?.Trim();
        return string.IsNullOrEmpty(configured) ? DefaultDataDir : configured;
    }

    /// The pre-shared key every client sends as `X-Vorcall-Key`.
    public static string ServerKey(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:ServerKey"];
        if (!string.IsNullOrWhiteSpace(configured))
        {
            return configured.Trim();
        }

        var (key, generated) = ReadOrCreate(
            configuration,
            "server-key",
            () => Convert.ToHexString(RandomNumberGenerator.GetBytes(ServerKeyBytes)).ToLowerInvariant());

        if (generated)
        {
            // The one secret printed on purpose: without it nobody can reach this server, and the
            // operator has no other way to learn what was generated.
            Log.Warning(
                "No Vorcall:ServerKey configured, so one was generated and saved in {Path}. "
                + "Give it to everyone who connects — it goes in the client's Server section. "
                + "Server key: {ServerKey}",
                Path.Combine(DataDir(configuration), "server-key"),
                key);
        }

        return key;
    }

    /// The HS256 key access tokens are signed with, base64 as `Vorcall:JwtSigningKey` expects.
    public static string JwtSigningKey(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:JwtSigningKey"];
        if (!string.IsNullOrWhiteSpace(configured))
        {
            return configured.Trim();
        }

        var (key, generated) = ReadOrCreate(
            configuration,
            "jwt-signing-key",
            () => Convert.ToBase64String(RandomNumberGenerator.GetBytes(SigningKeyBytes)));

        if (generated)
        {
            // Never the value: an access token is forgeable by anyone holding it.
            Log.Warning(
                "No Vorcall:JwtSigningKey configured, so one was generated and saved in {Path}. "
                + "Losing that file signs everyone out.",
                Path.Combine(DataDir(configuration), "jwt-signing-key"));
        }

        return key;
    }

    // Same file every boot, so restarting does not invalidate the key that is already out there.
    private static (string Value, bool Generated) ReadOrCreate(
        IConfiguration configuration,
        string name,
        Func<string> create)
    {
        var dir = DataDir(configuration);
        var path = Path.Combine(dir, name);

        try
        {
            if (File.Exists(path))
            {
                var existing = File.ReadAllText(path).Trim();
                if (existing.Length > 0)
                {
                    return (existing, false);
                }
            }

            Directory.CreateDirectory(dir);
            var value = create();
            File.WriteAllText(path, value + Environment.NewLine);

            // Best effort: Windows has no mode to set, and a bind mount may refuse it.
            if (!OperatingSystem.IsWindows())
            {
                try
                {
                    File.SetUnixFileMode(path, UnixFileMode.UserRead | UnixFileMode.UserWrite);
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or PlatformNotSupportedException)
                {
                    Log.Debug("Could not restrict the mode of {Path}: {Message}", path, ex.Message);
                }
            }

            return (value, true);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            throw new InvalidOperationException(
                $"No '{name}' was configured and '{path}' could not be written ({ex.Message}). "
                + "Set Vorcall:DataDir to a writable directory, or configure the secret explicitly.",
                ex);
        }
    }
}
