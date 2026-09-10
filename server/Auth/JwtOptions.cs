using Microsoft.IdentityModel.Tokens;

namespace Vorcall.Server.Auth;

public sealed record JwtOptions
{
    public const string Issuer = "vorcall";
    public const string Audience = "vorcall";

    public static readonly TimeSpan AccessTokenLifetime = TimeSpan.FromMinutes(15);
    public static readonly TimeSpan RefreshTokenLifetime = TimeSpan.FromDays(30);

    // HS256 with a key shorter than the digest weakens the signature, so anything under 32
    // bytes is treated as no key at all.
    private const int MinKeyBytes = 32;

    private JwtOptions(SymmetricSecurityKey key) => Key = key;

    public SymmetricSecurityKey Key { get; }

    public static JwtOptions FromConfiguration(IConfiguration configuration)
    {
        var configured = configuration["Vorcall:JwtSigningKey"];
        var key = string.IsNullOrWhiteSpace(configured) ? null : TryDecode(configured);
        if (key is null || key.Length < MinKeyBytes)
        {
            throw new InvalidOperationException("Missing or too short configuration 'Vorcall:JwtSigningKey' (base64 of at least 32 bytes).");
        }

        return new JwtOptions(new SymmetricSecurityKey(key));
    }

    private static byte[]? TryDecode(string base64)
    {
        try
        {
            return Convert.FromBase64String(base64);
        }
        catch (FormatException)
        {
            return null;
        }
    }
}
