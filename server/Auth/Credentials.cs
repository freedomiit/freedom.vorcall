using System.Buffers.Text;
using System.Diagnostics.CodeAnalysis;
using System.Security.Cryptography;
using System.Text;

namespace Vorcall.Server.Auth;

// Credential formats and the secrets derived from them. Limits are the ones in PROTOCOL.md:
// counted in Unicode scalars.
public static class Credentials
{
    public const int UsernameMaxScalars = 32;
    public const int PasswordMinScalars = 8;
    public const int PasswordMaxScalars = 128;
    public const int InviteCodeLength = 20;

    // No I, L, O, 0 or 1: an invite code is read aloud and typed back by hand.
    private const string InviteAlphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

    private const int InviteGroupLength = 5;
    private const int RefreshTokenBytes = 32;

    public static bool TryNormalizeUsername(string? raw, out string username, out string normalized)
    {
        username = string.Empty;
        normalized = string.Empty;
        if (raw is null)
        {
            return false;
        }

        var trimmed = raw.Trim();
        var scalars = 0;
        foreach (var rune in trimmed.EnumerateRunes())
        {
            if (Rune.IsControl(rune))
            {
                return false;
            }

            if (++scalars > UsernameMaxScalars)
            {
                return false;
            }
        }

        if (scalars == 0)
        {
            return false;
        }

        username = trimmed;
        normalized = trimmed.ToUpperInvariant();
        return true;
    }

    // Not trimmed: leading and trailing spaces are part of a password.
    public static bool IsValidPassword([NotNullWhen(true)] string? raw)
    {
        if (raw is null)
        {
            return false;
        }

        var scalars = 0;
        foreach (var _ in raw.EnumerateRunes())
        {
            if (++scalars > PasswordMaxScalars)
            {
                return false;
            }
        }

        return scalars >= PasswordMinScalars;
    }

    public static bool TryNormalizeInviteCode(string? raw, out string code)
    {
        code = string.Empty;
        if (raw is null)
        {
            return false;
        }

        var normalized = new StringBuilder(InviteCodeLength);
        foreach (var character in raw.ToUpperInvariant())
        {
            if (character == '-' || char.IsWhiteSpace(character))
            {
                continue;
            }

            if (normalized.Length == InviteCodeLength || !InviteAlphabet.Contains(character))
            {
                return false;
            }

            normalized.Append(character);
        }

        if (normalized.Length != InviteCodeLength)
        {
            return false;
        }

        code = normalized.ToString();
        return true;
    }

    public static string NewInviteCode()
    {
        var code = new char[InviteCodeLength];
        for (var i = 0; i < code.Length; i++)
        {
            code[i] = InviteAlphabet[RandomNumberGenerator.GetInt32(InviteAlphabet.Length)];
        }

        return new string(code);
    }

    public static string FormatInviteCode(string code) => string.Join(
        '-',
        Enumerable.Range(0, code.Length / InviteGroupLength).Select(group => code.Substring(group * InviteGroupLength, InviteGroupLength)));

    public static string NewRefreshToken() => Base64Url.EncodeToString(RandomNumberGenerator.GetBytes(RefreshTokenBytes));

    public static string Sha256Hex(string value) => Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(value)));
}
